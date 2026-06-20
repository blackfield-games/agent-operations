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

use arena_proto::{
    Action, ActionError, ActionIntent, Bam, MatchConfig, MatchPhase, MatchResult, Observation,
    ReplayRecord, SeatAction, SeatId, SeatOutcome, TeamId, TickRecord, Vec2, VersionMismatch,
    MOVE_INTENT_SCALE, POSITION_SCALE, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// East along +X — a seat on the left of the arena faces this way (toward
/// centre). The full turn is `u16`, so `0` is +X and [`WEST`] is -X.
const EAST: Bam = 0;
/// West along -X (half a turn) — a seat on the right of the arena faces centre.
const WEST: Bam = 0x8000;

/// Fixed-point scale of the octant facing unit vectors (Q12). Hit resolution
/// quantizes a seat's [`Bam`] facing to the nearest of eight octants so the beam
/// direction is an *exact* integer unit vector — no trig, no float, identical on
/// every platform. Eight-way aim is coarse by design for the reference core;
/// finer aim resolution is a later refinement.
const OCTANT_SCALE: i32 = 4096;
/// `round(OCTANT_SCALE / sqrt(2))` — the diagonal octant component.
const OCTANT_DIAG: i32 = 2896;
const OCTANTS: [(i32, i32); 8] = [
    (OCTANT_SCALE, 0),           // E
    (OCTANT_DIAG, OCTANT_DIAG),  // NE
    (0, OCTANT_SCALE),           // N
    (-OCTANT_DIAG, OCTANT_DIAG), // NW
    (-OCTANT_SCALE, 0),          // W
    (-OCTANT_DIAG, -OCTANT_DIAG),// SW
    (0, -OCTANT_SCALE),          // S
    (OCTANT_DIAG, -OCTANT_DIAG), // SE
];

/// The Q12 unit vector for a facing, snapped to the nearest octant. The `+4096`
/// is half an octant, so the division rounds to nearest rather than truncating.
fn octant_unit(bam: Bam) -> (i32, i32) {
    let idx = (((bam as u32) + 4096) >> 13) & 7;
    OCTANTS[idx as usize]
}

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
///
/// Every field is a determinant of the match outcome (movement, hit resolution,
/// scoring, spawns), so a [`MatchRecord`] embeds the `Rules` it ran under — a
/// record carrying the wrong tuning would replay to a different result. Serde so
/// the rules persist inside a self-determining record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Why the server refused an [`Action`] envelope at the Gateway boundary. A
/// refused action is not applied; the seat forfeits the tick exactly as if no
/// action had arrived, so a malformed or spoofed action can never advance state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The action's protocol version is not this build's.
    Version(VersionMismatch),
    /// The action names a different match than this one.
    WrongMatch { expected: Uuid, got: Uuid },
    /// The action claims a seat other than the connection's own — an agent may
    /// only act for itself.
    WrongSeat { expected: SeatId, got: SeatId },
    /// The action answers a tick that is not the current one (stale or ahead),
    /// so it cannot be applied to this tick.
    StaleTick { expected: u64, got: u64 },
    /// The seat is down (or not in the roster) — a corpse cannot act.
    SeatDown { seat: SeatId },
    /// The match is not in [`Live`], so no action is simulated.
    ///
    /// [`Live`]: MatchPhase::Live
    NotLive,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::Version(m) => write!(f, "action rejected: {m}"),
            RejectReason::WrongMatch { expected, got } => {
                write!(f, "action rejected: wrong match (expected {expected}, got {got})")
            }
            RejectReason::WrongSeat { expected, got } => {
                write!(f, "action rejected: wrong seat (expected {expected}, got {got})")
            }
            RejectReason::StaleTick { expected, got } => {
                write!(f, "action rejected: stale tick (expected {expected}, got {got})")
            }
            RejectReason::SeatDown { seat } => {
                write!(f, "action rejected: seat {seat} is down")
            }
            RejectReason::NotLive => write!(f, "action rejected: match is not live"),
        }
    }
}

impl std::error::Error for RejectReason {}

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
    /// Ticks remaining before this pawn may fire again.
    cooldown: u16,
    /// Cumulative damage this pawn has dealt to enemies — the match score.
    score: i32,
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
    /// The accepted (post-clamp) action stream, one record per simulated tick —
    /// the deterministic replay.
    ticks: Vec<TickRecord>,
    result: Option<MatchResult>,
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
                    cooldown: 0,
                    score: 0,
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
            ticks: Vec::new(),
            result: None,
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

    /// `true` only if `seat` is in the roster AND still alive — also the guard
    /// against an intent for an unknown seat.
    fn pawn_alive(&self, seat: SeatId) -> bool {
        self.pawns.iter().any(|p| p.seat == seat && p.alive)
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

    /// Validate a raw action envelope at the Gateway boundary and return the
    /// accepted, **clamped** intent. This is the single server-authoritative gate
    /// every seat's input passes through — the same path a human's input would
    /// take — so a crafted action can never be trusted as sent:
    ///
    /// - the protocol version must match (an action framed under another version
    ///   cannot be interpreted safely),
    /// - the match id and seat must be the connection's own (no acting for
    ///   another match or seat),
    /// - the tick must be the current one (a stale or future action is dropped,
    ///   not applied to the wrong tick),
    /// - the move magnitude is clamped to the rules ([`ActionIntent::clamped`]),
    ///   so no envelope can request god-mode speed.
    ///
    /// `seat` is the authoritative seat of the connection; `action.seat` is what
    /// the envelope claims, and the two must agree.
    pub fn ingest(&self, seat: SeatId, action: &Action) -> Result<ActionIntent, RejectReason> {
        action.validate().map_err(|ActionError::Version(m)| RejectReason::Version(m))?;
        if self.phase != MatchPhase::Live {
            return Err(RejectReason::NotLive);
        }
        if action.match_id != self.match_id {
            return Err(RejectReason::WrongMatch { expected: self.match_id, got: action.match_id });
        }
        if action.seat != seat {
            return Err(RejectReason::WrongSeat { expected: seat, got: action.seat });
        }
        if action.tick != self.tick {
            return Err(RejectReason::StaleTick { expected: self.tick, got: action.tick });
        }
        if !self.pawn_alive(seat) {
            return Err(RejectReason::SeatDown { seat });
        }
        Ok(action.intent.clamped())
    }

    /// Advance the match exactly one tick from the given intents. A seat absent
    /// from `intents` (or already down) forfeits the tick — it holds position and
    /// does not fire — so a slow or hung seat never stalls the match (the
    /// bounded-latency invariant; the transport enforces the wall-clock deadline
    /// and a timeout maps to an absent seat here). step trusts NO caller: every
    /// move is clamped here — idempotent on the already-clamped
    /// [`ingest`](Match::ingest)ed live path — and intents from downed or unknown
    /// seats are dropped, so the recorded stream is always canonical post-clamp
    /// and a forged over-speed replay re-runs clamped, not god-mode. The same
    /// seed + stream reproduces this tick exactly. A no-op once [`Ended`].
    ///
    /// [`Ended`]: MatchPhase::Ended
    pub fn step(&mut self, intents: &BTreeMap<SeatId, ActionIntent>) {
        if self.phase != MatchPhase::Live {
            return;
        }
        let current = self.tick;

        // Accept only intents from seats alive at the start of the tick, each
        // defensively clamped — this is what gets applied AND recorded, so no
        // caller (driver, replay, or direct) can move a pawn faster than the
        // rules or pad the replay with a corpse's action.
        let accepted: BTreeMap<SeatId, ActionIntent> = intents
            .iter()
            .filter(|(&seat, _)| self.pawn_alive(seat))
            .map(|(&seat, intent)| (seat, intent.clamped()))
            .collect();

        for p in self.pawns.iter_mut().filter(|p| p.alive) {
            p.cooldown = p.cooldown.saturating_sub(1);
        }

        // Move + aim, in seat order. A forfeited seat holds still.
        for i in 0..self.pawns.len() {
            if !self.pawns[i].alive {
                continue;
            }
            let seat = self.pawns[i].seat;
            let Some(intent) = accepted.get(&seat) else {
                self.pawns[i].vel = Vec2::ZERO;
                continue;
            };
            let max = self.rules.max_speed as i64;
            let dx = intent.move_dir.x as i64 * max / MOVE_INTENT_SCALE as i64;
            let dy = intent.move_dir.y as i64 * max / MOVE_INTENT_SCALE as i64;
            let bx = self.config.bounds.x as i64;
            let by = self.config.bounds.y as i64;
            let nx = (self.pawns[i].pos.x as i64 + dx).clamp(-bx, bx) as i32;
            let ny = (self.pawns[i].pos.y as i64 + dy).clamp(-by, by) as i32;
            self.pawns[i].vel = Vec2 { x: nx - self.pawns[i].pos.x, y: ny - self.pawns[i].pos.y };
            self.pawns[i].pos = Vec2 { x: nx, y: ny };
            self.pawns[i].facing = intent.aim;
        }

        // Reload + fire, in seat order. Sequential resolution: a pawn downed
        // earlier this tick cannot return fire, so a mutual-kill exchange is
        // decisive (seat order is the documented tie-break) rather than a double
        // KO that both sides survive or neither does.
        for i in 0..self.pawns.len() {
            if !self.pawns[i].alive {
                continue;
            }
            let seat = self.pawns[i].seat;
            let Some(intent) = accepted.get(&seat) else {
                continue;
            };
            if intent.buttons.reload {
                self.pawns[i].ammo = self.rules.mag_size;
                self.pawns[i].cooldown = self.rules.fire_cooldown;
                continue;
            }
            if intent.buttons.fire && self.pawns[i].cooldown == 0 && self.pawns[i].ammo > 0 {
                self.pawns[i].ammo -= 1;
                self.pawns[i].cooldown = self.rules.fire_cooldown;
                self.resolve_fire(i);
            }
        }

        let actions = accepted.iter().map(|(&seat, &intent)| SeatAction { seat, intent }).collect();
        self.ticks.push(TickRecord { tick: current, actions });

        self.tick += 1;
        self.maybe_finish();
    }

    /// Resolve one beam-hitscan shot from `shooter`: damage the nearest enemy
    /// whose body lies within the beam (in range, in front, within the lateral
    /// `hit_radius`). All integer: the beam direction is the exact octant unit
    /// vector, the in-front test is a dot-product sign, and the lateral offset is
    /// a squared perpendicular distance — no trig anywhere.
    fn resolve_fire(&mut self, shooter: usize) {
        let s = self.pawns[shooter];
        let (fx, fy) = octant_unit(s.facing);
        // i128 throughout: positions are i32 and bounded by config.bounds, so a
        // squared planar distance can exceed i64 at extreme (operator-set) arena
        // sizes. Widening keeps the hit test panic-free and exact regardless of
        // bounds, the same defensiveness the move clamp uses.
        let range2 = (self.rules.weapon_range as i128).pow(2);
        let radius2 = (self.rules.hit_radius as i128).pow(2);
        let mut best: Option<(usize, i128)> = None;
        for (j, t) in self.pawns.iter().enumerate() {
            if j == shooter || !t.alive || t.team == s.team {
                continue;
            }
            let dx = t.pos.x as i128 - s.pos.x as i128;
            let dy = t.pos.y as i128 - s.pos.y as i128;
            let dist2 = dx * dx + dy * dy;
            if dist2 > range2 {
                continue;
            }
            let dot = dx * fx as i128 + dy * fy as i128;
            if dot <= 0 {
                continue;
            }
            let proj = dot / OCTANT_SCALE as i128;
            let perp2 = dist2 - proj * proj;
            if perp2 > radius2 {
                continue;
            }
            if best.is_none_or(|(_, d)| dist2 < d) {
                best = Some((j, dist2));
            }
        }
        if let Some((j, _)) = best {
            let dmg = self.rules.damage.min(self.pawns[j].health);
            self.pawns[j].health -= dmg;
            if self.pawns[j].health == 0 {
                self.pawns[j].alive = false;
            }
            self.pawns[shooter].score += dmg as i32;
        }
    }

    /// End the match when at most one team still has an alive pawn (a winner, or
    /// everyone down) or the tick cap is reached, freezing the [`MatchResult`].
    /// This makes a single-team roster end on its first tick — a co-op/PvE mode
    /// needs enemy (e.g. NPC) teams or a different end rule.
    fn maybe_finish(&mut self) {
        let alive_teams: BTreeSet<TeamId> =
            self.pawns.iter().filter(|p| p.alive).map(|p| p.team).collect();
        if alive_teams.len() <= 1 || self.tick >= self.config.max_ticks {
            self.phase = MatchPhase::Ended;
            self.result = Some(self.build_result());
        }
    }

    fn build_result(&self) -> MatchResult {
        MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id: self.match_id,
            final_tick: self.tick,
            outcomes: self.outcomes(),
            replay_hash: hex::encode(self.build_replay().digest()),
        }
    }

    /// Per-seat outcomes, ascending by seat. Placement ranks alive over dead,
    /// then higher score, then lower seat; seats with the same alive flag and
    /// score share a placement.
    fn outcomes(&self) -> Vec<SeatOutcome> {
        let mut ranked: Vec<&Pawn> = self.pawns.iter().collect();
        ranked.sort_by(|a, b| {
            b.alive.cmp(&a.alive).then(b.score.cmp(&a.score)).then(a.seat.cmp(&b.seat))
        });
        let mut placement_of: BTreeMap<SeatId, u16> = BTreeMap::new();
        let mut place = 0u16;
        let mut prev: Option<(bool, i32)> = None;
        for (i, p) in ranked.iter().enumerate() {
            let key = (p.alive, p.score);
            if prev != Some(key) {
                place = (i + 1) as u16;
                prev = Some(key);
            }
            placement_of.insert(p.seat, place);
        }
        let mut outcomes: Vec<SeatOutcome> = self
            .pawns
            .iter()
            .map(|p| SeatOutcome {
                seat: p.seat,
                team: p.team,
                placement: placement_of[&p.seat],
                score: p.score,
                alive_at_end: p.alive,
            })
            .collect();
        outcomes.sort_by_key(|o| o.seat);
        outcomes
    }

    fn build_replay(&self) -> ReplayRecord {
        ReplayRecord {
            protocol_version: PROTOCOL_VERSION,
            match_id: self.match_id,
            seed: self.seed,
            seats: self.seats.clone(),
            ticks: self.ticks.clone(),
        }
    }

    /// The terminal result once the match has [`Ended`], else `None`.
    ///
    /// [`Ended`]: MatchPhase::Ended
    pub fn result(&self) -> Option<&MatchResult> {
        self.result.as_ref()
    }

    /// The complete [`MatchRecord`] for a finished match — the setup it ran under
    /// (`config` + `rules`), its [`ReplayRecord`], and the terminal
    /// [`MatchResult`], bundled into one self-determining artifact that
    /// [`MatchRecord::verify`] can re-run and check from scratch. `None` until the
    /// match has [`Ended`]: an in-progress match has no terminal result to commit,
    /// so there is nothing to record yet.
    ///
    /// [`Ended`]: MatchPhase::Ended
    pub fn to_record(&self) -> Option<MatchRecord> {
        let result = self.result.clone()?;
        Some(MatchRecord {
            config: self.config,
            rules: self.rules,
            replay: self.build_replay(),
            result,
        })
    }

    /// Consume the match for its deterministic [`ReplayRecord`] — seed, roster,
    /// and the ordered accepted-action stream, sufficient to re-run the match
    /// bit-for-bit via [`replay_match`].
    pub fn into_replay(self) -> ReplayRecord {
        ReplayRecord {
            protocol_version: PROTOCOL_VERSION,
            match_id: self.match_id,
            seed: self.seed,
            seats: self.seats,
            ticks: self.ticks,
        }
    }
}

/// A complete, self-determining record of one finished match.
///
/// A bare [`ReplayRecord`] carries the seed, roster, and action stream — but NOT
/// the [`MatchConfig`] (arena bounds, tick cap) or the [`Rules`] (all combat
/// tuning) the sim also consumes, so a `ReplayRecord` alone does not determine a
/// match: replayed under different bounds or damage it yields a different result.
/// A `MatchRecord` closes that gap by bundling the full determinant set (the
/// `config`, the `rules`, the `replay` inputs, and the terminal `result` they
/// produced), so [`verify`](MatchRecord::verify) re-runs it from the record
/// ALONE, with nothing supplied out of band. That self-containment is what an
/// on-chain settlement check (`contracts-agent-match-settlement` commits
/// `result.replay_hash`) and a spectator/replay feed both rely on: hand either
/// one this record and it can reproduce and check the match end to end.
///
/// It is plain serde (like the wire types), so the persisted form is its serde
/// representation; every container inside is an ordered `Vec`, so the encoding is
/// byte-stable and a round-trip through JSON re-verifies unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchRecord {
    /// The read-only rules summary (bounds, tick cap) the match ran under — a
    /// sim determinant: `bounds` clamps movement and `max_ticks` ends the match.
    pub config: MatchConfig,
    /// The server-authoritative combat tuning the match ran under — a sim
    /// determinant in every field (speed, hit resolution, scoring, spawns).
    pub rules: Rules,
    /// Seed, roster, and the ordered accepted-action stream — the per-tick inputs.
    pub replay: ReplayRecord,
    /// The terminal result the inputs produced, committing to
    /// [`ReplayRecord::digest`] via `replay_hash`. `verify` recomputes this and
    /// rejects the record if it does not match.
    pub result: MatchResult,
}

/// Why a [`MatchRecord`] failed [`verify`](MatchRecord::verify). Every malformed,
/// truncated, or tampered record maps to one of these — `verify` NEVER panics, so
/// a corrupt record is a cleanly rejected input, not a denial-of-service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// The record (replay or result) was built under a different
    /// [`PROTOCOL_VERSION`] than this core speaks, so it cannot be interpreted.
    Version(VersionMismatch),
    /// The replay and result name different matches — a spliced record.
    MatchIdMismatch { replay: Uuid, result: Uuid },
    /// The roster is empty, or `config.seats` disagrees with the roster length, or
    /// two seats share an id — the record cannot describe a real match.
    InvalidRoster,
    /// A recorded action names a seat that is not in the roster.
    UnknownSeat { tick: u64, seat: SeatId },
    /// A tick's actions are not in canonical ascending-unique seat order.
    SeatOrder { tick: u64 },
    /// The `ticks` are not the canonical contiguous `0,1,2,…` sequence (reordered,
    /// duplicated, or gapped) — the live sim records one record per tick in order.
    TickOrder { index: usize, tick: u64 },
    /// The recorded action stream did not drive the match to a terminal state, so
    /// it does not account for the whole match (e.g. a truncated record).
    NotTerminal,
    /// Re-running the inputs produced different outcomes than the record claims —
    /// a determinant was altered (seed, an action, the rules) or the result was
    /// tampered with.
    ResultMismatch,
    /// The outcomes reproduce, but the committed `replay_hash` does not match the
    /// digest of the re-run — a tampered or stale commitment.
    HashMismatch { expected: String, recomputed: String },
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::Version(m) => write!(f, "invalid replay: {m}"),
            ReplayError::MatchIdMismatch { replay, result } => {
                write!(f, "invalid replay: match id mismatch (replay {replay}, result {result})")
            }
            ReplayError::InvalidRoster => write!(f, "invalid replay: malformed roster"),
            ReplayError::UnknownSeat { tick, seat } => {
                write!(f, "invalid replay: tick {tick} names seat {seat} not in the roster")
            }
            ReplayError::SeatOrder { tick } => {
                write!(f, "invalid replay: tick {tick} actions are not in canonical seat order")
            }
            ReplayError::TickOrder { index, tick } => {
                write!(f, "invalid replay: tick {tick} at position {index} breaks canonical order")
            }
            ReplayError::NotTerminal => {
                write!(f, "invalid replay: action stream did not end the match")
            }
            ReplayError::ResultMismatch => {
                write!(f, "invalid replay: re-run did not reproduce the recorded result")
            }
            ReplayError::HashMismatch { expected, recomputed } => {
                write!(f, "invalid replay: replay hash mismatch (recorded {expected}, recomputed {recomputed})")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

/// A controller that answers each tick's observation with an action — the same
/// surface a human's controller and an external agent's `AgentController` both
/// present to the core. Returning `None` forfeits the tick (a hung or timed-out
/// agent maps here); the match advances regardless.
pub trait Policy {
    fn act(&mut self, obs: &Observation) -> Option<Action>;
}

/// Drive a match to its end with one policy per roster seat (indexed in roster
/// order). Each tick: observe every seat, let its policy answer, pass the answer
/// through the server-authoritative [`ingest`](Match::ingest) gate (a rejected or
/// absent action forfeits that seat's tick), then [`step`](Match::step). The loop
/// is bounded by the tick cap, so it always terminates even if every seat
/// forfeits. Panics if `policies.len()` does not match the roster.
pub fn run_match(mut m: Match, policies: &mut [Box<dyn Policy>]) -> Match {
    let seat_ids: Vec<SeatId> = m.seats().iter().map(|s| s.seat).collect();
    assert_eq!(policies.len(), seat_ids.len(), "one policy per roster seat");
    while m.phase() == MatchPhase::Live {
        let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
        for (idx, &seat) in seat_ids.iter().enumerate() {
            let obs = m.observe(seat);
            if let Some(action) = policies[idx].act(&obs) {
                if let Ok(intent) = m.ingest(seat, &action) {
                    intents.insert(seat, intent);
                }
            }
        }
        m.step(&intents);
    }
    m
}

/// Re-run a match from a recorded [`ReplayRecord`]'s action stream. `m` must be
/// freshly built with the same match id, config, rules, roster, and seed as the
/// recorded match (so spawns match); feeding the recorded post-clamp intents back
/// through [`step`](Match::step) reproduces the original result bit-for-bit.
pub fn replay_match(mut m: Match, replay: &ReplayRecord) -> Match {
    for tr in &replay.ticks {
        if m.phase() != MatchPhase::Live {
            break;
        }
        let intents: BTreeMap<SeatId, ActionIntent> =
            tr.actions.iter().map(|a| (a.seat, a.intent)).collect();
        m.step(&intents);
    }
    m
}

/// `true` if `b` is within `range` position units of `a` on the ground plane.
/// Squared comparison in `i128` so a large (operator-set) arena coordinate can't
/// overflow the distance check.
fn within(a: Vec2, b: Vec2, range: i32) -> bool {
    let dx = b.x as i128 - a.x as i128;
    let dy = b.y as i128 - a.y as i128;
    let r = range as i128;
    dx * dx + dy * dy <= r * r
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_proto::{ActionButtons, MatchConfig, SeatInfo};

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

    fn intent(move_dir: Vec2, aim: Bam, fire: bool) -> ActionIntent {
        ActionIntent {
            move_dir,
            aim,
            buttons: ActionButtons { fire, jump: false, ability: false, reload: false },
        }
    }

    fn action_at(m: &Match, seat: SeatId, intent: ActionIntent) -> Action {
        Action { protocol_version: PROTOCOL_VERSION, match_id: m.match_id(), seat, tick: m.tick(), intent }
    }

    fn step_with(m: &mut Match, intents: &[(SeatId, ActionIntent)]) {
        let map: BTreeMap<SeatId, ActionIntent> = intents.iter().copied().collect();
        m.step(&map);
    }

    /// Two seats spawned ~4 m apart (within weapon range), no jitter, so combat
    /// tests are exact.
    fn close_match(seed: u64) -> Match {
        let rules = Rules { spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), seed)
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

    #[test]
    fn ingest_accepts_well_formed_and_clamps_overlong_move() {
        let m = new_match(1);
        // FM2: a crafted 3-4-5 vector of magnitude 5000 is accepted but clamped to
        // the cap (600, 800) — no envelope buys god-mode speed.
        let a = action_at(&m, 0, intent(Vec2 { x: 3000, y: 4000 }, 0x4000, false));
        assert_eq!(m.ingest(0, &a).unwrap().move_dir, Vec2 { x: 600, y: 800 });
    }

    #[test]
    fn ingest_rejects_version_drift() {
        let m = new_match(1);
        let mut a = action_at(&m, 0, intent(Vec2::ZERO, 0, false));
        a.protocol_version = PROTOCOL_VERSION + 1;
        assert!(matches!(m.ingest(0, &a), Err(RejectReason::Version(_))));
    }

    #[test]
    fn ingest_rejects_acting_for_another_seat() {
        let m = new_match(1);
        // The envelope claims seat 1 but the connection is seat 0.
        let a = action_at(&m, 1, intent(Vec2::ZERO, 0, false));
        assert_eq!(m.ingest(0, &a), Err(RejectReason::WrongSeat { expected: 0, got: 1 }));
    }

    #[test]
    fn ingest_rejects_stale_or_future_tick() {
        let m = new_match(1);
        let mut a = action_at(&m, 0, intent(Vec2::ZERO, 0, false));
        a.tick = 99;
        assert_eq!(m.ingest(0, &a), Err(RejectReason::StaleTick { expected: 0, got: 99 }));
    }

    #[test]
    fn ingest_rejects_action_for_another_match() {
        let m = new_match(1);
        let mut a = action_at(&m, 0, intent(Vec2::ZERO, 0, false));
        a.match_id = Uuid::nil();
        assert!(matches!(m.ingest(0, &a), Err(RejectReason::WrongMatch { .. })));
    }

    #[test]
    fn step_moves_pawn_by_clamped_speed_and_reports_velocity() {
        let mut m = new_match(1);
        let before = m.observe(0).own.position;
        step_with(&mut m, &[(0, intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, EAST, false))]);
        let after = m.observe(0).own;
        assert_eq!(after.position.x - before.x, Rules::default().max_speed);
        assert_eq!(after.position.y, before.y);
        assert_eq!(after.velocity, Vec2 { x: Rules::default().max_speed, y: 0 });
        assert_eq!(m.tick(), 1);
    }

    #[test]
    fn a_shot_damages_an_enemy_in_the_beam() {
        // FM2: seat 0 (left, facing East) fires at seat 1 (right); seat 1 forfeits.
        let mut m = close_match(1);
        let before = m.observe(1).own.health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.observe(1).own.health, before - Rules::default().damage);
        assert_eq!(m.observe(0).own.health, before, "the shooter is unharmed");
    }

    #[test]
    fn cooldown_blocks_an_immediate_second_shot() {
        let mut m = close_match(1);
        let start = m.observe(1).own.health;
        // Fire on two consecutive ticks; the second is on cooldown, so one hit.
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.observe(1).own.health, start - Rules::default().damage);
    }

    #[test]
    fn movement_is_clamped_to_the_arena_bounds() {
        // FM2: drive seat 1 outward every tick; it pins at the edge, never past.
        let bounds = Vec2 { x: 21 * POSITION_SCALE, y: 50 * POSITION_SCALE };
        let cfg = MatchConfig { bounds, ..config(2) };
        let rules = Rules { spawn_jitter: 0, ..Default::default() };
        let mut m = Match::new(MID.parse().unwrap(), cfg, rules, two_seats(), 1);
        for _ in 0..100 {
            step_with(&mut m, &[(1, intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, WEST, false))]);
        }
        let p = m.observe(1).own;
        assert_eq!(p.position.x, 21 * POSITION_SCALE, "pinned at the bound");
        assert_eq!(p.velocity, Vec2::ZERO, "no displacement once pinned");
    }

    #[test]
    fn the_hit_test_does_not_overflow_at_extreme_arena_bounds() {
        // A squared planar distance between near-i32 coordinates exceeds i64; the
        // i128 hit math must not panic when checking such a (far, out-of-range)
        // target. Extreme operator config, not agent-reachable — defensive only.
        let edge = i32::MAX;
        let cfg = MatchConfig { bounds: Vec2 { x: edge, y: edge }, ..config(2) };
        let rules = Rules { spawn_radius: edge, spawn_jitter: 0, ..Default::default() };
        let mut m = Match::new(MID.parse().unwrap(), cfg, rules, two_seats(), 1);
        let before = m.observe(1).own.health;
        // Seats spawn at opposite i32 extremes; firing must not panic, and the
        // far target is out of range, so no hit lands — the point is no panic.
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.observe(1).own.health, before);
    }

    #[test]
    fn a_kill_ends_the_match_with_a_decisive_result() {
        let mut m = close_match(1);
        for _ in 0..200 {
            if m.phase() == MatchPhase::Ended {
                break;
            }
            step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        }
        assert_eq!(m.phase(), MatchPhase::Ended);
        let r = m.result().expect("an ended match has a result").clone();
        let s0 = r.outcomes.iter().find(|o| o.seat == 0).unwrap();
        let s1 = r.outcomes.iter().find(|o| o.seat == 1).unwrap();
        assert!(s0.alive_at_end && s0.placement == 1, "the survivor wins");
        assert!(!s1.alive_at_end && s1.placement == 2, "the downed seat places last");
        assert!(s0.score >= Rules::default().start_health as i32, "dealt at least lethal damage");
        assert_eq!(r.final_tick, m.tick());
        // The result commits to a non-empty replay digest.
        assert_eq!(r.replay_hash.len(), 64);
    }

    fn dist2(a: Vec2, b: Vec2) -> i64 {
        let dx = b.x as i64 - a.x as i64;
        let dy = b.y as i64 - a.y as i64;
        dx * dx + dy * dy
    }

    /// Nearest octant BAM toward `(dx, dy)` — integer argmax over the octant unit
    /// vectors, so the policy is float-free and a played match stays
    /// byte-reproducible on every platform.
    fn octant_bam_toward(dx: i32, dy: i32) -> Bam {
        let mut best_idx = 0usize;
        let mut best_dot = i64::MIN;
        for (i, &(ox, oy)) in OCTANTS.iter().enumerate() {
            let d = dx as i64 * ox as i64 + dy as i64 * oy as i64;
            if d > best_dot {
                best_dot = d;
                best_idx = i;
            }
        }
        (best_idx as u32 * 8192) as Bam
    }

    fn octant_move(dx: i32, dy: i32) -> Vec2 {
        let (ox, oy) = octant_unit(octant_bam_toward(dx, dy));
        Vec2 { x: ox * MOVE_INTENT_SCALE / OCTANT_SCALE, y: oy * MOVE_INTENT_SCALE / OCTANT_SCALE }
    }

    /// Closes on the nearest visible enemy until within weapon range, then holds
    /// and fires. Integer-only — no float anywhere — so a played match is
    /// byte-reproducible.
    struct Seeker;
    impl Policy for Seeker {
        fn act(&mut self, obs: &Observation) -> Option<Action> {
            let me = &obs.own;
            if !me.alive {
                return None;
            }
            let target =
                obs.visible.iter().filter(|e| e.team != me.team).min_by_key(|e| dist2(me.position, e.position))?;
            let dx = target.position.x - me.position.x;
            let dy = target.position.y - me.position.y;
            let range2 = (Rules::default().weapon_range as i64).pow(2);
            let in_range = dist2(me.position, target.position) <= range2;
            let move_dir = if in_range { Vec2::ZERO } else { octant_move(dx, dy) };
            Some(Action {
                protocol_version: obs.protocol_version,
                match_id: obs.match_id,
                seat: obs.seat,
                tick: obs.tick,
                intent: ActionIntent {
                    move_dir,
                    aim: octant_bam_toward(dx, dy),
                    buttons: ActionButtons { fire: in_range, jump: false, ability: false, reload: false },
                },
            })
        }
    }

    /// Never answers — a hung or timed-out seat. Every tick is forfeited.
    struct Silent;
    impl Policy for Silent {
        fn act(&mut self, _obs: &Observation) -> Option<Action> {
            None
        }
    }

    fn play(seed: u64) -> Match {
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker)];
        run_match(close_match(seed), &mut policies)
    }

    #[test]
    fn full_a2a_match_is_decisive() {
        let m = play(1);
        assert_eq!(m.phase(), MatchPhase::Ended);
        let r = m.result().unwrap();
        assert!(r.outcomes.iter().any(|o| o.placement == 1 && o.alive_at_end), "a winner emerges");
        assert!(r.outcomes.iter().any(|o| !o.alive_at_end), "a loser is downed");
        assert_eq!(r.replay_hash.len(), 64);
    }

    #[test]
    fn a_played_match_replays_byte_for_byte() {
        // FM1: run two trivial agents to the end, then re-run from the recorded
        // action stream on a fresh same-seed match — identical result + digest.
        let played = play(1);
        let result = played.result().unwrap().clone();
        let replay = played.into_replay();

        let replayed = replay_match(close_match(1), &replay);
        assert_eq!(replayed.phase(), MatchPhase::Ended);
        assert_eq!(replayed.result().unwrap(), &result, "replay diverged from the live result");
        assert_eq!(replayed.into_replay().digest(), replay.digest(), "replay digest diverged");
    }

    #[test]
    fn same_seed_is_byte_identical_across_runs() {
        // FM1: the whole pipeline (seed spawns + integer policy + integer sim) is
        // deterministic, so two independent runs are byte-identical — the basis
        // for grading and on-chain attestation.
        let a = play(7).into_replay();
        let b = play(7).into_replay();
        assert_eq!(a.digest(), b.digest());
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
    }

    #[test]
    fn a_hung_seat_forfeits_every_tick_and_the_match_still_ends() {
        // FM3 (bounded latency): seat 1 never answers; the match must still
        // advance and end rather than stall, and the hung seat does nothing.
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Silent)];
        let m = run_match(close_match(1), &mut policies);
        assert_eq!(m.phase(), MatchPhase::Ended);
        let r = m.result().unwrap();
        let s1 = r.outcomes.iter().find(|o| o.seat == 1).unwrap();
        assert!(!s1.alive_at_end, "the active seat downed the hung one");
        assert_eq!(s1.score, 0, "the hung seat never acted");
    }

    #[test]
    fn step_clamps_a_forged_overspeed_intent() {
        // step trusts no caller: an unclamped intent that never passed ingest is
        // still clamped before it moves a pawn — no direct caller buys speed.
        let mut m = new_match(1);
        let before = m.observe(0).own.position.x;
        step_with(&mut m, &[(0, intent(Vec2 { x: 1_000_000, y: 0 }, EAST, false))]);
        assert_eq!(m.observe(0).own.position.x - before, Rules::default().max_speed);
    }

    #[test]
    fn a_forged_overspeed_replay_re_runs_clamped() {
        // The cross-review exploit: a hand-forged ReplayRecord with a god-mode
        // move_dir must NOT reproduce god-mode movement on replay. step clamps it,
        // so a forged stream cannot be re-derived into a forged result.
        let seed = 1;
        let forged = ReplayRecord {
            protocol_version: PROTOCOL_VERSION,
            match_id: MID.parse().unwrap(),
            seed,
            seats: two_seats(),
            ticks: vec![TickRecord {
                tick: 0,
                actions: vec![SeatAction { seat: 0, intent: intent(Vec2 { x: 1_000_000, y: 0 }, EAST, false) }],
            }],
        };
        let start = close_match(seed).observe(0).own.position.x;
        let replayed = replay_match(close_match(seed), &forged);
        assert_eq!(
            replayed.observe(0).own.position.x - start,
            Rules::default().max_speed,
            "forged over-speed was clamped on replay"
        );
    }

    #[test]
    fn a_downed_seat_cannot_act() {
        // 3-seat FFA so the match stays Live after one seat is downed.
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "a".into() },
            SeatInfo { seat: 1, team: 1, controller: "b".into() },
            SeatInfo { seat: 2, team: 2, controller: "c".into() },
        ];
        let rules = Rules { spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        let mut m = Match::new(MID.parse().unwrap(), config(3), rules, seats, 1);
        // Seat 0 (left) guns down seat 1 (centre); seats 1 and 2 forfeit.
        while m.observe(1).own.alive && m.phase() == MatchPhase::Live {
            step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        }
        assert_eq!(m.phase(), MatchPhase::Live, "seat 2 keeps the match alive");
        assert!(!m.observe(1).own.alive, "seat 1 is down");
        // ingest rejects an action from the downed seat.
        let a = action_at(&m, 1, intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, EAST, false));
        assert_eq!(m.ingest(1, &a), Err(RejectReason::SeatDown { seat: 1 }));
        // And a direct step with the downed seat's intent moves nothing and is
        // not recorded — the replay carries no corpse actions.
        let before = m.observe(1).own.position;
        step_with(&mut m, &[(1, intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, EAST, false))]);
        assert_eq!(m.observe(1).own.position, before, "a downed pawn does not move");
        let replay = m.into_replay();
        assert!(
            replay.ticks.last().unwrap().actions.iter().all(|sa| sa.seat != 1),
            "a downed seat's intent is never recorded"
        );
    }
}
