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
    check_version, Action, ActionError, ActionIntent, Bam, Blocker, MatchConfig, MatchPhase,
    MatchResult, Observation, ReplayRecord, SeatAction, SeatId, SeatOutcome, TeamId, TickRecord,
    Vec2, VersionMismatch, MOVE_INTENT_SCALE, POSITION_SCALE, PROTOCOL_VERSION,
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

/// The octant index (`0..=7`, in [`OCTANTS`] order: E, NE, N, …) a [`Bam`] facing
/// snaps to. `+4096` is half an octant, so the division rounds to nearest rather
/// than truncating. Shared by [`octant_unit`] (the beam direction) and the FOV
/// cone, so a seat's aim and its perception cone are quantized identically.
fn octant_index(bam: Bam) -> usize {
    ((((bam as u32) + 4096) >> 13) & 7) as usize
}

/// The Q12 unit vector for a facing, snapped to the nearest octant.
fn octant_unit(bam: Bam) -> (i32, i32) {
    OCTANTS[octant_index(bam)]
}

/// The octant (an [`OCTANTS`] index) a bearing vector points into — the integer
/// argmax of the dot product against the eight octant unit vectors, so a direction
/// is classified into the same partition [`octant_index`] snaps a [`Bam`] into,
/// with no trig. `i64` keeps the dot exact at any (operator-set) arena coordinate;
/// ties resolve to the lower index (`>` is strict), a fixed, deterministic
/// convention. A zero vector returns octant 0 — callers handle the no-bearing case.
fn bearing_octant(dx: i64, dy: i64) -> usize {
    let mut best = 0;
    let mut best_dot = i64::MIN;
    for (i, &(ox, oy)) in OCTANTS.iter().enumerate() {
        let dot = dx * ox as i64 + dy * oy as i64;
        if dot > best_dot {
            best_dot = dot;
            best = i;
        }
    }
    best
}

/// Circular distance between two octant indices on the 8-ring, `0..=4`: the min of
/// the two ways round, so octants adjacent across the `0/7` seam are distance 1,
/// not 7.
fn circular_octant_distance(a: usize, b: usize) -> usize {
    let diff = (a + 8 - b) % 8;
    diff.min(8 - diff)
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

/// How a match's weapon resolves a fire — the match-level weapon model, a
/// server-authoritative [`Rules`] field (never sent to agents, the same posture as
/// every other combat constant; agents learn the weapon's behavior empirically).
///
/// [`Hitscan`](WeaponMode::Hitscan) is the default and resolves a shot instantly
/// along the beam ([`resolve_fire`](Match::resolve_fire)); a match left at the
/// default is byte-identical to every pre-projectile match and replay.
/// [`Projectile`](WeaponMode::Projectile) instead spawns a traveling shot that
/// advances per tick and can be dodged. The mode is a determinant of the outcome,
/// so it rides in the [`Rules`] a [`MatchRecord`] commits — a record re-run under a
/// different mode reproduces a different result and is rejected by
/// [`verify`](MatchRecord::verify), exactly as a tampered `damage` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeaponMode {
    /// Instant beam hitscan — the shot lands the tick it is fired.
    #[default]
    Hitscan,
    /// A traveling projectile — the shot spawns and flies, hitting only when its
    /// swept path crosses an enemy body on a later (or the same point-blank) tick.
    Projectile,
}

/// The default projectile travel speed ([`Rules::projectile_speed`]): 2 m/tick,
/// faster than the default `max_speed` so a shot outruns a strafing pawn yet is
/// slow enough to dodge over its flight. The `serde(default)` for the field, so a
/// `Rules` serialized before projectiles existed deserializes to this — harmless,
/// since the same record also defaults `weapon_mode` to `Hitscan`, which ignores it.
fn default_projectile_speed() -> i32 {
    2 * POSITION_SCALE
}

/// Entity-id base for projectiles, above the pawn id space (a pawn's `entity_id` is
/// its `SeatId`, ≤ 255). Each spawn takes the next id from here, so projectile and
/// pawn ids never collide and the canonical ascending-id visible set lists pawns
/// first, then projectiles.
const PROJECTILE_ID_BASE: u32 = 1 << 16;

/// DoS backstop: the most projectiles one match keeps in flight at once. Per-tick
/// collision is O(live · seats), so an unbounded live set under a fire-every-tick
/// agent (e.g. a `fire_cooldown == 0` rule) would grow per-tick work without bound; a
/// fire at the cap spends its ammo but spawns nothing. Generous — default fire
/// cadence + range-bounded flight keep a real seat to a handful of live shots — so it
/// never constrains real play, only the worst case.
pub const MAX_LIVE_PROJECTILES: usize = 1024;

/// Termination backstop: a projectile is force-expired after this many ticks of
/// flight regardless of range. Range-expiry (past `weapon_range`) is the normal end
/// and fires far sooner for any sane config; this guarantees a projectile ALWAYS
/// terminates even when the octant-snapped velocity rounds to zero (a sub-octant-scale
/// `projectile_speed`), so the live set can never retain a motionless shot forever.
const MAX_PROJECTILE_LIFETIME: u16 = 600;

/// Overflow guard + sanity clamp on `projectile_speed` at spawn: bounds a projectile's
/// per-tick travel (hence the swept segment length) so the `i128` segment-vs-disc math
/// cannot overflow at any in-bounds arena coordinate. ~1048 m/tick — absurdly fast, so
/// as a clamp it never constrains a real shot; it exists only so a misconfigured (or
/// hand-crafted record's) extreme speed degrades to a fast-but-finite shot, not a panic.
const MAX_PROJECTILE_SPEED: i32 = 1 << 20;

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
    /// tolerance that lets the coarse 8-way facing still land a shot. In
    /// [`WeaponMode::Projectile`] it doubles as the pawn body half-width a
    /// projectile's swept path must reach to hit.
    pub hit_radius: i32,
    /// How a fire resolves: instant [`WeaponMode::Hitscan`] (the default,
    /// byte-identical to every pre-projectile match) or a traveling
    /// [`WeaponMode::Projectile`]. `serde(default)` resolves to `Hitscan` so a
    /// record written before this field replays under the model it actually ran.
    #[serde(default)]
    pub weapon_mode: WeaponMode,
    /// Projectile travel speed in position units per tick (only consulted in
    /// [`WeaponMode::Projectile`]). Snapped to the firing octant and clamped to a
    /// sane non-negative bound at spawn, so it never produces an over-fast shot that
    /// could overflow the swept-collision integer math.
    #[serde(default = "default_projectile_speed")]
    pub projectile_speed: i32,
    /// Damage one landed shot deals.
    pub damage: u16,
    /// Ticks between shots; a pawn may fire only when its cooldown is `0`.
    pub fire_cooldown: u16,
    /// Rounds a full magazine holds; `reload` refills to this.
    pub mag_size: u16,
    /// How far a seat can perceive another entity, in position units.
    pub perception_range: i32,
    /// Forward field-of-view half-width as an octant spread (`0..=4`): an enemy in
    /// `perception_range` is perceived only if its bearing is within this many
    /// octants of the seat's facing. `4` (the default) is the full circle —
    /// omnidirectional, byte-identical to range-only perception; `0` is the facing
    /// octant alone (~45°). `serde(default)` resolves to `4` so a record written
    /// before this field deserializes to the omnidirectional behavior it ran under.
    #[serde(default = "full_circle_fov")]
    pub fov_octant_spread: u8,
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

/// The `serde(default)` for [`Rules::fov_octant_spread`]: full circle, so a record
/// serialized before the field existed deserializes to the omnidirectional
/// perception it actually ran under (not the narrowest cone `u8::default()` would give).
fn full_circle_fov() -> u8 {
    4
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            max_speed: 200,             // 0.2 m/tick → ~6 m/s at 30 Hz
            weapon_range: 30 * POSITION_SCALE,
            hit_radius: 1500,           // 1.5 m beam radius
            weapon_mode: WeaponMode::Hitscan,
            projectile_speed: default_projectile_speed(),
            damage: 25,                 // four shots to down a full-health pawn
            fire_cooldown: 6,           // five shots/sec at 30 Hz
            mag_size: 30,
            perception_range: 40 * POSITION_SCALE,
            fov_octant_spread: full_circle_fov(),
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

/// One in-flight projectile — match state derived entirely from a recorded fire
/// action (a [`WeaponMode::Projectile`] fire spawns it). Because a projectile is a
/// pure function of the seed + rules + the recorded action stream, it is never itself
/// recorded; replay re-runs the actions and respawns it identically, so the match
/// still reproduces bit-for-bit. The agent never sees this struct — a perceivable
/// projectile reaches it as a parity-bounded [`VisibleEntity`] carrying only its
/// `id`, `position`, and travel `facing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Projectile {
    /// Stable per-match id (from [`PROJECTILE_ID_BASE`]) — the wire `entity_id`.
    id: u32,
    /// The seat that fired it, credited the damage on a hit. NEVER emitted on the
    /// wire — it would reveal who fired the shot.
    shooter: SeatId,
    /// The shooter's team: a projectile passes through its shooter and allies, the
    /// same friendly-fire-off rule hitscan uses. NEVER emitted — a projectile is
    /// reported as the neutral team so its affiliation is not perceivable.
    team: TeamId,
    /// Spawn position — the anchor range expiry measures against.
    origin: Vec2,
    /// Current position; this tick's swept segment runs from here to `pos + vel`.
    pos: Vec2,
    /// Per-tick integer velocity (the firing octant scaled to the clamped
    /// `projectile_speed`).
    vel: Vec2,
    /// Travel heading (the snapped firing octant) — the only orientation a perceiver
    /// legitimately reads off a shot in flight.
    facing: Bam,
    /// Ticks in flight, checked against [`MAX_PROJECTILE_LIFETIME`].
    age: u16,
}

/// The arena match: roster, authoritative pawn state, and the lifecycle phase.
/// A match advances one fixed tick at a time and never moves backward.
pub struct Match {
    match_id: Uuid,
    config: arena_proto::MatchConfig,
    rules: Rules,
    seats: Vec<arena_proto::SeatInfo>,
    /// Static vision blockers; consulted only by [`observe`](Match::observe) for
    /// line-of-sight occlusion (vision-only in this first cut — movement and
    /// hitscan ignore them). Empty means omnidirectional, no-occluder perception.
    blockers: Vec<Blocker>,
    pawns: Vec<Pawn>,
    /// Projectiles in flight, ascending by id (spawn order). Always empty in
    /// [`WeaponMode::Hitscan`], so the per-tick advance is a no-op and a hitscan match
    /// is byte-identical. Derived state — never recorded; recomputed on replay from
    /// the action stream.
    projectiles: Vec<Projectile>,
    /// Monotonic projectile id source; a spawn takes this value, then increments.
    next_projectile_id: u32,
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
    /// `blockers` are the static vision occluders the match plays under (empty for
    /// no occlusion). They are not validated here — a record-driven re-run gates
    /// well-formed geometry in [`MatchRecord::verify`] before construction.
    ///
    /// [`Live`]: MatchPhase::Live
    /// [`Lobby`]: MatchPhase::Lobby
    /// [`Starting`]: MatchPhase::Starting
    pub fn new(
        match_id: Uuid,
        config: arena_proto::MatchConfig,
        rules: Rules,
        seats: Vec<arena_proto::SeatInfo>,
        blockers: Vec<Blocker>,
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
            blockers,
            pawns,
            projectiles: Vec::new(),
            next_projectile_id: PROJECTILE_ID_BASE,
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
    /// `perception_range`, inside the seat's forward FOV cone
    /// ([`Rules::fov_octant_spread`]; the default full circle imposes no angular
    /// bound), AND with a clear line of sight — not occluded by a vision
    /// [`Blocker`] — never itself, PLUS any in-flight projectile under the SAME
    /// range + cone + line-of-sight bound), in ascending `entity_id` order so the
    /// snapshot is canonical. The absence of any other field is the security bound —
    /// there is no path here to full world state. Each filter only ever REMOVES a
    /// perceivable entity, so none can widen the bound; a projectile is reported as
    /// the neutral team and carries no shooter/target, so it leaks no hidden state.
    pub fn observe(&self, seat: SeatId) -> arena_proto::Observation {
        let me = self.pawn(seat);
        let mut visible: Vec<arena_proto::VisibleEntity> = self
            .pawns
            .iter()
            .filter(|p| p.seat != seat && p.alive)
            .filter(|p| within(me.pos, p.pos, self.rules.perception_range))
            .filter(|p| in_fov(me.facing, me.pos, p.pos, self.rules.fov_octant_spread))
            .filter(|p| has_line_of_sight(&self.blockers, me.pos, p.pos))
            .map(|p| arena_proto::VisibleEntity {
                entity_id: p.seat as u32,
                kind: arena_proto::EntityKind::Player,
                team: p.team,
                position: p.pos,
                z: p.z,
                facing: p.facing,
                // Exclude-when-occluded: an entry only reaches the visible set if
                // its sightline is clear, so `in_line_of_sight` carries its honest
                // meaning — true for everything visible right now. (A last-known,
                // out-of-sight position would set this false, but that needs a
                // perception-memory model not in this first cut.)
                in_line_of_sight: true,
            })
            .collect();
        // Perceivable in-flight projectiles, under the IDENTICAL range + cone + LOS
        // bound the pawn filter uses, so a projectile is held to the same parity
        // contract. It is reported as the neutral team and carries no shooter/target,
        // so an observed shot reveals only its position and travel heading.
        for proj in &self.projectiles {
            if within(me.pos, proj.pos, self.rules.perception_range)
                && in_fov(me.facing, me.pos, proj.pos, self.rules.fov_octant_spread)
                && has_line_of_sight(&self.blockers, me.pos, proj.pos)
            {
                visible.push(arena_proto::VisibleEntity {
                    entity_id: proj.id,
                    kind: arena_proto::EntityKind::Projectile,
                    team: 0,
                    position: proj.pos,
                    z: 0,
                    facing: proj.facing,
                    in_line_of_sight: true,
                });
            }
        }
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
                // Report the fire cooldown as the NEXT action will see it. `step`
                // decrements `cooldown` at tick start before the fire gate, so the
                // raw value here is one ahead of what a fire submitted for this
                // observation's tick faces: a raw `1` still fires (it decrements to
                // `0` first). Subtracting that pending decrement makes the exposed
                // value `0` exactly when a fire is honored, so the agent's predicate
                // is the clean `cooldown == 0` with no off-by-one to model.
                cooldown: me.cooldown.saturating_sub(1),
                alive: me.alive,
            },
            visible,
        }
    }

    /// Project the whole-battlefield SPECTATOR view — every pawn's public on-stage
    /// state — as an [`arena_proto::Broadcast`]. This is the caster-camera feed a
    /// non-participant watches, the deliberate counterpart to
    /// [`observe`](Match::observe):
    ///
    /// - `observe` is one seat's PARITY-BOUNDED slice — its own full state plus only
    ///   the enemies it can perceive — and is the gameplay security boundary. This
    ///   method does not touch it: adding `broadcast` changes nothing an agent sees,
    ///   because the Gateway still answers a participant's connection only with
    ///   `observe`.
    /// - `broadcast` is omniscient over PUBLIC state only: it reports EVERY pawn
    ///   (alive or dead, in or out of anyone's perception) because a spectator sees
    ///   the whole stage, but it carries only what is on screen — position, team,
    ///   facing, the health bar, and the scoreboard `score`. It deliberately omits
    ///   the private HUD internals (`ammo`, `cooldown`) the parity bound also hides,
    ///   so the feed is not a tactical x-ray.
    ///
    /// SECURITY: `observe` and `broadcast` are two separate methods on the same
    /// `Match`; the parity bound holds only as long as a *participant's* gameplay
    /// connection is served `observe` and never `broadcast`. Keeping `broadcast` off
    /// the participant path — serving it solely to non-participant spectator
    /// connections — is a service-layer access-control obligation this method cannot
    /// enforce. A live ranked participant handed `broadcast` would gain omniscience
    /// and defeat the parity bound.
    ///
    /// Entities are in ascending `entity_id` (== seat) order, so the frame is
    /// canonical and replay-stable like every other record.
    pub fn broadcast(&self) -> arena_proto::Broadcast {
        let mut entities: Vec<arena_proto::BroadcastEntity> = self
            .pawns
            .iter()
            .map(|p| arena_proto::BroadcastEntity {
                entity_id: p.seat as u32,
                kind: arena_proto::EntityKind::Player,
                team: p.team,
                position: p.pos,
                z: p.z,
                facing: p.facing,
                health: p.health,
                max_health: p.max_health,
                score: p.score,
                alive: p.alive,
            })
            .collect();
        entities.sort_by_key(|e| e.entity_id);
        arena_proto::Broadcast {
            protocol_version: PROTOCOL_VERSION,
            match_id: self.match_id,
            tick: self.tick,
            phase: self.phase,
            entities,
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
                match self.rules.weapon_mode {
                    WeaponMode::Hitscan => self.resolve_fire(i),
                    WeaponMode::Projectile => self.spawn_projectile(i),
                }
            }
        }

        // Advance in-flight projectiles AFTER this tick's moves and fires: existing
        // shots collide against pawns at their post-move positions (so a strafe
        // dodges), and a shot spawned this tick takes its first step too. A no-op in
        // hitscan mode (the live set is always empty), so the hitscan path is unchanged.
        self.advance_projectiles();

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

    /// Spawn a projectile for `shooter`'s fire in [`WeaponMode::Projectile`]. It
    /// launches from the shooter along the snapped firing octant at the clamped
    /// `projectile_speed`, taking the next stable id. At the live cap nothing spawns —
    /// the trigger pull is wasted (ammo was already charged by the caller), the DoS
    /// backstop that bounds per-tick work.
    fn spawn_projectile(&mut self, shooter: usize) {
        if self.projectiles.len() >= MAX_LIVE_PROJECTILES {
            return;
        }
        let s = self.pawns[shooter];
        let oct = octant_index(s.facing);
        let (ox, oy) = OCTANTS[oct];
        // Clamp the speed to a sane non-negative bound: a negative speed becomes a
        // motionless shot (expires by lifetime) and an extreme one is capped so the
        // swept i128 math stays overflow-free. Scale in i64 (octant · speed before the
        // /OCTANT_SCALE divide) so the intermediate never overflows i32.
        let speed = self.rules.projectile_speed.clamp(0, MAX_PROJECTILE_SPEED) as i64;
        let vel = Vec2 {
            x: (ox as i64 * speed / OCTANT_SCALE as i64) as i32,
            y: (oy as i64 * speed / OCTANT_SCALE as i64) as i32,
        };
        let id = self.next_projectile_id;
        self.next_projectile_id += 1;
        self.projectiles.push(Projectile {
            id,
            shooter: s.seat,
            team: s.team,
            origin: s.pos,
            pos: s.pos,
            vel,
            facing: (oct as u32 * 8192) as Bam, // each octant spans 8192 BAM (65536 / 8)
            age: 0,
        });
    }

    /// Advance every live projectile one tick, resolve hits, and drop the spent ones.
    /// A no-op while nothing is in flight (every hitscan tick, and a projectile match
    /// before its first fire), so it never perturbs a hitscan match. Each projectile
    /// sweeps the segment from its previous to its new position and damages the nearest
    /// enemy body that segment crosses — swept, so a fast shot cannot tunnel through a
    /// pawn between ticks. A hit consumes the shot and credits its shooter (even if the
    /// shooter has since died — a shot already in the air still lands); a clean shot
    /// expires once it has travelled past `weapon_range` or hits the lifetime backstop.
    /// A simultaneous mutual exchange can down both seats (each shot is independent of
    /// its shooter's fate), unlike hitscan's seat-ordered decisiveness.
    fn advance_projectiles(&mut self) {
        if self.projectiles.is_empty() {
            return;
        }
        let range2 = (self.rules.weapon_range as i128).pow(2);
        let radius = self.rules.hit_radius;
        let flying = std::mem::take(&mut self.projectiles);
        let mut survivors = Vec::with_capacity(flying.len());
        for mut proj in flying {
            let from = proj.pos;
            let to = Vec2 {
                x: from.x.saturating_add(proj.vel.x),
                y: from.y.saturating_add(proj.vel.y),
            };
            // The nearest enemy body the swept segment reaches, by distance from the
            // launch end of the sweep (seat order breaks an exact tie) — the same
            // nearest-target rule hitscan uses.
            let mut hit: Option<usize> = None;
            let mut best = i128::MAX;
            for (j, t) in self.pawns.iter().enumerate() {
                if !t.alive || t.seat == proj.shooter || t.team == proj.team {
                    continue;
                }
                if !segment_hits_disc(from, to, t.pos, radius) {
                    continue;
                }
                let dx = t.pos.x as i128 - from.x as i128;
                let dy = t.pos.y as i128 - from.y as i128;
                let d2 = dx * dx + dy * dy;
                if d2 < best {
                    best = d2;
                    hit = Some(j);
                }
            }
            if let Some(j) = hit {
                let dmg = self.rules.damage.min(self.pawns[j].health);
                self.pawns[j].health -= dmg;
                if self.pawns[j].health == 0 {
                    self.pawns[j].alive = false;
                }
                if let Some(sp) = self.pawns.iter_mut().find(|p| p.seat == proj.shooter) {
                    sp.score += dmg as i32;
                }
                continue; // consumed on hit
            }
            proj.pos = to;
            proj.age += 1;
            let dx = proj.pos.x as i128 - proj.origin.x as i128;
            let dy = proj.pos.y as i128 - proj.origin.y as i128;
            if dx * dx + dy * dy > range2 || proj.age >= MAX_PROJECTILE_LIFETIME {
                continue; // expired by range or the lifetime backstop
            }
            survivors.push(proj);
        }
        self.projectiles = survivors;
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
            blockers: self.blockers.clone(),
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
            blockers: self.blockers,
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
    /// `config` or `rules` carry a value the live sim never produces — a negative
    /// arena bound or a negative spawn radius/jitter. Beyond being nonsensical,
    /// re-running such a setup would panic (a `min > max` clamp, a negated
    /// `i32::MIN`), so the record is rejected before any simulation rather than
    /// crashing the verifier.
    MalformedSetup,
    /// A vision blocker at `index` is inverted (`min` greater than `max` on an
    /// axis) — geometry the live sim never produces. Rejected before the re-run so
    /// a corrupt occluder is a clean error, not a degenerate sightline test.
    MalformedBlocker { index: usize },
    /// The record carries more vision blockers than the verifier will process.
    /// Every blocker is hashed into the digest, so an oversized list is a CPU-DoS;
    /// rejected here, before the re-run. A generous backstop, far above any real map.
    TooManyBlockers { blockers: usize, max: usize },
    /// The record carries more recorded ticks than the verifier will process.
    /// `verify` re-executes the whole match, so an attacker-controlled record with
    /// a huge tick stream is a CPU-DoS; it is rejected here, before the structural
    /// scan and the re-run. The bound is a generous backstop, far above any real
    /// match — not a gameplay limit.
    TooManyTicks { ticks: usize, max: usize },
    /// The roster is larger than the verifier will process. Combat is O(seats²) per
    /// re-run tick, so an oversized roster is a CPU-DoS; it is rejected before the
    /// roster scan and the re-run. A generous backstop, far above any real match.
    TooManySeats { seats: usize, max: usize },
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
            ReplayError::MalformedSetup => {
                write!(f, "invalid replay: config or rules out of the well-formed range")
            }
            ReplayError::MalformedBlocker { index } => {
                write!(f, "invalid replay: blocker {index} is inverted (min greater than max)")
            }
            ReplayError::TooManyBlockers { blockers, max } => {
                write!(f, "invalid replay: {blockers} blockers exceeds the verifier budget of {max}")
            }
            ReplayError::TooManyTicks { ticks, max } => {
                write!(f, "invalid replay: {ticks} recorded ticks exceeds the verifier budget of {max}")
            }
            ReplayError::TooManySeats { seats, max } => {
                write!(f, "invalid replay: roster of {seats} seats exceeds the verifier budget of {max}")
            }
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

/// Verifier cost backstop: the most recorded ticks [`MatchRecord::verify`] will
/// process before rejecting a record as oversized. `verify` re-executes the whole
/// match (and the structural scan touches every tick), so this bounds verifier CPU
/// against an attacker-controlled record. ~27 min of real play at 60 ticks/s —
/// far above any real match (default `max_ticks` is 3600); a DoS backstop, not a
/// gameplay limit.
pub const MAX_REPLAY_TICKS: usize = 100_000;

/// Verifier cost backstop: the largest roster [`MatchRecord::verify`] will process.
/// Per-tick combat is O(seats²), so this bounds the re-run cost. Far above any real
/// match (settlement is 1v1; arenas seat a handful) — a DoS backstop, not a limit.
pub const MAX_REPLAY_SEATS: usize = 64;

/// Verifier cost backstop: the most vision blockers [`MatchRecord::verify`] will
/// process. `verify` hashes every blocker into the digest (and any consumer that
/// observes a re-run pays O(seats · blockers) per tick), so an attacker-controlled
/// record with a huge blocker list is a CPU-DoS; it is rejected before the re-run.
/// Far above any real arena's occluder count — a DoS backstop, not a map limit.
pub const MAX_REPLAY_BLOCKERS: usize = 1024;

impl MatchRecord {
    /// Re-derive the match from this record ALONE and confirm it is a faithful,
    /// self-consistent commitment: the inputs (`config`, `rules`, and the
    /// `replay`'s seed + action stream) re-run through the same core to exactly
    /// the recorded `result`, and the result's `replay_hash` is the canonical
    /// [`digest`](ReplayRecord::digest) of that re-run. Returns the reproduced
    /// [`MatchResult`] on success.
    ///
    /// Every rejection is a typed [`ReplayError`] — `verify` NEVER panics, so a
    /// truncated or hand-tampered record is a cleanly rejected input, not a way to
    /// crash a verifier (on-chain settlement, a grader, a spectator feed). The
    /// cheap structural checks (protocol version, match-id agreement, a
    /// well-formed roster, canonical tick + seat order) run BEFORE the re-run, so
    /// a malformed record is rejected without simulating it and a bad action
    /// stream can never reach [`step`](Match::step) to panic or be silently
    /// reshaped into a different hash.
    pub fn verify(&self) -> Result<MatchResult, ReplayError> {
        check_version(self.replay.protocol_version).map_err(ReplayError::Version)?;
        check_version(self.result.protocol_version).map_err(ReplayError::Version)?;

        if self.replay.match_id != self.result.match_id {
            return Err(ReplayError::MatchIdMismatch {
                replay: self.replay.match_id,
                result: self.result.match_id,
            });
        }

        // Cost budget. `config`, `seats`, and `ticks` come from a deserialized,
        // attacker-controlled record, and `verify` re-executes the whole match
        // (`replay_match` steps once per recorded tick, each tick O(seats²) combat)
        // after an O(ticks) structural scan and an O(seats) roster scan. Reject an
        // oversized record HERE — before the roster scan below, the tick loop, and
        // `Match::new` — so a crafted stream cannot turn the verifier (or
        // `replay_frames`, which calls `verify` first) into a CPU-DoS. Two absolute
        // caps bound total cost to O(MAX_REPLAY_TICKS · MAX_REPLAY_SEATS²)
        // regardless of `config`; they are generous backstops, not gameplay limits.
        if self.replay.seats.len() > MAX_REPLAY_SEATS {
            return Err(ReplayError::TooManySeats {
                seats: self.replay.seats.len(),
                max: MAX_REPLAY_SEATS,
            });
        }
        if self.replay.ticks.len() > MAX_REPLAY_TICKS {
            return Err(ReplayError::TooManyTicks {
                ticks: self.replay.ticks.len(),
                max: MAX_REPLAY_TICKS,
            });
        }
        if self.replay.blockers.len() > MAX_REPLAY_BLOCKERS {
            return Err(ReplayError::TooManyBlockers {
                blockers: self.replay.blockers.len(),
                max: MAX_REPLAY_BLOCKERS,
            });
        }

        // A real match has at least one seat, `config.seats` agrees with the
        // roster length, and no two seats share an id. `roster` doubles as the
        // membership set for the per-tick seat check below.
        let mut roster: BTreeSet<SeatId> = BTreeSet::new();
        let seats = &self.replay.seats;
        if seats.is_empty()
            || self.config.seats as usize != seats.len()
            || !seats.iter().all(|s| roster.insert(s.seat))
        {
            return Err(ReplayError::InvalidRoster);
        }

        // Arena bounds and spawn geometry are non-negative half-extents in any
        // match the live sim produces. Reject a hand-crafted setup that breaks
        // that here — before Match::new — because re-running it would panic (a
        // `min > max` clamp on a negative bound, a negated `i32::MIN` jitter)
        // rather than fail cleanly, turning a corrupt record into a verifier DoS.
        if self.config.bounds.x < 0
            || self.config.bounds.y < 0
            || self.rules.spawn_radius < 0
            || self.rules.spawn_jitter < 0
        {
            return Err(ReplayError::MalformedSetup);
        }

        // A blocker is a closed AABB, so `min <= max` on each axis is the
        // well-formed invariant. An inverted box is geometry the live sim never
        // produces; reject it here so the sightline test only ever sees a
        // well-formed occluder. (A zero-extent box — a thin wall — is well-formed
        // and intentionally allowed.)
        for (index, b) in self.replay.blockers.iter().enumerate() {
            if b.min.x > b.max.x || b.min.y > b.max.y {
                return Err(ReplayError::MalformedBlocker { index });
            }
        }

        // The live sim records exactly one tick per simulated tick, in order, each
        // tick's actions ascending-unique by a seat in the roster. Enforce that
        // canonical shape up front: a reordered, gapped, or corpse-padded stream
        // is rejected here rather than silently re-clamped into a divergent hash.
        for (index, tr) in self.replay.ticks.iter().enumerate() {
            if tr.tick != index as u64 {
                return Err(ReplayError::TickOrder { index, tick: tr.tick });
            }
            let mut prev: Option<SeatId> = None;
            for a in &tr.actions {
                if !roster.contains(&a.seat) {
                    return Err(ReplayError::UnknownSeat { tick: tr.tick, seat: a.seat });
                }
                if prev.is_some_and(|p| p >= a.seat) {
                    return Err(ReplayError::SeatOrder { tick: tr.tick });
                }
                prev = Some(a.seat);
            }
        }

        let fresh = Match::new(
            self.replay.match_id,
            self.config,
            self.rules,
            self.replay.seats.clone(),
            self.replay.blockers.clone(),
            self.replay.seed,
        );
        let rerun = replay_match(fresh, &self.replay);
        let Some(reproduced) = rerun.result() else {
            return Err(ReplayError::NotTerminal);
        };

        if reproduced.outcomes != self.result.outcomes
            || reproduced.final_tick != self.result.final_tick
        {
            return Err(ReplayError::ResultMismatch);
        }
        if reproduced.replay_hash != self.result.replay_hash {
            return Err(ReplayError::HashMismatch {
                expected: self.result.replay_hash.clone(),
                recomputed: reproduced.replay_hash.clone(),
            });
        }
        Ok(reproduced.clone())
    }
}

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

/// `true` if `to` lies within the observer's forward field-of-view cone. The cone
/// is the observer's facing octant plus `spread` octants to either side — the same
/// 8-way quantization the beam uses — so `spread == 4` is the full circle (every
/// octant is within 4 of the facing on the 8-ring: omnidirectional, the default
/// that reproduces range-only perception byte-for-byte) and `spread == 0` is just
/// the facing octant (~45°). A `to` co-located with the observer (zero bearing) is
/// always in view. All integer: the bearing is classified by [`bearing_octant`]
/// and compared by [`circular_octant_distance`], no trig — so a configured cone is
/// as cross-platform-stable as the rest of the sim.
fn in_fov(facing: Bam, from: Vec2, to: Vec2, spread: u8) -> bool {
    if spread >= 4 {
        return true;
    }
    let dx = to.x as i64 - from.x as i64;
    let dy = to.y as i64 - from.y as i64;
    if dx == 0 && dy == 0 {
        return true;
    }
    circular_octant_distance(octant_index(facing), bearing_octant(dx, dy)) <= spread as usize
}

/// `true` if the sightline from `from` to `to` is clear of every vision blocker.
/// Occlusion only ever REMOVES a perceivable enemy, so it cannot widen perception
/// beyond the range+cone set — the parity bound holds a fortiori.
fn has_line_of_sight(blockers: &[Blocker], from: Vec2, to: Vec2) -> bool {
    !blockers.iter().any(|b| occludes(b, from, to))
}

/// `true` if point `p` lies within the closed AABB `b` (boundary inclusive).
fn blocker_contains(b: &Blocker, p: Vec2) -> bool {
    b.min.x <= p.x && p.x <= b.max.x && b.min.y <= p.y && p.y <= b.max.y
}

/// `true` if blocker `b` occludes the sightline from `from` to `to`: the segment
/// crosses the blocker's closed AABB AND neither endpoint is inside it. The
/// endpoint exemption is what makes a pawn standing in (or pressed against) an
/// occluder neither blind nor invisible — its own enclosing blocker is skipped,
/// while every other blocker still occludes it. Without it a spawn the seed
/// happened to place inside a blocker would be permanently self-occluded.
fn occludes(b: &Blocker, from: Vec2, to: Vec2) -> bool {
    if blocker_contains(b, from) || blocker_contains(b, to) {
        return false;
    }
    segment_intersects_aabb(from, to, b)
}

/// Integer segment-vs-AABB intersection by the separating-axis theorem. A segment
/// and an AABB are disjoint iff some axis separates their projections; for this
/// pair three axes suffice — the two AABB axes (a bounding-box overlap on X and on
/// Y) and the segment's normal (all four AABB corners strictly on one side of the
/// segment's supporting line). No separating axis ⇒ they touch or cross, and
/// boundary contact (a grazed corner or edge) counts as a hit — the conservative,
/// parity-tightening direction. All `i128`, no trig and no division, so a
/// degenerate (zero-extent thin-wall) or extreme AABB neither panics nor divides
/// by zero, and the test is byte-identical on every platform.
fn segment_intersects_aabb(from: Vec2, to: Vec2, b: &Blocker) -> bool {
    let (sx0, sx1) = (from.x.min(to.x), from.x.max(to.x));
    if sx1 < b.min.x || b.max.x < sx0 {
        return false;
    }
    let (sy0, sy1) = (from.y.min(to.y), from.y.max(to.y));
    if sy1 < b.min.y || b.max.y < sy0 {
        return false;
    }
    let dx = to.x as i128 - from.x as i128;
    let dy = to.y as i128 - from.y as i128;
    let (mut lo, mut hi) = (i128::MAX, i128::MIN);
    for (cx, cy) in [
        (b.min.x, b.min.y),
        (b.max.x, b.min.y),
        (b.min.x, b.max.y),
        (b.max.x, b.max.y),
    ] {
        let cross = dx * (cy as i128 - from.y as i128) - dy * (cx as i128 - from.x as i128);
        lo = lo.min(cross);
        hi = hi.max(cross);
    }
    lo <= 0 && hi >= 0
}

/// `true` if the closed segment `a → b` passes within `radius` of point `c` — the
/// swept collision of a projectile's per-tick travel against a pawn body (a disc of
/// `radius`). Integer point-to-segment squared distance, `i128` throughout (a squared
/// planar distance exceeds `i64` at extreme arena coordinates), no float and no
/// division, so it is exact and replay-stable. Because it tests the WHOLE segment, a
/// fast shot whose endpoints both miss but whose path crosses the body still hits — no
/// tunneling. `projectile_speed` is clamped at spawn, so the segment length (and these
/// products) cannot overflow `i128` at any in-bounds coordinate.
fn segment_hits_disc(a: Vec2, b: Vec2, c: Vec2, radius: i32) -> bool {
    let (ax, ay) = (a.x as i128, a.y as i128);
    let (bx, by) = (b.x as i128, b.y as i128);
    let (cx, cy) = (c.x as i128, c.y as i128);
    let r2 = (radius as i128) * (radius as i128);
    let (abx, aby) = (bx - ax, by - ay);
    let (apx, apy) = (cx - ax, cy - ay);
    let seg_len2 = abx * abx + aby * aby; // |AB|²
    let proj = apx * abx + apy * aby; // dot(AP, AB)
    if seg_len2 == 0 || proj <= 0 {
        // Closest point is A — a zero-length sweep, or C projects "behind" the launch.
        return apx * apx + apy * apy <= r2;
    }
    if proj >= seg_len2 {
        // Closest point is B — C projects past the far end.
        let (bpx, bpy) = (cx - bx, cy - by);
        return bpx * bpx + bpy * bpy <= r2;
    }
    // The perpendicular foot falls inside the segment: compare the perpendicular
    // squared distance |AP|² − proj²/|AB|² to r² by multiplying through by |AB|² (> 0),
    // keeping the whole test in exact integers.
    let ap2 = apx * apx + apy * apy;
    ap2 * seg_len2 - proj * proj <= r2 * seg_len2
}

/// How a finished match resolves for on-chain settlement, derived from its
/// canonical [`MatchResult`] outcomes.
///
/// This is the gameplay interpretation that picks `MatchSettlement.settle` vs
/// `settleDraw`: a [`Win`](Settlement::Win) is the single seat that placed first,
/// a [`Draw`](Settlement::Draw) is any tie for first. A `cancelMatch` is NOT a
/// match *outcome* — it is the recovery path for a match that produced no result
/// at all — so it is never derived here; the caller drives a cancel directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// Exactly one seat finished at placement 1 — a decisive winner.
    Win { seat: SeatId },
    /// Two or more seats tied for placement 1 (equal alive-flag and score, or an
    /// all-down match with a shared top score) — no decisive winner.
    Draw,
}

/// Classify a finished match for settlement from its outcomes alone.
///
/// `placement` is 1-based with tied seats sharing a rank (see [`SeatOutcome`]):
/// the ordering already folds alive-over-dead then higher-score, so a *single*
/// seat at placement 1 is the decisive winner and two-or-more sharing it is a
/// draw. Keying on placement (not `alive_at_end`) is deliberate — an all-down
/// match still has a higher-score winner that placement ranks first, and that is
/// the result the on-chain record commits.
pub fn settlement(result: &MatchResult) -> Settlement {
    let mut winner: Option<SeatId> = None;
    for o in &result.outcomes {
        if o.placement != 1 {
            continue;
        }
        if winner.is_some() {
            return Settlement::Draw;
        }
        winner = Some(o.seat);
    }
    match winner {
        Some(seat) => Settlement::Win { seat },
        None => Settlement::Draw,
    }
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
        Match::new(MID.parse().unwrap(), config(2), Rules::default(), two_seats(), Vec::new(), seed)
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
        Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), seed)
    }

    /// [`close_match`], but in projectile weapon mode — the same exact geometry, so
    /// the traveling-shot path is tested against known positions.
    fn projectile_close_match(seed: u64) -> Match {
        let rules = Rules {
            weapon_mode: WeaponMode::Projectile,
            spawn_radius: 2 * POSITION_SCALE,
            spawn_jitter: 0,
            ..Default::default()
        };
        Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), seed)
    }

    #[test]
    fn a_projectile_travels_and_downs_an_enemy() {
        // The end-to-end projectile path: seat 0 fires across the gap, the shots fly
        // and damage the still enemy until it is downed, and the shooter is credited
        // — proving spawn → advance → swept collision → score all wire up.
        let mut m = projectile_close_match(1);
        let aim = m.observe(0).own.facing; // seat 0 spawned facing the enemy (EAST)
        let mut saw_projectile = false;
        while m.phase() == MatchPhase::Live {
            // A shot in flight and perceivable shows as a neutral Projectile carrying
            // only its position and travel heading — no shooter, no team.
            if let Some(p) =
                m.observe(0).visible.iter().find(|e| e.kind == arena_proto::EntityKind::Projectile)
            {
                saw_projectile = true;
                assert_eq!(p.team, 0, "a projectile is reported neutral, not its shooter's team");
                assert!(p.entity_id >= PROJECTILE_ID_BASE, "projectile ids sit above the pawn id space");
                assert_eq!(p.facing, EAST, "the shot's heading is its travel octant");
            }
            step_with(&mut m, &[(0, intent(Vec2::ZERO, aim, true))]);
        }
        let r = m.result().unwrap();
        let s1 = r.outcomes.iter().find(|o| o.seat == 1).unwrap();
        assert!(!s1.alive_at_end, "the projectiles downed the still enemy");
        let s0 = r.outcomes.iter().find(|o| o.seat == 0).unwrap();
        assert!(s0.score >= Rules::default().start_health as i32, "the shooter is credited the damage");
        assert!(saw_projectile, "a shot was perceptible in flight");
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
    fn rules_default_to_hitscan_and_old_records_deserialize_to_it() {
        // The default weapon model is Hitscan, so every pre-projectile match is
        // byte-identical, and a Rules serialized before the projectile fields existed
        // deserializes to Hitscan + the default speed (not Projectile, not speed 0) —
        // the back-compat contract that lets old records replay under the model they
        // actually ran.
        assert_eq!(WeaponMode::default(), WeaponMode::Hitscan);
        assert_eq!(Rules::default().weapon_mode, WeaponMode::Hitscan);

        let mut v = serde_json::to_value(Rules::default()).unwrap();
        let obj = v.as_object_mut().unwrap();
        obj.remove("weapon_mode");
        obj.remove("projectile_speed");
        let old: Rules = serde_json::from_value(v).unwrap();
        assert_eq!(old.weapon_mode, WeaponMode::Hitscan, "absent weapon_mode defaults to Hitscan");
        assert_eq!(old.projectile_speed, default_projectile_speed(), "absent speed defaults, not zero");

        // The wire spelling is the snake_case tag both Gateway implementers share.
        assert_eq!(serde_json::to_value(WeaponMode::Projectile).unwrap(), serde_json::json!("projectile"));
        assert_eq!(serde_json::to_value(WeaponMode::Hitscan).unwrap(), serde_json::json!("hitscan"));
    }

    #[test]
    fn broadcast_is_the_whole_stage_not_a_parity_bounded_observation() {
        // FM1: the spectator broadcast is its OWN projection — the whole
        // battlefield — NOT a reuse of a seat's parity-bounded observation. Spawn two
        // seats out of each other's perception (1 m range under a 4 m spawn gap), so
        // each seat's `observe` sees NO enemy, yet the broadcast still shows BOTH
        // pawns: the caster view, distinct from any seat's.
        let rules = Rules {
            perception_range: POSITION_SCALE,
            spawn_radius: 2 * POSITION_SCALE,
            spawn_jitter: 0,
            ..Default::default()
        };
        let m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);

        assert!(m.observe(0).visible.is_empty(), "seat 0 perceives no enemy at this range");
        assert!(m.observe(1).visible.is_empty(), "seat 1 perceives no enemy at this range");

        let b = m.broadcast();
        assert_eq!(b.protocol_version, PROTOCOL_VERSION);
        assert_eq!(b.match_id, m.match_id());
        assert_eq!(b.tick, 0);
        assert_eq!(b.phase, MatchPhase::Live);
        let ids: Vec<u32> = b.entities.iter().map(|e| e.entity_id).collect();
        assert_eq!(ids, vec![0, 1], "broadcast shows every pawn, ascending by entity_id");
    }

    #[test]
    fn broadcast_agrees_with_owner_public_state_and_tracks_combat() {
        let mut m = close_match(7);
        // At spawn, the broadcast's per-pawn public facts match each seat's own
        // SelfState — the broadcast and the owner agree on public state.
        let b0 = m.broadcast();
        for seat in [0u8, 1u8] {
            let own = m.observe(seat).own;
            let e = b0.entities.iter().find(|e| e.entity_id == seat as u32).expect("seat in broadcast");
            assert_eq!(e.team, own.team);
            assert_eq!(e.position, own.position);
            assert_eq!(e.facing, own.facing);
            assert_eq!(e.health, own.health);
            assert_eq!(e.max_health, own.max_health);
            assert_eq!(e.alive, own.alive);
            assert_eq!(e.score, 0, "no damage dealt at spawn");
        }
        // Seat 0 fires on seat 1 (its spawned facing already points at the enemy)
        // until the match ends; the broadcast then shows seat 1 downed and seat 0's
        // scoreboard raised — a live spectator view tracked through combat.
        let aim = m.observe(0).own.facing;
        while m.phase() == MatchPhase::Live {
            step_with(&mut m, &[(0, intent(Vec2::ZERO, aim, true))]);
        }
        let b = m.broadcast();
        let s0 = b.entities.iter().find(|e| e.entity_id == 0).unwrap();
        let s1 = b.entities.iter().find(|e| e.entity_id == 1).unwrap();
        assert!(s0.score > 0, "seat 0's damage shows on the broadcast scoreboard");
        assert!(!s1.alive, "the downed pawn reads dead in the broadcast");
        assert_eq!(b.phase, MatchPhase::Ended);
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
        let m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 5);
        assert!(m.observe(0).visible.is_empty());
        assert!(m.observe(1).visible.is_empty());
    }

    /// White-box parity audit: against ground-truth pawn state, every seat's
    /// [`Observation`] exposes ONLY what it may legitimately perceive — its own
    /// full state, and exactly the OTHER pawns that are alive AND within
    /// perception this tick, each shown only by its real, current, perceivable
    /// facets. A pawn appears in the visible set IFF it is perceivable, so neither
    /// an out-of-range/downed pawn leaks in nor an in-range one is hidden.
    ///
    /// Returns the count of alive enemies correctly EXCLUDED for being out of
    /// perception — a nonzero total proves the range filter was load-bearing, so
    /// the audit can't pass vacuously in an everyone-always-in-range scenario.
    /// Live enemies the bound excluded, split by reason. Disjoint, in filter order:
    /// an out-of-range enemy counts as `out_of_range`; one in range but outside the
    /// FOV cone as `out_of_cone`; one in range and in cone but occluded by a vision
    /// blocker as `out_of_los`. A non-zero count is how a test proves the
    /// corresponding filter is load-bearing rather than vacuous.
    struct ParityCounts {
        out_of_range: usize,
        out_of_cone: usize,
        out_of_los: usize,
    }

    fn assert_parity_bound(m: &Match) -> ParityCounts {
        let perception = m.rules.perception_range;
        let spread = m.rules.fov_octant_spread;
        let mut counts = ParityCounts { out_of_range: 0, out_of_cone: 0, out_of_los: 0 };
        for truth in &m.pawns {
            let seat = truth.seat;
            let obs = m.observe(seat);
            // `own` is the observer's OWN real state — never another seat's.
            assert_eq!(obs.own.seat, seat, "own is the observer's own seat");
            assert_eq!(obs.own.position, truth.pos, "own position is the observer's real one");
            assert_eq!(obs.own.health, truth.health, "own health is the observer's real one");
            assert_eq!(obs.own.alive, truth.alive);
            // The observer never perceives itself.
            assert!(obs.visible.iter().all(|e| e.entity_id != seat as u32), "seat {seat} perceives itself");
            // The visible set is canonical: ascending entity_id, no duplicates.
            let ids: Vec<u32> = obs.visible.iter().map(|e| e.entity_id).collect();
            let mut canon = ids.clone();
            canon.sort_unstable();
            canon.dedup();
            assert_eq!(ids, canon, "seat {seat} visible set is not ascending+unique");
            // The bound, both directions, against ground truth.
            for other in &m.pawns {
                let in_range = within(truth.pos, other.pos, perception);
                let in_cone = in_fov(truth.facing, truth.pos, other.pos, spread);
                // Independently recompute LOS from the raw blockers (not via the
                // observed entry) so the audit is ground truth, not a tautology.
                let in_los = has_line_of_sight(&m.blockers, truth.pos, other.pos);
                let perceivable = other.seat != seat && other.alive && in_range && in_cone && in_los;
                let entry = obs.visible.iter().find(|e| e.entity_id == other.seat as u32);
                assert_eq!(
                    entry.is_some(),
                    perceivable,
                    "seat {seat}: parity violated for entity {} (perceivable={perceivable})",
                    other.seat
                );
                if let Some(e) = entry {
                    assert_eq!(e.position, other.pos, "perceived position must be the real current one");
                    assert_eq!(e.team, other.team, "perceived team must be the real one");
                    assert_eq!(e.kind, arena_proto::EntityKind::Player);
                }
                if other.seat != seat && other.alive {
                    if !in_range {
                        counts.out_of_range += 1;
                    } else if !in_cone {
                        counts.out_of_cone += 1;
                    } else if !in_los {
                        counts.out_of_los += 1;
                    }
                }
            }
        }
        counts
    }

    #[test]
    fn parity_bound_holds_for_every_seat_through_a_full_match() {
        // FM1+FM3: pin the per-seat bound against ground truth after EVERY tick of
        // a real 3-seat match — start, mid-match movement, AND the death
        // transition — not just t=0. A future observe() that leaks an out-of-range
        // or downed pawn, or mislabels own state, fails here.
        //
        // perception_range is deliberately TIGHT (3 m) against a 2 m spawn radius:
        // adjacent seats (2 m) perceive each other and fight, but the two ends
        // (4 m apart) start OUT of perception — so the range filter is load-bearing
        // and a leak-everything regression is actually caught here, not masked by a
        // scenario where everyone is always in range.
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "a".into() },
            SeatInfo { seat: 1, team: 1, controller: "b".into() },
            SeatInfo { seat: 2, team: 2, controller: "c".into() },
        ];
        let rules = Rules {
            spawn_radius: 2 * POSITION_SCALE,
            spawn_jitter: 0,
            perception_range: 3 * POSITION_SCALE,
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(3), rules, seats, Vec::new(), 1);
        let seat_ids: Vec<SeatId> = m.seats().iter().map(|s| s.seat).collect();
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker), Box::new(Seeker)];

        let mut excluded = assert_parity_bound(&m).out_of_range;
        let mut ticks = 0u64;
        while m.phase() == MatchPhase::Live {
            let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
            for (i, &seat) in seat_ids.iter().enumerate() {
                let obs = m.observe(seat);
                if let Some(a) = policies[i].act(&obs) {
                    if let Ok(intent) = m.ingest(seat, &a) {
                        intents.insert(seat, intent);
                    }
                }
            }
            m.step(&intents);
            excluded += assert_parity_bound(&m).out_of_range;
            ticks += 1;
        }
        assert!(ticks > 1, "the match ran multiple ticks");
        assert!(m.pawns.iter().any(|p| !p.alive), "the match exercised a death transition");
        assert!(excluded > 0, "the range filter was never load-bearing — the bound check was vacuous");
    }

    #[test]
    fn parity_bound_holds_under_a_tight_fov_cone_through_a_full_match() {
        // The arena-06 per-tick audit, now with the FOV cone load-bearing. A tight
        // spread-1 cone over a real 3-seat match with a GENEROUS perception range
        // (so range never excludes — the cone is the only filter under test): the
        // bound must hold for every seat every tick (an in-range enemy outside the
        // cone is NOT perceived), and the cone must actually exclude in-range
        // enemies (`out_of_cone > 0`) or the audit is vacuous.
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "a".into() },
            SeatInfo { seat: 1, team: 1, controller: "b".into() },
            SeatInfo { seat: 2, team: 2, controller: "c".into() },
        ];
        let rules = Rules {
            spawn_radius: 2 * POSITION_SCALE,
            spawn_jitter: 0,
            perception_range: 100 * POSITION_SCALE,
            fov_octant_spread: 1,
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(3), rules, seats, Vec::new(), 1);
        let seat_ids: Vec<SeatId> = m.seats().iter().map(|s| s.seat).collect();
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker), Box::new(Seeker)];

        let counts = assert_parity_bound(&m);
        assert_eq!(counts.out_of_range, 0, "perception range is generous — only the cone excludes");
        let mut cone_excluded = counts.out_of_cone;
        let mut ticks = 0u64;
        while m.phase() == MatchPhase::Live {
            let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
            for (i, &seat) in seat_ids.iter().enumerate() {
                let obs = m.observe(seat);
                if let Some(a) = policies[i].act(&obs) {
                    if let Ok(intent) = m.ingest(seat, &a) {
                        intents.insert(seat, intent);
                    }
                }
            }
            m.step(&intents);
            cone_excluded += assert_parity_bound(&m).out_of_cone;
            ticks += 1;
        }
        assert!(ticks > 1, "the match ran multiple ticks");
        assert!(cone_excluded > 0, "the FOV cone never excluded an in-range enemy — the bound check was vacuous");
    }

    #[test]
    fn in_fov_cone_geometry_is_exact_and_seam_correct() {
        // FM1/FM2/FM3: pin in_fov's exact integer geometry, facing EAST (octant 0).
        let o = Vec2::ZERO;
        let east = Vec2 { x: 10, y: 0 }; // octant 0 — dead ahead
        let west = Vec2 { x: -10, y: 0 }; // octant 4 — directly behind (distance 4)
        let ne = Vec2 { x: 10, y: 10 }; // octant 1 — one off E
        let se = Vec2 { x: 10, y: -10 }; // octant 7 — one off E ACROSS the 0/7 seam

        // Dead ahead is in the cone at every spread, including the narrowest.
        for spread in 0..=4 {
            assert!(in_fov(EAST, o, east, spread), "dead-ahead must be in any cone (spread {spread})");
        }
        // Directly behind is excluded by every cone tighter than the full circle,
        // and admitted only by the full circle — FM1, no omniscience regression.
        for spread in 0..4 {
            assert!(!in_fov(EAST, o, west, spread), "the rear must be outside a non-full cone (spread {spread})");
        }
        assert!(in_fov(EAST, o, west, 4), "the full circle (spread 4) admits the rear");
        // Symmetry + the 0/7 seam (FM2): NE (octant 1) and SE (octant 7) are BOTH one
        // octant off EAST — SE only because the distance wraps. A non-circular
        // distance would read SE as 7 away and wrongly drop it at spread 1.
        assert!(!in_fov(EAST, o, ne, 0) && !in_fov(EAST, o, se, 0), "spread 0 is the facing octant alone");
        assert!(in_fov(EAST, o, ne, 1), "NE is one octant off — inside a spread-1 cone");
        assert!(in_fov(EAST, o, se, 1), "SE is one octant off across the 0/7 seam — inside a spread-1 cone");
        // A co-located target (zero bearing) is always in view.
        assert!(in_fov(EAST, o, o, 0), "a co-located entity is always in view");
    }

    fn box_of(min: (i32, i32), max: (i32, i32)) -> Blocker {
        Blocker { min: Vec2 { x: min.0, y: min.1 }, max: Vec2 { x: max.0, y: max.1 } }
    }

    #[test]
    fn segment_vs_aabb_is_exact_at_the_boundary() {
        // FM2: the integer segment-vs-AABB test must classify the adversarial cases
        // — dead behind, grazing a corner, edge-on, just-missing — correctly, or a
        // blocked enemy leaks (a security regression) or a clear shot is hidden.
        let o = Vec2::ZERO;

        // A wall squarely between observer and an enemy 10 m to the east occludes.
        let wall = box_of((5, -2), (6, 2));
        assert!(segment_intersects_aabb(o, Vec2 { x: 10, y: 0 }, &wall), "a wall on the sightline blocks");
        // The same wall does not block an enemy off to the side (segment misses on Y).
        assert!(!segment_intersects_aabb(o, Vec2 { x: 10, y: 100 }, &wall), "an off-axis sightline is clear");
        // A wall BEYOND the enemy (both endpoints on the near side) does not block.
        assert!(!segment_intersects_aabb(o, Vec2 { x: 3, y: 0 }, &wall), "a wall past the target is clear");

        // Grazing: the sightline passing exactly through a corner counts as blocked
        // (the conservative, parity-tightening direction). The diagonal y=x clips
        // the corner (5,5).
        let corner = box_of((5, 5), (9, 9));
        assert!(segment_intersects_aabb(o, Vec2 { x: 20, y: 20 }, &corner), "a grazed corner blocks");
        // Shift the box one unit up-left so the y=x line passes just outside the
        // nearest corner (6,5) → no longer blocked. (At the box, x in [4,5]: the
        // line is at y=x≤5 while the box floor is y=6, so it clears.)
        let near_miss = box_of((4, 6), (5, 9));
        assert!(!segment_intersects_aabb(o, Vec2 { x: 20, y: 20 }, &near_miss), "a near-missed corner is clear");

        // Edge-on: a zero-width thin wall (a degenerate AABB) on the sightline
        // still blocks — no divide-by-zero, no panic.
        let thin = box_of((5, -3), (5, 3));
        assert!(segment_intersects_aabb(o, Vec2 { x: 10, y: 0 }, &thin), "a thin wall on the sightline blocks");
    }

    #[test]
    fn occlusion_exempts_a_blocker_containing_an_endpoint() {
        // FM4: a pawn standing in (or pressed against) an occluder must be neither
        // blind nor invisible — its own enclosing blocker is exempt, every other
        // still occludes.
        let b = box_of((-5, -5), (5, 5));
        let inside = Vec2 { x: 0, y: 0 };
        let on_edge = Vec2 { x: 5, y: 0 };
        let outside = Vec2 { x: 20, y: 0 };
        assert!(blocker_contains(&b, inside) && blocker_contains(&b, on_edge));
        // Observer inside the box: not occluded by it (would otherwise be blind).
        assert!(!occludes(&b, inside, outside), "the enclosing blocker does not blind its occupant");
        // Target on the box boundary: not occluded by the box it touches.
        assert!(!occludes(&b, outside, on_edge), "a target against a blocker is not hidden by it");
        // A DIFFERENT blocker still occludes the same pair.
        let between = box_of((10, -3), (11, 3));
        assert!(occludes(&between, outside, inside), "an unrelated blocker still occludes");
    }

    #[test]
    fn has_line_of_sight_is_clear_only_when_no_blocker_occludes() {
        // The set-level predicate: a sightline is clear iff NO blocker occludes it,
        // and an empty blocker set is always clear (the no-occlusion default).
        let from = Vec2::ZERO;
        let to = Vec2 { x: 10 * POSITION_SCALE, y: 0 };
        assert!(has_line_of_sight(&[], from, to), "no blockers means a clear sightline");
        let off_axis = box_of((5 * POSITION_SCALE, 5 * POSITION_SCALE), (6 * POSITION_SCALE, 6 * POSITION_SCALE));
        let on_axis = box_of((5 * POSITION_SCALE, -POSITION_SCALE), (6 * POSITION_SCALE, POSITION_SCALE));
        assert!(has_line_of_sight(&[off_axis], from, to), "an off-axis blocker leaves the sightline clear");
        assert!(!has_line_of_sight(&[off_axis, on_axis], from, to), "any blocker on the sightline occludes");
    }

    #[test]
    fn observe_applies_the_fov_cone_and_does_not_leak_the_excluded_position() {
        // Integration: an enemy in range but directly behind is perceived under the
        // default full circle and EXCLUDED under a tight cone — and its position
        // never leaks through the observation.
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "obs".into() },
            SeatInfo { seat: 1, team: 1, controller: "behind".into() },
        ];
        let rear = Vec2 { x: -10 * POSITION_SCALE, y: 0 };
        let make = |spread: u8| {
            let rules = Rules {
                perception_range: 100 * POSITION_SCALE,
                fov_octant_spread: spread,
                spawn_jitter: 0,
                ..Default::default()
            };
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, seats.clone(), Vec::new(), 1);
            for p in &mut m.pawns {
                match p.seat {
                    0 => {
                        p.pos = Vec2::ZERO;
                        p.facing = EAST;
                    }
                    1 => p.pos = rear,
                    _ => {}
                }
            }
            m
        };
        // Default full circle: the rear enemy is perceived.
        let full = make(4);
        let seen: Vec<u32> = full.observe(0).visible.iter().map(|e| e.entity_id).collect();
        assert_eq!(seen, vec![1], "the full circle perceives the rear enemy");
        // Tight cone (facing octant only): excluded, and its position is absent.
        let coned = make(0);
        let obs = coned.observe(0);
        assert!(obs.visible.is_empty(), "the rear enemy is outside the facing-octant cone");
        assert!(obs.visible.iter().all(|e| e.position != rear), "an out-of-cone enemy's position must not leak");
        assert_parity_bound(&coned);
    }

    #[test]
    fn observe_occludes_an_enemy_behind_a_blocker_and_does_not_leak_it() {
        // Integration: an in-range, in-cone enemy directly behind a wall is excluded
        // from the visible set and its position never leaks; remove the wall and the
        // same enemy is perceived — so the blocker, not range or cone, is what hid it.
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "obs".into() },
            SeatInfo { seat: 1, team: 1, controller: "behind".into() },
        ];
        let foe = Vec2 { x: 20 * POSITION_SCALE, y: 0 };
        let wall = Blocker {
            min: Vec2 { x: 9 * POSITION_SCALE, y: -2 * POSITION_SCALE },
            max: Vec2 { x: 11 * POSITION_SCALE, y: 2 * POSITION_SCALE },
        };
        let make = |blockers: Vec<Blocker>| {
            let rules = Rules { perception_range: 100 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, seats.clone(), blockers, 1);
            for p in &mut m.pawns {
                match p.seat {
                    0 => {
                        p.pos = Vec2::ZERO;
                        p.facing = EAST;
                    }
                    1 => p.pos = foe,
                    _ => {}
                }
            }
            m
        };
        let walled = make(vec![wall]);
        let obs = walled.observe(0);
        assert!(obs.visible.is_empty(), "the enemy behind the wall is occluded");
        assert!(obs.visible.iter().all(|e| e.position != foe), "an occluded enemy's position must not leak");
        assert_parity_bound(&walled);

        let clear = make(Vec::new());
        let seen: Vec<u32> = clear.observe(0).visible.iter().map(|e| e.entity_id).collect();
        assert_eq!(seen, vec![1], "with no wall the same enemy is perceived — the blocker was load-bearing");
    }

    #[test]
    fn line_of_sight_is_recomputed_as_the_observer_clears_cover() {
        // FM (perception is per-tick): a stationary enemy starts hidden behind a
        // wall; the observer strafes until its sightline clears the wall, and the
        // enemy appears ONLY once line of sight opens — never through the wall.
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "mover".into() },
            SeatInfo { seat: 1, team: 1, controller: "still".into() },
        ];
        let wall = Blocker {
            min: Vec2 { x: 9 * POSITION_SCALE, y: -2 * POSITION_SCALE },
            max: Vec2 { x: 11 * POSITION_SCALE, y: 2 * POSITION_SCALE },
        };
        let rules = Rules {
            perception_range: 100 * POSITION_SCALE,
            spawn_jitter: 0,
            max_speed: POSITION_SCALE,
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, seats, vec![wall], 1);
        for p in &mut m.pawns {
            match p.seat {
                0 => p.pos = Vec2::ZERO,
                1 => p.pos = Vec2 { x: 20 * POSITION_SCALE, y: 0 },
                _ => {}
            }
        }
        assert!(m.observe(0).visible.is_empty(), "the enemy starts hidden behind the wall");

        let north = intent(Vec2 { x: 0, y: MOVE_INTENT_SCALE }, EAST, false);
        let mut seen = false;
        while m.phase() == MatchPhase::Live && !seen && m.tick() < 100 {
            step_with(&mut m, &[(0, north)]);
            // Whenever the enemy is visible, the sightline must genuinely be clear —
            // never perceived through the wall.
            let me = m.pawns.iter().find(|p| p.seat == 0).unwrap().pos;
            let foe = m.pawns.iter().find(|p| p.seat == 1).unwrap().pos;
            seen = !m.observe(0).visible.is_empty();
            assert_eq!(seen, has_line_of_sight(&m.blockers, me, foe), "visibility tracks line of sight exactly");
        }
        assert!(seen, "the enemy appeared once the observer strafed past the wall");
    }

    #[test]
    fn parity_bound_holds_under_occlusion_through_a_full_match() {
        // The arena-06 per-tick audit with line of sight load-bearing. A wall around
        // the centre seat occludes the two flank seats from each other (full circle
        // FOV + generous range, so only LOS excludes); the bound must hold for every
        // seat every tick, and the occlusion must actually fire (`out_of_los > 0`).
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "a".into() },
            SeatInfo { seat: 1, team: 1, controller: "b".into() },
            SeatInfo { seat: 2, team: 2, controller: "c".into() },
        ];
        let wall = Blocker {
            min: Vec2 { x: -500, y: -3 * POSITION_SCALE },
            max: Vec2 { x: 500, y: 3 * POSITION_SCALE },
        };
        let rules = Rules {
            spawn_radius: 2 * POSITION_SCALE,
            spawn_jitter: 0,
            perception_range: 100 * POSITION_SCALE,
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(3), rules, seats, vec![wall], 1);
        let seat_ids: Vec<SeatId> = m.seats().iter().map(|s| s.seat).collect();
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker), Box::new(Seeker)];

        let counts = assert_parity_bound(&m);
        assert_eq!(counts.out_of_range, 0, "range is generous — it never excludes");
        let mut los_excluded = counts.out_of_los;
        let mut ticks = 0u64;
        while m.phase() == MatchPhase::Live {
            let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
            for (i, &seat) in seat_ids.iter().enumerate() {
                let obs = m.observe(seat);
                if let Some(a) = policies[i].act(&obs) {
                    if let Ok(intent) = m.ingest(seat, &a) {
                        intents.insert(seat, intent);
                    }
                }
            }
            m.step(&intents);
            los_excluded += assert_parity_bound(&m).out_of_los;
            ticks += 1;
        }
        assert!(ticks > 1, "the match ran multiple ticks");
        assert!(los_excluded > 0, "the wall never occluded anyone — the LOS bound check was vacuous");
    }

    #[test]
    fn a_pawn_spawned_inside_a_blocker_is_neither_blind_nor_invisible() {
        // FM4: the seed-driven spawn can land a pawn inside a vision blocker. The
        // endpoint exemption makes that safe with no setup rejection: the occupant
        // still perceives the enemy (not self-blinded) and is still perceived by it
        // (not hidden), while any OTHER blocker would still occlude.
        let rules = Rules { spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        // close_match spawns seat 0 at x = -2 m; wrap that spawn in a blocker.
        let around_spawn0 = Blocker {
            min: Vec2 { x: -3 * POSITION_SCALE, y: -POSITION_SCALE },
            max: Vec2 { x: -POSITION_SCALE, y: POSITION_SCALE },
        };
        let m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), vec![around_spawn0], 1);
        let spawn0 = m.pawns.iter().find(|p| p.seat == 0).unwrap().pos;
        assert!(blocker_contains(&around_spawn0, spawn0), "seat 0 really spawned inside the blocker");
        // Not blind: the enclosed seat still perceives the enemy (in default range).
        let seen0: Vec<u32> = m.observe(0).visible.iter().map(|e| e.entity_id).collect();
        assert_eq!(seen0, vec![1], "a pawn in a blocker is not self-blinded");
        // Not invisible: the enemy still perceives the enclosed seat.
        let seen1: Vec<u32> = m.observe(1).visible.iter().map(|e| e.entity_id).collect();
        assert_eq!(seen1, vec![0], "a pawn in a blocker is not hidden by it");
        assert_parity_bound(&m);
    }

    #[test]
    fn an_enemy_just_past_perception_is_absent_everywhere() {
        // Adversarial: an enemy exactly AT the perception edge is perceived
        // (`within` is inclusive); one a single unit BEYOND it is absent, and its
        // position never appears anywhere in the observation — no occluded entity
        // leaks through.
        let r = 10 * POSITION_SCALE;
        let rules = Rules { perception_range: r, spawn_jitter: 0, ..Default::default() };
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "obs".into() },
            SeatInfo { seat: 1, team: 1, controller: "edge".into() },
            SeatInfo { seat: 2, team: 2, controller: "beyond".into() },
        ];
        let mut m = Match::new(MID.parse().unwrap(), config(3), rules, seats, Vec::new(), 1);
        let beyond_pos = Vec2 { x: r + 1, y: 0 };
        for p in &mut m.pawns {
            match p.seat {
                0 => p.pos = Vec2 { x: 0, y: 0 },
                1 => p.pos = Vec2 { x: r, y: 0 },
                2 => p.pos = beyond_pos,
                _ => {}
            }
        }
        let obs = m.observe(0);
        let ids: Vec<u32> = obs.visible.iter().map(|e| e.entity_id).collect();
        assert_eq!(ids, vec![1], "only the at-edge enemy is perceived");
        assert!(obs.visible.iter().all(|e| e.position != beyond_pos), "a hidden enemy's position must not appear");
        // And the bound holds for every seat in this hand-placed configuration.
        assert_parity_bound(&m);
    }

    #[test]
    fn perception_is_recomputed_as_a_seat_crosses_into_range() {
        // FM3: perception is a per-tick computation, not a t=0 snapshot. A
        // stationary enemy starts beyond range (absent); the observer walks toward
        // it and it appears ONLY once the observer has closed to within range.
        let r = 10 * POSITION_SCALE;
        let rules =
            Rules { perception_range: r, spawn_jitter: 0, max_speed: POSITION_SCALE, ..Default::default() };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        for p in &mut m.pawns {
            match p.seat {
                0 => p.pos = Vec2 { x: 0, y: 0 },
                1 => p.pos = Vec2 { x: r + 3 * POSITION_SCALE, y: 0 },
                _ => {}
            }
        }
        assert!(m.observe(0).visible.is_empty(), "enemy starts beyond perception");

        let east = intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, EAST, false);
        let mut seen = false;
        while m.phase() == MatchPhase::Live && !seen && m.tick() < 100 {
            step_with(&mut m, &[(0, east)]);
            seen = !m.observe(0).visible.is_empty();
        }
        assert!(seen, "the enemy entered perception as the observer closed in");
        assert_eq!(
            m.observe(0).visible.iter().map(|e| e.entity_id).collect::<Vec<_>>(),
            vec![1],
            "exactly the now-in-range enemy is perceived"
        );
        let me = m.pawns.iter().find(|p| p.seat == 0).unwrap().pos;
        let foe = m.pawns.iter().find(|p| p.seat == 1).unwrap().pos;
        assert!(within(me, foe, r), "visibility flipped on exactly when within perception");
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
    fn observed_cooldown_is_zero_exactly_when_a_fire_is_honored() {
        // FM1: step() decrements cooldown at tick start BEFORE the fire gate, so the
        // exposed cooldown subtracts that pending decrement — observe().own.cooldown
        // == 0 IFF a fire submitted for this tick is honored. A raw-counter exposure
        // breaks the contract (at raw cooldown 1 a fire still lands, decrementing to
        // 0 first, yet raw != 0), so this pins the saturating_sub(1).
        let rules = Rules::default();
        let fc = rules.fire_cooldown;
        let dmg = rules.damage;
        let mut m = close_match(1);

        // A fresh pawn reads fire-ready and its shot lands.
        assert_eq!(m.observe(0).own.cooldown, 0, "a fresh pawn is fire-ready");
        let start = m.observe(1).own.health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.observe(1).own.health, start - dmg, "the fresh shot lands");

        // After firing, the exposed cooldown counts down fc-1, fc-2, ..., 1, 0 across
        // the next fc observations, reaching 0 exactly on the re-eligible tick (the
        // raw counter would read fc..1 and never 0 here). Hold each tick so the
        // cooldown window is undisturbed.
        let mut observed = Vec::new();
        for _ in 0..fc {
            observed.push(m.observe(0).own.cooldown);
            step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, false))]);
        }
        assert_eq!(
            observed,
            (0..fc).rev().collect::<Vec<u16>>(),
            "exposed cooldown counts down to 0 on the re-eligible tick"
        );

        // The shot at exposed cooldown 0 lands (a fire one tick earlier, exposed > 0,
        // was a no-op throughout the window above — the enemy took no damage).
        assert_eq!(m.observe(0).own.cooldown, 0, "fire-ready after the window");
        let before = m.observe(1).own.health;
        assert_eq!(before, start - dmg, "no shot landed while exposed cooldown > 0");
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.observe(1).own.health, before - dmg, "the shot at cooldown==0 lands");
    }

    #[test]
    fn movement_is_clamped_to_the_arena_bounds() {
        // FM2: drive seat 1 outward every tick; it pins at the edge, never past.
        let bounds = Vec2 { x: 21 * POSITION_SCALE, y: 50 * POSITION_SCALE };
        let cfg = MatchConfig { bounds, ..config(2) };
        let rules = Rules { spawn_jitter: 0, ..Default::default() };
        let mut m = Match::new(MID.parse().unwrap(), cfg, rules, two_seats(), Vec::new(), 1);
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
        let mut m = Match::new(MID.parse().unwrap(), cfg, rules, two_seats(), Vec::new(), 1);
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

    /// Like [`play`] but with static vision blockers riding into the record. The
    /// blockers go off the y=0 combat line ([`off_line_blocker`]) so the quick,
    /// decisive match is byte-identical in OUTCOME — they exercise the
    /// record/digest binding, not the result.
    fn play_with_blockers(seed: u64, blockers: Vec<Blocker>) -> Match {
        let rules = Rules { spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        let m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), blockers, seed);
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker)];
        run_match(m, &mut policies)
    }

    /// A well-formed blocker off the y=0 combat line: present in the record and the
    /// digest, but not on the combatants' sightline, so the match still ends fast.
    fn off_line_blocker() -> Blocker {
        Blocker {
            min: Vec2 { x: 30 * POSITION_SCALE, y: 30 * POSITION_SCALE },
            max: Vec2 { x: 31 * POSITION_SCALE, y: 31 * POSITION_SCALE },
        }
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
            blockers: Vec::new(),
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
        let mut m = Match::new(MID.parse().unwrap(), config(3), rules, seats, Vec::new(), 1);
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

    #[test]
    fn a_finished_match_record_verifies_and_returns_its_result() {
        // The happy path: a match played to the end re-runs from its record ALONE
        // back to the same result + committed hash.
        let rec = play(1).to_record().unwrap();
        let verified = rec.verify().expect("a faithful record verifies");
        assert_eq!(verified, rec.result, "verify returns the reproduced result");
        assert_eq!(verified.replay_hash.len(), 64, "the committed hash is 32-byte hex");
    }

    #[test]
    fn to_record_only_commits_a_finished_match() {
        // No terminal result yet → nothing to settle, so no record.
        let mut live = close_match(1);
        step_with(&mut live, &[]);
        assert_eq!(live.phase(), MatchPhase::Live);
        assert!(live.to_record().is_none(), "an in-progress match has no record");
        assert!(play(1).to_record().is_some(), "a finished match yields a record");
    }

    #[test]
    fn the_committed_hash_is_stable_across_independent_runs() {
        // FM2: the same seed drives a byte-identical record on two independent
        // runs, and its serde form round-trips and re-verifies unchanged — this is
        // what makes the on-chain commitment meaningful across platforms.
        let a = play(7).to_record().unwrap();
        let b = play(7).to_record().unwrap();
        assert_eq!(a, b, "same seed → identical record");
        assert_eq!(a.verify().unwrap(), a.result);

        let json = serde_json::to_string(&a).unwrap();
        let parsed: MatchRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, a, "record round-trips through JSON");
        assert_eq!(parsed.verify().unwrap(), a.result, "the persisted form re-verifies");
    }

    #[test]
    fn a_swapped_seed_is_rejected_via_the_committed_hash() {
        // The seed is bound into the replay digest even in this jitter-free match
        // where it does not change combat, so swapping it breaks the committed
        // hash — a record cannot be re-settled under a seed it was not played on.
        let mut rec = play(1).to_record().unwrap();
        rec.replay.seed ^= 0xABCD;
        assert!(matches!(rec.verify(), Err(ReplayError::HashMismatch { .. })));
    }

    #[test]
    fn tampered_rules_fail_to_reproduce() {
        // FM1: rules are a determinant the replay stream does not carry. Quadruple
        // the damage and the same actions re-run to a different match — rejected,
        // not silently accepted under a hash that no longer describes it.
        let mut rec = play(1).to_record().unwrap();
        rec.rules.damage = rec.rules.damage.saturating_mul(4);
        assert!(rec.verify().is_err(), "altered rules must not reproduce the record");
    }

    #[test]
    fn a_tampered_action_fails_to_reproduce() {
        // FM1: alter one recorded action (stop every seat firing on a tick) and
        // the re-run diverges — the stream must be the one that was played.
        let mut rec = play(1).to_record().unwrap();
        let tick = rec
            .replay
            .ticks
            .iter_mut()
            .find(|t| t.actions.iter().any(|a| a.intent.buttons.fire))
            .expect("a tick where someone fires");
        for a in &mut tick.actions {
            a.intent.buttons.fire = false;
        }
        assert!(rec.verify().is_err(), "a doctored action stream must not reproduce");
    }

    #[test]
    fn a_truncated_record_is_not_terminal() {
        // FM3: a record cut short does not drive the match to an end. It must fail
        // cleanly as NotTerminal, never hang or panic the verifier.
        let mut rec = play(1).to_record().unwrap();
        assert!(rec.replay.ticks.len() > 1, "the match ran multiple ticks");
        rec.replay.ticks.truncate(1);
        assert_eq!(rec.verify(), Err(ReplayError::NotTerminal));
    }

    #[test]
    fn an_adversarial_setup_is_rejected_without_panicking() {
        // FM3: re-running a record with a negative arena bound or a negated
        // i32::MIN spawn jitter would panic the sim (a `min > max` clamp, a
        // negation overflow). verify must reject these as MalformedSetup BEFORE
        // simulating — the test completing instead of panicking is the assertion.
        let good = play(1).to_record().unwrap();

        let mut r = good.clone();
        r.config.bounds.x = -1;
        assert_eq!(r.verify(), Err(ReplayError::MalformedSetup), "negative x bound");

        let mut r = good.clone();
        r.config.bounds.y = -1;
        assert_eq!(r.verify(), Err(ReplayError::MalformedSetup), "negative y bound");

        let mut r = good.clone();
        r.rules.spawn_jitter = i32::MIN;
        assert_eq!(r.verify(), Err(ReplayError::MalformedSetup), "negated-i32::MIN jitter");

        let mut r = good.clone();
        r.rules.spawn_radius = -1;
        assert_eq!(r.verify(), Err(ReplayError::MalformedSetup), "negative spawn radius");
    }

    #[test]
    fn a_tampered_blocker_breaks_the_committed_hash() {
        // FM1: blockers are vision-only, so a tampered blocker re-runs to the SAME
        // outcomes — the only thing that catches it is the digest. Move one corner
        // and the recomputed hash diverges: the record cannot be re-settled under a
        // blocker set it was not committed with.
        let mut rec = play_with_blockers(1, vec![off_line_blocker()]).to_record().unwrap();
        assert!(!rec.replay.blockers.is_empty(), "the blocker rode into the record");
        rec.replay.blockers[0].max.x += 1;
        assert!(
            matches!(rec.verify(), Err(ReplayError::HashMismatch { .. })),
            "a tampered blocker must break the commitment even though outcomes are unchanged"
        );
    }

    #[test]
    fn a_blocker_record_is_byte_identical_across_runs_and_re_verifies() {
        // Determinism: two independent runs with the same seed AND blockers produce
        // identical digests and serde, and the record round-trips and re-verifies.
        let a = play_with_blockers(7, vec![off_line_blocker()]).into_replay();
        let b = play_with_blockers(7, vec![off_line_blocker()]).into_replay();
        assert_eq!(a.digest(), b.digest(), "blocker records must hash identically across runs");
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());

        let rec = play_with_blockers(7, vec![off_line_blocker()]).to_record().unwrap();
        assert!(rec.verify().is_ok(), "a well-formed blocker record verifies");
        let json = serde_json::to_string(&rec).unwrap();
        let parsed: MatchRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rec, "blocker record round-trips through JSON");
        assert!(parsed.verify().is_ok(), "and re-verifies unchanged");
    }

    #[test]
    fn verify_rejects_an_inverted_blocker() {
        // FM3: an inverted AABB (min greater than max) is geometry the sim never
        // produces; rejected as MalformedBlocker BEFORE the re-run, naming its index.
        let mut r = play_with_blockers(1, vec![off_line_blocker()]).to_record().unwrap();
        r.replay.blockers[0] = Blocker { min: Vec2 { x: 10, y: 0 }, max: Vec2 { x: 0, y: 0 } };
        assert_eq!(r.verify(), Err(ReplayError::MalformedBlocker { index: 0 }), "inverted on x");

        let mut r = play_with_blockers(1, vec![off_line_blocker()]).to_record().unwrap();
        r.replay.blockers[0] = Blocker { min: Vec2 { x: 0, y: 10 }, max: Vec2 { x: 0, y: 0 } };
        assert_eq!(r.verify(), Err(ReplayError::MalformedBlocker { index: 0 }), "inverted on y");
    }

    #[test]
    fn verify_rejects_an_over_budget_blocker_list() {
        // A crafted record with more blockers than the budget is rejected BEFORE the
        // re-run (every blocker is hashed into the digest, an O(n) CPU-DoS).
        let mut r = play_with_blockers(1, vec![off_line_blocker()]).to_record().unwrap();
        let filler = r.replay.blockers[0];
        r.replay.blockers.resize(MAX_REPLAY_BLOCKERS + 1, filler);
        assert_eq!(
            r.verify(),
            Err(ReplayError::TooManyBlockers { blockers: MAX_REPLAY_BLOCKERS + 1, max: MAX_REPLAY_BLOCKERS }),
        );

        // Boundary: exactly at the cap is NOT a budget rejection (the guard is `>`,
        // not `>=`) — it fails later as a HashMismatch (the digest changed), never
        // TooManyBlockers.
        let mut at = play_with_blockers(1, vec![off_line_blocker()]).to_record().unwrap();
        let filler = at.replay.blockers[0];
        at.replay.blockers.resize(MAX_REPLAY_BLOCKERS, filler);
        assert!(!matches!(at.verify(), Err(ReplayError::TooManyBlockers { .. })));
    }

    #[test]
    fn segment_vs_aabb_does_not_overflow_at_coordinate_extremes() {
        // The cross products are i128 because an i32 endpoint difference (~4.3e9)
        // times an i32 corner difference can exceed i64 (~1.8e19 worst case). Drive
        // the widest products; the call completing without panic — and returning the
        // correct result — is the assertion.
        let from = Vec2 { x: i32::MIN, y: i32::MIN };
        let to = Vec2 { x: i32::MAX, y: i32::MAX };
        let full_height = Blocker { min: Vec2 { x: -1, y: i32::MIN }, max: Vec2 { x: 1, y: i32::MAX } };
        assert!(segment_intersects_aabb(from, to, &full_height), "the extreme diagonal crosses a full-height slab");
        let off = Blocker { min: Vec2 { x: i32::MIN, y: i32::MAX - 1 }, max: Vec2 { x: i32::MIN + 1, y: i32::MAX } };
        assert!(!segment_intersects_aabb(from, to, &off), "the diagonal misses a far extreme corner box");
    }

    #[test]
    fn verify_rejects_structural_corruption_cleanly() {
        // FM3: every malformed shape is a typed Err, never a panic — a verifier
        // (settlement, grader, spectator) cannot be DoS'd by a bad record. The
        // test running to completion is itself the no-panic assertion.
        let good = play(1).to_record().unwrap();

        let mut r = good.clone();
        r.replay.protocol_version += 1;
        assert!(matches!(r.verify(), Err(ReplayError::Version(_))), "stale replay version");

        let mut r = good.clone();
        r.result.protocol_version += 1;
        assert!(matches!(r.verify(), Err(ReplayError::Version(_))), "stale result version");

        let mut r = good.clone();
        r.result.match_id = "11111111-1111-1111-1111-111111111111".parse().unwrap();
        assert!(matches!(r.verify(), Err(ReplayError::MatchIdMismatch { .. })), "spliced match id");

        let mut r = good.clone();
        r.config.seats += 1;
        assert!(matches!(r.verify(), Err(ReplayError::InvalidRoster)), "seat count disagrees");

        let mut r = good.clone();
        r.replay.seats.clear();
        r.config.seats = 0;
        assert!(matches!(r.verify(), Err(ReplayError::InvalidRoster)), "empty roster");

        let mut r = good.clone();
        r.replay.seats[1] = r.replay.seats[0].clone();
        assert!(matches!(r.verify(), Err(ReplayError::InvalidRoster)), "duplicate seat id");

        let mut r = good.clone();
        r.replay.ticks[0].actions[0].seat = 9;
        assert!(matches!(r.verify(), Err(ReplayError::UnknownSeat { seat: 9, .. })), "unknown seat");

        let mut r = good.clone();
        let multi = r.replay.ticks.iter().position(|t| t.actions.len() >= 2).expect("a two-actor tick");
        r.replay.ticks[multi].actions.reverse();
        assert!(matches!(r.verify(), Err(ReplayError::SeatOrder { .. })), "non-canonical seat order");

        let mut r = good.clone();
        r.replay.ticks[0].tick = 5;
        assert!(matches!(r.verify(), Err(ReplayError::TickOrder { index: 0, tick: 5 })), "tick order");

        let mut r = good.clone();
        r.result.outcomes[0].score += 1;
        assert!(matches!(r.verify(), Err(ReplayError::ResultMismatch)), "tampered outcome");

        let mut r = good.clone();
        r.result.replay_hash = "0".repeat(64);
        assert!(matches!(r.verify(), Err(ReplayError::HashMismatch { .. })), "tampered commitment");
    }

    #[test]
    fn verify_rejects_an_over_budget_tick_stream() {
        // A crafted record with more ticks than the budget is rejected BEFORE the
        // structural scan and the re-run, so it cannot become a CPU-DoS. Padding
        // with empty ticks keeps the test cheap; the cap reads only `len()`.
        let good = play(1).to_record().unwrap();
        let mut r = good.clone();
        let mut filler = r.replay.ticks[0].clone();
        filler.actions.clear();
        r.replay.ticks.resize(MAX_REPLAY_TICKS + 1, filler);
        assert_eq!(
            r.verify(),
            Err(ReplayError::TooManyTicks { ticks: MAX_REPLAY_TICKS + 1, max: MAX_REPLAY_TICKS }),
        );

        // Boundary: exactly at the cap is NOT rejected by the budget (the guard is
        // `>`, not `>=`) — it fails later for a different reason, never TooManyTicks.
        let mut at = good.clone();
        let mut filler = at.replay.ticks[0].clone();
        filler.actions.clear();
        at.replay.ticks.resize(MAX_REPLAY_TICKS, filler);
        assert!(!matches!(at.verify(), Err(ReplayError::TooManyTicks { .. })));
    }

    #[test]
    fn verify_rejects_an_over_budget_roster() {
        // A crafted oversized roster (O(seats²) combat per re-run tick) is rejected
        // before the roster scan and the re-run. The seat cap is checked before the
        // roster's duplicate-id check, so padding with clones still trips it.
        let good = play(1).to_record().unwrap();
        let mut r = good.clone();
        let filler = r.replay.seats[0].clone();
        r.replay.seats.resize(MAX_REPLAY_SEATS + 1, filler);
        assert_eq!(
            r.verify(),
            Err(ReplayError::TooManySeats { seats: MAX_REPLAY_SEATS + 1, max: MAX_REPLAY_SEATS }),
        );

        // Boundary: exactly at the cap is NOT a budget rejection (it fails the
        // roster check on the duplicate ids instead) — pins `>` not `>=`.
        let mut at = good.clone();
        let filler = at.replay.seats[0].clone();
        at.replay.seats.resize(MAX_REPLAY_SEATS, filler);
        assert!(!matches!(at.verify(), Err(ReplayError::TooManySeats { .. })));
    }

    #[test]
    fn replay_errors_render_a_message() {
        // Every ReplayError prints a stable, prefixed diagnostic — exercises the
        // Display arms so they cannot silently rot.
        let errs = [
            ReplayError::Version(VersionMismatch { ours: 1, theirs: 2 }),
            ReplayError::MatchIdMismatch { replay: MID.parse().unwrap(), result: MID.parse().unwrap() },
            ReplayError::InvalidRoster,
            ReplayError::MalformedSetup,
            ReplayError::MalformedBlocker { index: 2 },
            ReplayError::TooManyTicks { ticks: 1_000_000, max: MAX_REPLAY_TICKS },
            ReplayError::TooManySeats { seats: 500, max: MAX_REPLAY_SEATS },
            ReplayError::TooManyBlockers { blockers: 5000, max: MAX_REPLAY_BLOCKERS },
            ReplayError::UnknownSeat { tick: 3, seat: 9 },
            ReplayError::SeatOrder { tick: 3 },
            ReplayError::TickOrder { index: 2, tick: 5 },
            ReplayError::NotTerminal,
            ReplayError::ResultMismatch,
            ReplayError::HashMismatch { expected: "aa".into(), recomputed: "bb".into() },
        ];
        for e in &errs {
            assert!(e.to_string().starts_with("invalid replay:"), "{e:?}");
        }
    }

    fn result_with(outcomes: Vec<SeatOutcome>) -> MatchResult {
        MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id: MID.parse().unwrap(),
            final_tick: 10,
            outcomes,
            replay_hash: "00".repeat(32),
        }
    }

    #[test]
    fn settlement_picks_the_unique_first_place_seat() {
        let r = result_with(vec![
            SeatOutcome { seat: 0, team: 0, placement: 1, score: 7, alive_at_end: true },
            SeatOutcome { seat: 1, team: 1, placement: 2, score: 3, alive_at_end: false },
        ]);
        assert_eq!(settlement(&r), Settlement::Win { seat: 0 });
    }

    #[test]
    fn settlement_is_a_draw_when_first_place_is_tied() {
        // Both alive at the cap with equal score share placement 1 — a draw, not
        // an arbitrary win for the lower seat. A classifier that took the FIRST
        // placement-1 seat would wrongly settle seat 0.
        let r = result_with(vec![
            SeatOutcome { seat: 0, team: 0, placement: 1, score: 5, alive_at_end: true },
            SeatOutcome { seat: 1, team: 1, placement: 1, score: 5, alive_at_end: true },
        ]);
        assert_eq!(settlement(&r), Settlement::Draw);
    }

    #[test]
    fn settlement_ranks_an_all_down_match_by_placement_not_alive() {
        // Both seats died, but one out-scored the other, so placement ranks it
        // first — a decisive winner. A classifier keyed on `alive_at_end` would
        // call this a draw (nobody alive). Note the winner is the HIGHER seat id,
        // so this also rejects any "first seat wins" shortcut.
        let r = result_with(vec![
            SeatOutcome { seat: 0, team: 0, placement: 2, score: 1, alive_at_end: false },
            SeatOutcome { seat: 1, team: 1, placement: 1, score: 4, alive_at_end: false },
        ]);
        assert_eq!(settlement(&r), Settlement::Win { seat: 1 });
    }

    #[test]
    fn settlement_is_a_draw_when_all_down_with_equal_score() {
        let r = result_with(vec![
            SeatOutcome { seat: 0, team: 0, placement: 1, score: 2, alive_at_end: false },
            SeatOutcome { seat: 1, team: 1, placement: 1, score: 2, alive_at_end: false },
        ]);
        assert_eq!(settlement(&r), Settlement::Draw);
    }

    #[test]
    fn settlement_of_a_real_played_match_matches_its_outcomes() {
        // End-to-end: a genuinely simulated match's result classifies the same way
        // its canonical outcomes rank, so the classifier tracks the sim, not a
        // hand-built fixture.
        let mut m = new_match(7);
        let mut guard = 0;
        while m.phase() == MatchPhase::Live {
            m.step(&BTreeMap::new());
            guard += 1;
            assert!(guard <= 4000, "match should terminate at the tick cap");
        }
        let result = m.result().expect("ended").clone();
        let firsts: Vec<SeatId> =
            result.outcomes.iter().filter(|o| o.placement == 1).map(|o| o.seat).collect();
        match settlement(&result) {
            Settlement::Win { seat } => assert_eq!(firsts, vec![seat]),
            Settlement::Draw => assert!(firsts.len() != 1),
        }
    }
}
