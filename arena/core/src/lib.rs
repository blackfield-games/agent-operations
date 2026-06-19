//! Headless, server-authoritative match core — the reference twin of the UE5
//! dedicated server that implements the arena-01 Gateway in plain Rust.
//!
//! This is NOT a second source of gameplay truth. It is the protocol-conformance
//! reference and the A2A benchmark harness: a deterministic, fixed-tick match
//! simulation that ingests each seat's [`Action`], validates and clamps it the
//! same way a human's input would be validated, advances combat and scoring, and
//! emits each seat's parity-bounded [`Observation`]. Given a seed and the same
//! action stream it produces byte-identical results, so a match can be replayed,
//! graded, and attested on-chain.
//!
//! Everything that touches match state is integer fixed-point — positions in
//! `arena_proto::POSITION_SCALE` units, facing in [`Bam`], a [`SplitMix64`] PRNG
//! seeded from the match seed. There is deliberately no float, no `HashMap`
//! iteration over state, and no wall-clock anywhere in the tick path, because any
//! one of those would make a replay diverge and break grading.

use arena_proto::{Bam, MatchPhase, SeatId, TeamId, Vec2, POSITION_SCALE};
use uuid::Uuid;

/// East along +X — a seat on the left of the arena faces this way (toward
/// centre). The full turn is `u16`, so `0` is +X and [`WEST`] is -X.
const EAST: Bam = 0;
/// West along -X (half a turn) — a seat on the right of the arena faces centre.
const WEST: Bam = 0x8000;

/// A deterministic SplitMix64 PRNG. Pure integer state so spawns (and any future
/// sim randomness) are byte-reproducible from the match seed on every platform —
/// the same reason the wire types avoid floats.
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `lo..=hi`, inclusive. `lo <= hi` is required by the caller; the
    /// modulo is slightly biased for large spans but the spawn jitter spans are
    /// tiny, so the bias is immaterial and — crucially — deterministic.
    pub fn range_i32(&mut self, lo: i32, hi: i32) -> i32 {
        let span = (hi as i64 - lo as i64 + 1) as u64;
        lo + (self.next_u64() % span) as i32
    }
}

/// The combat tuning a match runs under — distinct from `arena_proto::MatchConfig`
/// (which is the read-only rules summary sent to agents). These are the
/// server-authoritative constants the sim clamps and resolves against; an agent
/// never sets them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    /// Max planar displacement per tick at full move intent, in position units.
    pub max_speed: i32,
    /// Beam-hitscan reach, in position units.
    pub weapon_range: i32,
    /// Lateral half-width of the hitscan beam, in position units — the aim
    /// tolerance that lets the coarse 8-way facing still land a shot.
    pub hit_radius: i32,
    /// Damage one landed shot deals.
    pub damage: u16,
    /// Ticks between shots; a pawn may fire only when its cooldown is `0`.
    pub fire_cooldown: u16,
    /// Rounds a full magazine holds; `reload` refills to this.
    pub mag_size: u16,
    /// How far a seat can perceive another entity, in position units.
    pub perception_range: i32,
    /// Starting (and max) health.
    pub start_health: u16,
    /// Half-width of the spawn line; seats spread across `[-r, +r]` on the X axis.
    pub spawn_radius: i32,
    /// Per-axis spawn jitter so the seed perturbs the opening, in position units.
    pub spawn_jitter: i32,
    /// Microseconds a seat has to answer an observation before the tick is
    /// forfeited on its behalf — carried on every [`Observation`].
    pub action_deadline_micros: u32,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            max_speed: 200,             // 0.2 m/tick → ~6 m/s at 30 Hz
            weapon_range: 30 * POSITION_SCALE,
            hit_radius: 1500,           // 1.5 m beam radius
            damage: 25,                 // four shots to down a full-health pawn
            fire_cooldown: 6,           // five shots/sec at 30 Hz
            mag_size: 30,
            perception_range: 40 * POSITION_SCALE,
            start_health: 100,
            spawn_radius: 20 * POSITION_SCALE,
            spawn_jitter: 2 * POSITION_SCALE,
            action_deadline_micros: 50_000,
        }
    }
}

/// One pawn's authoritative state. The agent never sees this struct — it sees a
/// parity-bounded [`Observation`] derived from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pawn {
    seat: SeatId,
    team: TeamId,
    pos: Vec2,
    z: i32,
    facing: Bam,
    /// The move delta actually applied last tick (post-clamp), reported to the
    /// owning seat as its velocity.
    vel: Vec2,
    health: u16,
    max_health: u16,
    ammo: u16,
    alive: bool,
}

/// The arena match: roster, authoritative pawn state, and the lifecycle phase.
/// A match advances one fixed tick at a time and never moves backward.
pub struct Match {
    match_id: Uuid,
    config: arena_proto::MatchConfig,
    rules: Rules,
    seats: Vec<arena_proto::SeatInfo>,
    pawns: Vec<Pawn>,
    tick: u64,
    phase: MatchPhase,
    seed: u64,
}

impl Match {
    /// Seat the roster and spawn its pawns, then open the match in [`Live`]. The
    /// headless reference begins simulating immediately — lobby/countdown
    /// ([`Lobby`]/[`Starting`]) are matchmaking concerns layered on later, not the
    /// core sim's job — so observations stream from tick 0.
    ///
    /// Spawns are deterministic from `seed`: seats spread evenly along the X axis
    /// in `[-spawn_radius, +spawn_radius]`, each jittered by the PRNG and facing
    /// the arena centre. `config.seats` must equal `seats.len()`.
    ///
    /// [`Live`]: MatchPhase::Live
    /// [`Lobby`]: MatchPhase::Lobby
    /// [`Starting`]: MatchPhase::Starting
    pub fn new(
        match_id: Uuid,
        config: arena_proto::MatchConfig,
        rules: Rules,
        seats: Vec<arena_proto::SeatInfo>,
        seed: u64,
    ) -> Self {
        let n = seats.len();
        let mut rng = SplitMix64::new(seed);
        let pawns = seats
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let base_x = if n <= 1 {
                    0
                } else {
                    let span = 2 * rules.spawn_radius as i64;
                    (-(rules.spawn_radius as i64) + (i as i64 * span) / (n as i64 - 1)) as i32
                };
                let jx = rng.range_i32(-rules.spawn_jitter, rules.spawn_jitter);
                let jy = rng.range_i32(-rules.spawn_jitter, rules.spawn_jitter);
                let pos = Vec2 { x: base_x + jx, y: jy };
                let facing = if pos.x > 0 { WEST } else { EAST };
                Pawn {
                    seat: s.seat,
                    team: s.team,
                    pos,
                    z: 0,
                    facing,
                    vel: Vec2::ZERO,
                    health: rules.start_health,
                    max_health: rules.start_health,
                    ammo: rules.mag_size,
                    alive: true,
                }
            })
            .collect();
        Match {
            match_id,
            config,
            rules,
            seats,
            pawns,
            tick: 0,
            phase: MatchPhase::Live,
            seed,
        }
    }

    pub fn phase(&self) -> MatchPhase {
        self.phase
    }

    pub fn tick(&self) -> u64 {
        self.tick
    }

    pub fn match_id(&self) -> Uuid {
        self.match_id
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The read-only rules summary this match runs under — the same value sent to
    /// agents at `GatewayMsg::Start`.
    pub fn config(&self) -> arena_proto::MatchConfig {
        self.config
    }

    /// The roster, in seat order.
    pub fn seats(&self) -> &[arena_proto::SeatInfo] {
        &self.seats
    }

    fn pawn(&self, seat: SeatId) -> &Pawn {
        self.pawns
            .iter()
            .find(|p| p.seat == seat)
            .expect("seat is in the roster")
    }

    /// Build the parity-bounded observation for one seat: its own pawn in full,
    /// plus only the entities it can perceive this tick (alive pawns within
    /// `perception_range`, never itself), in ascending `entity_id` order so the
    /// snapshot is canonical. The absence of any other field is the security
    /// bound — there is no path here to full world state.
    pub fn observe(&self, seat: SeatId) -> arena_proto::Observation {
        let me = self.pawn(seat);
        let mut visible: Vec<arena_proto::VisibleEntity> = self
            .pawns
            .iter()
            .filter(|p| p.seat != seat && p.alive)
            .filter(|p| within(me.pos, p.pos, self.rules.perception_range))
            .map(|p| arena_proto::VisibleEntity {
                entity_id: p.seat as u32,
                kind: arena_proto::EntityKind::Player,
                team: p.team,
                position: p.pos,
                z: p.z,
                facing: p.facing,
                in_line_of_sight: true,
            })
            .collect();
        visible.sort_by_key(|e| e.entity_id);
        arena_proto::Observation {
            protocol_version: arena_proto::PROTOCOL_VERSION,
            match_id: self.match_id,
            seat,
            tick: self.tick,
            phase: self.phase,
            deadline_micros: self.rules.action_deadline_micros,
            own: arena_proto::SelfState {
                seat: me.seat,
                team: me.team,
                position: me.pos,
                z: me.z,
                facing: me.facing,
                velocity: me.vel,
                health: me.health,
                max_health: me.max_health,
                ammo: me.ammo,
                alive: me.alive,
            },
            visible,
        }
    }
}

/// `true` if `b` is within `range` position units of `a` on the ground plane.
/// Squared comparison in `i64` so an attacker-sized coordinate can't overflow.
fn within(a: Vec2, b: Vec2, range: i32) -> bool {
    let dx = b.x as i64 - a.x as i64;
    let dy = b.y as i64 - a.y as i64;
    let r = range as i64;
    dx * dx + dy * dy <= r * r
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_proto::{MatchConfig, SeatInfo};

    const MID: &str = "550e8400-e29b-41d4-a716-446655440000";
    /// First SplitMix64 output for seed 1, verified against the reference
    /// implementation (seed 0 yields the canonical `0xe220a8397b1dcdaf`).
    const GOLDEN_SPLITMIX64_SEED1: u64 = 0x910a_2dec_8902_5cc1;

    fn config(seats: u8) -> MatchConfig {
        MatchConfig {
            tick_hz: 30,
            max_ticks: 3600,
            bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
            seats,
        }
    }

    fn two_seats() -> Vec<SeatInfo> {
        vec![
            SeatInfo { seat: 0, team: 0, controller: "0xaaaa".into() },
            SeatInfo { seat: 1, team: 1, controller: "0xbbbb".into() },
        ]
    }

    fn new_match(seed: u64) -> Match {
        Match::new(MID.parse().unwrap(), config(2), Rules::default(), two_seats(), seed)
    }

    #[test]
    fn splitmix_is_deterministic_and_stable() {
        let mut a = SplitMix64::new(0xdead_beef);
        let mut b = SplitMix64::new(0xdead_beef);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        // A golden value pins the PRNG so spawns never silently shift.
        let mut g = SplitMix64::new(1);
        assert_eq!(g.next_u64(), GOLDEN_SPLITMIX64_SEED1);
    }

    #[test]
    fn match_opens_live_at_tick_zero() {
        let m = new_match(1);
        assert_eq!(m.phase(), MatchPhase::Live);
        assert_eq!(m.tick(), 0);
    }

    #[test]
    fn spawns_are_seed_deterministic_and_mirrored() {
        let a = new_match(7);
        let b = new_match(7);
        // Same seed → identical spawns.
        assert_eq!(a.observe(0).own.position, b.observe(0).own.position);
        // Seats spread across the X axis: seat 0 left, seat 1 right, each facing
        // the centre.
        assert!(a.observe(0).own.position.x < 0);
        assert!(a.observe(1).own.position.x > 0);
        assert_eq!(a.observe(0).own.facing, EAST);
        assert_eq!(a.observe(1).own.facing, WEST);
        // A different seed perturbs the opening.
        let c = new_match(8);
        assert_ne!(a.observe(0).own.position, c.observe(0).own.position);
    }

    #[test]
    fn observation_is_parity_bounded() {
        let m = new_match(3);
        let obs = m.observe(0);
        // Own state is full and is the observer's own seat.
        assert_eq!(obs.own.seat, 0);
        assert_eq!(obs.own.health, Rules::default().start_health);
        // The observer never appears in its own visible set.
        assert!(obs.visible.iter().all(|e| e.entity_id != 0));
        // Within perception range, the enemy is visible — as a Player entity with
        // only perceivable fields (the type itself forbids hidden state).
        assert_eq!(obs.visible.len(), 1);
        assert_eq!(obs.visible[0].entity_id, 1);
        assert_eq!(obs.visible[0].kind, arena_proto::EntityKind::Player);
    }

    #[test]
    fn perception_range_hides_distant_entities() {
        // Spawn far apart with a tiny perception range: no one is visible.
        let rules = Rules { perception_range: 1, ..Default::default() };
        let m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), 5);
        assert!(m.observe(0).visible.is_empty());
        assert!(m.observe(1).visible.is_empty());
    }
}
