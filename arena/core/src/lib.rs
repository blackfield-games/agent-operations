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
    MatchResult, Observation, PickupKind, PickupSpawn, ReplayRecord, SeatAction, SeatId,
    SeatOutcome, TeamId, TickRecord, Vec2, VersionMismatch, MOVE_INTENT_SCALE, POSITION_SCALE,
    PROTOCOL_VERSION,
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

/// Fixed-point scale of the [`FINE_LUT`] unit vectors (Q15). Larger than
/// [`OCTANT_SCALE`] so the 64-way table's rounded entries sit tighter to the unit
/// circle than the octant diagonals do — the squared-perpendicular hit test divides
/// `dot` by this exact scale, the same shape the octant path divides by [`OCTANT_SCALE`].
const FINE_SCALE: i32 = 32768;
/// Directions in the finer-aim table — 64, one every 5.625° (8× the octant's 45°).
const FINE_DIRS: usize = 64;
/// Quarter-circle span of [`FINE_QSIN`]: it holds sine for `0..=90°`, and the other
/// three quadrants fall out by reflection.
const FINE_QUARTER: usize = FINE_DIRS / 4;
/// First-quadrant sine, `round(FINE_SCALE · sin(k · 90° / FINE_QUARTER))` for
/// `k = 0..=FINE_QUARTER`. Authored once and pinned exactly against an f64 reference
/// (`fine_lut_matches_an_exact_trig_reference`); `[0]`/`[FINE_QUARTER]` are the exact
/// axis endpoints `0` and `FINE_SCALE`.
const FINE_QSIN: [i32; FINE_QUARTER + 1] = [
    0, 3212, 6393, 9512, 12540, 15447, 18205, 20788, 23170, 25330, 27246, 28899, 30274,
    31357, 32138, 32610, 32768,
];

/// Build the full 64-way unit-vector table from [`FINE_QSIN`] at compile time, so a
/// fire is a branchless `O(1)` lookup with no per-shot trig. Each direction's
/// `(cos, sin)` is the quarter table reflected into its quadrant — `q0=(b,a)`,
/// `q1=(−a,b)`, `q2=(−b,−a)`, `q3=(a,−b)` with `a=QSIN[r]`, `b=QSIN[Q−r]` — so the four
/// quadrants are exact mirror images: the table is symmetric by construction, not by
/// rounding luck (the property a finer-aim approximation must not break).
const fn build_fine_lut() -> [(i32, i32); FINE_DIRS] {
    let mut lut = [(0i32, 0i32); FINE_DIRS];
    let mut i = 0;
    while i < FINE_DIRS {
        let r = i % FINE_QUARTER;
        let a = FINE_QSIN[r];
        let b = FINE_QSIN[FINE_QUARTER - r];
        lut[i] = match i / FINE_QUARTER {
            0 => (b, a),
            1 => (-a, b),
            2 => (-b, -a),
            _ => (a, -b),
        };
        i += 1;
    }
    lut
}

/// The 64-way Q15 unit-vector table, indexed by the finer-aim beam direction.
const FINE_LUT: [(i32, i32); FINE_DIRS] = build_fine_lut();

/// The Q15 unit vector for a facing, resolved to the nearest of 64 directions — the
/// [`AimMode::Fine`] beam. Adding half a step (`512` BAM, the table's spacing is
/// `1024`) rounds to nearest rather than truncating, the same half-step bias
/// [`octant_index`] uses.
fn fine_unit(bam: Bam) -> (i32, i32) {
    FINE_LUT[(((bam as u32 + 512) >> 10) & 63) as usize]
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

/// The [`MOVE_INTENT_SCALE`] unit vector a knockback shoves a target along, for a
/// shooter→target bearing `(dx, dy)` — the radial direction classified into the same 8
/// octants combat uses ([`bearing_octant`]), rescaled from [`OCTANT_SCALE`] to the
/// [`MOVE_INTENT_SCALE`] [`Match::slide`] expects. A zero bearing (a target exactly on
/// the shooter) has no direction, so it returns `None` and the caller imparts no shove —
/// the safe degenerate, never an arbitrary lurch. Integer-only, so the shove direction is
/// bit-stable for the twin.
fn knockback_unit(dx: i64, dy: i64) -> Option<Vec2> {
    if dx == 0 && dy == 0 {
        return None;
    }
    let (ox, oy) = OCTANTS[bearing_octant(dx, dy)];
    Some(Vec2 {
        x: (ox as i64 * MOVE_INTENT_SCALE as i64 / OCTANT_SCALE as i64) as i32,
        y: (oy as i64 * MOVE_INTENT_SCALE as i64 / OCTANT_SCALE as i64) as i32,
    })
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
    /// A close-quarters swing — a fire press instantly strikes EVERY enemy within
    /// [`Rules::melee_range`] and the frontal arc (a cleave, not the beam's
    /// nearest-only hit), needs no ammunition, and is gated on
    /// [`Rules::melee_cooldown`]. The always-available option when out of ammo or in
    /// someone's face.
    Melee,
}

/// How finely a fire's beam direction tracks the seat's aim — a match-level
/// [`Rules`] field (server-authoritative, never sent to agents, the same posture as
/// every other combat constant).
///
/// [`Octant`](AimMode::Octant) is the default: hit resolution snaps the full-resolution
/// [`Bam`] facing to the nearest of eight 45° octants ([`octant_unit`]), so a match
/// left at the default is byte-identical to every match and replay that predates this
/// field. [`Fine`](AimMode::Fine) instead derives the beam from a 64-way (5.625°)
/// integer unit-vector table ([`fine_unit`]), so the in-front and lateral-offset
/// tests use the true aim within integer precision and a sub-octant lead lands the
/// shot the octant snap would have missed. The mode changes which shots connect, so
/// it is a determinant of the outcome: it rides in the [`Rules`] a [`MatchRecord`]
/// commits, and a record re-run under a different mode replays to a different result
/// and is rejected by [`verify`](MatchRecord::verify), exactly as a tampered `damage` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AimMode {
    /// Beam snapped to the nearest of eight octants (45° steps) — coarse by design,
    /// the original reference behavior.
    #[default]
    Octant,
    /// Beam taken from the 64-way Q15 unit-vector table — aim resolved to 5.625°.
    Fine,
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

/// Entity-id base for pickups, above BOTH the pawn id space (≤ 255) and the
/// projectile id space (from [`PROJECTILE_ID_BASE`] = 2^16, monotonic but bounded
/// by the match length, never approaching 2^24). A pickup's id is its config index
/// off this base, stable across its whole collect/respawn life, so the canonical
/// ascending-id visible set lists pawns, then projectiles, then pickups, and the
/// three id spaces never collide.
const PICKUP_ID_BASE: u32 = 1 << 24;

/// Default heal/ammo collection radius when a match leaves [`Rules::pickup_radius`]
/// at the serde default — 1 m, a tight contact disc a pawn must actually reach.
fn default_pickup_radius() -> i32 {
    POSITION_SCALE
}

/// Default dormant duration after collection when a match leaves
/// [`Rules::pickup_respawn_cooldown`] at the serde default — 300 ticks (~10 s at
/// 30 Hz), long enough that a contested pickup is a real tempo decision.
fn default_pickup_respawn_cooldown() -> u16 {
    300
}

/// The serde/`Default` value for [`Rules::melee_range`] — a 2 m reach (only read in
/// [`WeaponMode::Melee`]).
fn default_melee_range() -> i32 {
    2 * POSITION_SCALE
}

/// The serde/`Default` value for [`Rules::melee_damage`] — 50, so two swings down a
/// full-health pawn.
fn default_melee_damage() -> u16 {
    50
}

/// The serde/`Default` value for [`Rules::melee_cooldown`] — 15 ticks, a slower
/// cadence than the default ranged [`fire_cooldown`](Rules::fire_cooldown).
fn default_melee_cooldown() -> u16 {
    15
}

/// Frontal half-width of a [`WeaponMode::Melee`] swing as an octant spread, reusing
/// the perception-cone geometry ([`in_fov`]): a fixed `1` (~135° arc, the facing
/// octant ± its two neighbours) — wider than the nearest-only beam but not the full
/// 180° a `dot > 0` half-plane would sweep. A const, not a `Rules` field, so it adds
/// no digest surface.
const MELEE_ARC_SPREAD: u8 = 1;

/// Upward velocity a grounded jump launches with, in position units per tick (1.2 m
/// at [`POSITION_SCALE`]). The fixed impulse for every jump — only [`Rules::gravity`]
/// (the digest-bound knob) tunes the resulting arc, so this stays a compile-time
/// constant the UE5 twin mirrors exactly (the same discipline as [`MELEE_ARC_SPREAD`]),
/// adding no digest surface. Vertical physics are off unless `gravity > 0`, so this is
/// inert by default.
pub const JUMP_VELOCITY: i32 = 1200;

/// Distance a dash bursts along `move_dir`, in position units (3 m at
/// [`POSITION_SCALE`] — roughly fifteen times a default `max_speed` walk step). The
/// fixed burst impulse for every dash — only [`Rules::dash_cooldown`] (the digest-bound
/// knob) tunes the cadence, so the distance stays a compile-time constant the UE5 twin
/// mirrors exactly (the same discipline as [`JUMP_VELOCITY`]/[`MELEE_ARC_SPREAD`]),
/// adding no digest surface. The dash is off unless `dash_cooldown > 0`, so this is
/// inert by default.
pub const DASH_DISTANCE: i32 = 3000;

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
    /// How finely a fire's beam tracks the seat's aim: the 8-way [`AimMode::Octant`]
    /// (the default, byte-identical to every pre-finer-aim match) or the 64-way
    /// [`AimMode::Fine`]. `serde(default)` resolves to `Octant` so a record written
    /// before this field replays under the quantization it actually ran.
    #[serde(default)]
    pub aim_mode: AimMode,
    /// Damage one landed shot deals.
    pub damage: u16,
    /// Ticks between shots; a pawn may fire only when its cooldown is `0`.
    pub fire_cooldown: u16,
    /// Rounds a full magazine holds; `reload` refills to this.
    pub mag_size: u16,
    /// Whether a shot can damage an allied pawn. `false` (the default) hardcodes the
    /// pre-friendly-fire rule — a shot never touches a same-team pawn — so a record
    /// written before this field replays byte-identically. When `true`, a hit lands on
    /// the nearest body regardless of team (the shooter itself is still always
    /// excluded); the team hit deals the same capped damage but credits the shooter no
    /// score (a friendly hit is never rewarded). `serde(default)` resolves to `false`.
    #[serde(default)]
    pub friendly_fire: bool,
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
    /// Contact radius for collecting a world pickup, in position units: an alive
    /// pawn collects a pickup whose centre is within this distance. `serde(default)`
    /// (1 m) so a record written before pickups existed deserializes unchanged.
    #[serde(default = "default_pickup_radius")]
    pub pickup_radius: i32,
    /// Ticks a collected pickup stays dormant before it respawns at its spawn point.
    /// Deterministic per-tick countdown (no wall-clock). `serde(default)` (300) for
    /// back-compat. A pickup match with no pickups never consults it.
    #[serde(default = "default_pickup_respawn_cooldown")]
    pub pickup_respawn_cooldown: u16,
    /// Reach of a [`WeaponMode::Melee`] swing, in position units: the swing strikes
    /// every enemy whose centre is within this distance AND inside the frontal arc.
    /// Consulted only in melee mode; `serde(default)` (2 m) so a pre-melee record
    /// deserializes unchanged.
    #[serde(default = "default_melee_range")]
    pub melee_range: i32,
    /// Damage one [`WeaponMode::Melee`] swing deals to each enemy it cleaves (clamped
    /// to the target's health). `serde(default)` (50 — two swings down a full pawn,
    /// rewarding the closed distance).
    #[serde(default = "default_melee_damage")]
    pub melee_damage: u16,
    /// Ticks between [`WeaponMode::Melee`] swings (the melee analogue of
    /// [`fire_cooldown`](Rules::fire_cooldown); melee consumes no ammo, so this is its
    /// only rate gate). `serde(default)` (15 — a slower cadence than a ranged shot).
    #[serde(default = "default_melee_cooldown")]
    pub melee_cooldown: u16,
    /// Cap on a pawn's damage-absorbing shield pool. A pawn starts with `0` shield and
    /// earns it by collecting a [`PickupKind::Shield`], clamped to this ceiling;
    /// incoming damage drains shield before health. `serde(default)`
    /// (`0`) DISABLES shield — no pawn can hold any, so a match without it (the
    /// default, and every pre-shield record) plays byte-identically. A non-zero value
    /// turns shield pickups on.
    #[serde(default)]
    pub max_shield: u16,
    /// Downward velocity a jump loses per tick, in position units per tick — the
    /// integer gravity for vertical (z-axis) movement. `serde(default)` (`0`) DISABLES
    /// vertical physics: a [`ActionButtons::jump`](arena_proto::ActionButtons) press is
    /// inert, every pawn's `z` stays `0`, and the match plays byte-identically to a
    /// purely 2D one (the default, and every pre-jump record). A positive value turns
    /// jumping on — a grounded jump launches at the fixed [`JUMP_VELOCITY`] and this
    /// gravity pulls it back to the ground. Higher gravity ⇒ a lower, shorter arc.
    /// On its own gravity changes only the observable `z` trajectory, never a combat
    /// outcome — `z` enters HIT resolution only when
    /// [`vertical_hit_tolerance`](Rules::vertical_hit_tolerance)` > 0` (otherwise combat
    /// stays planar, byte-identical to a 2D match). Fall damage remains a deferred
    /// follow-up, but its prerequisite — variable fall heights — is now supplied by
    /// [`knockback_velocity`](Rules::knockback_velocity).
    #[serde(default)]
    pub gravity: i32,
    /// Ticks between dashes — the rate gate for the
    /// [`ability`](arena_proto::ActionButtons::ability) dash, and its on/off switch.
    /// `serde(default)` (`0`) DISABLES the dash entirely: an ability press is inert and
    /// the match plays byte-identically to one without it (the default, and every
    /// pre-dash record). A non-zero value turns the dash on — a grounded ability press
    /// with a movement direction bursts the pawn [`DASH_DISTANCE`] units along
    /// `move_dir` (bounds- and blocker-clamped like a normal step), then this many ticks
    /// must elapse before the next dash. The burst distance is the fixed [`DASH_DISTANCE`]
    /// constant; this cooldown is the only dash tuning the digest binds.
    #[serde(default)]
    pub dash_cooldown: u16,
    /// When `true`, a move whose full swept path is refused by a blocker retries the
    /// axis-separated components (X-only, then Y-only) through the same
    /// [`path_hits_blocker`] test + bounds clamp, so a pawn grazing a wall SLIDES along
    /// the unblocked axis instead of dead-stopping at the step origin; an inside corner
    /// (both axes refused) still holds. `serde(default)` (`false`) keeps the historical
    /// stop-at-origin — byte-identical to every pre-slide record. Reused by the walk AND
    /// the dash (both go through [`slide`](Match::slide)), so enabling it changes both.
    /// No surface-snap (no contact-point rounding), so the rule stays the same integer
    /// segment test the UE5 twin reproduces exactly.
    #[serde(default)]
    pub wall_slide: bool,
    /// Ticks a seat remembers the LAST-KNOWN position of an entity it has lost sight
    /// of — the perception-memory window. `serde(default)` (`0`) DISABLES memory: a
    /// lost entity vanishes from the visible set the instant it leaves
    /// perception ([`observe`](Match::observe) reports only currently-perceived
    /// entities, every one `in_line_of_sight == true`), byte-identical to every
    /// pre-memory record. A non-zero value turns memory on — once a seat has
    /// perceived an entity, when it later passes out of perception its last PERCEIVED
    /// position is surfaced as a [`VisibleEntity`] with `in_line_of_sight == false`,
    /// for this many ticks, then dropped. Only an entity the seat ACTUALLY perceived
    /// can be remembered (the memory refresh uses the same [`perceives`](Match::perceives)
    /// test the visible set does), so memory adds no omniscient signal — it is the
    /// realistic "you remember where you last saw someone". Memory is DERIVED from the
    /// action stream (never recorded), so a replay re-runs it bit-for-bit; this field
    /// folds into [`canonical_encoding`](Rules::canonical_encoding) so the digest binds it.
    #[serde(default)]
    pub perception_memory_ticks: u16,
    /// Max vertical separation, in position units, at which a shot still connects —
    /// the z-coupling of combat. `serde(default)` (`0`) DISABLES it: combat is planar,
    /// a target's elevation is ignored, and a match plays byte-identically to every
    /// pre-z-combat record — the historical behavior where a pawn mid-jump is hit
    /// exactly as on the ground. A positive value couples `z` into EVERY weapon mode:
    /// a hitscan beam, a melee swing, and a projectile (which flies LEVEL at its launch
    /// elevation — the protocol has no vertical aim) each land only when
    /// `|shooter_z - target_z| <= vertical_hit_tolerance` (inclusive), so a pawn that
    /// jumps higher than the tolerance clears the planar shot and jumping becomes a real
    /// evasive tool. Only meaningful with [`gravity`](Rules::gravity)` > 0` (the sole
    /// source of any non-zero `z`); with gravity off every pawn's `z` stays `0`, the
    /// bound never triggers, and the match is byte-identical even with a tolerance set.
    /// Couples HIT resolution only — detection stays planar (a jumping pawn is still
    /// SEEN: perception range and the FOV cone ignore `z`, since a player plainly sees
    /// someone jump). Line-of-sight occlusion is independently z-aware via a
    /// height-bounded [`Blocker`], but that is a separate rule, not this field. Fall
    /// damage is the remaining deferred follow-up — its variable-fall-height
    /// prerequisite is now supplied by [`knockback_velocity`](Rules::knockback_velocity).
    /// Folds into [`canonical_encoding`](Rules::canonical_encoding) so the digest binds it.
    #[serde(default)]
    pub vertical_hit_tolerance: i32,
    /// Upward `z` velocity, in position units per tick, that a landed DAMAGING hit
    /// imparts to the SURVIVING target — the variable-fall-height source. `serde(default)`
    /// (`0`) DISABLES knockback: a hit imparts no vertical impulse and the match plays
    /// byte-identically to every pre-knockback record (the default). A positive value
    /// pops a hit pawn upward: the impulse is added (saturating, stacking onto any
    /// existing `z_vel`, so a mid-air target is launched higher) in the shared
    /// [`damage_pawn`](Match::damage_pawn) sink every weapon mode funnels through, so a
    /// landing's impact velocity now VARIES with the hit it took — a launched pawn can
    /// land harder than a self-jump. Only meaningful with [`gravity`](Rules::gravity)` > 0`
    /// (the sole source of any non-zero `z`); with gravity off the impulse is suppressed
    /// and every pawn's `z` stays `0`, so a 2D match is byte-identical even with knockback
    /// set. Never fires on a miss, on a killed pawn (a corpse is not launched), or on the
    /// shooter. This is VERTICAL-ONLY (a pop-up); directional (horizontal) knockback is a
    /// deferred follow-up. Makes fall damage a non-degenerate, revivable mechanic. Folds
    /// into [`canonical_encoding`](Rules::canonical_encoding) so the digest binds it.
    #[serde(default)]
    pub knockback_velocity: i32,
    /// Horizontal distance, in position units, a landed DAMAGING hit shoves the
    /// SURVIVING target AWAY from the shooter — the planar sibling of the vertical
    /// [`knockback_velocity`](Rules::knockback_velocity) pop-up. `serde(default)` (`0`)
    /// DISABLES it: a hit imparts no planar shove and the match plays byte-identically
    /// to every pre-directional record (the default). A positive value displaces the hit
    /// pawn one step of this length along the shooter→target octant (a projectile pushes
    /// along its travel direction), through the SAME bounds clamp + blocker refusal a
    /// walk uses ([`slide`](Match::slide)) — so the shove can no more tunnel a wall or
    /// leave the arena than a step can; it stops AT a wall, never through it. Applied in
    /// the shared post-hit path every weapon mode reaches, gated on
    /// `knockback_horizontal > 0` ALONE — UNLIKE the vertical impulse it needs NO gravity,
    /// since a planar shove is meaningful in a 2D match. Never on a miss, a killed pawn (a
    /// corpse is not shoved), the shooter, or a target coincident with the shooter (no
    /// bearing ⇒ no shove). Folds into
    /// [`canonical_encoding`](Rules::canonical_encoding) so the digest binds it.
    #[serde(default)]
    pub knockback_horizontal: i32,
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
            aim_mode: AimMode::Octant,
            damage: 25,                 // four shots to down a full-health pawn
            fire_cooldown: 6,           // five shots/sec at 30 Hz
            mag_size: 30,
            friendly_fire: false,
            perception_range: 40 * POSITION_SCALE,
            fov_octant_spread: full_circle_fov(),
            start_health: 100,
            spawn_radius: 20 * POSITION_SCALE,
            spawn_jitter: 2 * POSITION_SCALE,
            action_deadline_micros: 50_000,
            pickup_radius: default_pickup_radius(),
            pickup_respawn_cooldown: default_pickup_respawn_cooldown(),
            melee_range: default_melee_range(),
            melee_damage: default_melee_damage(),
            melee_cooldown: default_melee_cooldown(),
            max_shield: 0, // shield disabled by default — earned only when configured
            gravity: 0,    // vertical physics off by default — jump inert, z stays 0
            dash_cooldown: 0, // dash disabled by default — ability press inert
            wall_slide: false, // a grazing step stops at its origin (no slide) by default
            perception_memory_ticks: 0, // perception memory off — a lost entity vanishes at once
            vertical_hit_tolerance: 0, // combat planar by default — z ignored in hit resolution
            knockback_velocity: 0, // hits impart no vertical impulse by default — z stays 0
            knockback_horizontal: 0, // hits impart no planar shove by default — pos unchanged
        }
    }
}

impl Rules {
    /// The canonical, ordered, integer-only encoding of every sim-affecting field —
    /// the bytes the replay digest folds so the committed `replay_hash` binds the
    /// combat tuning, not just the seed/roster/world/actions. Without this a record
    /// presented with swapped `Rules` (a weaker weapon, friendly fire flipped, a
    /// wider FOV) shares a hash with the match it never ran, caught only by
    /// re-execution; folding this into the digest makes the tuning part of the
    /// commitment a hash-only consumer can check.
    ///
    /// Fields are appended in declaration order, big-endian, fixed-width — so the
    /// concatenation is unambiguous without internal length prefixes and the same
    /// `Rules` yields the same bytes on every platform (no float, no map). Each enum
    /// is an EXPLICIT byte (not its `as` discriminant), so the wire mapping is the
    /// fixed contract a second implementation reproduces — the same discipline the
    /// pickup-kind byte and [`join_digest`](arena_proto::join_digest) follow.
    pub fn canonical_encoding(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.max_speed.to_be_bytes());
        b.extend_from_slice(&self.weapon_range.to_be_bytes());
        b.extend_from_slice(&self.hit_radius.to_be_bytes());
        b.push(match self.weapon_mode {
            WeaponMode::Hitscan => 0,
            WeaponMode::Projectile => 1,
            WeaponMode::Melee => 2,
        });
        b.extend_from_slice(&self.projectile_speed.to_be_bytes());
        b.push(match self.aim_mode {
            AimMode::Octant => 0,
            AimMode::Fine => 1,
        });
        b.extend_from_slice(&self.damage.to_be_bytes());
        b.extend_from_slice(&self.fire_cooldown.to_be_bytes());
        b.extend_from_slice(&self.mag_size.to_be_bytes());
        b.push(self.friendly_fire as u8);
        b.extend_from_slice(&self.perception_range.to_be_bytes());
        b.push(self.fov_octant_spread);
        b.extend_from_slice(&self.start_health.to_be_bytes());
        b.extend_from_slice(&self.spawn_radius.to_be_bytes());
        b.extend_from_slice(&self.spawn_jitter.to_be_bytes());
        b.extend_from_slice(&self.action_deadline_micros.to_be_bytes());
        b.extend_from_slice(&self.pickup_radius.to_be_bytes());
        b.extend_from_slice(&self.pickup_respawn_cooldown.to_be_bytes());
        b.extend_from_slice(&self.melee_range.to_be_bytes());
        b.extend_from_slice(&self.melee_damage.to_be_bytes());
        b.extend_from_slice(&self.melee_cooldown.to_be_bytes());
        b.extend_from_slice(&self.max_shield.to_be_bytes());
        b.extend_from_slice(&self.gravity.to_be_bytes());
        b.extend_from_slice(&self.dash_cooldown.to_be_bytes());
        b.push(self.wall_slide as u8);
        b.extend_from_slice(&self.perception_memory_ticks.to_be_bytes());
        b.extend_from_slice(&self.vertical_hit_tolerance.to_be_bytes());
        b.extend_from_slice(&self.knockback_velocity.to_be_bytes());
        b.extend_from_slice(&self.knockback_horizontal.to_be_bytes());
        b
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
    /// Vertical velocity for the integer jump arc — the per-tick change applied to
    /// `z`, decremented by [`Rules::gravity`] each tick. `0` on the ground; set to
    /// [`JUMP_VELOCITY`] by a grounded jump press. Never recorded (derived from the
    /// action stream like every other live quantity), so replay rebuilds the arc
    /// bit-for-bit. Always `0` when `gravity == 0` (vertical physics off).
    z_vel: i32,
    facing: Bam,
    /// The move delta actually applied last tick (post-clamp), reported to the
    /// owning seat as its velocity.
    vel: Vec2,
    health: u16,
    max_health: u16,
    /// Damage-absorbing armor pool, drained BEFORE health on every hit (overflow
    /// spills to health). Starts at `0` — earned by collecting a
    /// [`PickupKind::Shield`], capped at [`Rules::max_shield`] — so a match with no
    /// shield pickups (or `max_shield == 0`) never has any and plays byte-identically.
    shield: u16,
    ammo: u16,
    /// Ticks remaining before this pawn may fire again.
    cooldown: u16,
    /// Ticks remaining before this pawn may dash again. `0` when the dash is ready (or
    /// disabled). Never recorded (derived from the action stream like `cooldown`/`vel`),
    /// so replay rebuilds it bit-for-bit; always `0` when `dash_cooldown == 0`.
    dash_cooldown: u16,
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
    /// Launch elevation — the shooter's `z` when it fired. The shot flies LEVEL at this
    /// `z` (the protocol has no vertical aim, so there is no ballistic arc), so it lands
    /// only on a body within [`Rules::vertical_hit_tolerance`] of this elevation. Derived
    /// from the recorded fire like every other projectile field, so replay rebuilds it.
    /// Always `0` when gravity is off (the shooter is grounded), so a planar match is
    /// byte-identical.
    z: i32,
    /// Ticks in flight, checked against [`MAX_PROJECTILE_LIFETIME`].
    age: u16,
}

/// One pickup's live match state — derived entirely from a [`PickupSpawn`] config
/// entry plus the tick count, so it is never recorded; replay rebuilds it from the
/// config and re-runs the same collect/respawn timeline bit-for-bit. The agent
/// never sees this struct: a perceivable ACTIVE pickup reaches it as a
/// parity-bounded [`VisibleEntity`] carrying only its `id` and `position` — never
/// its `kind`, `amount`, or `dormant` timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pickup {
    /// Stable per-match id (config index off [`PICKUP_ID_BASE`]) — the wire
    /// `entity_id`, unchanged across its whole collect/respawn life.
    id: u32,
    kind: PickupKind,
    /// Spawn position; a pickup never moves, and respawns here.
    pos: Vec2,
    /// Effect magnitude (heal or ammo), clamped to the pawn's ceiling at collection.
    amount: u16,
    /// Collectible now? `false` while dormant after a collection.
    active: bool,
    /// Ticks remaining dormant; `0` while active. Counts down to a respawn.
    respawn_in: u16,
}

/// One entity a seat has perceived, frozen at the LAST tick it was in sight — the
/// per-seat perception-memory entry (gated by [`Rules::perception_memory_ticks`]).
/// The stored facets are exactly the perceivable ones a live [`VisibleEntity`]
/// carries, captured at `last_seen`; [`observe`](Match::observe) re-surfaces them
/// with `in_line_of_sight == false` while the entry is within the memory window.
/// Never the entity's current (unseen) state — only what the seat actually saw.
#[derive(Clone, Copy)]
struct Remembered {
    kind: arena_proto::EntityKind,
    team: TeamId,
    pos: Vec2,
    z: i32,
    facing: Bam,
    /// The tick the seat last perceived this entity; the entry decays
    /// `perception_memory_ticks` after it.
    last_seen: u64,
}

/// A named arena's static geometry — the vision [`Blocker`]s and world
/// [`PickupSpawn`]s a match plays under. The match core already consumes both
/// (blockers gate perception in [`observe`](Match::observe); pickups drive the
/// collect/respawn loop) and binds them into the replay digest, but every
/// formation path passed empty vecs, so occlusion and items were unreachable in a
/// live match. An `ArenaMap` is the per-arena source the matchmaker loads at
/// formation through [`Match::new_with_pickups`].
///
/// Geometry only: combat tuning ([`Rules`]) stays a separate match input, so an
/// arena's layout and its rules vary independently.
///
/// Its serde form is the data-driven arena map format ([`ArenaMap::from_json`]):
/// `deny_unknown_fields` so an authoring typo (`blocker` for `blockers`) fails
/// loudly instead of silently yielding an empty arena, and each field is
/// `serde(default)` so a blockers-only or pickups-only map still loads (an absent
/// array is empty).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArenaMap {
    /// Static vision occluders (see [`Blocker`]). Empty = no occlusion.
    #[serde(default)]
    pub blockers: Vec<Blocker>,
    /// Collectible world-item spawns (see [`PickupSpawn`]). Empty = no items.
    #[serde(default)]
    pub pickups: Vec<PickupSpawn>,
}

impl ArenaMap {
    /// The empty arena — no occluders, no items: the geometry every match formed
    /// without a named arena plays under, byte-identical to the pre-map-loading
    /// behaviour.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse + validate a data-driven arena map from its JSON form.
    ///
    /// The parser is string-in — the harness/match edge reads files — so arena-core
    /// stays filesystem-free and deterministic (the UE5 twin and any no-fs context
    /// can reuse it). After a successful parse it enforces the SAME structural bounds
    /// [`MatchRecord::verify`] requires — at most [`MAX_REPLAY_BLOCKERS`] blockers and
    /// [`MAX_REPLAY_PICKUPS`] pickups — so any map that loads is guaranteed to pass
    /// verify (the loader can never accept geometry a downstream record would reject),
    /// plus two well-formedness checks the sim relies on: no degenerate (`min > max`)
    /// blocker AABB (the line-of-sight test and the movement clamp both assume
    /// `min <= max`; a zero-area `min == max` point is allowed) and no zero-amount
    /// (no-op) pickup.
    pub fn from_json(json: &str) -> Result<ArenaMap, ArenaMapError> {
        let map: ArenaMap =
            serde_json::from_str(json).map_err(|e| ArenaMapError::Parse(e.to_string()))?;
        if map.blockers.len() > MAX_REPLAY_BLOCKERS {
            return Err(ArenaMapError::TooManyBlockers {
                count: map.blockers.len(),
                max: MAX_REPLAY_BLOCKERS,
            });
        }
        if map.pickups.len() > MAX_REPLAY_PICKUPS {
            return Err(ArenaMapError::TooManyPickups { count: map.pickups.len(), max: MAX_REPLAY_PICKUPS });
        }
        for (index, b) in map.blockers.iter().enumerate() {
            if b.min.x > b.max.x || b.min.y > b.max.y {
                return Err(ArenaMapError::DegenerateBlocker { index });
            }
        }
        for (index, p) in map.pickups.iter().enumerate() {
            if p.amount == 0 {
                return Err(ArenaMapError::EmptyPickup { index });
            }
        }
        Ok(map)
    }
}

/// Why [`ArenaMap::from_json`] rejected a map. Validation runs after a successful
/// parse, so [`Parse`](ArenaMapError::Parse) covers malformed JSON (a syntax error,
/// a wrong field type, or — via `deny_unknown_fields` — an unrecognised key), while
/// the structural variants reject a well-formed-JSON map the sim could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArenaMapError {
    /// The bytes are not a valid [`ArenaMap`] JSON document.
    Parse(String),
    /// More `blockers` than [`MAX_REPLAY_BLOCKERS`] — the cap [`MatchRecord::verify`]
    /// enforces, applied here so a loadable map always passes verify.
    TooManyBlockers { count: usize, max: usize },
    /// More `pickups` than [`MAX_REPLAY_PICKUPS`] (same rationale as
    /// [`TooManyBlockers`](ArenaMapError::TooManyBlockers)).
    TooManyPickups { count: usize, max: usize },
    /// The blocker at `index` is an inverted AABB (`min > max` on an axis), which the
    /// line-of-sight test and the movement clamp both assume cannot happen.
    DegenerateBlocker { index: usize },
    /// The pickup at `index` grants nothing (`amount == 0`) — a no-op item.
    EmptyPickup { index: usize },
}

impl std::fmt::Display for ArenaMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArenaMapError::Parse(msg) => write!(f, "malformed arena map JSON: {msg}"),
            ArenaMapError::TooManyBlockers { count, max } => {
                write!(f, "arena map has {count} blockers, over the {max} cap")
            }
            ArenaMapError::TooManyPickups { count, max } => {
                write!(f, "arena map has {count} pickups, over the {max} cap")
            }
            ArenaMapError::DegenerateBlocker { index } => {
                write!(f, "arena map blocker {index} is degenerate (min > max)")
            }
            ArenaMapError::EmptyPickup { index } => {
                write!(f, "arena map pickup {index} grants nothing (amount == 0)")
            }
        }
    }
}

/// Resolve a builtin arena key to its [`ArenaMap`]. The empty/default key (`""`)
/// and any UNKNOWN key both resolve to [`ArenaMap::empty`] — an unrecognised arena
/// degrades safe to no geometry, never panicking or guessing — so the default
/// formation path stays byte-identical and a misconfigured key can't brick a
/// match.
///
/// `"reference"` is a small symmetric NON-production arena that exercises the
/// loading path end-to-end; real arena layouts are authored content (operator
/// level-design), not hardcoded here.
pub fn arena_map(key: &str) -> ArenaMap {
    match key {
        "reference" => reference_arena(),
        _ => ArenaMap::empty(),
    }
}

/// A minimal, symmetric reference arena: one central square vision occluder and
/// two health pickups mirrored east/west, so neither seat of a 2-seat
/// free-for-all opens with a sightline or item edge. A path-exercising demo,
/// deliberately NOT a tuned production map.
///
/// It is itself authored as the data-driven map format (`maps/reference.json`,
/// embedded at compile time) and loaded through [`ArenaMap::from_json`] — the same
/// path content-authored arenas take — so the format is dogfooded end to end and
/// there is one source of truth for the reference geometry. The asset is a
/// compile-time constant, so a malformed embedded map is a build/programmer error,
/// hence the `expect`.
fn reference_arena() -> ArenaMap {
    ArenaMap::from_json(include_str!("../maps/reference.json"))
        .expect("the embedded reference arena is a valid map")
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
    /// The configured pickup spawn points, in declared order — the immutable input
    /// recorded into the [`ReplayRecord`]. Empty means no world items (byte-identical
    /// to every pre-pickup match).
    pickup_config: Vec<PickupSpawn>,
    /// Live pickup state, one per `pickup_config` entry in the same order (so ids and
    /// the digest stay stable). Derived — never recorded; rebuilt from the config on
    /// replay.
    pickups: Vec<Pickup>,
    /// Per-seat perception memory (index == seat), each a map from `entity_id` to the
    /// last-perceived snapshot of that entity. Populated only when
    /// [`Rules::perception_memory_ticks`] is non-zero; refreshed and pruned in
    /// [`step`](Match::step), read by [`observe`](Match::observe). Derived state — never
    /// recorded; reconstructed identically on replay because `step` is deterministic.
    /// A `BTreeMap` so iteration is canonical (ascending `entity_id`).
    seat_memory: Vec<BTreeMap<u32, Remembered>>,
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
    /// A match built this way has NO world pickups; use
    /// [`new_with_pickups`](Match::new_with_pickups) to configure them. This keeps
    /// every existing (no-pickup) call site and replay byte-identical.
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
        Match::new_with_pickups(match_id, config, rules, seats, blockers, Vec::new(), seed)
    }

    /// Like [`new`](Match::new), plus the configured world `pickups`. Each spawn
    /// becomes a live, active pickup with a stable id ([`PICKUP_ID_BASE`] + its
    /// config index); the live collect/dormant/respawn state evolves purely from the
    /// tick stream, so it is derived (never recorded) and replay rebuilds it from
    /// this same config. The `pickups` are not validated here — a record-driven
    /// re-run gates them in [`MatchRecord::verify`] before construction.
    pub fn new_with_pickups(
        match_id: Uuid,
        config: arena_proto::MatchConfig,
        rules: Rules,
        seats: Vec<arena_proto::SeatInfo>,
        blockers: Vec<Blocker>,
        pickups: Vec<PickupSpawn>,
        seed: u64,
    ) -> Self {
        let live_pickups = pickups
            .iter()
            .enumerate()
            .map(|(i, p)| Pickup {
                id: PICKUP_ID_BASE + i as u32,
                kind: p.kind,
                pos: p.position,
                amount: p.amount,
                active: true,
                respawn_in: 0,
            })
            .collect();
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
                    z_vel: 0,
                    facing,
                    vel: Vec2::ZERO,
                    health: rules.start_health,
                    max_health: rules.start_health,
                    shield: 0,
                    ammo: rules.mag_size,
                    cooldown: 0,
                    dash_cooldown: 0,
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
            pickup_config: pickups,
            pickups: live_pickups,
            seat_memory: (0..n).map(|_| BTreeMap::new()).collect(),
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

    /// The static vision/cover blockers this match runs under, in declared order —
    /// the same set sent to agents at `GatewayMsg::Start` so an `AgentController` can
    /// path around physical cover. Read-only; the companion to [`config`](Self::config).
    pub fn blockers(&self) -> &[Blocker] {
        &self.blockers
    }

    /// The static pickup spawn config this match runs under, in declared order — the
    /// source the harness projects to the position-only `GatewayMsg::Start.pickup_points`
    /// it sends agents (the spawn POSITIONS are map layout a human knows; the kind/amount
    /// stays the server-side determinant it is here). The read-only companion to
    /// [`blockers`](Self::blockers).
    pub fn pickup_spawns(&self) -> &[PickupSpawn] {
        &self.pickup_config
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

    /// Whether an observer at `(eye, eye_z, facing)` perceives a target at
    /// `(target, target_z)` this tick: inside `perception_range`, within the forward
    /// FOV cone, and with a clear line of sight past every [`Blocker`]. Range and cone
    /// stay PLANAR (a jumping entity is no harder to spot); only the line-of-sight test
    /// reads the elevations, so a height-bounded wall stops occluding a target seen
    /// over its top. The ONE perception test, shared by the
    /// live visible set ([`observe`](Self::observe)) and the perception-memory refresh
    /// ([`refresh_perception_memory`](Self::refresh_perception_memory)) — so a
    /// remembered entity can never be one the observer could not actually see, the
    /// parity bound holding identically across both channels.
    fn perceives(&self, eye: Vec2, eye_z: i32, facing: Bam, target: Vec2, target_z: i32) -> bool {
        within(eye, target, self.rules.perception_range)
            && in_fov(facing, eye, target, self.rules.fov_octant_spread)
            && has_line_of_sight(&self.blockers, eye, eye_z, target, target_z)
    }

    /// Refresh every alive seat's perception memory at `tick`: record each entity the
    /// seat perceives right now (the SAME [`perceives`](Self::perceives) test the live
    /// visible set uses, so memory can only ever hold an actually-seen entity) as its
    /// last-known snapshot, then drop entries older than the window so memory stays
    /// bounded. Called at the START of [`step`](Self::step) — before any movement — so
    /// the snapshot is the tick-`tick` state the seat's `observe` reported. Runs only
    /// when [`Rules::perception_memory_ticks`] is non-zero (the caller gates it).
    fn refresh_perception_memory(&mut self, tick: u64) {
        let window = self.rules.perception_memory_ticks as u64;
        // Snapshot the alive observers first, so the perceive-and-gather below borrows
        // self immutably and never overlaps the mutable seat_memory write that applies it.
        let observers: Vec<(Vec2, i32, Bam, SeatId)> =
            self.pawns.iter().filter(|p| p.alive).map(|p| (p.pos, p.z, p.facing, p.seat)).collect();
        for (eye, eye_z, facing, seat) in observers {
            let mut seen: Vec<(u32, Remembered)> = Vec::new();
            for p in &self.pawns {
                if p.seat != seat && p.alive && self.perceives(eye, eye_z, facing, p.pos, p.z) {
                    seen.push((p.seat as u32, Remembered {
                        kind: arena_proto::EntityKind::Player,
                        team: p.team, pos: p.pos, z: p.z, facing: p.facing, last_seen: tick,
                    }));
                }
            }
            for proj in &self.projectiles {
                if self.perceives(eye, eye_z, facing, proj.pos, proj.z) {
                    seen.push((proj.id, Remembered {
                        kind: arena_proto::EntityKind::Projectile,
                        team: 0, pos: proj.pos, z: 0, facing: proj.facing, last_seen: tick,
                    }));
                }
            }
            for pk in &self.pickups {
                if pk.active && self.perceives(eye, eye_z, facing, pk.pos, 0) {
                    seen.push((pk.id, Remembered {
                        kind: arena_proto::EntityKind::Pickup,
                        team: 0, pos: pk.pos, z: 0, facing: 0, last_seen: tick,
                    }));
                }
            }
            let mem = &mut self.seat_memory[seat as usize];
            for (id, snap) in seen {
                mem.insert(id, snap);
            }
            // Decay: drop anything last seen more than `window` ticks ago. Pruning at
            // age > window never removes a still-surfaceable entry — the last tick an
            // entry surfaces is age == window, and that observe runs before this prune.
            mem.retain(|_, r| tick.saturating_sub(r.last_seen) <= window);
        }
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
            .filter(|p| self.perceives(me.pos, me.z, me.facing, p.pos, p.z))
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
            if self.perceives(me.pos, me.z, me.facing, proj.pos, proj.z) {
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
        // Perceivable ACTIVE pickups, under the IDENTICAL range + cone + LOS bound. A
        // dormant (collected, not-yet-respawned) pickup is NOT emitted, so its absence
        // tracks its real state; a perceived pickup carries only its id and position
        // (neutral team, no facing) and NEVER its kind, amount, or respawn timer.
        for pk in &self.pickups {
            if pk.active && self.perceives(me.pos, me.z, me.facing, pk.pos, 0) {
                visible.push(arena_proto::VisibleEntity {
                    entity_id: pk.id,
                    kind: arena_proto::EntityKind::Pickup,
                    team: 0,
                    position: pk.pos,
                    z: 0,
                    facing: 0,
                    in_line_of_sight: true,
                });
            }
        }
        // Perception memory (gated default-off): surface the last-known position of an
        // entity this seat has lost sight of, as a VisibleEntity with
        // in_line_of_sight == false, until it decays past the window. An entity still in
        // `visible` this tick is skipped (no duplicate); a remembered entry is never one
        // the seat did not actually perceive (the refresh that stored it used the same
        // `perceives` test), so memory surfaces no omniscient signal — only a stale,
        // honestly-flagged echo of a real prior sighting. Off (window 0) adds nothing.
        if self.rules.perception_memory_ticks > 0 {
            let window = self.rules.perception_memory_ticks as u64;
            let live: BTreeSet<u32> = visible.iter().map(|e| e.entity_id).collect();
            for (&id, r) in &self.seat_memory[seat as usize] {
                if live.contains(&id) {
                    continue;
                }
                let age = self.tick.saturating_sub(r.last_seen);
                if age >= 1 && age <= window {
                    visible.push(arena_proto::VisibleEntity {
                        entity_id: id,
                        kind: r.kind,
                        team: r.team,
                        position: r.pos,
                        z: r.z,
                        facing: r.facing,
                        in_line_of_sight: false,
                    });
                }
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
                shield: me.shield,
                ammo: me.ammo,
                // Report the fire cooldown as the NEXT action will see it. `step`
                // decrements `cooldown` at tick start before the fire gate, so the
                // raw value here is one ahead of what a fire submitted for this
                // observation's tick faces: a raw `1` still fires (it decrements to
                // `0` first). Subtracting that pending decrement makes the exposed
                // value `0` exactly when a fire is honored, so the agent's predicate
                // is the clean `cooldown == 0` with no off-by-one to model.
                cooldown: me.cooldown.saturating_sub(1),
                // The dash cooldown decrements at the same tick start as `cooldown`, so
                // it carries the identical pending-decrement adjustment — the agent's
                // dash-ready predicate is the clean `dash_cooldown == 0`.
                dash_cooldown: me.dash_cooldown.saturating_sub(1),
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

    /// Apply a clamped, blocker-respecting displacement from `from`: scale `move_dir`
    /// (in [`MOVE_INTENT_SCALE`] units) by `magnitude` position units and clamp the
    /// destination to the arena bounds. If the full swept path crosses a blocker the
    /// step is refused; with [`Rules::wall_slide`] off (the default) the pawn holds at
    /// `from`, and with it on the axis-separated components are retried — X-only, then
    /// Y-only — through the SAME [`path_hits_blocker`] test + clamp, so a pawn grazing a
    /// wall slides along the unblocked axis while an inside corner (both axes refused)
    /// still holds. The shared movement-safety primitive for both the per-tick walk
    /// (`magnitude == max_speed`) and the dash burst (`magnitude == DASH_DISTANCE`), so
    /// a dash can no more tunnel a wall or leave the arena than a walk can; each
    /// axis-separated retry is a strict single-axis subset of the full step, so it
    /// cannot tunnel either.
    fn slide(&self, from: Vec2, z: i32, move_dir: Vec2, magnitude: i32) -> Vec2 {
        let dx = move_dir.x as i64 * magnitude as i64 / MOVE_INTENT_SCALE as i64;
        let dy = move_dir.y as i64 * magnitude as i64 / MOVE_INTENT_SCALE as i64;
        let bx = self.config.bounds.x as i64;
        let by = self.config.bounds.y as i64;
        let target = |ddx: i64, ddy: i64| Vec2 {
            x: (from.x as i64 + ddx).clamp(-bx, bx) as i32,
            y: (from.y as i64 + ddy).clamp(-by, by) as i32,
        };
        let to = target(dx, dy);
        if !path_hits_blocker(&self.blockers, from, z, to, z) {
            return to;
        }
        // The full move is refused. With wall_slide on, a genuinely diagonal step
        // retries the axis-separated components (X-only first, then Y-only) through the
        // same segment test + clamp, so a pawn grazing a wall slides along the unblocked
        // axis; an inside corner (both refused) holds, and a pure-axis move — no
        // perpendicular component to slide along — simply holds too. Off keeps the
        // historical stop-at-origin, byte-identical. Each retry is a strict single-axis
        // subset of the full step, so it can no more tunnel than the full move can.
        if self.rules.wall_slide && dx != 0 && dy != 0 {
            let slide_x = target(dx, 0);
            if !path_hits_blocker(&self.blockers, from, z, slide_x, z) {
                return slide_x;
            }
            let slide_y = target(0, dy);
            if !path_hits_blocker(&self.blockers, from, z, slide_y, z) {
                return slide_y;
            }
        }
        from
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

        // Perception memory (gated default-off): record what each seat perceives at
        // THIS tick's pre-movement state — the same state its observe(current) reported
        // — so a later tick can surface a since-lost entity's last-known position. Off
        // (window 0) skips this entirely, byte-identical to exclusion-only perception.
        if self.rules.perception_memory_ticks > 0 {
            self.refresh_perception_memory(current);
        }

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
            p.dash_cooldown = p.dash_cooldown.saturating_sub(1);
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
            // A blocker is physical cover: a step whose swept path crosses one is
            // refused outright (the pawn holds position this tick, vel zero) rather
            // than walking through it. Stopping at the START of the blocked step —
            // not snapping to the wall surface — keeps the rule a single integer
            // segment test the UE5 twin reproduces exactly, no rounding to a contact
            // point. A no-blocker match never reaches the refusal (empty set), so it
            // is byte-identical. Surface-snap and wall-sliding are deferred follow-ups.
            let from = self.pawns[i].pos;
            // The pawn's start-of-tick z: the XY slide happens at this elevation, then
            // the vertical block (below) integrates z. So an airborne pawn (z above a
            // low wall's height) walks OVER it; a grounded one (z == 0) is stopped by
            // the footprint exactly as before.
            let z = self.pawns[i].z;
            let to = self.slide(from, z, intent.move_dir, self.rules.max_speed);
            self.pawns[i].facing = intent.aim;

            // Dash: an ability press bursts the pawn an extra DASH_DISTANCE along
            // move_dir, on its own cooldown. Gated default-off (dash_cooldown == 0
            // disables it). It reuses slide() from the POST-walk position, so the burst
            // clamps to bounds and is refused by a blocker exactly like the walk — no
            // tunnel, no out-of-bounds. A zero-move_dir press is a no-op that does NOT
            // consume the cooldown (no direction to dash); otherwise the cooldown is
            // consumed on trigger, even when a wall refuses the burst (the ability was
            // committed — a wall is not a free retry). vel reports the whole tick's
            // applied delta (walk + dash) from the pre-walk origin.
            let dashing = self.rules.dash_cooldown > 0
                && self.pawns[i].dash_cooldown == 0
                && intent.buttons.ability
                && intent.move_dir != Vec2::ZERO;
            let landed = if dashing {
                self.pawns[i].dash_cooldown = self.rules.dash_cooldown;
                self.slide(to, z, intent.move_dir, DASH_DISTANCE)
            } else {
                to
            };
            self.pawns[i].vel = Vec2 { x: landed.x - from.x, y: landed.y - from.y };
            self.pawns[i].pos = landed;
        }

        // Vertical (z-axis) movement: a grounded jump launches at JUMP_VELOCITY and
        // gravity pulls it back down to z == 0. Integer semi-implicit Euler, gated on
        // gravity > 0 so a 2D match (the default) never enters here and stays
        // byte-identical. z is deliberately kept OUT of hit/LOS/perception this slice —
        // combat stays planar — so gravity changes only the observable z trajectory,
        // never an outcome; z-coupled combat and fall damage are follow-ups.
        if self.rules.gravity > 0 {
            for i in 0..self.pawns.len() {
                if !self.pawns[i].alive {
                    continue;
                }
                // Launch only from the ground (z == 0 AND not already moving
                // vertically) so a held jump can't double- or infinite-jump; at the
                // apex z_vel == 0 but z != 0, so the z == 0 conjunct still refuses a
                // re-jump until the pawn has landed.
                let grounded = self.pawns[i].z == 0 && self.pawns[i].z_vel == 0;
                let seat = self.pawns[i].seat;
                if grounded && accepted.get(&seat).is_some_and(|a| a.buttons.jump) {
                    self.pawns[i].z_vel = JUMP_VELOCITY;
                }
                // Integrate every pawn — a forfeited or airborne one keeps falling. The
                // add widens to i64 defensively (z is bounded by the apex
                // JUMP_VELOCITY² / 2·gravity, well within i32); landing snaps exactly to
                // z == 0 and zeroes the velocity, so any (JUMP_VELOCITY, gravity) lands
                // on the ground with no drift and never tunnels to negative z.
                let nz = self.pawns[i].z as i64 + self.pawns[i].z_vel as i64;
                if nz <= 0 {
                    self.pawns[i].z = 0;
                    self.pawns[i].z_vel = 0;
                } else {
                    self.pawns[i].z = nz as i32;
                    self.pawns[i].z_vel = self.pawns[i].z_vel.saturating_sub(self.rules.gravity);
                }
            }
        }

        // Respawn due pickups, then collect at this tick's post-move positions —
        // before fire, so a pawn that walks onto an ammo/health pickup fights with
        // it this tick. A no-op when no pickups are configured (byte-identical).
        self.process_pickups();

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
            if intent.buttons.fire && self.pawns[i].cooldown == 0 {
                match self.rules.weapon_mode {
                    // Melee needs no ammunition — the always-available close-quarters
                    // swing, gated only by its own cooldown.
                    WeaponMode::Melee => {
                        self.pawns[i].cooldown = self.rules.melee_cooldown;
                        self.resolve_melee(i);
                    }
                    // Ranged modes draw a round from the magazine; an empty mag is a
                    // no-op (byte-identical to the pre-melee `ammo > 0` gate).
                    WeaponMode::Hitscan if self.pawns[i].ammo > 0 => {
                        self.pawns[i].ammo -= 1;
                        self.pawns[i].cooldown = self.rules.fire_cooldown;
                        self.resolve_fire(i);
                    }
                    WeaponMode::Projectile if self.pawns[i].ammo > 0 => {
                        self.pawns[i].ammo -= 1;
                        self.pawns[i].cooldown = self.rules.fire_cooldown;
                        self.spawn_projectile(i);
                    }
                    _ => {}
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

    /// Apply `raw` incoming damage to pawn `idx`: drain its [`shield`](Pawn::shield)
    /// pool first, then spill the remainder to health, downing the pawn if health
    /// reaches `0`. Returns the EFFECTIVE damage dealt — shield absorbed plus health
    /// removed, never more than the pawn's effective HP — which the caller credits as
    /// score for an enemy hit. This is the single place the shield/health split lives,
    /// so every weapon (hitscan, melee, projectile) and the UE5 twin share ONE
    /// absorption rule.
    ///
    /// With no shield (the default `max_shield == 0` ⇒ every pawn's `shield == 0`)
    /// this is exactly `raw.min(health)` removed from health and returned, the clean
    /// generalization of the prior per-site clamp — so a shieldless match is
    /// byte-identical. All saturating/clamped integer math: `absorbed <= raw` and
    /// `to_health <= raw - absorbed`, so the return never overflows `u16`.
    fn damage_pawn(&mut self, idx: usize, raw: u16) -> u16 {
        // Read the knockback tuning before borrowing the pawn — disjoint fields, but
        // taking the copies first keeps the impulse block a clean read of the pawn alone.
        let (gravity, knockback) = (self.rules.gravity, self.rules.knockback_velocity);
        let p = &mut self.pawns[idx];
        let absorbed = raw.min(p.shield);
        p.shield -= absorbed;
        let to_health = (raw - absorbed).min(p.health);
        p.health -= to_health;
        if p.health == 0 {
            p.alive = false;
        }
        let effective = absorbed + to_health;
        // Vertical knockback: a hit that DEALT damage to a SURVIVING pawn pops it upward
        // — the variable-fall-height source. Gated on gravity > 0 (the sole source of any
        // non-zero z, so a 2D match stays byte-identical) AND knockback_velocity > 0 (off
        // by default). Saturating, stacking onto any existing z_vel so a mid-air target is
        // launched higher; the z integration's i64 widening absorbs the larger velocity
        // without overflow. A downed pawn is skipped — a corpse leaves gravity
        // integration, so launching it would be dead state churn — and a 0-damage hit
        // imparts nothing, so a miss (which never reaches here) and a fully-absorbed-by-0
        // hit are both inert. This is the single sink all three weapon modes funnel
        // through, so the rule is identical for hitscan, melee, and a projectile, and a
        // friendly-fire hit that deals damage knocks back too (it dealt real damage).
        if knockback > 0 && gravity > 0 && effective > 0 && p.alive {
            p.z_vel = p.z_vel.saturating_add(knockback);
        }
        effective
    }

    /// Apply the planar knockback shove to a SURVIVING target that just took a damaging
    /// hit: displace it [`Rules::knockback_horizontal`] position units along `dir` (the
    /// shooter→target octant from [`knockback_unit`]) through the shared
    /// [`slide`](Self::slide) — the SAME bounds clamp + blocker refusal a walk uses, so
    /// the shove stops AT a wall and never tunnels or leaves the arena. A no-op when
    /// knockback is off (`<= 0`) or the target is down (a corpse is not shoved). Gated on
    /// knockback ALONE — no gravity, a planar shove is meaningful in a 2D match. The
    /// caller passes `dir` only for a hit that DEALT damage and only when the bearing is
    /// non-zero, so a miss and a point-blank coincident target both impart nothing. The
    /// horizontal sibling of the vertical impulse in [`damage_pawn`](Self::damage_pawn) —
    /// it lives here, not in that sink, because the shove needs the shooter's bearing the
    /// sink does not carry.
    fn knock_back_horizontal(&mut self, target: usize, dir: Vec2) {
        if self.rules.knockback_horizontal <= 0 || !self.pawns[target].alive {
            return;
        }
        let (pos, z) = (self.pawns[target].pos, self.pawns[target].z);
        self.pawns[target].pos = self.slide(pos, z, dir, self.rules.knockback_horizontal);
    }

    /// Resolve one beam-hitscan shot from `shooter`: damage the nearest body within
    /// the beam (in range, in front, within the lateral `hit_radius`) — an enemy by
    /// default, or an ally too under [`Rules::friendly_fire`], never the shooter.
    /// All integer: the beam direction is the aim-mode unit vector
    /// (the 8-way [`octant_unit`] or the finer [`fine_unit`]), the in-front test is
    /// a dot-product sign, and the lateral offset is a squared perpendicular
    /// distance — no trig anywhere. The unit vector's scale divides `dot` back to a
    /// real along-beam distance, so each mode carries its own scale.
    fn resolve_fire(&mut self, shooter: usize) {
        let s = self.pawns[shooter];
        let (fx, fy, scale) = match self.rules.aim_mode {
            AimMode::Octant => {
                let (x, y) = octant_unit(s.facing);
                (x, y, OCTANT_SCALE)
            }
            AimMode::Fine => {
                let (x, y) = fine_unit(s.facing);
                (x, y, FINE_SCALE)
            }
        };
        // i128 throughout: positions are i32 and bounded by config.bounds, so a
        // squared planar distance can exceed i64 at extreme (operator-set) arena
        // sizes. Widening keeps the hit test panic-free and exact regardless of
        // bounds, the same defensiveness the move clamp uses.
        let range2 = (self.rules.weapon_range as i128).pow(2);
        let radius2 = (self.rules.hit_radius as i128).pow(2);
        let mut best: Option<(usize, i128)> = None;
        for (j, t) in self.pawns.iter().enumerate() {
            if j == shooter || !t.alive || (!self.rules.friendly_fire && t.team == s.team) {
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
            let proj = dot / scale as i128;
            let perp2 = dist2 - proj * proj;
            if perp2 > radius2 {
                continue;
            }
            // A blocker is physical cover: an enemy in the beam but behind a wall
            // takes no hit — the SAME sightline test perception uses, so a shooter
            // can damage exactly what it could see. Checked last (only for a target
            // already in the beam) so the O(blockers) scan runs only when it matters.
            if !has_line_of_sight(&self.blockers, s.pos, s.z, t.pos, t.z) {
                continue;
            }
            // z-coupled combat (gated default-off): a target too far above/below the
            // shooter's elevation clears the planar beam. A no-op when the tolerance is
            // 0 (combat planar) or every z is 0 (gravity off), so a 2D match is
            // byte-identical.
            if !within_vertical_tolerance(s.z, t.z, self.rules.vertical_hit_tolerance) {
                continue;
            }
            if best.is_none_or(|(_, d)| dist2 < d) {
                best = Some((j, dist2));
            }
        }
        if let Some((j, _)) = best {
            let friendly = self.pawns[j].team == s.team;
            let raw = self.rules.damage;
            let dealt = self.damage_pawn(j, raw);
            // A friendly hit (only reachable under `friendly_fire`) deals damage but
            // never scores — a team hit is never rewarded.
            if !friendly {
                self.pawns[shooter].score += dealt as i32;
            }
            // Planar knockback: a damaging hit shoves the survivor away from the shooter
            // along their bearing (no-op when knockback_horizontal is off).
            if dealt > 0 {
                if let Some(dir) = knockback_unit(
                    self.pawns[j].pos.x as i64 - s.pos.x as i64,
                    self.pawns[j].pos.y as i64 - s.pos.y as i64,
                ) {
                    self.knock_back_horizontal(j, dir);
                }
            }
        }
    }

    /// Resolve one [`WeaponMode::Melee`] swing from `shooter`: a CLEAVE that strikes
    /// EVERY enemy within [`melee_range`](Rules::melee_range), inside the frontal arc
    /// ([`MELEE_ARC_SPREAD`] octants of facing, the same integer cone perception
    /// uses), and with a clear sightline — never the shooter, and an ally only under
    /// [`friendly_fire`](Rules::friendly_fire). Unlike the nearest-only beam, all
    /// matched targets take damage. Targets are collected in seat order, then damaged,
    /// so a same-tick multi-hit is deterministic and the immutable scan can't alias the
    /// mutation. All integer: a squared planar range compare + the octant arc test. A
    /// point-blank enemy (exactly on the shooter) is always struck regardless of facing,
    /// since `in_fov` treats a zero offset as in-arc — the same edge perception uses.
    fn resolve_melee(&mut self, shooter: usize) {
        let s = self.pawns[shooter];
        let range2 = (self.rules.melee_range as i128).pow(2);
        let mut hits: Vec<usize> = Vec::new();
        for (j, t) in self.pawns.iter().enumerate() {
            if j == shooter || !t.alive || (!self.rules.friendly_fire && t.team == s.team) {
                continue;
            }
            let dx = t.pos.x as i128 - s.pos.x as i128;
            let dy = t.pos.y as i128 - s.pos.y as i128;
            if dx * dx + dy * dy > range2 {
                continue;
            }
            if !in_fov(s.facing, s.pos, t.pos, MELEE_ARC_SPREAD) {
                continue;
            }
            if !has_line_of_sight(&self.blockers, s.pos, s.z, t.pos, t.z) {
                continue;
            }
            // z-coupled combat (gated default-off): an enemy out of the shooter's
            // vertical reach is not cleaved — the same rule the beam uses, so a
            // mid-air enemy escapes a ground swing. No-op when planar (tolerance 0).
            if !within_vertical_tolerance(s.z, t.z, self.rules.vertical_hit_tolerance) {
                continue;
            }
            hits.push(j);
        }
        for j in hits {
            let friendly = self.pawns[j].team == s.team;
            let raw = self.rules.melee_damage;
            let dealt = self.damage_pawn(j, raw);
            // A friendly hit (only under `friendly_fire`) deals damage but never
            // scores — mirrors resolve_fire.
            if !friendly {
                self.pawns[shooter].score += dealt as i32;
            }
            // Planar knockback: every cleaved survivor is shoved away from the shooter
            // along its own bearing (so a cleave fans the struck enemies outward).
            if dealt > 0 {
                if let Some(dir) = knockback_unit(
                    self.pawns[j].pos.x as i64 - s.pos.x as i64,
                    self.pawns[j].pos.y as i64 - s.pos.y as i64,
                ) {
                    self.knock_back_horizontal(j, dir);
                }
            }
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
            z: s.z, // launch elevation — the shot flies level at the shooter's z
            age: 0,
        });
    }

    /// Advance every live projectile one tick, resolve hits, and drop the spent ones.
    /// A no-op while nothing is in flight (every hitscan tick, and a projectile match
    /// before its first fire), so it never perturbs a hitscan match. Each projectile
    /// sweeps the segment from its previous to its new position and damages the nearest
    /// body that segment crosses (an enemy, or an ally under [`Rules::friendly_fire`];
    /// never its own shooter) — swept, so a fast shot cannot tunnel through a pawn
    /// between ticks. A hit consumes the shot and credits its shooter (even if the
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
            // The nearest body the swept segment reaches, by distance from the launch
            // end of the sweep (seat order breaks an exact tie) — the same
            // nearest-target rule hitscan uses.
            let mut hit: Option<usize> = None;
            let mut best = i128::MAX;
            for (j, t) in self.pawns.iter().enumerate() {
                if !t.alive || t.seat == proj.shooter || (!self.rules.friendly_fire && t.team == proj.team) {
                    continue;
                }
                if !segment_hits_disc(from, to, t.pos, radius) {
                    continue;
                }
                // Physical cover: a body behind a wall is not hit through it. The
                // same shooter->target sightline test hitscan and perception use, so
                // a pawn IN FRONT of a wall (clear sightline from the shot) still
                // takes the hit while one behind it is spared.
                if !has_line_of_sight(&self.blockers, from, proj.z, t.pos, t.z) {
                    continue;
                }
                // z-coupled combat (gated default-off): the shot flies level at its
                // launch z, so a target too far above/below it is not struck. No-op
                // when planar (tolerance 0) or grounded (proj.z and t.z both 0).
                if !within_vertical_tolerance(proj.z, t.z, self.rules.vertical_hit_tolerance) {
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
                let friendly = self.pawns[j].team == proj.team;
                let raw = self.rules.damage;
                let dealt = self.damage_pawn(j, raw);
                // A friendly hit deals damage but never scores — mirrors resolve_fire.
                if !friendly {
                    if let Some(sp) = self.pawns.iter_mut().find(|p| p.seat == proj.shooter) {
                        sp.score += dealt as i32;
                    }
                }
                // Planar knockback: a projectile shoves the survivor along its TRAVEL
                // direction (away from the shooter it flew from), not a recomputed bearing.
                if dealt > 0 {
                    if let Some(dir) = knockback_unit(proj.vel.x as i64, proj.vel.y as i64) {
                        self.knock_back_horizontal(j, dir);
                    }
                }
                continue; // consumed on hit
            }
            // No body in front: a swept step that runs into a wall is spent against
            // it (no damage past cover), reusing the crosses-a-blocker predicate
            // movement uses so a shot and a pawn agree on what a wall stops. A pawn
            // in front of the wall was already hit above; only a clean step reaching
            // a wall lands here, and a no-blocker match never does.
            // The shot flies level at its launch z, so it is absorbed only by a wall
            // tall enough to stand in its path — a low wall it flies over does not stop
            // it (the same z-aware rule its sightline check above uses).
            if path_hits_blocker(&self.blockers, from, proj.z, to, proj.z) {
                continue; // absorbed by the blocker
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

    /// Tick the world pickups: respawn any whose dormant timer has elapsed, then let
    /// each alive pawn (in seat order) collect every active pickup its body reaches.
    /// A no-op when no pickups are configured, so a pickup-free match is byte-identical.
    ///
    /// Collection is ATOMIC and SEAT-ORDERED: a pickup is consumed (deactivated +
    /// its respawn timer armed) the instant a pawn collects it, BEFORE the next pawn
    /// is checked, so two pawns reaching one pickup the same tick resolve to exactly
    /// one collector — the lower seat — with no double-grant. The effect is CLAMPED
    /// to the pawn's own ceiling (heal to `max_health`, ammo to `mag_size`) with a
    /// `saturating_add`, so collecting at the cap is a no-op that never overflows.
    fn process_pickups(&mut self) {
        if self.pickups.is_empty() {
            return;
        }
        let cooldown = self.rules.pickup_respawn_cooldown;
        let radius = self.rules.pickup_radius;
        // Respawn first, so a pickup whose cooldown elapses is collectible this tick.
        for pk in &mut self.pickups {
            if !pk.active {
                pk.respawn_in = pk.respawn_in.saturating_sub(1);
                if pk.respawn_in == 0 {
                    pk.active = true;
                }
            }
        }
        for i in 0..self.pawns.len() {
            if !self.pawns[i].alive {
                continue;
            }
            let pos = self.pawns[i].pos;
            for pk in &mut self.pickups {
                if !pk.active || !within(pos, pk.pos, radius) {
                    continue;
                }
                match pk.kind {
                    PickupKind::Health => {
                        let max = self.pawns[i].max_health;
                        self.pawns[i].health = self.pawns[i].health.saturating_add(pk.amount).min(max);
                    }
                    PickupKind::Ammo => {
                        let max = self.rules.mag_size;
                        self.pawns[i].ammo = self.pawns[i].ammo.saturating_add(pk.amount).min(max);
                    }
                    PickupKind::Shield => {
                        let max = self.rules.max_shield;
                        self.pawns[i].shield = self.pawns[i].shield.saturating_add(pk.amount).min(max);
                    }
                }
                pk.active = false;
                pk.respawn_in = cooldown;
            }
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

    /// Per-seat outcomes, ascending by seat. Placement is by TEAM: every seat on a
    /// team shares one placement, so teammates never contend as rivals. Teams rank
    /// by survivors, then total score, then lowest seat (a stable, deterministic
    /// tiebreak); teams with an equal (survivors, total score) share a placement.
    /// With each seat its own team — the free-for-all default — this reduces
    /// exactly to the per-seat alive>score>seat ranking (survivors is the seat's
    /// 0/1 alive flag, total score is its own), so FFA outcomes stay byte-identical.
    fn outcomes(&self) -> Vec<SeatOutcome> {
        // (survivors, total score, lowest seat) per team.
        let mut teams: BTreeMap<TeamId, (u32, i64, SeatId)> = BTreeMap::new();
        for p in &self.pawns {
            let (survivors, total, low_seat) = teams.entry(p.team).or_insert((0, 0, p.seat));
            *survivors += p.alive as u32;
            *total += p.score as i64;
            *low_seat = (*low_seat).min(p.seat);
        }
        let mut ranked: Vec<(TeamId, u32, i64, SeatId)> =
            teams.into_iter().map(|(t, (surv, total, seat))| (t, surv, total, seat)).collect();
        // Best first: more survivors, then higher total score, then lowest seat.
        ranked.sort_by(|&(_, a_surv, a_sc, a_seat), &(_, b_surv, b_sc, b_seat)| {
            b_surv.cmp(&a_surv).then(b_sc.cmp(&a_sc)).then(a_seat.cmp(&b_seat))
        });
        let mut placement_of_team: BTreeMap<TeamId, u16> = BTreeMap::new();
        let mut place = 0u16;
        let mut prev: Option<(u32, i64)> = None;
        for (i, &(team, surv, sc, _)) in ranked.iter().enumerate() {
            let key = (surv, sc);
            if prev != Some(key) {
                place = (i + 1) as u16;
                prev = Some(key);
            }
            placement_of_team.insert(team, place);
        }
        let mut outcomes: Vec<SeatOutcome> = self
            .pawns
            .iter()
            .map(|p| SeatOutcome {
                seat: p.seat,
                team: p.team,
                placement: placement_of_team[&p.team],
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
            pickups: self.pickup_config.clone(),
            rules_commit: self.rules.canonical_encoding(),
            config: self.config,
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
            rules_commit: self.rules.canonical_encoding(),
            config: self.config,
            seats: self.seats,
            blockers: self.blockers,
            pickups: self.pickup_config,
            ticks: self.ticks,
        }
    }
}

/// A complete, self-determining record of one finished match.
///
/// A bare [`ReplayRecord`] carries the seed, roster, world, action stream, the
/// [`MatchConfig`] (arena bounds, tick cap — whose determinants the digest folds),
/// and a COMMITMENT to the [`Rules`] (its `rules_commit`, which the digest folds) —
/// but not the typed [`Rules`] the sim re-execution consumes, so a `ReplayRecord`
/// alone cannot be RE-RUN: its hash commits the config and the tuning, but
/// reproducing the match still needs the rules themselves (a fingerprint, unlike the
/// config it carries, cannot be re-executed).
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
    /// The record carries more pickups than the verifier will process. Per-tick
    /// collection is O(seats · pickups) and every pickup is hashed into the digest, so
    /// an oversized list is a CPU-DoS; rejected before the re-run. A generous backstop.
    TooManyPickups { pickups: usize, max: usize },
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
    /// The record's stored `replay.rules_commit` does not encode its `rules` — a
    /// self-contradictory record whose rules fingerprint lies about the tuning it
    /// claims. The re-run reconstructs the commit from `rules`, so this is the one
    /// inconsistency re-execution alone would miss; rejected so every accepted record
    /// hashes (for a re-run-free consumer) exactly as its rules say.
    RulesCommitMismatch,
    /// The record's stored `replay.config` does not match its `config` — the same
    /// self-contradiction as [`RulesCommitMismatch`](ReplayError::RulesCommitMismatch), for the arena configuration. The
    /// re-run is built from `config` (not `replay.config`), so a doctored `replay.config`
    /// re-runs clean yet makes the stored `replay` hash differently for a re-run-free
    /// consumer; rejected so every accepted record hashes exactly as its config says.
    ConfigMismatch,
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
            ReplayError::TooManyPickups { pickups, max } => {
                write!(f, "invalid replay: {pickups} pickups exceeds the verifier budget of {max}")
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
            ReplayError::RulesCommitMismatch => {
                write!(f, "invalid replay: stored rules commitment does not match the record's rules")
            }
            ReplayError::ConfigMismatch => {
                write!(f, "invalid replay: stored config does not match the record's config")
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

/// Verifier cost backstop: the most pickups [`MatchRecord::verify`] will process.
/// Per-tick collection is O(seats · pickups) and every pickup is hashed into the
/// digest, so an oversized list is a CPU-DoS; it is rejected before the re-run. Far
/// above any real arena's item count — a DoS backstop, not a gameplay limit.
pub const MAX_REPLAY_PICKUPS: usize = 256;

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
        if self.replay.pickups.len() > MAX_REPLAY_PICKUPS {
            return Err(ReplayError::TooManyPickups {
                pickups: self.replay.pickups.len(),
                max: MAX_REPLAY_PICKUPS,
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

        let fresh = Match::new_with_pickups(
            self.replay.match_id,
            self.config,
            self.rules,
            self.replay.seats.clone(),
            self.replay.blockers.clone(),
            self.replay.pickups.clone(),
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
        // The re-run above commits the rules it ran under (`self.rules`); also pin that
        // the STORED `self.replay.rules_commit` is consistent with them. It is the one
        // `replay` field the re-run RECONSTRUCTS (from `self.rules`) rather than feeds
        // back in — every other field is fed into the fresh match, so tampering it
        // diverges the re-run — so a doctored `rules_commit` (with honest rules and
        // result) would re-run clean here yet make `self.replay` hash differently for a
        // hash-only consumer. Reject that self-contradiction so the stored fingerprint
        // always truthfully encodes the record's rules. (Unlike the digest as a whole,
        // this is exact: post-terminal tick padding is tolerated, a lying commit is not.)
        if self.replay.rules_commit != self.rules.canonical_encoding() {
            return Err(ReplayError::RulesCommitMismatch);
        }
        // Same self-consistency pin for the config: the re-run is built from
        // `self.config` (NOT `self.replay.config`), so `self.replay.config` is the
        // other field re-execution reconstructs rather than consumes — a doctored copy
        // re-runs clean here yet makes `self.replay` hash differently for a hash-only
        // consumer. Exact equality (post-terminal tick padding is still tolerated; a
        // lying config copy is not), matching the `rules_commit` check above.
        if self.replay.config != self.config {
            return Err(ReplayError::ConfigMismatch);
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

/// `true` if a shot from elevation `shooter_z` may land on a target at elevation
/// `target_z` under [`Rules::vertical_hit_tolerance`]. `tolerance == 0` DISABLES the
/// bound — combat is planar, `z` is ignored, byte-identical to every pre-z-combat
/// match — so a decoupled match short-circuits to `true` before touching `z`. A
/// positive tolerance lands the shot only when the elevations are within `tolerance`
/// units (INCLUSIVE), so a pawn that jumps higher than the tolerance clears it. The
/// difference widens to `i64` so it never overflows at any (operator-set) `z`, and the
/// rule is one integer compare a UE5 twin reproduces exactly. Shared by every weapon
/// mode (hitscan, melee, projectile) so the vertical rule lives in ONE place.
fn within_vertical_tolerance(shooter_z: i32, target_z: i32, tolerance: i32) -> bool {
    tolerance == 0 || (shooter_z as i64 - target_z as i64).abs() <= tolerance as i64
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

/// `true` if the sightline from `(from, from_z)` to `(to, to_z)` is clear of every
/// vision blocker. Occlusion only ever REMOVES a perceivable entity, so it cannot
/// widen perception beyond the range+cone set — the parity bound holds a fortiori.
/// The elevations matter only for a height-bounded [`Blocker`]: a wall with
/// `height > 0` no longer occludes a sightline that passes over its top, so a pawn
/// high enough (mid-jump) sees and shoots over low cover; an infinitely-tall wall
/// (`height == 0`) ignores `z` and occludes exactly as the planar test always did.
fn has_line_of_sight(blockers: &[Blocker], from: Vec2, from_z: i32, to: Vec2, to_z: i32) -> bool {
    !blockers.iter().any(|b| occludes(b, from, from_z, to, to_z))
}

/// `true` if point `p` lies within the closed AABB footprint of `b` (boundary
/// inclusive). Planar — the building block of the z-aware [`blocker_contains_3d`],
/// which pairs it with the wall's vertical band for the sight + movement exemptions.
fn blocker_contains(b: &Blocker, p: Vec2) -> bool {
    b.min.x <= p.x && p.x <= b.max.x && b.min.y <= p.y && p.y <= b.max.y
}

/// `true` if `(p, pz)` lies within the closed 3D box of `b` — its footprint AND, for
/// a height-bounded wall, the vertical band `0..=height`. An infinitely-tall wall
/// (`height == 0`) is contained by the footprint alone (any `z`). The endpoint
/// exemption for BOTH the sightline ([`occludes`], both ends) and physical travel
/// ([`path_hits_blocker`], the start only): a pawn standing INSIDE the box is neither
/// blind/invisible through nor trapped by its own occluder, but a pawn ABOVE a low
/// wall is not "inside" it (so it is neither occluded by, nor exempt from, that wall —
/// it simply clears it).
fn blocker_contains_3d(b: &Blocker, p: Vec2, pz: i32) -> bool {
    blocker_contains(b, p) && (b.height == 0 || (0 <= pz && pz <= b.height))
}

/// `true` if blocker `b` occludes the sightline from `(from, from_z)` to `(to, to_z)`:
/// the 3D segment crosses the blocker's closed box AND neither endpoint is inside it.
/// The endpoint exemption is what makes a pawn standing in (or pressed against) an
/// occluder neither blind nor invisible — its own enclosing blocker is skipped, while
/// every other blocker still occludes it. Without it a spawn the seed happened to
/// place inside a blocker would be permanently self-occluded.
fn occludes(b: &Blocker, from: Vec2, from_z: i32, to: Vec2, to_z: i32) -> bool {
    if blocker_contains_3d(b, from, from_z) || blocker_contains_3d(b, to, to_z) {
        return false;
    }
    segment_intersects_box_3d(from, from_z, to, to_z, b)
}

/// Integer 3D segment-vs-box intersection by the separating-axis theorem — the
/// vertical generalization of [`segment_intersects_aabb`]. The box rises from the
/// ground (`z == 0`) to `b.height`; a sightline that passes OVER the top is no longer
/// occluded. `height == 0` is an infinitely-tall wall, so the planar test decides and
/// the elevations are ignored (byte-identical to every pre-height match).
///
/// For a segment vs an AABB, six axes suffice: the three box face normals (a
/// per-axis slab overlap) and the three cross products of the segment direction with
/// each box axis. No separating axis ⇒ they touch or cross, and boundary contact (a
/// grazed top edge) counts as a hit — the same conservative, parity-tightening
/// direction the planar test takes. Everything is doubled (`×2`) so the box centre,
/// half-extents, segment midpoint, and half-vector stay exact integers, and all
/// products are `i128`, so an extreme (operator-set) coordinate neither overflows nor
/// divides — the test is platform-stable like the rest of the sim.
fn segment_intersects_box_3d(from: Vec2, from_z: i32, to: Vec2, to_z: i32, b: &Blocker) -> bool {
    if b.height == 0 {
        return segment_intersects_aabb(from, to, b);
    }
    let (ax, ay, az) = (from.x as i128, from.y as i128, from_z as i128);
    let (bx, by, bz) = (to.x as i128, to.y as i128, to_z as i128);
    let (lox, loy, loz) = (b.min.x as i128, b.min.y as i128, 0i128);
    let (hix, hiy, hiz) = (b.max.x as i128, b.max.y as i128, b.height as i128);
    // Doubled box full-extent, segment full-vector, and (midpoint−centre)×2.
    let (ex, ey, ez) = (hix - lox, hiy - loy, hiz - loz);
    let (dx, dy, dz) = (bx - ax, by - ay, bz - az);
    let (tx, ty, tz) = (ax + bx - (lox + hix), ay + by - (loy + hiy), az + bz - (loz + hiz));
    // Box face normals (per-axis slab): separated if the gap exceeds both spans.
    if tx.abs() > ex + dx.abs() || ty.abs() > ey + dy.abs() || tz.abs() > ez + dz.abs() {
        return false;
    }
    // Segment-direction × box-axis cross products.
    if (ty * dz - tz * dy).abs() > ey * dz.abs() + ez * dy.abs()
        || (tz * dx - tx * dz).abs() > ez * dx.abs() + ex * dz.abs()
        || (tx * dy - ty * dx).abs() > ex * dy.abs() + ey * dx.abs()
    {
        return false;
    }
    true
}

/// `true` if the swept travel `from → to` (at the constant elevation
/// `from_z`/`to_z`) runs into a physical blocker — the collision predicate shared by
/// movement and projectile flight, so the two agree bit-for-bit on "this path
/// crosses a wall". z-aware via [`segment_intersects_box_3d`]: a height-bounded wall
/// (`height > 0`) is cleared by a path that travels OVER its top, so a pawn high
/// enough (mid-jump) walks over low cover and a level shot flies over it — the
/// physical twin of the z-aware SIGHT rule ([`occludes`]), reusing the SAME
/// [`Blocker::height`] so what bounds sight also bounds traversal (no see-over /
/// walk-into split). Both callers pass a CONSTANT z: the sim integrates XY and z
/// sequentially within a tick (movement slides at the pawn's start-of-tick z, then
/// the vertical block integrates z) and a projectile flies level at its launch z, so
/// there is no z-interval to sweep here — though the [`from_z`, `to_z`] form supports
/// one for free if a future ballistic arc needs it. An infinitely-tall wall
/// (`height == 0`) and a grounded path (`z == 0`) both reduce to the planar test, so
/// every 2D match is byte-identical.
///
/// A blocker stops the path unless the path STARTS inside its 3D box: the start-only
/// exemption (vs [`occludes`], which exempts BOTH endpoints) lets a pawn or shot that
/// begins in or pressed against a wall leave it — the same safety valve that keeps a
/// seat the seed spawned inside a blocker from being trapped — while any blocker
/// AHEAD still stops it. Unlike sight, travel is directional: the destination is NOT
/// exempt, so a step whose endpoint lands inside a wall is blocked rather than walking
/// into it. Swept (the whole segment, not the endpoints), so a fast mover cannot
/// tunnel a thin wall in one step; all-integer via [`segment_intersects_box_3d`], so
/// it never panics or divides by zero on a degenerate blocker and is platform-stable.
fn path_hits_blocker(blockers: &[Blocker], from: Vec2, from_z: i32, to: Vec2, to_z: i32) -> bool {
    blockers.iter().any(|b| {
        !blocker_contains_3d(b, from, from_z) && segment_intersects_box_3d(from, from_z, to, to_z, b)
    })
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

// ===========================================================================
// Ranked rating — the deterministic, zero-sum reputation delta a settled A2A
// match applies. A sibling of `settlement()`: where that classifies a match's
// OUTCOME, this turns the outcome plus the two participants' standing ratings
// into the signed reputation each carries on-chain. It is the variable, skill-
// scaled delta the on-chain `AgentRegistry.recordMatchResult(agent, delta)`
// consumes — the fair-MMR curve the fixed win=+k/loss=-k placeholder deferred.
// Pure integer (an Elo logistic sampled into an anchor table, no float), so it
// is byte-stable and a UE5 / on-chain twin reproduces every delta bit-for-bit;
// the `rating_deltas` parity category pins the curve as the cross-impl contract.
// The K-factor MAGNITUDE is an owner-set economic knob (operator-gated before
// mainnet, like the rate params); the COMPUTATION is the build.
// ===========================================================================

/// Conventional seed rating for a fresh ranked agent — the Elo midpoint a new
/// identity starts from before its first match moves it. A ladder keeps the live
/// per-agent value; this is only the centre the [`rating_delta`] curve is built
/// around (two seed-rated agents are an even match, expected score
/// `RATING_SCALE / 2`).
pub const DEFAULT_RATING: i32 = 1500;

/// Fixed-point scale for an expected score: [`expected_score_bp`] returns basis
/// points in `0..=RATING_SCALE` (`10_000` = a certain win, `5_000` = an even
/// match), so the whole rating computation stays in exact integers with no float.
pub const RATING_SCALE: i32 = 10_000;

/// The rating difference past which the expected score is treated as flat. At an
/// 800-point gap the favourite already wins ~99% (the [`EXPECTED_SCORE_TABLE`]
/// tail), so clamping here costs no meaningful resolution and bounds the table
/// index — and, because the clamp is applied before the mirror negate, guarantees
/// the negate can never overflow.
pub const RATING_DIFF_CAP: i32 = 800;

/// Rating-difference step between adjacent [`EXPECTED_SCORE_TABLE`] anchors.
const RATING_DIFF_STEP: i32 = 40;

/// Expected score (basis points) of the higher-rated side at rating differences
/// `0, 40, 80, … 800` — the standard Elo logistic `1 / (1 + 10^(-d/400))` sampled
/// every 40 points and rounded to the nearest basis point. Hand-authored integer
/// constants (no float at runtime), so the curve is identical on every platform and
/// a twin reproduces it exactly. Strictly increasing, so the expected score is
/// monotonic in the rating gap.
const EXPECTED_SCORE_TABLE: [i32; 21] = [
    5000, 5573, 6131, 6661, 7153, 7597, 7992, 8337, 8632, 8882, 9091, 9264, 9406, 9523, 9617, 9693,
    9755, 9804, 9844, 9876, 9901,
];

/// The Elo expected score (basis points, `0..=RATING_SCALE`) of player A against
/// player B from the rating difference `diff = rating_a - rating_b`.
///
/// Pure integer: clamp `diff` to `±RATING_DIFF_CAP`, look up the bracketing
/// [`EXPECTED_SCORE_TABLE`] anchors for its magnitude, linearly interpolate between
/// them, and mirror for a negative difference, so the two sides' expected scores
/// always sum to `RATING_SCALE` (`E(-d) == RATING_SCALE - E(d)`) and `E(0)` is the
/// even-match midpoint. This is the curve a ranked twin and the on-chain reputation
/// must agree on bit-for-bit; it is pinned in the `rating_deltas` parity vectors.
pub fn expected_score_bp(diff: i32) -> i32 {
    let d = diff.clamp(-RATING_DIFF_CAP, RATING_DIFF_CAP);
    let mag = d.abs(); // <= RATING_DIFF_CAP, so the clamp already made the negate safe
    let i = (mag / RATING_DIFF_STEP) as usize;
    let e_pos = if i >= EXPECTED_SCORE_TABLE.len() - 1 {
        EXPECTED_SCORE_TABLE[EXPECTED_SCORE_TABLE.len() - 1]
    } else {
        let (lo, hi) = (EXPECTED_SCORE_TABLE[i], EXPECTED_SCORE_TABLE[i + 1]);
        lo + (hi - lo) * (mag % RATING_DIFF_STEP) / RATING_DIFF_STEP
    };
    if d < 0 {
        RATING_SCALE - e_pos
    } else {
        e_pos
    }
}

/// The result of a head-to-head ranked match from player A's point of view — the
/// input to [`rating_delta`] alongside the two ratings. `Draw` is any shared top
/// placement (see [`settlement`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchOutcome {
    /// Player A won (placed strictly first).
    WinA,
    /// The match was drawn (a shared first placement).
    Draw,
    /// Player B won.
    WinB,
}

/// The signed reputation change a settled ranked match applies to each player.
/// Always zero-sum (`a == -b`), so feeding `a` and `b` to the two
/// `AgentRegistry.recordMatchResult` calls conserves total reputation exactly — no
/// reputation is minted or burned by a match, the on-chain invariant the contract's
/// symmetric +delta/-delta accounting relies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatingDelta {
    /// Reputation change for player A (the first participant).
    pub a: i32,
    /// Reputation change for player B — exactly `-a`.
    pub b: i32,
}

/// The zero-sum Elo reputation delta for a ranked match between players A and B at
/// the given integer ratings, scaled by the owner-set K-factor.
///
/// `delta_a = K · (S_a − E_a)` where `S_a ∈ {1, ½, 0}` for A's win/draw/loss and
/// `E_a` is A's [`expected_score_bp`]; `delta_b = −delta_a`. The product is taken in
/// `i64` then divided by [`RATING_SCALE`], truncating toward zero — so `|delta| ≤ K`
/// (an upset moves a full K, an expected win moves less), the result fits `i32` for
/// any `i32` K, and the negation is exact (`(−x)/d == −(x/d)`), keeping it exactly
/// zero-sum. A negative K is meaningless and clamped to `0` (an inert match).
///
/// Favouring falls out of the curve: the favourite gains LESS for a win, loses MORE
/// for an upset, and a draw moves the favourite DOWN toward the underdog — the self-
/// correcting pressure that keeps a ladder honest. The K MAGNITUDE is an owner-set
/// economic decision (operator-gated before mainnet, like the rate params); only the
/// shape is fixed here.
pub fn rating_delta(rating_a: i32, rating_b: i32, outcome: MatchOutcome, k: i32) -> RatingDelta {
    let k = k.max(0);
    let e_a = expected_score_bp(rating_a.saturating_sub(rating_b));
    let s_a = match outcome {
        MatchOutcome::WinA => RATING_SCALE,
        MatchOutcome::Draw => RATING_SCALE / 2,
        MatchOutcome::WinB => 0,
    };
    let raw = i64::from(k) * i64::from(s_a - e_a);
    let a = (raw / i64::from(RATING_SCALE)) as i32;
    RatingDelta { a, b: -a }
}

/// The zero-sum rating delta for a settled 1v1 ranked match, deriving the outcome
/// from the match's own [`settlement`].
///
/// `rating_a`/`rating_b` are the pre-match ratings of the FIRST and SECOND seat in
/// the result's canonical ascending-seat `outcomes`, so the caller pairs ratings to
/// seats the way the record orders them. Returns `None` for anything but a two-seat
/// result — ranked settlement is head-to-head, and a non-1v1 result has no A-vs-B
/// reputation to apply here. The decisive seat from `settlement` selects `WinA` vs
/// `WinB`; a tie is a `Draw`.
pub fn ranked_delta(result: &MatchResult, rating_a: i32, rating_b: i32, k: i32) -> Option<RatingDelta> {
    let [a, b] = result.outcomes.as_slice() else {
        return None;
    };
    let outcome = match settlement(result) {
        Settlement::Win { seat } if seat == a.seat => MatchOutcome::WinA,
        Settlement::Win { seat } if seat == b.seat => MatchOutcome::WinB,
        // The winner is always one of the two seats for a well-formed result; bail
        // rather than misattribute if a malformed result ever says otherwise.
        Settlement::Win { .. } => return None,
        Settlement::Draw => MatchOutcome::Draw,
    };
    Some(rating_delta(rating_a, rating_b, outcome, k))
}

/// One seat's signed reputation change in a settled multi-seat ranked match — the
/// multi-player analog of a single side of [`RatingDelta`], carrying its `seat` so a
/// caller maps each delta to the right agent without re-deriving the canonical order
/// (a swap would credit the wrong identity on-chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatDelta {
    /// The seat this delta applies to, from the result's canonical ascending order.
    pub seat: SeatId,
    /// The signed reputation change for this seat; the whole field's deltas sum to
    /// exactly `0` (see [`ranked_field_delta`]).
    pub delta: i32,
}

/// The zero-sum, pure-integer reputation deltas for a settled multi-seat (FFA / 3+)
/// ranked match — the generalization of the 1v1 [`ranked_delta`] to a full placement
/// field.
///
/// Each seat plays one virtual head-to-head against every other seat: the pairwise
/// outcome is read from the two seats' `placement` (a lower placement places better —
/// a win; equal placements — a draw), scored through the identical [`rating_delta`]
/// curve, and the pairwise results summed per seat. This is the standard
/// multiplayer-Elo "score against the field vs expected" rule, written as a sum of
/// pairwise games so zero-sum is STRUCTURAL: every pair contributes `(x, -x)` (a
/// [`RatingDelta`] is exactly mirrored), so the whole field sums to exactly `0` — no
/// reputation minted or burned, the on-chain conservation invariant now across N
/// settlements instead of two.
///
/// Deliberately NOT normalized by field size. A seat's swing grows with the field (it
/// plays more pairwise games) and the MAGNITUDE is the owner-set K knob, exactly as in
/// the 1v1 curve; averaging by `N − 1` would add a field-size divisor (a divide-by-zero
/// edge) AND break exact zero-sum, because per-seat integer truncation would no longer
/// cancel across the field. A raw pairwise sum needs no divisor and stays bit-exact.
///
/// `ratings[i]` is the pre-match rating of `result.outcomes[i].seat` — the same
/// positional pairing [`ranked_delta`] uses for its two seats — and the returned
/// [`SeatDelta`]s are in that canonical ascending-seat order. Returns `None` unless the
/// field has at least two seats and `ratings` aligns 1:1 with the outcomes (a
/// degenerate field has no rivalry to settle). On a well-formed two-seat result this
/// agrees bit-for-bit with [`ranked_delta`] — a single pairwise game IS the 1v1.
///
/// Pure integer throughout: the per-seat sum is accumulated in `i64` so a large field
/// never overflows mid-fold, then clamped into `i32` as a panic-free guard. Within the
/// owner-set K band (`(N − 1)·|K|` inside `i32`) the clamp is inert and the field is
/// exactly zero-sum; the computation is byte-stable across platforms and pinned in the
/// `field_deltas` parity category.
pub fn ranked_field_delta(result: &MatchResult, ratings: &[i32], k: i32) -> Option<Vec<SeatDelta>> {
    let outcomes = &result.outcomes;
    if outcomes.len() < 2 || ratings.len() != outcomes.len() {
        return None;
    }
    let n = outcomes.len();
    let mut sums = vec![0i64; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let outcome = if outcomes[i].placement < outcomes[j].placement {
                MatchOutcome::WinA
            } else if outcomes[i].placement > outcomes[j].placement {
                MatchOutcome::WinB
            } else {
                MatchOutcome::Draw
            };
            let d = rating_delta(ratings[i], ratings[j], outcome, k);
            sums[i] += i64::from(d.a);
            sums[j] += i64::from(d.b);
        }
    }
    Some(
        outcomes
            .iter()
            .zip(sums)
            .map(|(o, s)| SeatDelta { seat: o.seat, delta: s.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32 })
            .collect(),
    )
}

// ===========================================================================
// Cross-implementation parity vectors — the UE5-twin conformance set.
//
// Every parity audit elsewhere in this crate checks the reference core against
// ITSELF (it recomputes the same predicate and confirms `observe`/combat agree
// with it), so it pins self-consistency but NOT a contract a *second*
// implementation must meet. The types below model a small, canonical, versioned
// set of `inputs -> exact integer outputs` vectors generated FROM this reference
// core: the spec the UE5 dedicated-server twin (operator-gated, does not exist
// yet) must reproduce bit-for-bit. Every field is integer, every container an
// ordered `Vec`, so the set serializes byte-stably with no float, map, or
// platform-formatting leak. See [`parity_vectors`] for the generator and the
// honest scope of what a passing self-check does and does not prove.
// ===========================================================================

/// One seat's pinned spawn state, read through the public `observe(seat).own` —
/// the integer facets the deterministic spawn formula derives from the seed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnVector {
    pub seat: SeatId,
    pub team: TeamId,
    pub position: Vec2,
    pub facing: Bam,
    pub health: u16,
    pub ammo: u16,
}

/// A pinned spawn case: a fixed `(seed, config, rules, roster)` and the exact
/// per-seat spawn state it must reproduce. Discriminating because it carries a
/// multi-seat roster (exercising the spawn-line spread divisor), non-zero jitter
/// (the PRNG draw order), and seats on both sides of centre (the facing rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnCase {
    pub label: String,
    pub seed: u64,
    pub config: MatchConfig,
    pub rules: Rules,
    pub roster: Vec<arena_proto::SeatInfo>,
    pub spawns: Vec<SpawnVector>,
}

/// Whether — and by which filter, in the sim's range → cone → line-of-sight
/// order — a candidate entity is excluded from an observer's parity-bounded
/// visible set. The FIRST failing filter is the verdict, exactly as
/// [`Match::observe`] applies them, so a divergent twin pinpoints the convention
/// it broke rather than only seeing a different visible set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerceptionVerdict {
    Visible,
    OutOfRange,
    OutOfCone,
    Occluded,
}

/// One candidate enemy in a [`PerceptionCase`]: its ground-truth state and the
/// verdict the observer reaches for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerceptionCandidate {
    pub seat: SeatId,
    pub team: TeamId,
    pub position: Vec2,
    pub alive: bool,
    pub verdict: PerceptionVerdict,
}

/// A pinned perception case: an observer at a fixed pose under a fixed range +
/// FOV cone + blocker set, the candidates around it, and the exact visible set it
/// must produce. The load-bearing edges — an enemy past range, one outside the
/// cone seam, one behind a wall — are the conventions a UE5 twin must match, not
/// a happy-path everyone-in-view tautology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerceptionCase {
    pub label: String,
    pub observer_position: Vec2,
    pub observer_facing: Bam,
    pub perception_range: i32,
    pub fov_octant_spread: u8,
    pub blockers: Vec<Blocker>,
    pub candidates: Vec<PerceptionCandidate>,
    /// The observer's resulting visible entity ids, ascending — the conformance
    /// target. Equals exactly the candidates whose verdict is `Visible`.
    pub visible: Vec<u32>,
}

/// A pinned hitscan case: a shot fired from an explicit pose at an explicit
/// target under a given [`AimMode`] and occluder set, and the damage it deals
/// (`0` == a clean miss). The sub-octant boundary — a fine-aim direction that
/// snaps to an octant axis — is where the two aim modes diverge; a non-empty
/// `blockers` set is physical cover, so a beam to a target behind a wall is
/// blocked. A twin that snaps the fine beam to the octant, or that shoots through
/// a wall, fails the matching case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitCase {
    pub label: String,
    pub shooter_position: Vec2,
    pub shooter_facing: Bam,
    pub target_position: Vec2,
    pub weapon_range: i32,
    pub hit_radius: i32,
    /// Vision blockers between (or around) the shooter and target — physical
    /// cover. Empty for the pure-aim cases; a wall on the sightline forces a miss.
    pub blockers: Vec<Blocker>,
    pub aim_mode: AimMode,
    pub damage: u16,
}

/// A pinned projectile case: a shot launched from an explicit pose toward a
/// target, swept one tick at a time. `ticks_to_hit` is the flight age at which
/// the swept segment first reached the target (`None` == it never did) — swept
/// collision is what stops a fast shot tunnelling through a body between ticks, so
/// a twin doing per-tick point collision fails the fast case. A non-empty
/// `blockers` set is physical cover: a wall on the flight path absorbs the shot
/// (it never reaches a target behind the wall), which a twin that flies a
/// projectile through walls fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectileCase {
    pub label: String,
    pub shooter_position: Vec2,
    pub shooter_facing: Bam,
    pub target_position: Vec2,
    pub projectile_speed: i32,
    pub weapon_range: i32,
    pub hit_radius: i32,
    /// Vision blockers on (or off) the flight path — physical cover. Empty for the
    /// pure-sweep cases; a wall on the path absorbs the shot before the target.
    pub blockers: Vec<Blocker>,
    pub ticks_to_hit: Option<u16>,
    pub damage: u16,
    pub target_downed: bool,
}

/// A pinned full-match case: a complete, self-contained [`MatchRecord`] whose
/// `result` (outcomes + `replay_hash`) a twin must reproduce by re-running the
/// `(config, rules, seed, roster, blockers, action stream)` through its own core
/// — exactly what [`MatchRecord::verify`] does, now as a cross-implementer
/// contract. The three weapon/aim modes prove the determinant binding: the digest
/// commits the INPUTS, the rules, AND the config determinants (v5), so the octant
/// and fine matches — identical action streams differing only in `aim_mode` — hash
/// apart on the rules alone, while the rules also bind the OUTCOMES (the projectile
/// match diverges and a flipped `weapon_mode` fails re-execution).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchCase {
    pub label: String,
    pub record: MatchRecord,
}

/// A pinned movement case: one [`Match::step`] move from an explicit pose under an
/// explicit speed, bounds, and occluder set, and the exact post-step position it
/// produces. Physical cover is the convention it pins: a step whose swept path
/// crosses a blocker is REFUSED (the pawn holds, `end == start`), a step alongside
/// or away from a wall is allowed, a fast step is stopped by a thin wall it would
/// tunnel, and a pawn starting inside a wall can still leave it. A twin that walks
/// through walls, that point-tests movement (and so tunnels), or that traps a
/// wall-spawned pawn fails the matching case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveCase {
    pub label: String,
    pub start: Vec2,
    /// The move intent (`MOVE_INTENT_SCALE` units, server-clamped to `max_speed`).
    pub move_dir: Vec2,
    pub max_speed: i32,
    pub bounds: Vec2,
    pub blockers: Vec<Blocker>,
    /// Seat 0's position after exactly one `step` with this intent.
    pub end: Vec2,
    /// `true` when the step produced no displacement — for these non-zero intents
    /// away from the bounds, a blocker refusal.
    pub blocked: bool,
}

/// A pinned z-coupled-combat case: a shot fired DEAD-ON in the plane (the target is
/// in range, in front, and on a clear sightline) at a target at elevation `target_z`,
/// under a given weapon `mode` and [`Rules::vertical_hit_tolerance`], and the damage
/// it deals (`0` == cleared by elevation). The planar geometry is a point-blank shot
/// that lands in EVERY weapon mode under [`Rules::default`] tuning, so the only thing
/// that can produce `damage == 0` is the vertical rule: with the tolerance off (`0`)
/// `z` is ignored and any elevation is hit; with it on, a target within `tolerance` is
/// hit (the boundary is INCLUSIVE) and one above it clears the shot — for hitscan, a
/// melee swing, and the level-flying projectile alike. A twin that ignores `z` under a
/// set tolerance, that uses an exclusive boundary, or that couples only some weapon
/// modes fails the matching case. All other tuning is [`Rules::default`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerticalHitCase {
    pub label: String,
    pub weapon_mode: WeaponMode,
    pub shooter_position: Vec2,
    pub shooter_z: i32,
    pub target_position: Vec2,
    pub target_z: i32,
    pub vertical_hit_tolerance: i32,
    /// Damage the one shot deals — `0` iff elevation cleared it (never the planar setup).
    pub damage: u16,
}

/// A pinned knockback case (domain v9): a point-blank DAMAGING hit on a GROUNDED target
/// under a given weapon `mode`, with [`Rules::gravity`] and [`Rules::knockback_velocity`]
/// set, and the upward `z_vel` the hit imparts to the target. The planar geometry is the
/// same point-blank shot [`VerticalHitCase`] uses (it lands in every weapon mode under
/// [`Rules::default`] tuning), so the only thing that moves the target's `z_vel` is the
/// knockback rule: a damaging hit on a surviving pawn adds exactly `knockback_velocity`
/// (the shooter never recoils), gated on `gravity > 0` AND `knockback_velocity > 0` —
/// with either off the impulse is suppressed and `z_vel` stays `0`. A twin that drops the
/// impulse, signs it downward, applies it to the shooter, or ignores the gate fails the
/// matching case. All other tuning is [`Rules::default`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnockbackCase {
    pub label: String,
    pub weapon_mode: WeaponMode,
    pub gravity: i32,
    pub knockback_velocity: i32,
    /// Damage the one shot deals — `> 0` confirms the hit landed (knockback rides a real hit).
    pub damage: u16,
    /// The target's `z_vel` immediately after the hit: `knockback_velocity` for a grounded
    /// survivor, `0` if the impulse was suppressed (gravity/knockback off) or dropped.
    pub target_z_vel: i32,
    /// The shooter's `z_vel` after the hit — always `0` (knockback never recoils the shooter).
    pub shooter_z_vel: i32,
    /// Whether the target survived the hit (a corpse is never launched).
    pub target_alive: bool,
}

/// A pinned z-aware-occlusion case: a sightline from `(from, from_z)` to `(to, to_z)`
/// against a single height-bounded [`Blocker`], and whether that wall occludes it. The
/// rule a twin must reproduce: a wall with `height > 0` blocks a ground-level look but
/// NOT one that passes over its top, while a `height == 0` wall is infinitely tall and
/// occludes at any elevation. A twin that ignores the height (occludes the high look),
/// or that lets a still-in-band rising look through, fails the matching case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisionOverCoverCase {
    pub label: String,
    pub from: Vec2,
    pub from_z: i32,
    pub to: Vec2,
    pub to_z: i32,
    pub blocker: Blocker,
    /// `true` iff the wall blocks this sightline.
    pub occluded: bool,
}

/// A pinned z-aware-traversal case: a level path from `(from, from_z)` to `(to, to_z)`
/// against a single height-bounded [`Blocker`], and whether that wall blocks it — the
/// physical twin of [`VisionOverCoverCase`]. The rule a twin must reproduce: a wall
/// with `height > 0` stops a ground path but NOT one that travels over its top, a
/// `height == 0` wall is infinitely tall and stops it at any elevation, and travel is
/// DIRECTIONAL — only the START is exempt (a path beginning inside a wall may leave it),
/// so a path ENDING inside a wall is still blocked (unlike sight, which exempts both
/// ends). A twin that ignores the height, or that exempts the destination, fails the
/// matching case. Movement and a level projectile share the predicate, so one set pins
/// both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovementOverCoverCase {
    pub label: String,
    pub from: Vec2,
    pub from_z: i32,
    pub to: Vec2,
    pub to_z: i32,
    pub blocker: Blocker,
    /// `true` iff the wall blocks this path (movement holds / a projectile is absorbed).
    pub blocked: bool,
}

/// A pinned ranked-rating case: two pre-match ratings, the [`MatchOutcome`], and the
/// K-factor, with the exact integer expected score and the zero-sum per-seat
/// reputation [`RatingDelta`] the curve must produce. The rule a twin must reproduce
/// bit-for-bit: the integer Elo [`expected_score_bp`] from the rating gap, then
/// `K·(S − E)/RATING_SCALE` truncated toward zero, with `delta.b == -delta.a`. A twin
/// with a float curve, a different rounding, or a non-zero-sum split fails the
/// matching case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatingDeltaCase {
    pub label: String,
    pub rating_a: i32,
    pub rating_b: i32,
    pub outcome: MatchOutcome,
    pub k: i32,
    /// A's expected score in basis points ([`expected_score_bp`]).
    pub expected_a_bp: i32,
    /// The zero-sum reputation delta ([`rating_delta`]); `delta.b == -delta.a`.
    pub delta: RatingDelta,
}

/// One seat's input to a [`FieldDeltaCase`]: its canonical `seat`, final `placement`
/// (1-based, tied seats share a rank), and pre-match `rating`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSeat {
    pub seat: SeatId,
    pub placement: u16,
    pub rating: i32,
}

/// A pinned multi-seat ranked-rating case: a placement field with each seat's pre-match
/// rating and the K-factor, and the exact zero-sum per-seat reputation deltas
/// [`ranked_field_delta`] must produce. The rule a twin reproduces: read each pair's
/// outcome from the two seats' relative placement, score it through the integer
/// [`rating_delta`] curve, and sum per seat — every field summing to exactly `0`. A
/// twin with a float curve, a field-size normalization, or a non-zero-sum split fails
/// the matching case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDeltaCase {
    pub label: String,
    /// The placement field, each seat carrying its pre-match rating; canonical
    /// ascending seat order.
    pub seats: Vec<FieldSeat>,
    pub k: i32,
    /// The zero-sum per-seat deltas ([`ranked_field_delta`]): `deltas[i].seat ==
    /// seats[i].seat`, and the `delta`s sum to `0`.
    pub deltas: Vec<SeatDelta>,
}

/// The canonical cross-implementation parity-vector set — the conformance spec
/// the UE5 twin must reproduce. Self-determining and byte-stable: serialize it
/// and the bytes are the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParityVectors {
    /// Domain tag versioning the set's MEANING; a bump signals a deliberate
    /// convention change the twin must follow.
    pub domain: String,
    pub protocol_version: u32,
    pub spawns: Vec<SpawnCase>,
    pub perception: Vec<PerceptionCase>,
    pub moves: Vec<MoveCase>,
    pub hits: Vec<HitCase>,
    pub projectiles: Vec<ProjectileCase>,
    /// z-coupled-combat cases (domain v4): the vertical hit rule across every weapon mode.
    pub vertical_hits: Vec<VerticalHitCase>,
    /// z-aware-occlusion cases (domain v5): a height-bounded wall is cleared by a
    /// high-enough sightline.
    pub vision_over_cover: Vec<VisionOverCoverCase>,
    /// z-aware-traversal cases (domain v6): a height-bounded wall is cleared by a
    /// high-enough level path — the physical twin of `vision_over_cover` (movement and a
    /// level projectile share the collision predicate).
    pub movement_over_cover: Vec<MovementOverCoverCase>,
    /// ranked-rating cases (domain v7): the zero-sum Elo reputation delta a settled
    /// A2A match applies — a settlement-layer rule, NOT a replay-digest input.
    pub rating_deltas: Vec<RatingDeltaCase>,
    /// multi-seat ranked-rating cases (domain v8): the zero-sum per-seat reputation
    /// delta a settled FFA / 3+ ranked match applies — the multi-player generalization
    /// of `rating_deltas`, also a settlement-layer rule.
    pub field_deltas: Vec<FieldDeltaCase>,
    /// knockback cases (domain v9): a damaging hit pops a surviving target upward by
    /// [`Rules::knockback_velocity`] through the one shared damage sink (so every weapon
    /// mode launches), gated on gravity and knockback both on — the variable-fall source.
    pub knockback: Vec<KnockbackCase>,
    pub matches: Vec<MatchCase>,
}

/// Domain tag for the parity-vector set — see [`ParityVectors::domain`]. Bumped to
/// v2 when blockers became physical cover (movement/hitscan/projectile now respect
/// them); bumped to v3 when the replay digest began committing the `MatchConfig`
/// determinants (arena bounds + tick cap); bumped to v4 for z-coupled combat — the
/// gated [`Rules::vertical_hit_tolerance`] widened `canonical_encoding` (so every
/// committed match hash moved) and a new `vertical_hits` category pins the rule;
/// bumped to v5 when blockers gained a `height` (the replay digest folds it at tag
/// v6, moving every committed match hash, and a positive height bounds vision
/// occlusion so a pawn high enough sees over low cover); bumped to v6 when that height
/// also bounds physical TRAVERSAL (movement + a level projectile pass over low cover,
/// the twin of the v5 sight rule) and a new `movement_over_cover` category pins it — a
/// pure-logic change, so no replay-digest tag move this time; bumped to v7 for the
/// ranked-rating delta (a new `rating_deltas` category pinning the zero-sum Elo
/// reputation curve) — a settlement-layer rule, so again no replay-digest tag move;
/// bumped to v8 for the multi-seat ranked-rating delta (a new `field_deltas` category
/// pinning the zero-sum per-seat reputation a settled FFA / 3+ match applies, the
/// multi-player generalization of v7) — also settlement-layer, so no replay-digest tag
/// move; bumped to v9 for the vertical knockback impulse — a gated
/// [`Rules::knockback_velocity`] widened `canonical_encoding` (so every committed match
/// hash moved) and a new `knockback` category pins the rule that a damaging hit pops a
/// surviving target upward; bumped to v10 for DIRECTIONAL knockback — a gated
/// [`Rules::knockback_horizontal`] widened `canonical_encoding` again (every committed
/// match hash moved) and the `knockback` category gained the planar shove that pushes a
/// surviving target away from the shooter. Each is a deliberate convention change every twin must follow.
const PARITY_VECTORS_DOMAIN: &str = "blackfield/arena/parity-vectors/v10";
/// A fixed, v4-shaped match id so every generated record is byte-reproducible (a
/// random id would hash into the digest and make the set non-canonical).
const PARITY_MATCH_ID: &str = "00000000-0000-4000-8000-0000000000a1";
/// The seed the full-match cases run from — fixed, so the spawns and the digest
/// are stable.
const PARITY_MATCH_SEED: u64 = 0x0102_0304_0506_0708;

fn parity_match_id() -> Uuid {
    PARITY_MATCH_ID.parse().expect("a valid fixed v4 match id")
}

fn parity_config(seats: u8) -> MatchConfig {
    MatchConfig {
        tick_hz: 30,
        max_ticks: 3600,
        bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
        seats,
    }
}

/// A roster seat with a fixed, byte-stable controller label (the label is hashed
/// into the digest, so it must be deterministic, not a uuid).
fn parity_seat(seat: SeatId, team: TeamId) -> arena_proto::SeatInfo {
    arena_proto::SeatInfo { seat, team, controller: format!("0x{seat:02x}") }
}

fn parity_intent(move_dir: Vec2, aim: Bam, fire: bool) -> ActionIntent {
    ActionIntent {
        move_dir,
        aim,
        buttons: arena_proto::ActionButtons { fire, jump: false, ability: false, reload: false },
    }
}

/// Build a spawn case: construct the match and read each seat's spawn state back
/// through the public `observe(seat).own`, exactly the surface the twin exposes.
fn spawn_case(label: &str, seed: u64, rules: Rules, roster: Vec<arena_proto::SeatInfo>) -> SpawnCase {
    let config = parity_config(roster.len() as u8);
    let m = Match::new(parity_match_id(), config, rules, roster.clone(), Vec::new(), seed);
    let spawns = roster
        .iter()
        .map(|s| {
            let own = m.observe(s.seat).own;
            SpawnVector {
                seat: own.seat,
                team: own.team,
                position: own.position,
                facing: own.facing,
                health: own.health,
                ammo: own.ammo,
            }
        })
        .collect();
    SpawnCase { label: label.to_string(), seed, config, rules, roster, spawns }
}

/// The verdict for one candidate, in the IDENTICAL filter order [`Match::observe`]
/// applies (range → cone → line of sight) — the first failing filter is the
/// reason, so the annotation is the real load-bearing convention, not a guess.
fn perception_verdict(
    observer: Vec2,
    facing: Bam,
    range: i32,
    spread: u8,
    blockers: &[Blocker],
    candidate: Vec2,
) -> PerceptionVerdict {
    if !within(observer, candidate, range) {
        return PerceptionVerdict::OutOfRange;
    }
    if !in_fov(facing, observer, candidate, spread) {
        return PerceptionVerdict::OutOfCone;
    }
    // The perception parity cases are planar (grounded observer + full-height walls),
    // so z is 0 on both ends; the height-bounded over-the-wall rule is pinned by the
    // dedicated vision-over-cover cases, not here.
    if !has_line_of_sight(blockers, observer, 0, candidate, 0) {
        return PerceptionVerdict::Occluded;
    }
    PerceptionVerdict::Visible
}

/// Build a perception case: place the observer (seat 0) and the candidate enemies
/// at explicit positions, then read the observer's real visible set. The
/// engineered geometry is realized by direct state construction — the reference's
/// in-crate privilege — but the case records the explicit positions, so the twin
/// reproduces the same scenario through its own core with no private access.
fn perception_case(
    label: &str,
    observer: Vec2,
    facing: Bam,
    range: i32,
    spread: u8,
    blockers: Vec<Blocker>,
    candidates: Vec<(SeatId, TeamId, Vec2)>,
) -> PerceptionCase {
    let mut roster = vec![parity_seat(0, 0)];
    for &(seat, team, _) in &candidates {
        roster.push(parity_seat(seat, team));
    }
    let rules = Rules { perception_range: range, fov_octant_spread: spread, spawn_jitter: 0, ..Default::default() };
    let config = parity_config(roster.len() as u8);
    let mut m = Match::new(parity_match_id(), config, rules, roster, blockers.clone(), 1);
    for p in &mut m.pawns {
        if p.seat == 0 {
            p.pos = observer;
            p.facing = facing;
        } else if let Some(&(_, _, pos)) = candidates.iter().find(|&&(s, _, _)| s == p.seat) {
            p.pos = pos;
        }
    }
    let visible: Vec<u32> = m.observe(0).visible.iter().map(|e| e.entity_id).collect();
    let candidates = candidates
        .iter()
        .map(|&(seat, team, position)| PerceptionCandidate {
            seat,
            team,
            position,
            alive: true,
            verdict: perception_verdict(observer, facing, range, spread, &blockers, position),
        })
        .collect();
    PerceptionCase {
        label: label.to_string(),
        observer_position: observer,
        observer_facing: facing,
        perception_range: range,
        fov_octant_spread: spread,
        blockers,
        candidates,
        visible,
    }
}

/// Build a hitscan case: place shooter (seat 0) and target (seat 1), fire once,
/// and record the damage dealt.
#[allow(clippy::too_many_arguments)]
fn hit_case(
    label: &str,
    shooter: Vec2,
    facing: Bam,
    target: Vec2,
    weapon_range: i32,
    hit_radius: i32,
    blockers: Vec<Blocker>,
    aim_mode: AimMode,
) -> HitCase {
    let rules = Rules { weapon_range, hit_radius, aim_mode, spawn_jitter: 0, ..Default::default() };
    let roster = vec![parity_seat(0, 0), parity_seat(1, 1)];
    let mut m = Match::new(parity_match_id(), parity_config(2), rules, roster, blockers.clone(), 1);
    m.pawns[0].pos = shooter;
    m.pawns[0].facing = facing;
    m.pawns[1].pos = target;
    let before = m.pawns[1].health;
    m.resolve_fire(0);
    HitCase {
        label: label.to_string(),
        shooter_position: shooter,
        shooter_facing: facing,
        target_position: target,
        weapon_range,
        hit_radius,
        blockers,
        aim_mode,
        damage: before - m.pawns[1].health,
    }
}

/// Build a projectile case: launch one shot and sweep it tick by tick until it
/// hits, expires, or reaches the lifetime backstop, recording when (if ever) the
/// swept path first reached the target.
#[allow(clippy::too_many_arguments)]
fn projectile_case(
    label: &str,
    shooter: Vec2,
    facing: Bam,
    target: Vec2,
    projectile_speed: i32,
    weapon_range: i32,
    hit_radius: i32,
    blockers: Vec<Blocker>,
) -> ProjectileCase {
    let rules = Rules {
        weapon_mode: WeaponMode::Projectile,
        projectile_speed,
        weapon_range,
        hit_radius,
        spawn_jitter: 0,
        ..Default::default()
    };
    let roster = vec![parity_seat(0, 0), parity_seat(1, 1)];
    let mut m = Match::new(parity_match_id(), parity_config(2), rules, roster, blockers.clone(), 1);
    m.pawns[0].pos = shooter;
    m.pawns[0].facing = facing;
    m.pawns[1].pos = target;
    let start_health = m.pawns[1].health;
    m.spawn_projectile(0);
    let mut ticks_to_hit = None;
    let mut age = 0u16;
    while !m.projectiles.is_empty() && age < MAX_PROJECTILE_LIFETIME {
        let before = m.pawns[1].health;
        m.advance_projectiles();
        age += 1;
        if m.pawns[1].health < before {
            ticks_to_hit = Some(age);
            break;
        }
    }
    ProjectileCase {
        label: label.to_string(),
        shooter_position: shooter,
        shooter_facing: facing,
        target_position: target,
        projectile_speed,
        weapon_range,
        hit_radius,
        blockers,
        ticks_to_hit,
        damage: start_health - m.pawns[1].health,
        target_downed: !m.pawns[1].alive,
    }
}

/// Build a movement case: place seat 0 at `start` in a 2-seat (stays-Live) match
/// under the given speed/bounds/blockers, `step` once with the move intent, and
/// record the resulting position. Pins the physical-cover movement convention.
fn move_case(
    label: &str,
    start: Vec2,
    move_dir: Vec2,
    max_speed: i32,
    bounds: Vec2,
    blockers: Vec<Blocker>,
) -> MoveCase {
    let rules = Rules { max_speed, spawn_jitter: 0, ..Default::default() };
    let config = MatchConfig { tick_hz: 30, max_ticks: 3600, bounds, seats: 2 };
    let roster = vec![parity_seat(0, 0), parity_seat(1, 1)];
    let mut m = Match::new(parity_match_id(), config, rules, roster, blockers.clone(), 1);
    m.pawns[0].pos = start;
    // Seat 1 idles far off so the match stays Live and seat 0 alone moves.
    m.pawns[1].pos = Vec2 { x: bounds.x, y: bounds.y };
    let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
    intents.insert(0, parity_intent(move_dir, EAST, false));
    m.step(&intents);
    let end = m.pawns[0].pos;
    MoveCase { label: label.to_string(), start, move_dir, max_speed, bounds, blockers, end, blocked: end == start }
}

/// Build a z-coupled-combat case: place shooter (seat 0) at the origin facing east
/// and target (seat 1) point-blank dead ahead — a planar setup that lands in EVERY
/// weapon mode under default tuning — set their elevations and the tolerance, fire
/// once in `mode`, and record the damage. `damage == 0` therefore isolates the
/// vertical rule (elevation cleared the shot), never the planar geometry.
fn vertical_hit_case(label: &str, mode: WeaponMode, shooter_z: i32, target_z: i32, tolerance: i32) -> VerticalHitCase {
    let rules = Rules { weapon_mode: mode, vertical_hit_tolerance: tolerance, spawn_jitter: 0, ..Default::default() };
    let shooter = Vec2::ZERO;
    // Point-blank (1.5 m) so the shot lands in melee range AND on the projectile's
    // first swept step AND down the hitscan beam — one geometry, all three modes.
    let target = Vec2 { x: 1500, y: 0 };
    let roster = vec![parity_seat(0, 0), parity_seat(1, 1)];
    let mut m = Match::new(parity_match_id(), parity_config(2), rules, roster, Vec::new(), 1);
    m.pawns[0].pos = shooter;
    m.pawns[0].facing = EAST;
    m.pawns[0].z = shooter_z;
    m.pawns[1].pos = target;
    m.pawns[1].z = target_z;
    let before = m.pawns[1].health;
    match mode {
        WeaponMode::Hitscan => m.resolve_fire(0),
        WeaponMode::Melee => m.resolve_melee(0),
        WeaponMode::Projectile => {
            m.spawn_projectile(0);
            let mut age = 0u16;
            while !m.projectiles.is_empty() && age < MAX_PROJECTILE_LIFETIME {
                m.advance_projectiles();
                age += 1;
            }
        }
    }
    VerticalHitCase {
        label: label.to_string(),
        weapon_mode: mode,
        shooter_position: shooter,
        shooter_z,
        target_position: target,
        target_z,
        vertical_hit_tolerance: tolerance,
        damage: before - m.pawns[1].health,
    }
}

/// Build a knockback case: fire one point-blank shot at a grounded target with
/// `gravity`/`knockback_velocity` set and record the target's post-hit upward z_vel (the
/// variable-fall source), the shooter's (always 0 — never recoils), and whether the
/// target survived. Mirrors `vertical_hit_case`'s one-geometry-all-modes point-blank
/// setup; both pawns sit at `z == 0`, so the planar shot lands in every mode and the only
/// z motion is the knockback itself.
fn knockback_case(label: &str, mode: WeaponMode, gravity: i32, knockback_velocity: i32) -> KnockbackCase {
    let rules = Rules { weapon_mode: mode, gravity, knockback_velocity, spawn_jitter: 0, ..Default::default() };
    let shooter = Vec2::ZERO;
    let target = Vec2 { x: 1500, y: 0 };
    let roster = vec![parity_seat(0, 0), parity_seat(1, 1)];
    let mut m = Match::new(parity_match_id(), parity_config(2), rules, roster, Vec::new(), 1);
    m.pawns[0].pos = shooter;
    m.pawns[0].facing = EAST;
    m.pawns[1].pos = target;
    let before = m.pawns[1].health;
    match mode {
        WeaponMode::Hitscan => m.resolve_fire(0),
        WeaponMode::Melee => m.resolve_melee(0),
        WeaponMode::Projectile => {
            m.spawn_projectile(0);
            let mut age = 0u16;
            while !m.projectiles.is_empty() && age < MAX_PROJECTILE_LIFETIME {
                m.advance_projectiles();
                age += 1;
            }
        }
    }
    KnockbackCase {
        label: label.to_string(),
        weapon_mode: mode,
        gravity,
        knockback_velocity,
        damage: before - m.pawns[1].health,
        target_z_vel: m.pawns[1].z_vel,
        shooter_z_vel: m.pawns[0].z_vel,
        target_alive: m.pawns[1].alive,
    }
}

/// Build a z-aware-occlusion case: test the one sightline against the one wall and
/// record whether it is occluded — the predicate the twin reproduces.
fn vision_over_cover_case(label: &str, from: Vec2, from_z: i32, to: Vec2, to_z: i32, blocker: Blocker) -> VisionOverCoverCase {
    VisionOverCoverCase {
        label: label.to_string(),
        from,
        from_z,
        to,
        to_z,
        blocker,
        occluded: !has_line_of_sight(&[blocker], from, from_z, to, to_z),
    }
}

/// Build a z-aware-traversal case: test the one level path against the one wall and
/// record whether it is blocked — the predicate movement and a level projectile share.
fn movement_over_cover_case(label: &str, from: Vec2, from_z: i32, to: Vec2, to_z: i32, blocker: Blocker) -> MovementOverCoverCase {
    MovementOverCoverCase {
        label: label.to_string(),
        from,
        from_z,
        to,
        to_z,
        blocker,
        blocked: path_hits_blocker(&[blocker], from, from_z, to, to_z),
    }
}

/// Build a ranked-rating case: record the expected score and the zero-sum delta the
/// curve produces for the given ratings, outcome, and K — the inputs and the exact
/// integer outputs a twin must reproduce.
fn rating_delta_case(label: &str, rating_a: i32, rating_b: i32, outcome: MatchOutcome, k: i32) -> RatingDeltaCase {
    RatingDeltaCase {
        label: label.to_string(),
        rating_a,
        rating_b,
        outcome,
        k,
        expected_a_bp: expected_score_bp(rating_a - rating_b),
        delta: rating_delta(rating_a, rating_b, outcome, k),
    }
}

/// Build a multi-seat ranked-rating case from a `(seat, placement, rating)` field:
/// synthesize the canonical placement result, run [`ranked_field_delta`], and record
/// the field and the exact zero-sum per-seat deltas a twin must reproduce. The
/// synthesized [`SeatOutcome`] carries only what the delta reads (seat + placement);
/// the per-seat `score`/`alive_at_end`/`team` do not enter the computation.
fn field_delta_case(label: &str, field: &[(SeatId, u16, i32)], k: i32) -> FieldDeltaCase {
    let outcomes = field
        .iter()
        .map(|&(seat, placement, _)| SeatOutcome {
            seat,
            team: seat as TeamId,
            placement,
            score: 0,
            alive_at_end: placement == 1,
        })
        .collect();
    let ratings: Vec<i32> = field.iter().map(|&(_, _, rating)| rating).collect();
    let result = MatchResult {
        protocol_version: PROTOCOL_VERSION,
        match_id: parity_match_id(),
        final_tick: 0,
        outcomes,
        replay_hash: "00".into(),
    };
    let deltas = ranked_field_delta(&result, &ratings, k).expect("a >=2-seat field aligned with its ratings");
    FieldDeltaCase {
        label: label.to_string(),
        seats: field.iter().map(|&(seat, placement, rating)| FieldSeat { seat, placement, rating }).collect(),
        k,
        deltas,
    }
}

/// Build a full-match case under a fixed, tiny scripted action stream: seat 0
/// steps east on the opening tick, fires due east on the next, then everyone
/// idles. A hitscan match ends on the kill; a projectile match runs on until the
/// shot arrives — so the weapon mode is an outcome determinant the record commits.
fn match_case(label: &str, rules: Rules) -> MatchCase {
    match_case_with_pickups(label, rules, Vec::new())
}

/// [`match_case`], plus a configured pickup set — seat 0 collects a pickup at its
/// spawn on the opening tick (the item layout binds the v3 digest and the collect
/// is reproduced by re-execution), so the case pins the pickup pipeline end to end.
fn match_case_with_pickups(label: &str, rules: Rules, pickups: Vec<PickupSpawn>) -> MatchCase {
    let roster = vec![parity_seat(0, 0), parity_seat(1, 1)];
    let mut m = Match::new_with_pickups(
        parity_match_id(),
        parity_config(2),
        rules,
        roster,
        Vec::new(),
        pickups,
        PARITY_MATCH_SEED,
    );
    let mut tick = 0u64;
    while m.phase() == MatchPhase::Live && tick < 64 {
        let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
        let action = match tick {
            0 => Some(parity_intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, EAST, false)),
            1 => Some(parity_intent(Vec2::ZERO, EAST, true)),
            _ => None,
        };
        if let Some(a) = action {
            intents.insert(0, a);
        }
        m.step(&intents);
        tick += 1;
    }
    let record = m.to_record().expect("the scripted parity match reaches a terminal state");
    MatchCase { label: label.to_string(), record }
}

/// Generate the canonical cross-implementation parity-vector set from this
/// reference core.
///
/// This is the headless half of the UE5-twin conformance contract. Every existing
/// parity audit checks the core against ITSELF (recomputing the same predicate);
/// this instead emits a small, versioned set of `inputs -> exact integer outputs`
/// vectors that a SECOND implementation — the UE5 dedicated server that also
/// implements the arena Gateway — must reproduce bit-for-bit. The cases are chosen
/// to be load-bearing, not happy-path: spawn determinism (PRNG + spread + jitter
/// order + facing rule), the perception range/cone/line-of-sight exclusion edges,
/// physical cover (movement refused into a wall, a fast step that must not tunnel a
/// thin wall, a wall-spawned pawn that can still leave, a hitscan beam blocked by a
/// wall, a projectile absorbed by a wall, and a target in front of a wall still
/// hit), the octant-vs-fine sub-octant hit boundary, a swept fast projectile that
/// must not tunnel, the z-coupled-combat rule across every weapon mode (a target
/// above [`Rules::vertical_hit_tolerance`] clears a hitscan/melee/projectile shot,
/// the boundary inclusive, the tolerance-off default ignoring elevation), the
/// z-aware-occlusion rule (a height-bounded [`Blocker`] is cleared by a high-enough
/// sightline, an infinitely-tall one never is), the z-aware-traversal rule (a level
/// path or projectile clears a low wall, the physical twin of the sight rule), the
/// zero-sum ranked-rating delta (the favourite gains less for a win, the underdog
/// more for the mirror upset, a draw moves the favourite down, every case zero-sum),
/// the multi-seat ranked-rating delta (an FFA / 3+ field settled as a sum of pairwise
/// games — the n=2 reduction agreeing with the 1v1, a placement tie scored a draw, the
/// ±cap honoured, every field zero-sum), and four full-match records proving the digest
/// commits the inputs, the rules, AND
/// the config determinants — the octant and fine cases run the identical action
/// stream yet hash differently because their aim_mode differs — while the rules also
/// bind the outcomes a re-run reproduces.
///
/// Set domain is `parity-vectors/v10`: the digest binds the combat `rules` and the
/// `config` determinants (arena bounds + tick cap); v4 added the z-coupled-combat rule
/// ([`Rules::vertical_hit_tolerance`] widened the rules encoding and the `vertical_hits`
/// cases pin it); v5 adds blocker `height` — the digest folds it at replay tag v6
/// (moving every committed match hash) and the `vision_over_cover` cases pin the
/// see-over-low-cover rule; v6 adds the `movement_over_cover` cases (the same height
/// bounds physical traversal); v7 adds the `rating_deltas` cases (the settlement-layer
/// zero-sum Elo reputation curve); v8 adds the `field_deltas` cases (the multi-seat
/// generalization of that curve) — v6, v7, and v8 are pure-logic, so none moves the
/// replay-digest tag; v9 adds the vertical knockback impulse ([`Rules::knockback_velocity`]
/// widened the rules encoding, moving every committed match hash again, and the
/// `knockback` cases pin that a damaging hit pops a surviving target upward); v10 adds
/// DIRECTIONAL knockback ([`Rules::knockback_horizontal`] widened the rules encoding once
/// more, moving every committed match hash, and the `knockback` cases gain the planar
/// shove that pushes a surviving target away from the shooter). A twin must
/// fold the wider encodings into its match digest and reproduce every rule or it diverges;
/// the v2 blockers-as-physical-cover convention
/// still holds. These are deliberate conventions every twin must follow.
///
/// The generator is pure integer, ordered, and float/map-free, so the set is
/// byte-stable on every platform and the same on every run. It realizes the
/// engineered hit/perception geometries by direct state construction (the
/// reference's in-crate privilege) but records the explicit inputs, so a twin with
/// only the public protocol reproduces every case through its own core.
///
/// SCOPE: a passing self-check proves this reference is self-consistent and
/// PINNED — that the conventions cannot silently drift — NOT that any second
/// implementation agrees. There is no UE5 consumer yet; building and conforming it
/// is operator-gated. Treat this as the committed spec the twin is held to, the
/// way the contracts ABI-drift gate pins a wire shape no off-chain caller has yet
/// consumed.
pub fn parity_vectors() -> ParityVectors {
    let default = Rules::default();
    let (range, radius) = (default.weapon_range, default.hit_radius);
    let jittered = Rules { spawn_radius: 20 * POSITION_SCALE, spawn_jitter: 2 * POSITION_SCALE, ..Default::default() };
    let wall = Blocker { min: Vec2 { x: 8_000, y: 800 }, max: Vec2 { x: 12_000, y: 2_200 }, height: 0 };
    // A wall astride the +X axis between origin and 10 m, the physical-cover
    // occluder the movement/hitscan/projectile cases fire against.
    let east_wall = Blocker { min: Vec2 { x: 4_000, y: -2_000 }, max: Vec2 { x: 5_000, y: 2_000 }, height: 0 };

    ParityVectors {
        domain: PARITY_VECTORS_DOMAIN.to_string(),
        protocol_version: PROTOCOL_VERSION,
        spawns: vec![
            spawn_case(
                "four_seats_jittered",
                0x1234_5678_9abc_def0,
                jittered,
                vec![parity_seat(0, 0), parity_seat(1, 1), parity_seat(2, 2), parity_seat(3, 3)],
            ),
            // A lone seat takes the n<=1 branch (base x = 0, no spread), then jitters.
            spawn_case("single_seat_no_spread", 7, jittered, vec![parity_seat(0, 0)]),
        ],
        perception: vec![perception_case(
            "range_cone_los_edges",
            Vec2::ZERO,
            EAST,
            30 * POSITION_SCALE,
            1,
            vec![wall],
            vec![
                (1, 1, Vec2 { x: 20_000, y: -3_000 }), // in range + cone + clear LOS -> visible
                (2, 2, Vec2 { x: -20_000, y: 0 }),     // in range, behind the facing -> out of cone
                (3, 3, Vec2 { x: 40_000, y: 0 }),      // dead ahead but past perception -> out of range
                (4, 4, Vec2 { x: 20_000, y: 3_000 }),  // in range + cone, behind the wall -> occluded
            ],
        )],
        moves: vec![
            // A 5 m/tick step due east straight into a wall is refused: the pawn holds.
            move_case("into_wall_blocked", Vec2::ZERO, Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, 5 * POSITION_SCALE, Vec2 { x: 50_000, y: 50_000 }, vec![east_wall]),
            // The same speed NORTH, with the wall off to the east, is unobstructed.
            move_case("alongside_wall_allowed", Vec2::ZERO, Vec2 { x: 0, y: MOVE_INTENT_SCALE }, 5 * POSITION_SCALE, Vec2 { x: 50_000, y: 50_000 }, vec![east_wall]),
            // A 20 m/tick step over a ZERO-width wall is still stopped — a point test
            // at the destination (past the wall) would tunnel; the swept test catches it.
            move_case("fast_step_no_tunnel", Vec2::ZERO, Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, 20 * POSITION_SCALE, Vec2 { x: 50_000, y: 50_000 }, vec![Blocker { min: Vec2 { x: 5_000, y: -2_000 }, max: Vec2 { x: 5_000, y: 2_000 }, height: 0 }]),
            // Spawned INSIDE a wall, a pawn can still step out (start-containment
            // exemption) — the seed-spawn-in-cover safety valve, no trap.
            move_case("spawn_in_wall_escapes", Vec2::ZERO, Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, 5 * POSITION_SCALE, Vec2 { x: 50_000, y: 50_000 }, vec![Blocker { min: Vec2 { x: -2_000, y: -2_000 }, max: Vec2 { x: 2_000, y: 2_000 }, height: 0 }]),
        ],
        hits: vec![
            hit_case("dead_on_octant", Vec2::ZERO, EAST, Vec2 { x: 10 * POSITION_SCALE, y: 0 }, range, radius, vec![], AimMode::Octant),
            hit_case("dead_on_fine", Vec2::ZERO, EAST, Vec2 { x: 10 * POSITION_SCALE, y: 0 }, range, radius, vec![], AimMode::Fine),
            // 11.25 degrees: a fine direction that snaps to the East octant. The
            // octant beam misses the off-axis target; the finer beam lands it.
            hit_case("sub_octant_octant_misses", Vec2::ZERO, 2048, Vec2 { x: 19_617, y: 3_902 }, range, radius, vec![], AimMode::Octant),
            hit_case("sub_octant_fine_hits", Vec2::ZERO, 2048, Vec2 { x: 19_617, y: 3_902 }, range, radius, vec![], AimMode::Fine),
            // Same dead-on shot, now with a wall on the sightline: physical cover, so
            // the beam is blocked and the target takes nothing (vs dead_on_octant).
            hit_case("blocked_by_wall", Vec2::ZERO, EAST, Vec2 { x: 10 * POSITION_SCALE, y: 0 }, range, radius, vec![east_wall], AimMode::Octant),
        ],
        projectiles: vec![
            // 20 m/tick overshoots a 5 m target in one step: the endpoints both miss,
            // the swept segment hits.
            projectile_case("fast_sweep_no_tunnel", Vec2::ZERO, EAST, Vec2 { x: 5 * POSITION_SCALE, y: 0 }, 20 * POSITION_SCALE, range, radius, vec![]),
            // Off the firing line: the sweep never reaches it, so the shot expires clean.
            projectile_case("off_line_clean_miss", Vec2::ZERO, EAST, Vec2 { x: 0, y: 5 * POSITION_SCALE }, 2 * POSITION_SCALE, range, radius, vec![]),
            // A wall on the flight path absorbs the shot before it reaches the target
            // behind it: never hits, no damage.
            projectile_case("blocked_by_wall", Vec2::ZERO, EAST, Vec2 { x: 10 * POSITION_SCALE, y: 0 }, 2 * POSITION_SCALE, range, radius, vec![east_wall]),
            // A target IN FRONT of a wall is still hit on the same swept step that also
            // reaches the wall — the shot resolves the body first, so cover behind the
            // target gives it nothing (a wall-first twin would wrongly absorb the shot).
            projectile_case("pawn_in_front_of_wall_is_hit", Vec2::ZERO, EAST, Vec2 { x: 3 * POSITION_SCALE, y: 0 }, 5 * POSITION_SCALE, range, radius, vec![Blocker { min: Vec2 { x: 3_200, y: -2_000 }, max: Vec2 { x: 4_000, y: 2_000 }, height: 0 }]),
        ],
        vertical_hits: vec![
            // Tolerance off: z is ignored, so a target 5 m up is hit exactly as on the
            // ground — the default-off (planar) behavior, for hitscan and the projectile.
            vertical_hit_case("hitscan_off_ignores_elevation", WeaponMode::Hitscan, 0, 5_000, 0),
            vertical_hit_case("projectile_off_ignores_elevation", WeaponMode::Projectile, 0, 5_000, 0),
            // Tolerance on: a target 2 m up under a 1 m tolerance clears the shot — for
            // hitscan, a melee swing, AND the level-flying projectile alike (one rule).
            vertical_hit_case("hitscan_above_tolerance_cleared", WeaponMode::Hitscan, 0, 2_000, 1_000),
            vertical_hit_case("melee_above_tolerance_cleared", WeaponMode::Melee, 0, 2_000, 1_000),
            vertical_hit_case("projectile_above_tolerance_cleared", WeaponMode::Projectile, 0, 2_000, 1_000),
            // The boundary is INCLUSIVE: a target exactly at the tolerance is still hit.
            vertical_hit_case("hitscan_at_tolerance_lands", WeaponMode::Hitscan, 0, 1_000, 1_000),
        ],
        vision_over_cover: {
            // A 2 m wall astride the +X axis at x in [4 m, 6 m]; the sightline runs
            // from the origin to 10 m east, crossing the wall footprint.
            let wall = Blocker { min: Vec2 { x: 4_000, y: -1_000 }, max: Vec2 { x: 6_000, y: 1_000 }, height: 2_000 };
            let target = Vec2 { x: 10 * POSITION_SCALE, y: 0 };
            vec![
                // Both ends on the ground: the wall blocks the look.
                vision_over_cover_case("ground_look_blocked", Vec2::ZERO, 0, target, 0, wall),
                // Both ends well above the top: the sightline passes over -> clear.
                vision_over_cover_case("high_look_clears_the_wall", Vec2::ZERO, 5_000, target, 5_000, wall),
                // The same high look against an infinitely-tall (height 0) twin -> blocked.
                vision_over_cover_case("infinite_wall_blocks_high_look", Vec2::ZERO, 5_000, target, 5_000, Blocker { height: 0, ..wall }),
                // A look rising from the ground but still below the top where it crosses
                // the wall (z ~720 at the near edge) enters the box -> blocked.
                vision_over_cover_case("rising_look_still_in_band_blocked", Vec2::ZERO, 0, target, 1_800, wall),
            ]
        },
        movement_over_cover: {
            // The same 2 m wall the sight set uses, now as a physical occluder: a level
            // path from the origin to 10 m east crosses its footprint.
            let wall = Blocker { min: Vec2 { x: 4_000, y: -1_000 }, max: Vec2 { x: 6_000, y: 1_000 }, height: 2_000 };
            let target = Vec2 { x: 10 * POSITION_SCALE, y: 0 };
            vec![
                // On the ground: the wall stops the path.
                movement_over_cover_case("ground_path_blocked", Vec2::ZERO, 0, target, 0, wall),
                // Above the top: the path travels over -> clear.
                movement_over_cover_case("high_path_clears_the_wall", Vec2::ZERO, 5_000, target, 5_000, wall),
                // The same high path against an infinitely-tall (height 0) twin -> blocked.
                movement_over_cover_case("infinite_wall_blocks_high_path", Vec2::ZERO, 5_000, target, 5_000, Blocker { height: 0, ..wall }),
                // Grazing the top exactly (z == height): boundary contact counts -> blocked.
                movement_over_cover_case("grazing_top_blocked", Vec2::ZERO, 2_000, target, 2_000, wall),
                // Directional exemption: a path STARTING inside the wall may leave it.
                movement_over_cover_case("start_inside_is_exempt", Vec2 { x: 5_000, y: 0 }, 0, target, 0, wall),
                // ...but a path ENDING inside the wall is still blocked (unlike sight).
                movement_over_cover_case("end_inside_is_blocked", Vec2::ZERO, 0, Vec2 { x: 5_000, y: 0 }, 0, wall),
            ]
        },
        rating_deltas: vec![
            // Even match: a win is half of K and a draw moves nobody — the curve's centre.
            rating_delta_case("even_win", 1500, 1500, MatchOutcome::WinA, 32),
            rating_delta_case("even_draw", 1500, 1500, MatchOutcome::Draw, 32),
            // Favoured A (+300): A's win earns little; the mirror upset (B wins the same
            // pairing) earns much more — the same rating gap, opposite reward.
            rating_delta_case("favoured_a_wins", 1700, 1400, MatchOutcome::WinA, 32),
            rating_delta_case("upset_b_wins", 1700, 1400, MatchOutcome::WinB, 32),
            // A draw when A is favoured moves A DOWN toward B.
            rating_delta_case("favoured_a_draws", 1700, 1400, MatchOutcome::Draw, 32),
            // A gap past the cap saturates the expected score, so a heavy favourite's
            // win rounds to nothing (anti-farming) — the clamp made load-bearing.
            rating_delta_case("beyond_cap_favoured_win", 3000, 1000, MatchOutcome::WinA, 32),
        ],
        field_deltas: vec![
            // A two-seat field routed through the multi-seat path: one pairwise game IS
            // the 1v1, so it must agree bit-for-bit with ranked_delta (the n=2 reduction).
            field_delta_case("two_seat_matches_ranked_delta", &[(0, 1, 1700), (1, 2, 1400)], 32),
            // A 3-way FFA with distinct rating gaps and a clean 1/2/3 finish: the basic
            // multi-seat fold and the seat->delta mapping over an asymmetric field.
            field_delta_case("three_way_skill_spread", &[(0, 1, 1800), (1, 2, 1500), (2, 3, 1400)], 32),
            // A tie for 2nd between a favourite (seat 1) and an underdog (seat 2): the
            // tied pair scores a Draw — the favourite moves DOWN toward the underdog, not
            // a mutual win — so the equal-placement branch is load-bearing.
            field_delta_case("four_way_with_tie", &[(0, 1, 1500), (1, 2, 1700), (2, 2, 1300), (3, 4, 1500)], 32),
            // All seats at DEFAULT_RATING with a strict 1..5 finish: placement ALONE drives
            // the deltas, which come out symmetric (+48, +24, 0, -24, -48) — a larger field
            // and an exact-value pin a placement-mapping bug would break.
            field_delta_case(
                "all_equal_field",
                &[(0, 1, 1500), (1, 2, 1500), (2, 3, 1500), (3, 4, 1500), (4, 5, 1500)],
                24,
            ),
            // An upset inside a field with gaps PAST the ±800 cap: the 3000-rated seat 0
            // places 2nd behind the 1500 seat 1, and its expected score saturates — so the
            // upset costs it ~a full K while both expected wins over the 200 seat 2 round to
            // nothing. Pins the cap in the multi-seat path.
            field_delta_case("saturated_gap_upset", &[(0, 2, 3000), (1, 1, 1500), (2, 3, 200)], 32),
        ],
        knockback: vec![
            // The impulse: a damaging hitscan hit pops the grounded survivor upward by
            // exactly knockback_velocity, and the shooter never recoils.
            knockback_case("hitscan_launches_grounded_target", WeaponMode::Hitscan, 60, 800),
            // The one shared damage sink: a melee swing launches identically — every
            // weapon mode funnels through it, so the rule is mode-agnostic.
            knockback_case("melee_shares_the_knockback_sink", WeaponMode::Melee, 60, 800),
            // Off by default: knockback_velocity 0 leaves the target grounded though the
            // hit still lands — the byte-identity (no-launch) case.
            knockback_case("knockback_off_no_launch", WeaponMode::Hitscan, 60, 0),
            // Gravity gates it: with vertical physics off the impulse is suppressed even
            // with a knockback velocity set, so a 2D match is unchanged.
            knockback_case("gravity_off_no_launch", WeaponMode::Hitscan, 0, 800),
        ],
        matches: vec![
            match_case("octant_hitscan", Rules { damage: 100, spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() }),
            match_case("fine_hitscan", Rules { damage: 100, aim_mode: AimMode::Fine, spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() }),
            match_case("projectile", Rules { damage: 100, weapon_mode: WeaponMode::Projectile, spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() }),
            // Same script as octant_hitscan, plus an ammo pickup at seat 0's spawn it
            // collects on the opening tick: the item layout binds the v3 digest (so this
            // differs from octant_hitscan) and the collect is reproduced bit-for-bit.
            match_case_with_pickups(
                "pickup_collected",
                Rules { damage: 100, spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, pickup_radius: 1500, ..Default::default() },
                vec![PickupSpawn { kind: PickupKind::Ammo, position: Vec2 { x: -2 * POSITION_SCALE, y: 0 }, amount: 5 }],
            ),
        ],
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

    #[test]
    fn expected_score_is_even_at_equal_ratings() {
        assert_eq!(expected_score_bp(0), RATING_SCALE / 2);
    }

    #[test]
    fn expected_score_is_symmetric_and_sums_to_scale() {
        // E(d) + E(-d) == RATING_SCALE for every difference, the property that makes
        // the rating delta exactly zero-sum (FM1). Holds by construction (the mirror),
        // including past the clamp where both sides saturate.
        for d in [-5000, -800, -437, -40, -1, 0, 1, 25, 200, 401, 800, 5000] {
            assert_eq!(
                expected_score_bp(d) + expected_score_bp(-d),
                RATING_SCALE,
                "E(d) + E(-d) must equal RATING_SCALE at d={d}"
            );
        }
    }

    #[test]
    fn expected_score_is_monotonic_in_the_rating_gap() {
        // Non-decreasing across the whole clamped range, and strictly higher across a
        // full anchor step — a bigger lead never lowers the favourite's expected score
        // (FM3: wrong monotonicity would make the ladder farmable).
        let mut prev = expected_score_bp(-RATING_DIFF_CAP);
        for d in (-RATING_DIFF_CAP + 1)..=RATING_DIFF_CAP {
            let e = expected_score_bp(d);
            assert!(e >= prev, "expected score dipped at d={d}: {e} < {prev}");
            prev = e;
        }
        assert!(expected_score_bp(40) > expected_score_bp(0));
        assert!(expected_score_bp(800) > expected_score_bp(760));
    }

    #[test]
    fn expected_score_clamps_beyond_the_cap() {
        let cap = expected_score_bp(RATING_DIFF_CAP);
        assert_eq!(expected_score_bp(RATING_DIFF_CAP + 1), cap);
        assert_eq!(expected_score_bp(1_000_000), cap);
        // The extreme inputs must not overflow on the mirror negate (FM4).
        assert_eq!(expected_score_bp(i32::MAX), cap);
        assert_eq!(expected_score_bp(i32::MIN), RATING_SCALE - cap);
    }

    #[test]
    fn expected_score_interpolates_between_anchors() {
        // Halfway through the first 40-point step is the mean of the two anchors —
        // linear integer interpolation, not a step function.
        let mid = expected_score_bp(20);
        assert_eq!(mid, 5000 + (5573 - 5000) / 2);
        assert!(expected_score_bp(0) < mid && mid < expected_score_bp(40));
    }

    #[test]
    fn rating_delta_is_zero_sum() {
        // delta_a + delta_b == 0 for every outcome across a spread of ratings + K — the
        // contract's reputation-conservation invariant (FM1).
        for (ra, rb) in [(1500, 1500), (1800, 1200), (1200, 1800), (1500, 1505), (0, 3000)] {
            for outcome in [MatchOutcome::WinA, MatchOutcome::Draw, MatchOutcome::WinB] {
                for k in [0, 1, 16, 24, 32, 64, 1000] {
                    let d = rating_delta(ra, rb, outcome, k);
                    assert_eq!(d.a + d.b, 0, "not zero-sum at {ra}/{rb} {outcome:?} k={k}");
                    assert_eq!(d.b, -d.a);
                }
            }
        }
    }

    #[test]
    fn favoured_winner_gains_less_than_an_upset() {
        // The favourite winning earns less than a coin-flip win, which earns less than
        // an upset — the monotone ordering that keeps the ladder honest (FM3).
        let k = 32;
        let favoured = rating_delta(1800, 1200, MatchOutcome::WinA, k).a;
        let even = rating_delta(1500, 1500, MatchOutcome::WinA, k).a;
        let upset = rating_delta(1200, 1800, MatchOutcome::WinA, k).a;
        assert!(favoured < even && even < upset, "{favoured} < {even} < {upset}");
        assert_eq!(even, k / 2, "an even win is half of K");
    }

    #[test]
    fn a_draw_moves_the_favourite_down() {
        let d = rating_delta(1800, 1200, MatchOutcome::Draw, 32);
        assert!(d.a < 0 && d.b > 0, "favoured A loses, underdog B gains on a draw: {d:?}");
        // An even draw moves nobody.
        assert_eq!(rating_delta(1500, 1500, MatchOutcome::Draw, 32), RatingDelta { a: 0, b: 0 });
    }

    #[test]
    fn winning_beats_drawing_beats_losing_for_one_pairing() {
        let (ra, rb, k) = (1500, 1500, 32);
        let win = rating_delta(ra, rb, MatchOutcome::WinA, k).a;
        let draw = rating_delta(ra, rb, MatchOutcome::Draw, k).a;
        let loss = rating_delta(ra, rb, MatchOutcome::WinB, k).a;
        assert!(win > draw && draw > loss, "{win} > {draw} > {loss}");
    }

    #[test]
    fn delta_is_bounded_by_k_and_a_nonpositive_k_is_inert() {
        // |delta| <= K for any gap incl. the saturating extremes (FM4: the i64 product
        // never overflows i32); a zero or negative K moves nothing.
        for (ra, rb) in [(1500, 1500), (0, 5000), (5000, 0), (i32::MIN, i32::MAX)] {
            for outcome in [MatchOutcome::WinA, MatchOutcome::Draw, MatchOutcome::WinB] {
                let d = rating_delta(ra, rb, outcome, 40);
                assert!(d.a.abs() <= 40, "|delta| exceeded K at {ra}/{rb}: {}", d.a);
            }
        }
        assert_eq!(rating_delta(1200, 1800, MatchOutcome::WinA, 0), RatingDelta { a: 0, b: 0 });
        assert_eq!(rating_delta(1200, 1800, MatchOutcome::WinA, -50), RatingDelta { a: 0, b: 0 });
    }

    #[test]
    fn ranked_delta_reads_the_outcome_from_settlement() {
        // A 1v1 result: seat 0 first vs seat 1 first vs a shared first place maps to
        // WinA/WinB/Draw via settlement(), and the bridge equals the primitive for it.
        let result = |p0: u16, p1: u16| MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id: MID.parse().unwrap(),
            final_tick: 10,
            outcomes: vec![
                SeatOutcome { seat: 0, team: 0, placement: p0, score: 0, alive_at_end: p0 == 1 },
                SeatOutcome { seat: 1, team: 1, placement: p1, score: 0, alive_at_end: p1 == 1 },
            ],
            replay_hash: "00".into(),
        };
        let (ra, rb, k) = (1600, 1400, 32);
        assert_eq!(
            ranked_delta(&result(1, 2), ra, rb, k),
            Some(rating_delta(ra, rb, MatchOutcome::WinA, k)),
            "seat 0 first -> WinA"
        );
        assert_eq!(
            ranked_delta(&result(2, 1), ra, rb, k),
            Some(rating_delta(ra, rb, MatchOutcome::WinB, k)),
            "seat 1 first -> WinB"
        );
        assert_eq!(
            ranked_delta(&result(1, 1), ra, rb, k),
            Some(rating_delta(ra, rb, MatchOutcome::Draw, k)),
            "a shared first placement -> Draw"
        );
    }

    #[test]
    fn ranked_delta_requires_a_head_to_head_result() {
        // Ranked settlement is 1v1: a non-two-seat result yields no A-vs-B delta.
        let solo = MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id: MID.parse().unwrap(),
            final_tick: 1,
            outcomes: vec![SeatOutcome { seat: 0, team: 0, placement: 1, score: 0, alive_at_end: true }],
            replay_hash: "00".into(),
        };
        assert_eq!(ranked_delta(&solo, 1500, 1500, 32), None);
    }

    // Build an N-seat ranked result from `(seat, placement)` pairs — the fixture for the
    // multi-seat field-delta tests. Only `placement` feeds the delta; score/alive/team do
    // not, so they are filled with stable placeholders.
    fn field_result(seats: &[(SeatId, u16)]) -> MatchResult {
        MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id: MID.parse().unwrap(),
            final_tick: 10,
            outcomes: seats
                .iter()
                .map(|&(seat, placement)| SeatOutcome { seat, team: seat as TeamId, placement, score: 0, alive_at_end: placement == 1 })
                .collect(),
            replay_hash: "00".into(),
        }
    }

    #[test]
    fn field_delta_is_zero_sum_over_fuzzed_fields() {
        // FM1: the per-seat deltas sum to EXACTLY 0 across fuzzed seat counts, placements,
        // ratings, and K — multi-seat reputation conservation (harder than the 1v1 single
        // negate: N settlements must cancel, not two). A deterministic integer LCG drives
        // the fuzz, so a failure reproduces.
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for _ in 0..2000 {
            let n = 2 + (next() % 7) as usize; // 2..=8 seats
            let seats: Vec<(SeatId, u16)> = (0..n).map(|i| (i as SeatId, (1 + next() % n as u32) as u16)).collect();
            let ratings: Vec<i32> = (0..n).map(|_| (next() % 4001) as i32 + 300).collect(); // 300..=4300
            let k = (next() % 65) as i32; // 0..=64
            let deltas = ranked_field_delta(&field_result(&seats), &ratings, k).expect("a >=2-seat aligned field");
            let sum: i64 = deltas.iter().map(|d| d.delta as i64).sum();
            assert_eq!(sum, 0, "field not zero-sum: seats={seats:?} ratings={ratings:?} k={k}");
        }
    }

    #[test]
    fn field_delta_two_seat_agrees_with_ranked_delta_bit_for_bit() {
        // FM3: a two-seat result routed through the multi-seat path equals ranked_delta
        // exactly — one pairwise game IS the 1v1 — including the seat->delta mapping
        // (seat 0 -> a, seat 1 -> b), across win/loss/draw, fuzzed ratings, and K.
        for (p0, p1) in [(1u16, 2u16), (2, 1), (1, 1)] {
            let result = field_result(&[(0, p0), (1, p1)]);
            for (ra, rb) in [(1500, 1500), (1700, 1400), (1400, 1700), (3000, 1000), (300, 4300)] {
                for k in [0, 1, 16, 32, 64, 1000] {
                    let field = ranked_field_delta(&result, &[ra, rb], k).unwrap();
                    let one = ranked_delta(&result, ra, rb, k).unwrap();
                    assert_eq!(field.len(), 2);
                    assert_eq!((field[0].seat, field[1].seat), (0, 1), "deltas keep canonical seat order");
                    assert_eq!(field[0].delta, one.a, "seat 0 != ranked_delta.a at {ra}/{rb} k={k} p={p0}/{p1}");
                    assert_eq!(field[1].delta, one.b, "seat 1 != ranked_delta.b at {ra}/{rb} k={k} p={p0}/{p1}");
                }
            }
        }
    }

    #[test]
    fn field_delta_maps_each_delta_to_its_canonical_seat() {
        // FM4: the deltas come back in the result's canonical ascending-seat order, each
        // carrying its OWN seat, so a caller credits the right agent. Non-contiguous seat
        // ids prove the mapping is read from the outcome, not the index.
        let result = field_result(&[(3, 1), (5, 2), (9, 3)]);
        let deltas = ranked_field_delta(&result, &[1500, 1500, 1500], 24).unwrap();
        assert_eq!(deltas.iter().map(|d| d.seat).collect::<Vec<_>>(), vec![3, 5, 9], "deltas keep canonical seat order");
        assert!(deltas[0].delta > 0 && deltas[2].delta < 0, "first place gains, last place loses");
        assert_eq!(deltas.iter().map(|d| d.delta as i64).sum::<i64>(), 0);

        // Move the winning placement to seat 5: the credit follows the placement, not the
        // slot — a swap would otherwise reward the wrong agent.
        let swapped = field_result(&[(3, 2), (5, 1), (9, 3)]);
        let d = ranked_field_delta(&swapped, &[1500, 1500, 1500], 24).unwrap();
        assert_eq!(d[1].seat, 5);
        assert!(d[1].delta > d[0].delta && d[1].delta > d[2].delta, "the new first-place seat is credited most");
    }

    #[test]
    fn field_delta_handles_ties_all_equal_and_saturation() {
        // FM3: degenerate fields settle cleanly — no panic, no divide-by-zero (the design
        // has no field-size divisor), every field still zero-sum.

        // An all-tie field: equal ratings + one shared placement -> every pairwise game a
        // draw -> nobody moves.
        let all_first = field_result(&[(0, 1), (1, 1), (2, 1)]);
        let d = ranked_field_delta(&all_first, &[1500, 1500, 1500], 32).unwrap();
        assert!(d.iter().all(|x| x.delta == 0), "an all-tie field moves nobody: {d:?}");

        // A tie for 2nd between unequal ratings: the tied pair scores a draw, so the
        // favoured of the two moves DOWN toward the underdog (not a mutual win).
        let tie = field_result(&[(0, 1), (1, 2), (2, 2)]);
        let d = ranked_field_delta(&tie, &[1500, 1800, 1200], 32).unwrap();
        assert_eq!(d.iter().map(|x| x.delta as i64).sum::<i64>(), 0);
        assert!(d[1].delta < d[2].delta, "the favoured tied seat ends below the underdog it only tied");

        // All-equal ratings, strict finish: a placement-symmetric spread (seat i and seat
        // n-1-i exact opposites) summing to 0.
        let strict = field_result(&[(0, 1), (1, 2), (2, 3), (3, 4)]);
        let vals: Vec<i32> = ranked_field_delta(&strict, &[1500; 4], 32).unwrap().iter().map(|x| x.delta).collect();
        let neg_rev: Vec<i32> = vals.iter().rev().map(|x| -x).collect();
        assert_eq!(vals, neg_rev, "equal ratings give a placement-symmetric spread: {vals:?}");

        // Saturation: every gap past the ±cap. The favourite winning as expected gains
        // nothing, and extreme i32 ratings never overflow the expected-score mirror.
        let capped = field_result(&[(0, 1), (1, 2), (2, 3)]);
        let d = ranked_field_delta(&capped, &[5000, 1500, 100], 32).unwrap();
        assert_eq!(d.iter().map(|x| x.delta as i64).sum::<i64>(), 0);
        assert_eq!(d[0].delta, 0, "a prohibitive favourite winning as expected gains nothing past the cap");
        let extreme = ranked_field_delta(&capped, &[i32::MAX, 0, i32::MIN], 32).unwrap();
        assert_eq!(extreme.iter().map(|x| x.delta as i64).sum::<i64>(), 0, "extreme ratings stay zero-sum");
    }

    #[test]
    fn field_delta_rejects_a_degenerate_or_misaligned_field() {
        // A field needs >= 2 seats and ratings aligned 1:1 with the outcomes; anything
        // else has no well-defined settlement, so it yields None rather than a wrong delta.
        assert_eq!(ranked_field_delta(&field_result(&[(0, 1)]), &[1500], 32), None, "a one-seat field has no pairwise game");
        assert_eq!(ranked_field_delta(&field_result(&[]), &[], 32), None, "an empty field settles nothing");
        let three = field_result(&[(0, 1), (1, 2), (2, 3)]);
        assert_eq!(ranked_field_delta(&three, &[1500, 1500], 32), None, "too few ratings is rejected");
        assert_eq!(ranked_field_delta(&three, &[1500, 1500, 1500, 1500], 32), None, "too many ratings is rejected");
    }

    #[test]
    fn blockers_accessor_returns_the_match_geometry_sent_to_agents() {
        // The accessor is the source the harness fills GatewayMsg::Start.blockers
        // from, so it must return EXACTLY the set the match runs under — the FM3
        // exact-geometry pin (an agent must never path against a phantom map).
        let wall = Blocker { min: Vec2 { x: 4_000, y: -2_000 }, max: Vec2 { x: 5_000, y: 2_000 }, height: 0 };
        let m = Match::new(MID.parse().unwrap(), config(2), Rules::default(), two_seats(), vec![wall], 1);
        assert_eq!(m.blockers(), &[wall]);

        // A no-occluder match surfaces an explicit empty set, never a phantom entry.
        assert!(new_match(1).blockers().is_empty());
    }

    #[test]
    fn pickup_spawns_accessor_returns_the_static_layout_sent_to_agents() {
        // The accessor is the source the harness projects GatewayMsg::Start.pickup_points
        // from, so it must return EXACTLY the static config the match runs under — the
        // FM3 exact-layout pin. The harness drops kind/amount (position-only); the
        // accessor itself keeps the full server-side config.
        let health = PickupSpawn { kind: PickupKind::Health, position: Vec2 { x: 1_000, y: 500 }, amount: 25 };
        let ammo = PickupSpawn { kind: PickupKind::Ammo, position: Vec2 { x: -1_000, y: -500 }, amount: 30 };
        let m = Match::new_with_pickups(
            MID.parse().unwrap(), config(2), Rules::default(), two_seats(), Vec::new(), vec![health, ammo], 1,
        );
        assert_eq!(m.pickup_spawns(), &[health, ammo]);
        // The position-only projection the harness sends — the points, kind/amount dropped.
        let points: Vec<Vec2> = m.pickup_spawns().iter().map(|p| p.position).collect();
        assert_eq!(points, vec![Vec2 { x: 1_000, y: 500 }, Vec2 { x: -1_000, y: -500 }]);

        // A no-pickup match surfaces an explicit empty layout.
        assert!(new_match(1).pickup_spawns().is_empty());
    }

    // Build a 2-seat match with perception memory set to `window`, pawn 0 at the
    // origin and pawn 1 a controllable enemy — the fixture for the memory tests.
    fn memory_match(window: u16) -> Match {
        let rules = Rules { perception_memory_ticks: window, ..Rules::default() };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].facing = EAST;
        m
    }
    const SEEN_AT: Vec2 = Vec2 { x: 5 * POSITION_SCALE, y: 0 }; // inside the 40 m range
    const OUT_OF_RANGE: Vec2 = Vec2 { x: 45 * POSITION_SCALE, y: 0 }; // beyond it

    #[test]
    fn perception_memory_surfaces_a_lost_enemys_last_known_position() {
        // FM1: once seat 0 has perceived seat 1, losing sight of it surfaces its LAST
        // PERCEIVED position with in_line_of_sight == false — never its live, unseen one.
        let mut m = memory_match(3);
        m.pawns[1].pos = SEEN_AT;
        m.step(&BTreeMap::new()); // tick 0: memory records seat 1 at SEEN_AT; tick -> 1
        m.pawns[1].pos = OUT_OF_RANGE; // seat 1 moves out of sight

        let visible = m.observe(0).visible;
        assert_eq!(visible.len(), 1, "the lost enemy is surfaced from memory");
        assert_eq!(visible[0].entity_id, 1);
        assert!(!visible[0].in_line_of_sight, "a remembered entity is flagged out of sight");
        assert_eq!(visible[0].position, SEEN_AT, "memory holds the last PERCEIVED position, not the live one");
    }

    #[test]
    fn perception_memory_never_remembers_a_never_perceived_enemy() {
        // FM1 leak guard: memory can ONLY hold an entity the seat actually perceived. An
        // enemy never in sight is never surfaced — the memory channel adds no omniscience.
        let mut m = memory_match(5);
        m.pawns[1].pos = OUT_OF_RANGE; // out of range the whole match
        for _ in 0..4 {
            assert!(m.observe(0).visible.is_empty(), "a never-seen enemy is never remembered");
            m.step(&BTreeMap::new());
        }
        assert!(m.seat_memory[0].is_empty(), "nothing about it was ever recorded");
    }

    #[test]
    fn perception_memory_decays_and_is_dropped_after_the_window() {
        // FM2: a remembered entry surfaces for exactly `window` ticks past the last
        // sighting, then is dropped from the store — bounded memory, not a permanent x-ray.
        let mut m = memory_match(2);
        m.pawns[1].pos = SEEN_AT;
        m.step(&BTreeMap::new()); // tick 0 -> 1: records seat 1 at last_seen 0
        m.pawns[1].pos = OUT_OF_RANGE; // lost

        let remembered = |m: &Match| m.observe(0).visible.iter().any(|e| e.entity_id == 1 && !e.in_line_of_sight);
        assert!(remembered(&m), "age 1: within the window");
        m.step(&BTreeMap::new()); // -> tick 2
        assert!(remembered(&m), "age 2 == window: still surfaced");
        m.step(&BTreeMap::new()); // -> tick 3
        assert!(!remembered(&m), "age 3 > window: decayed");
        m.step(&BTreeMap::new()); // -> tick 4: the stale entry is pruned
        assert!(m.seat_memory[0].is_empty(), "the decayed entry is dropped, so memory stays bounded");
    }

    #[test]
    fn perception_memory_off_is_identical_to_exclusion() {
        // FM2 default-off: window 0 (the default) records nothing and surfaces nothing —
        // a lost enemy vanishes at once, byte-identical to exclusion-only perception.
        let mut m = memory_match(0);
        m.pawns[1].pos = SEEN_AT;
        m.step(&BTreeMap::new());
        m.pawns[1].pos = OUT_OF_RANGE; // lost
        assert!(m.observe(0).visible.is_empty(), "off: a lost enemy is not remembered");
        assert!(m.seat_memory[0].is_empty(), "off: the refresh is skipped, no memory accrues");
    }

    #[test]
    fn perception_memory_skips_a_still_visible_entity_and_keeps_canonical_order() {
        // FM4: the merged visible set stays ascending by entity_id and never duplicates a
        // still-visible entity. Seat 0 sees seats 1 and 2, then loses 1 while 2 stays in
        // view: 2 is reported live, 1 from memory, once each, ordered.
        let rules = Rules { perception_memory_ticks: 3, ..Rules::default() };
        let roster = vec![
            SeatInfo { seat: 0, team: 0, controller: "0xa".into() },
            SeatInfo { seat: 1, team: 1, controller: "0xb".into() },
            SeatInfo { seat: 2, team: 2, controller: "0xc".into() },
        ];
        let mut m = Match::new(MID.parse().unwrap(), config(3), rules, roster, Vec::new(), 1);
        m.pawns[0].pos = Vec2::ZERO;
        let two_at = Vec2 { x: 0, y: 5 * POSITION_SCALE };
        m.pawns[1].pos = SEEN_AT;
        m.pawns[2].pos = two_at;
        m.step(&BTreeMap::new()); // records seats 1 and 2
        m.pawns[1].pos = OUT_OF_RANGE; // seat 1 lost; seat 2 stays in view

        let visible = m.observe(0).visible;
        let ids: Vec<u32> = visible.iter().map(|e| e.entity_id).collect();
        assert_eq!(ids, vec![1, 2], "ascending entity_id, each exactly once (no live/remembered dup)");
        let one = visible.iter().find(|e| e.entity_id == 1).unwrap();
        let two = visible.iter().find(|e| e.entity_id == 2).unwrap();
        assert!(!one.in_line_of_sight, "the lost enemy is remembered (out of sight)");
        assert!(two.in_line_of_sight, "the still-visible enemy is live, not a stale memory echo");
        assert_eq!(two.position, two_at, "the live entity carries its current position");
    }

    #[test]
    fn perception_memory_is_deterministic_across_identical_runs() {
        // FM3: memory is a pure function of the deterministic step loop, so two identical
        // runs surface byte-identical observations — including the remembered entries.
        let run = || {
            let mut m = memory_match(3);
            m.pawns[1].pos = SEEN_AT;
            m.step(&BTreeMap::new());
            m.pawns[1].pos = OUT_OF_RANGE;
            m.step(&BTreeMap::new());
            m.observe(0).visible
        };
        assert_eq!(run(), run(), "identical runs produce identical memory surfacing");
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
        /// Live projectiles a seat could NOT perceive (out of range/cone/LOS) and so
        /// correctly did not appear in its observation — summed over seats. A non-zero
        /// count proves the projectile filter is load-bearing, not vacuous.
        proj_excluded: usize,
    }

    fn assert_parity_bound(m: &Match) -> ParityCounts {
        let perception = m.rules.perception_range;
        let spread = m.rules.fov_octant_spread;
        let mut counts =
            ParityCounts { out_of_range: 0, out_of_cone: 0, out_of_los: 0, proj_excluded: 0 };
        for truth in &m.pawns {
            let seat = truth.seat;
            let obs = m.observe(seat);
            // `own` is the observer's OWN real state — never another seat's.
            assert_eq!(obs.own.seat, seat, "own is the observer's own seat");
            assert_eq!(obs.own.position, truth.pos, "own position is the observer's real one");
            assert_eq!(obs.own.health, truth.health, "own health is the observer's real one");
            assert_eq!(obs.own.shield, truth.shield, "own shield is the observer's real one");
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
                let in_los = has_line_of_sight(&m.blockers, truth.pos, truth.z, other.pos, other.z);
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
            // Projectiles obey the IDENTICAL bound — recomputed here from ground truth
            // (the raw projectile position), not read back from the observation, so the
            // audit is independent. A perceivable shot appears with exactly its real
            // position, the neutral team, and the Projectile kind; an out-of-bound shot
            // is absent and its position never leaks.
            for proj in &m.projectiles {
                let in_range = within(truth.pos, proj.pos, perception);
                let in_cone = in_fov(truth.facing, truth.pos, proj.pos, spread);
                let in_los = has_line_of_sight(&m.blockers, truth.pos, truth.z, proj.pos, proj.z);
                let perceivable = in_range && in_cone && in_los;
                let entry = obs.visible.iter().find(|e| e.entity_id == proj.id);
                assert_eq!(
                    entry.is_some(),
                    perceivable,
                    "seat {seat}: parity violated for projectile {} (perceivable={perceivable})",
                    proj.id
                );
                if let Some(e) = entry {
                    assert_eq!(e.position, proj.pos, "perceived projectile position must be the real one");
                    assert_eq!(e.team, 0, "a projectile must be reported neutral");
                    assert_eq!(e.kind, arena_proto::EntityKind::Projectile);
                    assert!(e.in_line_of_sight, "a perceived projectile is in sight");
                } else {
                    counts.proj_excluded += 1;
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
        Blocker { min: Vec2 { x: min.0, y: min.1 }, max: Vec2 { x: max.0, y: max.1 }, height: 0 }
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
        assert!(!occludes(&b, inside, 0, outside, 0), "the enclosing blocker does not blind its occupant");
        // Target on the box boundary: not occluded by the box it touches.
        assert!(!occludes(&b, outside, 0, on_edge, 0), "a target against a blocker is not hidden by it");
        // A DIFFERENT blocker still occludes the same pair.
        let between = box_of((10, -3), (11, 3));
        assert!(occludes(&between, outside, 0, inside, 0), "an unrelated blocker still occludes");
    }

    #[test]
    fn has_line_of_sight_is_clear_only_when_no_blocker_occludes() {
        // The set-level predicate: a sightline is clear iff NO blocker occludes it,
        // and an empty blocker set is always clear (the no-occlusion default). Ground
        // level (z 0) against full-height walls, so the planar behavior is unchanged.
        let from = Vec2::ZERO;
        let to = Vec2 { x: 10 * POSITION_SCALE, y: 0 };
        assert!(has_line_of_sight(&[], from, 0, to, 0), "no blockers means a clear sightline");
        let off_axis = box_of((5 * POSITION_SCALE, 5 * POSITION_SCALE), (6 * POSITION_SCALE, 6 * POSITION_SCALE));
        let on_axis = box_of((5 * POSITION_SCALE, -POSITION_SCALE), (6 * POSITION_SCALE, POSITION_SCALE));
        assert!(has_line_of_sight(&[off_axis], from, 0, to, 0), "an off-axis blocker leaves the sightline clear");
        assert!(!has_line_of_sight(&[off_axis, on_axis], from, 0, to, 0), "any blocker on the sightline occludes");
    }

    #[test]
    fn a_height_bounded_wall_is_cleared_by_a_high_enough_sightline() {
        // A wall on the sightline with a finite height occludes a ground-level look but
        // NOT one that passes over its top — the see-over-low-cover rule. An
        // infinitely-tall wall (height 0) occludes regardless of elevation.
        let from = Vec2::ZERO;
        let to = Vec2 { x: 10 * POSITION_SCALE, y: 0 };
        let low = Blocker { min: Vec2 { x: 4 * POSITION_SCALE, y: -POSITION_SCALE }, max: Vec2 { x: 6 * POSITION_SCALE, y: POSITION_SCALE }, height: 2000 };
        // Both ends on the ground: the 2 m wall blocks the look.
        assert!(!has_line_of_sight(&[low], from, 0, to, 0), "a ground-level look is blocked by the wall");
        // Both ends well above the wall top: the sightline passes over, so it is clear.
        assert!(has_line_of_sight(&[low], from, 5000, to, 5000), "a look from above the wall top clears it");
        // An infinitely-tall twin of the same wall still occludes the high look.
        let infinite = Blocker { height: 0, ..low };
        assert!(!has_line_of_sight(&[infinite], from, 5000, to, 5000), "an infinitely-tall wall occludes at any height");
        // A look rising from the ground that is still BELOW the top where it crosses
        // the wall (z 1600 at the near edge) enters the box and is blocked, even though
        // its far end (z 4000) is above the top.
        assert!(!has_line_of_sight(&[low], from, 0, to, 4000), "a sightline still below the top at the wall is blocked");
    }

    #[test]
    fn segment_box_3d_differential_against_the_2d_test_and_an_exact_oracle() {
        // The 3D SAT is cross-checked three sound ways over many deterministic
        // pseudo-random integer cases: (1) when both endpoints sit within the box's
        // z-band [0,height], z cannot separate, so the 3D test MUST equal the proven 2D
        // segment_intersects_aabb; (2) when both endpoints are strictly above the top
        // (or below the floor), the monotone z stays outside, so it MUST be clear;
        // (3) an EXACT on-segment point (rational t = k/N, checked by scaling by N so
        // there is no truncation) lying strictly inside the box forces a hit — no false
        // negative. Pure integer + fixed seed, so the sweep is byte-reproducible.
        let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let coord = |v: u64, lo: i32, hi: i32| lo + (v % ((hi - lo + 1) as u64)) as i32;
        for _ in 0..50_000 {
            let height = coord(next(), 1, 40); // > 0 so the 3D path runs (not the 2D delegate)
            let minx = coord(next(), -20, 20);
            let maxx = minx + coord(next(), 0, 20);
            let miny = coord(next(), -20, 20);
            let maxy = miny + coord(next(), 0, 20);
            let b = Blocker { min: Vec2 { x: minx, y: miny }, max: Vec2 { x: maxx, y: maxy }, height };
            let from = Vec2 { x: coord(next(), -30, 30), y: coord(next(), -30, 30) };
            let to = Vec2 { x: coord(next(), -30, 30), y: coord(next(), -30, 30) };
            let fz = coord(next(), -10, 60);
            let tz = coord(next(), -10, 60);
            let hit = segment_intersects_box_3d(from, fz, to, tz, &b);
            // (1) both endpoints in the z-band -> reduces to the proven 2D test.
            if (0..=height).contains(&fz) && (0..=height).contains(&tz) {
                assert_eq!(hit, segment_intersects_aabb(from, to, &b), "in-band must match the 2D test");
            }
            // (2) wholly above the top, or wholly below the floor -> always clear.
            if (fz > height && tz > height) || (fz < 0 && tz < 0) {
                assert!(!hit, "a sightline wholly outside the z-band must clear");
            }
            // (3) an exact on-segment lattice point strictly inside the box forces a hit.
            let n: i64 = 97; // prime, dense; scale by it to test exact rational points
            let (dx, dy, dz) = (to.x as i64 - from.x as i64, to.y as i64 - from.y as i64, tz as i64 - fz as i64);
            for k in 0..=n {
                let (pxn, pyn, pzn) = (from.x as i64 * n + dx * k, from.y as i64 * n + dy * k, fz as i64 * n + dz * k);
                if (minx as i64) * n < pxn && pxn < (maxx as i64) * n
                    && (miny as i64) * n < pyn && pyn < (maxy as i64) * n
                    && 0 < pzn && pzn < height as i64 * n
                {
                    assert!(hit, "an exact point strictly inside the box must register a hit");
                    break;
                }
            }
        }
    }

    #[test]
    fn a_low_wall_is_seen_and_shot_over_when_the_pawn_is_elevated() {
        // End to end: a wall of height 2000 directly between two grounded pawns blocks
        // both perception and fire; lifting the shooter high enough that the sightline
        // to the (grounded) enemy clears the top where it crosses the wall opens BOTH —
        // it now perceives the enemy and its hitscan beam lands (the default
        // vertical_hit_tolerance 0 leaves the shot itself z-uncoupled). Because the look
        // dips toward the grounded target, the shooter must rise well above the 2 m top,
        // not merely to it. The same wall with height 0 (infinitely tall) stays blocking.
        let rules = Rules { spawn_radius: 5 * POSITION_SCALE, spawn_jitter: 0, perception_range: 100 * POSITION_SCALE, ..Default::default() };
        // A wall spanning the midline between seat 0 (left) and seat 1 (right).
        let low = Blocker { min: Vec2 { x: -500, y: -10 * POSITION_SCALE }, max: Vec2 { x: 500, y: 10 * POSITION_SCALE }, height: 2000 };
        let setup = |wall: Blocker, shooter_z: i32| {
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), vec![wall], 1);
            m.pawns[0].pos = Vec2 { x: -5 * POSITION_SCALE, y: 0 };
            m.pawns[0].facing = EAST;
            m.pawns[0].z = shooter_z;
            m.pawns[1].pos = Vec2 { x: 5 * POSITION_SCALE, y: 0 };
            m
        };
        // Grounded: the wall occludes — seat 1 is not perceived and the shot is absorbed.
        let mut grounded = setup(low, 0);
        assert!(grounded.observe(0).visible.is_empty(), "the low wall hides the grounded enemy");
        grounded.resolve_fire(0);
        assert_eq!(grounded.pawns[1].health, rules.start_health, "the grounded shot is blocked by the wall");
        // Elevated above the 2 m top: seat 1 comes into view and the shot lands.
        let mut elevated = setup(low, 6000);
        assert_eq!(elevated.observe(0).visible.len(), 1, "from above the wall the enemy is perceived");
        elevated.resolve_fire(0);
        assert!(elevated.pawns[1].health < rules.start_health, "from above the wall the shot lands");
        // An infinitely-tall wall (height 0) keeps blocking even from the same height.
        let mut infinite = setup(Blocker { height: 0, ..low }, 6000);
        assert!(infinite.observe(0).visible.is_empty(), "an infinitely-tall wall blocks at any height");
        infinite.resolve_fire(0);
        assert_eq!(infinite.pawns[1].health, rules.start_health, "the infinite wall blocks the shot too");
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
            height: 0,
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
            height: 0,
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
            assert_eq!(seen, has_line_of_sight(&m.blockers, me, 0, foe, 0), "visibility tracks line of sight exactly");
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
            height: 0,
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

    /// A projectile at a chosen position with a NON-neutral internal team, so a test
    /// can confirm the wire entry is reported as the neutral team regardless.
    fn test_projectile(id: u32, pos: Vec2) -> Projectile {
        Projectile { id, shooter: 0, team: 7, origin: pos, pos, vel: Vec2::ZERO, facing: EAST, z: 0, age: 0 }
    }

    #[test]
    fn parity_bound_holds_for_projectiles_through_their_flight() {
        // Security (the arena invariant): a projectile obeys the SAME parity bound as
        // a pawn. Three seats spread wider than the perception range, seat 0 firing
        // slow shots across the line — so a shot in flight is in some seats' perception
        // and out of others'. The per-tick audit holds the projectile to the bound for
        // every seat every tick, and the projectile filter must actually exclude
        // (`proj_excluded > 0`) or the check is vacuous.
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "a".into() },
            SeatInfo { seat: 1, team: 1, controller: "b".into() },
            SeatInfo { seat: 2, team: 2, controller: "c".into() },
        ];
        let rules = Rules {
            weapon_mode: WeaponMode::Projectile,
            spawn_radius: 10 * POSITION_SCALE,
            spawn_jitter: 0,
            perception_range: 6 * POSITION_SCALE, // tighter than the seat spacing
            projectile_speed: POSITION_SCALE,     // 1 m/tick — lingers in flight
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(3), rules, seats, Vec::new(), 1);
        let aim = m.observe(0).own.facing;
        let mut proj_excluded = 0;
        let mut saw_proj = false;
        let mut ticks = 0u64;
        while m.phase() == MatchPhase::Live && ticks < 120 {
            step_with(&mut m, &[(0, intent(Vec2::ZERO, aim, true))]);
            let c = assert_parity_bound(&m);
            proj_excluded += c.proj_excluded;
            saw_proj |= !m.projectiles.is_empty();
            ticks += 1;
        }
        assert!(saw_proj, "projectiles were in flight");
        assert!(proj_excluded > 0, "no projectile was ever out of a seat's perception — the audit was vacuous");
    }

    #[test]
    fn a_projectile_beyond_perception_is_absent_and_its_position_never_leaks() {
        // A shot beyond a seat's perception never appears in its observation and its
        // position never leaks; the SAME shot one unit inside is perceived — as a
        // neutral Projectile at its real position, even though its internal team is
        // non-neutral (the shooter affiliation is scrubbed on the wire).
        let r = 10 * POSITION_SCALE;
        let rules = Rules {
            weapon_mode: WeaponMode::Projectile,
            perception_range: r,
            spawn_jitter: 0,
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        for p in &mut m.pawns {
            match p.seat {
                0 => {
                    p.pos = Vec2::ZERO;
                    p.facing = EAST;
                }
                1 => p.pos = Vec2 { x: 100 * POSITION_SCALE, y: 0 }, // far away, not under test
                _ => {}
            }
        }
        let far = Vec2 { x: r + 1, y: 0 };
        m.projectiles.push(test_projectile(PROJECTILE_ID_BASE, far));
        let obs = m.observe(0);
        assert!(
            obs.visible.iter().all(|e| e.kind != arena_proto::EntityKind::Projectile),
            "a shot beyond perception is absent"
        );
        assert!(obs.visible.iter().all(|e| e.position != far), "an unperceived shot's position must not leak");
        assert_parity_bound(&m);

        let near = Vec2 { x: r - 1, y: 0 };
        m.projectiles[0].pos = near;
        let obs = m.observe(0);
        let p = obs
            .visible
            .iter()
            .find(|e| e.kind == arena_proto::EntityKind::Projectile)
            .expect("the in-range shot is perceived");
        assert_eq!(p.position, near, "perceived at its real position");
        assert_eq!(p.team, 0, "reported neutral, not the internal shooter team (7)");
        assert_parity_bound(&m);
    }

    #[test]
    fn segment_hits_disc_geometry_is_exact() {
        // The swept primitive's exact integer cases: endpoint-inside, perpendicular
        // interior (the tunneling-catcher), the inclusive boundary, and the misses.
        let v = |x, y| Vec2 { x, y };
        let r = 1000;
        // Zero-length sweep with the point at the centre — hit.
        assert!(segment_hits_disc(v(0, 0), v(0, 0), v(0, 0), r));
        // The nearer endpoint is inside the disc — hit.
        assert!(segment_hits_disc(v(0, 0), v(5000, 0), v(500, 0), r));
        // Perpendicular interior: the segment sweeps broadside within r though BOTH
        // endpoints are far past the target on the axis (perp distance 800 < 1000).
        assert!(segment_hits_disc(v(-5000, 800), v(5000, 800), v(0, 0), r));
        // Boundary is inclusive (conservative): a parallel sweep exactly r away hits.
        assert!(segment_hits_disc(v(-5000, 1000), v(5000, 1000), v(0, 0), r));
        // One unit further is a clean miss.
        assert!(!segment_hits_disc(v(-5000, 1001), v(5000, 1001), v(0, 0), r));
        // Both endpoints past the target on the same side — closest point is an
        // endpoint, and both are out of range, so it misses.
        assert!(!segment_hits_disc(v(2000, 0), v(5000, 0), v(0, 0), r));
    }

    #[test]
    #[ignore = "exhaustive ~12M-case differential; run with `cargo test --release -- --ignored`"]
    fn segment_hits_disc_matches_an_independent_oracle_exhaustively() {
        // Adversarial cross-review of the swept-collision primitive (FM2): a bug here
        // silently DROPS hits (tunneling) or invents them. Brute-force segment_hits_disc
        // over a dense integer grid against a STRUCTURALLY INDEPENDENT oracle:
        //   - it checks BOTH endpoints unconditionally (the impl picks ONE region by
        //     where the foot lands, so a wrong branch/sign/axis diverges here), and
        //   - its interior perpendicular test uses the cross-product form
        //     (AP × AB)² ≤ r²·|AB|² — exact by Lagrange's identity but computed
        //     differently from the impl's |AP|²·|AB|² − (AP·AB)², so a bug in that
        //     formula is caught too.
        // All i128, no float, exact.
        fn oracle(a: Vec2, b: Vec2, c: Vec2, r: i32) -> bool {
            let r2 = (r as i128) * (r as i128);
            let ax = a.x as i128;
            let ay = a.y as i128;
            let bx = b.x as i128;
            let by = b.y as i128;
            let cx = c.x as i128;
            let cy = c.y as i128;
            let ac2 = (cx - ax) * (cx - ax) + (cy - ay) * (cy - ay);
            let bc2 = (cx - bx) * (cx - bx) + (cy - by) * (cy - by);
            if ac2 <= r2 || bc2 <= r2 {
                return true; // an endpoint lies in the disc — a hit regardless of the foot
            }
            let dx = bx - ax;
            let dy = by - ay;
            let seg2 = dx * dx + dy * dy;
            if seg2 == 0 {
                return false; // zero-length sweep; the A==B endpoint was already tested
            }
            let apx = cx - ax;
            let apy = cy - ay;
            let proj = apx * dx + apy * dy;
            if proj <= 0 || proj >= seg2 {
                return false; // closest point is an endpoint, both already out of range
            }
            let cross = apx * dy - apy * dx; // AP × AB
            cross * cross <= r2 * seg2
        }

        let (lo, hi) = (-5i32, 5i32);
        let mut cases = 0u64;
        for ax in lo..=hi {
            for ay in lo..=hi {
                for bx in lo..=hi {
                    for by in lo..=hi {
                        let a = Vec2 { x: ax, y: ay };
                        let b = Vec2 { x: bx, y: by };
                        for cx in lo..=hi {
                            for cy in lo..=hi {
                                let c = Vec2 { x: cx, y: cy };
                                for r in 0..=6 {
                                    cases += 1;
                                    assert_eq!(
                                        segment_hits_disc(a, b, c, r),
                                        oracle(a, b, c, r),
                                        "swept-collision disagreement at a={a:?} b={b:?} c={c:?} r={r}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(cases > 12_000_000, "the differential must cover the full grid (was {cases})");
    }

    #[test]
    fn a_fast_projectile_sweeps_through_a_target_without_tunneling() {
        // FM2: a shot fast enough to overshoot the target in a single tick still hits,
        // because the collision is the SWEPT segment, not a per-tick point. Both the
        // pre-move and post-move points miss the target (a naive point check tunnels),
        // yet the segment between them passes through it.
        let target = Vec2 { x: 5000, y: 0 };
        let radius = Rules::default().hit_radius; // 1500
        let before = Vec2 { x: 0, y: 0 };
        let after = Vec2 { x: 20_000, y: 0 }; // one 20 m/tick step overshoots a 5 m target
        assert!(!within(before, target, radius), "the launch point alone misses");
        assert!(!within(after, target, radius), "the post-move point alone misses");
        assert!(segment_hits_disc(before, after, target, radius), "the swept segment hits");

        // The same in the live sim: the shot spawns, advances 20 m across the 5 m
        // target in one tick, and damages it — no tunneling on the real path.
        let rules = Rules {
            weapon_mode: WeaponMode::Projectile,
            projectile_speed: 20 * POSITION_SCALE,
            spawn_jitter: 0,
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        for p in &mut m.pawns {
            match p.seat {
                0 => {
                    p.pos = before;
                    p.facing = EAST;
                }
                1 => p.pos = target,
                _ => {}
            }
        }
        let before_hp = m.pawns.iter().find(|p| p.seat == 1).unwrap().health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        let after_hp = m.pawns.iter().find(|p| p.seat == 1).unwrap().health;
        assert!(after_hp < before_hp, "the fast shot hit the target it swept through");
    }

    #[test]
    fn a_clean_shot_expires_past_weapon_range() {
        // FM4: a shot that hits nothing flies until it has travelled past weapon_range,
        // then leaves the live set — it does not accumulate forever.
        let rules = Rules { weapon_mode: WeaponMode::Projectile, spawn_jitter: 0, ..Default::default() };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        for p in &mut m.pawns {
            match p.seat {
                0 => {
                    p.pos = Vec2::ZERO;
                    p.facing = EAST;
                }
                1 => p.pos = Vec2 { x: 0, y: 100 * POSITION_SCALE }, // far off the shot's line
                _ => {}
            }
        }
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.projectiles.len(), 1, "the shot is in flight");
        let mut ticks = 0;
        while !m.projectiles.is_empty() && ticks < 200 {
            step_with(&mut m, &[]); // everyone idle; the shot flies on
            ticks += 1;
        }
        assert!(m.projectiles.is_empty(), "the shot expired");
        // 30 m range at 2 m/tick clears in ~15 ticks — range expiry, far short of the
        // lifetime backstop.
        assert!(ticks < 30, "expired promptly by range, not the lifetime backstop");
    }

    #[test]
    fn a_motionless_shot_expires_by_the_lifetime_backstop() {
        // FM4: a sub-octant-scale speed rounds the octant velocity to zero, so the shot
        // never moves and never exceeds range. The lifetime backstop still terminates
        // it, so the live set can never retain a motionless shot forever.
        let rules =
            Rules { weapon_mode: WeaponMode::Projectile, projectile_speed: 0, spawn_jitter: 0, ..Default::default() };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        for p in &mut m.pawns {
            if p.seat == 1 {
                p.pos = Vec2 { x: 0, y: 100 * POSITION_SCALE }; // nothing to hit
            }
        }
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.projectiles.len(), 1);
        let start = m.projectiles[0].pos;
        // age reaches 1 on the spawn tick; it expires when age >= MAX_PROJECTILE_LIFETIME.
        for _ in 0..(MAX_PROJECTILE_LIFETIME as usize - 2) {
            step_with(&mut m, &[]);
        }
        assert_eq!(m.projectiles.len(), 1, "still alive just before the backstop");
        assert_eq!(m.projectiles[0].pos, start, "a zero-speed shot never moves");
        step_with(&mut m, &[]); // age hits the backstop
        assert!(m.projectiles.is_empty(), "the lifetime backstop expired the motionless shot");
    }

    #[test]
    fn swept_collision_does_not_panic_at_extreme_coordinates() {
        // The i128 swept math must not overflow at any in-bounds coordinate, given the
        // spawn-time speed clamp that bounds the segment length. Drive the geometry
        // with i32 extremes and a max-length (clamped) sweep; completing without a
        // panic is the assertion.
        let seg = MAX_PROJECTILE_SPEED;
        let _ = segment_hits_disc(
            Vec2 { x: i32::MAX, y: i32::MAX },
            Vec2 { x: i32::MAX - seg, y: i32::MAX },
            Vec2 { x: i32::MIN, y: i32::MIN },
            i32::MAX,
        );
        let _ = segment_hits_disc(
            Vec2 { x: i32::MIN, y: i32::MIN },
            Vec2 { x: i32::MIN + seg, y: i32::MIN + seg },
            Vec2 { x: i32::MAX, y: i32::MAX },
            i32::MAX,
        );
        // The live spawn clamps an over-max speed to a finite, bounded velocity instead
        // of overflowing the octant scaling.
        let rules = Rules {
            weapon_mode: WeaponMode::Projectile,
            projectile_speed: i32::MAX,
            weapon_range: i32::MAX,
            spawn_jitter: 0,
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        for p in &mut m.pawns {
            if p.seat == 1 {
                p.pos = Vec2 { x: 0, y: 40 * POSITION_SCALE }; // off the shot's line
            }
        }
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.projectiles[0].vel.x, MAX_PROJECTILE_SPEED, "an over-max speed is clamped at spawn");
    }

    #[test]
    fn the_live_projectile_cap_bounds_a_fire_spammer() {
        // FM4 (DoS): a fire-every-tick agent cannot grow the live set — hence per-tick
        // O(live · seats) work — without bound. With fire_cooldown 0 and a deep
        // magazine both seats spawn a shot every tick, and the (motionless, so
        // persisting) live set climbs to MAX_LIVE_PROJECTILES and never past it: at the
        // cap a fire spends ammo but spawns nothing.
        let rules = Rules {
            weapon_mode: WeaponMode::Projectile,
            fire_cooldown: 0,
            mag_size: u16::MAX,
            projectile_speed: 0, // shots sit and persist, so the set actually fills
            spawn_radius: 2 * POSITION_SCALE,
            spawn_jitter: 0,
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        let aim0 = m.observe(0).own.facing;
        let aim1 = m.observe(1).own.facing;
        let mut reached_cap = false;
        let mut held = 0;
        let mut ticks = 0u64;
        while m.phase() == MatchPhase::Live && ticks < 2 * MAX_LIVE_PROJECTILES as u64 {
            step_with(
                &mut m,
                &[(0, intent(Vec2::ZERO, aim0, true)), (1, intent(Vec2::ZERO, aim1, true))],
            );
            assert!(m.projectiles.len() <= MAX_LIVE_PROJECTILES, "the live set exceeded the cap");
            if m.projectiles.len() == MAX_LIVE_PROJECTILES {
                reached_cap = true;
                held += 1;
                if held > 20 {
                    break; // the cap has held for a stretch — enough
                }
            }
            ticks += 1;
        }
        assert!(reached_cap, "the cap was never reached — the bound check was vacuous");
        assert_eq!(m.phase(), MatchPhase::Live, "the cap bounds spawns, it does not end the match");
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
            height: 0,
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
    fn path_hits_blocker_is_exact_at_the_boundary_and_exempts_only_the_start() {
        // FM1 (integer determinism): the shared movement/projectile collision
        // predicate must be exact at the boundary (a graze counts) so two
        // implementations agree on what a path crosses, and it exempts ONLY the start
        // corner (a body leaving a wall it is in) — never the destination, so a step
        // that would END inside a wall is still refused.
        let wall = Blocker { min: Vec2 { x: 5000, y: -2000 }, max: Vec2 { x: 6000, y: 2000 }, height: 0 };
        let o = Vec2::ZERO;
        assert!(path_hits_blocker(&[wall], o, 0, Vec2 { x: 10_000, y: 0 }, 0), "dead-centre through the wall is blocked");
        assert!(!path_hits_blocker(&[wall], o, 0, Vec2 { x: 3_000, y: 0 }, 0), "a path short of the wall is clear");
        assert!(!path_hits_blocker(&[wall], o, 0, Vec2 { x: 0, y: 10_000 }, 0), "a parallel path that never reaches it is clear");
        // Runs along the near (x = 5000) edge — boundary contact counts (conservative).
        assert!(path_hits_blocker(&[wall], Vec2 { x: 5000, y: -3000 }, 0, Vec2 { x: 5000, y: 3000 }, 0), "a path along the wall edge is blocked");
        // Ends ON a corner — the destination is NOT exempt, so it is refused.
        assert!(path_hits_blocker(&[wall], Vec2 { x: 0, y: 4000 }, 0, Vec2 { x: 5000, y: 2000 }, 0), "a step ending on the wall is refused");
        // Starts INSIDE the wall — exempt, so it can leave even toward a point outside.
        let inside = Vec2 { x: 5500, y: 0 };
        assert!(blocker_contains(&wall, inside), "the start point is genuinely inside the wall");
        assert!(!path_hits_blocker(&[wall], inside, 0, Vec2 { x: 10_000, y: 0 }, 0), "a path starting inside the wall is exempt");
        // ...but a DIFFERENT wall ahead still stops a path that left the first.
        let ahead = Blocker { min: Vec2 { x: 8000, y: -2000 }, max: Vec2 { x: 9000, y: 2000 }, height: 0 };
        assert!(path_hits_blocker(&[wall, ahead], inside, 0, Vec2 { x: 10_000, y: 0 }, 0), "a wall ahead still stops a path that left another");
    }

    #[test]
    fn path_hits_blocker_is_z_aware_and_clears_a_low_wall_over_the_top() {
        // FM2 (z-sampling) + FM1 (byte-identity) + FM3 (sight/traversal agree): the
        // shared collision predicate clears a height-bounded wall when the level path
        // runs above its top, blocks it on the ground or through the body, treats an
        // infinitely-tall (height 0) wall as full-height at every elevation, and agrees
        // with the sight rule on what a given elevation clears.
        let from = Vec2::ZERO;
        let to = Vec2 { x: 10 * POSITION_SCALE, y: 0 };
        let low = Blocker { min: Vec2 { x: 4 * POSITION_SCALE, y: -POSITION_SCALE }, max: Vec2 { x: 6 * POSITION_SCALE, y: POSITION_SCALE }, height: 2000 };
        assert!(path_hits_blocker(&[low], from, 0, to, 0), "a grounded path into the wall is blocked");
        assert!(path_hits_blocker(&[low], from, 1000, to, 1000), "a path through the wall body (below the top) is blocked");
        assert!(path_hits_blocker(&[low], from, 2000, to, 2000), "a path grazing the top is blocked (conservative boundary, matches the SAT)");
        assert!(!path_hits_blocker(&[low], from, 2001, to, 2001), "a path just above the top clears");
        assert!(!path_hits_blocker(&[low], from, 5000, to, 5000), "a path well above the top clears");
        // An infinitely-tall twin blocks at any elevation (byte-identical to the old rule).
        let infinite = Blocker { height: 0, ..low };
        assert!(path_hits_blocker(&[infinite], from, 5000, to, 5000), "an infinitely-tall wall blocks at any height");
        // FM1: on the ground a height-bounded and a full-height wall collide IDENTICALLY.
        assert_eq!(
            path_hits_blocker(&[low], from, 0, to, 0),
            path_hits_blocker(&[infinite], from, 0, to, 0),
            "grounded: a height-bounded wall collides exactly like a full-height one",
        );
        // FM3: with both endpoints OUTSIDE the wall (so the exemptions are inert), what
        // bounds SIGHT bounds TRAVERSAL — see-over iff move-over, no confusing split.
        for z in [0, 1000, 2000, 2001, 5000] {
            assert_eq!(
                !path_hits_blocker(&[low], from, z, to, z),
                has_line_of_sight(&[low], from, z, to, z),
                "movement and sight agree on clearing the wall at z={z}",
            );
        }
    }

    #[test]
    fn an_airborne_pawn_walks_over_low_cover_and_a_grounded_one_is_stopped() {
        // End to end through step: a low wall in seat 0's path stops a grounded step
        // (held, velocity zero) but not one taken above the wall top — the movement twin
        // of a_low_wall_is_seen_and_shot_over. z is set directly to isolate the collision
        // rule (the jump arc that produces a non-zero z is covered by the gravity tests);
        // gravity stays 0 so the manual z persists through the tick.
        let low = Blocker { min: Vec2 { x: 50, y: -2 * POSITION_SCALE }, max: Vec2 { x: 150, y: 2 * POSITION_SCALE }, height: 800 };
        let east = intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, EAST, false);
        let place = |z: i32| {
            let rules = Rules { spawn_jitter: 0, ..Default::default() };
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), vec![low], 1);
            m.pawns[0].pos = Vec2::ZERO;
            m.pawns[0].z = z;
            m.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 }; // far idle, keeps the match Live
            m
        };
        let mut grounded = place(0);
        step_with(&mut grounded, &[(0, east)]);
        assert_eq!(grounded.pawns[0].pos, Vec2::ZERO, "a grounded pawn is stopped by the low wall");
        assert_eq!(grounded.observe(0).own.velocity, Vec2::ZERO, "the refused move reports zero velocity");

        let mut airborne = place(900); // above the 800 wall top
        step_with(&mut airborne, &[(0, east)]);
        assert_eq!(airborne.pawns[0].pos.x, Rules::default().max_speed, "an airborne pawn walks over the low wall");
        assert_eq!(airborne.pawns[0].pos.y, 0, "the over-the-wall step holds its lateral line");
    }

    #[test]
    fn a_level_shot_flies_over_low_cover_when_launched_above_it() {
        // The projectile twin: a low wall between shooter and target absorbs a grounded
        // shot (target spared, shot despawned) but not one launched above the wall top —
        // fired from and at the wall-clearing elevation the shot flies over and lands.
        // Mirrors a_blocker_between_two_pawns_stops_the_shot with elevation added.
        let low = Blocker { min: Vec2 { x: -500, y: -2 * POSITION_SCALE }, max: Vec2 { x: 500, y: 2 * POSITION_SCALE }, height: 800 };
        let rules = Rules { weapon_mode: WeaponMode::Projectile, projectile_speed: 20 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        let place = |z: i32| {
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), vec![low], 1);
            m.pawns[0].pos = Vec2 { x: -5 * POSITION_SCALE, y: 0 };
            m.pawns[0].facing = EAST;
            m.pawns[0].z = z;
            m.pawns[1].pos = Vec2 { x: 5 * POSITION_SCALE, y: 0 };
            m.pawns[1].z = z; // target at the same elevation so the level shot can strike
            m
        };
        // Grounded: the shot is absorbed by the wall; the target behind it is spared.
        let mut grounded = place(0);
        let hp = grounded.pawns[1].health;
        grounded.spawn_projectile(0);
        grounded.advance_projectiles();
        assert_eq!(grounded.pawns[1].health, hp, "a grounded shot is absorbed by the low wall");
        assert!(grounded.projectiles.is_empty(), "the absorbed shot is despawned");
        // Launched above the top (both ends elevated): flies over the wall and lands.
        let mut high = place(900);
        let hp2 = high.pawns[1].health;
        high.spawn_projectile(0);
        high.advance_projectiles();
        assert_eq!(high.pawns[1].health, hp2 - Rules::default().damage, "an elevated shot flies over the low wall and lands");
    }

    #[test]
    fn a_pawn_cannot_walk_through_a_blocker() {
        // FM2 (intended change + no-blocker byte-identity): a step whose swept path
        // crosses a wall is refused (the pawn holds, velocity zero); the SAME step
        // with no wall advances by max_speed, so the wall is load-bearing and the
        // no-blocker path is unchanged.
        let wall = Blocker { min: Vec2 { x: 50, y: -2 * POSITION_SCALE }, max: Vec2 { x: 150, y: 2 * POSITION_SCALE }, height: 0 };
        let east = intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, EAST, false);
        let place = |blockers: Vec<Blocker>| {
            let rules = Rules { spawn_jitter: 0, ..Default::default() };
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), blockers, 1);
            m.pawns[0].pos = Vec2::ZERO;
            m.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 }; // far idle, keeps the match Live
            m
        };
        let mut walled = place(vec![wall]);
        step_with(&mut walled, &[(0, east)]);
        assert_eq!(walled.pawns[0].pos, Vec2::ZERO, "the step into the wall is refused");
        assert_eq!(walled.observe(0).own.velocity, Vec2::ZERO, "a refused move reports zero velocity");

        let mut clear = place(vec![]);
        step_with(&mut clear, &[(0, east)]);
        assert_eq!(clear.pawns[0].pos.x, Rules::default().max_speed, "with no wall the same step advances");
    }

    #[test]
    fn a_blocker_between_two_pawns_stops_the_shot() {
        // FM2: a wall between shooter and target blocks the beam through the live step
        // loop — the target takes nothing; remove the wall and the same shot lands.
        let place = |blockers: Vec<Blocker>| {
            let rules = Rules { spawn_jitter: 0, ..Default::default() };
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), blockers, 1);
            m.pawns[0].pos = Vec2 { x: -5 * POSITION_SCALE, y: 0 };
            m.pawns[0].facing = EAST;
            m.pawns[1].pos = Vec2 { x: 5 * POSITION_SCALE, y: 0 };
            m
        };
        let wall = Blocker { min: Vec2 { x: -500, y: -2 * POSITION_SCALE }, max: Vec2 { x: 500, y: 2 * POSITION_SCALE }, height: 0 };
        let mut walled = place(vec![wall]);
        let hp = walled.pawns[1].health;
        step_with(&mut walled, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(walled.pawns[1].health, hp, "the shot is blocked by the wall");

        let mut clear = place(vec![]);
        step_with(&mut clear, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(clear.pawns[1].health, hp - Rules::default().damage, "with no wall the same shot lands");
    }

    #[test]
    fn a_projectile_is_stopped_by_a_wall_and_cannot_tunnel_it() {
        // FM3: a fast projectile whose single swept step jumps a THIN wall is still
        // absorbed (no tunnel) and a target behind the wall is never hit; a target IN
        // FRONT of the same wall is hit on that step — cover behind it shields nothing.
        let thin = Blocker { min: Vec2 { x: 5 * POSITION_SCALE, y: -2 * POSITION_SCALE }, max: Vec2 { x: 5 * POSITION_SCALE, y: 2 * POSITION_SCALE }, height: 0 };
        let rules = Rules { weapon_mode: WeaponMode::Projectile, projectile_speed: 20 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        let place = |target_x: i32| {
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), vec![thin], 1);
            m.pawns[0].pos = Vec2::ZERO;
            m.pawns[0].facing = EAST;
            m.pawns[1].pos = Vec2 { x: target_x, y: 0 };
            m
        };
        // Behind the thin wall: a point test at the post-step position (past the wall)
        // would tunnel; the swept step is absorbed at the wall, so no hit.
        let mut behind = place(10 * POSITION_SCALE);
        let hp = behind.pawns[1].health;
        behind.spawn_projectile(0);
        behind.advance_projectiles();
        assert_eq!(behind.pawns[1].health, hp, "the shot is absorbed by the thin wall, not tunneled");
        assert!(behind.projectiles.is_empty(), "the absorbed shot is despawned");

        // In front of the wall: hit on the same swept step.
        let mut front = place(3 * POSITION_SCALE);
        let hp2 = front.pawns[1].health;
        front.spawn_projectile(0);
        front.advance_projectiles();
        assert_eq!(front.pawns[1].health, hp2 - Rules::default().damage, "a target in front of the wall is hit");
    }

    #[test]
    fn a_pawn_spawned_inside_a_blocker_can_walk_out_and_a_thin_wall_is_safe() {
        // FM4 (spawn/degenerate safety): a pawn the seed places inside a blocker is
        // not trapped — it steps out via the start-containment exemption — and a
        // zero-extent thin wall on the path stops the move without dividing by zero or
        // panicking the clamp.
        let around = Blocker { min: Vec2 { x: -2 * POSITION_SCALE, y: -2 * POSITION_SCALE }, max: Vec2 { x: 2 * POSITION_SCALE, y: 2 * POSITION_SCALE }, height: 0 };
        let rules = Rules { max_speed: 5 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        let east = intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, EAST, false);
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), vec![around], 1);
        m.pawns[0].pos = Vec2::ZERO;
        assert!(blocker_contains(&around, m.pawns[0].pos), "seat 0 starts inside the blocker");
        m.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 };
        step_with(&mut m, &[(0, east)]);
        assert_eq!(m.pawns[0].pos.x, 5 * POSITION_SCALE, "a wall-spawned pawn steps out, not trapped");

        // A zero-extent thin wall (a vertical line at x = 3 m) exactly on the path:
        // the step is refused deterministically with no panic.
        let thin = Blocker { min: Vec2 { x: 3 * POSITION_SCALE, y: -2 * POSITION_SCALE }, max: Vec2 { x: 3 * POSITION_SCALE, y: 2 * POSITION_SCALE }, height: 0 };
        let mut d = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), vec![thin], 1);
        d.pawns[0].pos = Vec2::ZERO;
        d.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 };
        step_with(&mut d, &[(0, east)]);
        assert_eq!(d.pawns[0].pos, Vec2::ZERO, "a thin wall on the path stops the step, no panic");
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

    /// A two-seat match with vertical physics enabled at `gravity`. Seeds are
    /// irrelevant to the z arc (z always starts at 0 and the arc is driven by the
    /// JUMP_VELOCITY/gravity constants, not the spawn draw), but threaded so a test can
    /// prove that seed-independence.
    fn jump_match(gravity: i32, seed: u64) -> Match {
        Match::new(MID.parse().unwrap(), config(2), Rules { gravity, ..Default::default() }, two_seats(), Vec::new(), seed)
    }

    fn jump_press() -> ActionIntent {
        ActionIntent {
            move_dir: Vec2::ZERO,
            aim: EAST,
            buttons: ActionButtons { fire: false, jump: true, ability: false, reload: false },
        }
    }

    #[test]
    fn jump_arc_is_integer_reproducible_and_lands_exactly() {
        // FM1: the vertical arc is pure integer physics — a recorded jump reproduces
        // byte-for-byte across runs and lands EXACTLY at z==0. gravity 500 does not
        // divide JUMP_VELOCITY 1200, so the descent crosses zero BETWEEN ticks and the
        // land-clamp snaps it to exactly 0 (no drift, no negative-z tunnel).
        let arc = |seed: u64| {
            let mut m = jump_match(500, seed);
            // Jump on the opening tick, then ride the arc with no further input.
            step_with(&mut m, &[(0, jump_press())]);
            let mut zs = vec![m.pawns[0].z];
            while m.pawns[0].z > 0 {
                step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, false))]);
                zs.push(m.pawns[0].z);
            }
            zs
        };
        let expected = vec![1200, 1900, 2100, 1800, 1000, 0];
        assert_eq!(arc(1), expected, "the integer arc rises to the apex and lands exactly at 0");
        assert_eq!(arc(7), expected, "a different seed runs the IDENTICAL arc — z is seed-independent and reproducible");
        assert!(expected.iter().all(|&z| z >= 0), "z never tunnels below the ground");
        assert_eq!(*expected.last().unwrap(), 0, "the pawn lands exactly on the ground, no fractional drift");
    }

    #[test]
    fn gravity_zero_keeps_z_inert_and_the_match_2d() {
        // FM2: gravity 0 (the default) DISABLES vertical physics — a spammed jump never
        // lifts a pawn and the match plays exactly as the pre-jump 2D one. Two default
        // matches with identical fire+move scripts differing ONLY in whether jump is
        // held must reach byte-identical sim state, and every z must stay 0.
        let play = |jump: bool| {
            let mut m = close_match(1);
            let press = ActionIntent {
                move_dir: Vec2 { x: MOVE_INTENT_SCALE, y: 0 },
                aim: EAST,
                buttons: ActionButtons { fire: true, jump, ability: false, reload: false },
            };
            for _ in 0..24 {
                if m.phase() != MatchPhase::Live {
                    break;
                }
                step_with(&mut m, &[(0, press), (1, press)]);
            }
            (
                m.phase(),
                m.pawns.iter().map(|p| (p.seat, p.pos, p.z, p.health, p.score, p.alive)).collect::<Vec<_>>(),
            )
        };
        let with_jump = play(true);
        let without_jump = play(false);
        assert_eq!(with_jump, without_jump, "with gravity 0 the jump button changes nothing — the match is 2D");
        assert!(with_jump.1.iter().all(|s| s.2 == 0), "every pawn's z stays 0 while vertical physics are disabled");
    }

    #[test]
    fn an_airborne_pawn_is_hit_and_perceived_exactly_as_on_the_ground() {
        // FM2 (default-off): with vertical_hit_tolerance 0 (the default) z is IGNORED in
        // hit resolution — combat is planar, so the SAME shot lands the SAME damage on a
        // target lifted to a high z (as if mid-jump) as at z==0. Perception stays planar
        // at ANY tolerance (this slice couples HIT only, never vision), so the airborne
        // enemy is still seen and its z is REPORTED on the wire. z-coupling is exercised
        // by the vertical_hit_tolerance tests below.
        let mut ground = close_match(1);
        let ground_visible: Vec<u32> = ground.observe(0).visible.iter().map(|e| e.entity_id).collect();
        step_with(&mut ground, &[(0, intent(Vec2::ZERO, EAST, true))]);
        let ground_dmg = Rules::default().start_health - ground.pawns[1].health;
        assert!(ground_dmg > 0, "the baseline shot lands on the grounded target");

        // Identical scenario, target lifted to z=5000 before the shot (gravity 0 holds it).
        let mut air = close_match(1);
        air.pawns[1].z = 5000;
        let air_visible: Vec<u32> = air.observe(0).visible.iter().map(|e| e.entity_id).collect();
        let reported_z = air.observe(0).visible.iter().find(|e| e.entity_id == 1).map(|e| e.z);
        step_with(&mut air, &[(0, intent(Vec2::ZERO, EAST, true))]);
        let air_dmg = Rules::default().start_health - air.pawns[1].health;

        assert_eq!(air_visible, ground_visible, "perception is unchanged by z — the airborne enemy is still seen");
        assert_eq!(reported_z, Some(5000), "z is reported on the visible entity (reported, not used to gate)");
        assert_eq!(air_dmg, ground_dmg, "with the tolerance off, the hit lands the same damage regardless of z");
    }

    /// A controlled 2-seat hit scenario for z-coupling: shooter (seat 0) at the origin
    /// facing east, target (seat 1) dead-on at 10 m at elevation `target_z`, under
    /// `mode`/`tolerance`. Returns the damage the one shot deals (0 == cleared). Uses the
    /// in-crate privilege to place pawns and z exactly, the same as the parity helpers.
    fn vertical_hit_damage(mode: WeaponMode, tolerance: i32, target_z: i32) -> u16 {
        let rules = Rules {
            weapon_mode: mode,
            vertical_hit_tolerance: tolerance,
            spawn_jitter: 0,
            ..Default::default()
        };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].facing = EAST;
        m.pawns[1].pos = Vec2 { x: 10 * POSITION_SCALE, y: 0 };
        m.pawns[1].z = target_z;
        let before = m.pawns[1].health;
        match mode {
            WeaponMode::Hitscan => m.resolve_fire(0),
            WeaponMode::Melee => {
                m.pawns[1].pos = Vec2 { x: m.rules.melee_range, y: 0 }; // inside the swing
                m.resolve_melee(0);
            }
            WeaponMode::Projectile => {
                m.spawn_projectile(0);
                let mut age = 0;
                while !m.projectiles.is_empty() && age < MAX_PROJECTILE_LIFETIME {
                    m.advance_projectiles();
                    age += 1;
                }
            }
        }
        before - m.pawns[1].health
    }

    #[test]
    fn vertical_tolerance_gates_a_hitscan_shot_at_the_inclusive_boundary() {
        // The core mechanic: a shot lands only within |Δz| <= tolerance. With tolerance
        // off (0) an elevated target is hit (planar); with it on, a target above the
        // tolerance clears the shot, one at exactly the tolerance is still hit (the
        // boundary is INCLUSIVE), and one a single unit higher is missed.
        let dmg = Rules::default().damage;
        assert_eq!(vertical_hit_damage(WeaponMode::Hitscan, 0, 9999), dmg, "tolerance off: z ignored, elevated target hit");
        assert_eq!(vertical_hit_damage(WeaponMode::Hitscan, 1000, 800), dmg, "within tolerance: hit");
        assert_eq!(vertical_hit_damage(WeaponMode::Hitscan, 1000, 1000), dmg, "exactly at the tolerance: hit (inclusive)");
        assert_eq!(vertical_hit_damage(WeaponMode::Hitscan, 1000, 1001), 0, "one unit over the tolerance: cleared");
        assert_eq!(vertical_hit_damage(WeaponMode::Hitscan, 1000, 5000), 0, "far above the tolerance: cleared");
        // Symmetric below the shooter: |Δz| uses the absolute difference, so a target
        // dug in BELOW the shooter clears the shot the same way one above does.
        assert_eq!(vertical_hit_damage(WeaponMode::Hitscan, 1000, -5000), 0, "far below the tolerance: cleared too");
    }

    #[test]
    fn vertical_tolerance_gates_melee_and_projectiles_too() {
        // Every weapon mode shares the one vertical rule: an elevated target beyond the
        // tolerance escapes a melee swing and a projectile alike, while the tolerance-off
        // (0) default lands both regardless of z — so enabling z-combat is consistent
        // across modes and disabling it is byte-identical for all of them.
        let melee = Rules::default().melee_damage;
        assert_eq!(vertical_hit_damage(WeaponMode::Melee, 0, 5000), melee, "melee tolerance off: elevated target cleaved");
        assert_eq!(vertical_hit_damage(WeaponMode::Melee, 1000, 5000), 0, "melee on: a mid-air enemy escapes the swing");
        assert_eq!(vertical_hit_damage(WeaponMode::Melee, 1000, 0), melee, "melee on, same elevation: still cleaved");

        let proj = Rules::default().damage;
        assert_eq!(vertical_hit_damage(WeaponMode::Projectile, 0, 5000), proj, "projectile tolerance off: elevated target hit");
        assert_eq!(vertical_hit_damage(WeaponMode::Projectile, 1000, 5000), 0, "projectile on: the level shot flies under a high target");
        assert_eq!(vertical_hit_damage(WeaponMode::Projectile, 1000, 0), proj, "projectile on, same elevation: hits");
    }

    #[test]
    fn vertical_hit_math_saturates_at_extreme_z_and_is_deterministic() {
        // FM3 (overflow / non-determinism): the |Δz| compare widens to i64, so even the
        // widest possible separation (i32::MAX above, i32::MIN below) resolves without a
        // panic, and the resolver is a pure function of its inputs (two identical setups
        // deal identical damage). Extreme separation clears the shot; a maxed tolerance
        // re-lands a same-elevation target.
        assert_eq!(vertical_hit_damage(WeaponMode::Hitscan, i32::MAX, i32::MAX), Rules::default().damage, "max tolerance, same z: hit, no overflow");
        // |i32::MAX - i32::MIN| ~= 2^32 > any i32 tolerance, so the shot is cleared (the
        // i64 widening is what keeps this from panicking on subtract/abs).
        let rules = Rules { vertical_hit_tolerance: i32::MAX, spawn_jitter: 0, ..Default::default() };
        let shoot_extreme = || {
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
            m.pawns[0].pos = Vec2::ZERO;
            m.pawns[0].facing = EAST;
            m.pawns[0].z = i32::MAX;
            m.pawns[1].pos = Vec2 { x: 10 * POSITION_SCALE, y: 0 };
            m.pawns[1].z = i32::MIN;
            let before = m.pawns[1].health;
            m.resolve_fire(0);
            before - m.pawns[1].health
        };
        assert_eq!(shoot_extreme(), 0, "the widest z separation clears the shot without panicking");
        assert_eq!(shoot_extreme(), shoot_extreme(), "the z-coupled resolver is deterministic");
    }

    #[test]
    fn z_coupled_match_is_deterministic_and_jumping_reduces_damage() {
        // FM3 (determinism) + the payoff: in a gravity match a pawn that jumps above the
        // tolerance takes strictly LESS fire than the same pawn planar (tolerance 0),
        // and two identical z-coupled runs evolve to the IDENTICAL sim state. seat 0
        // fires east every tick (lands on cooldown); seat 1 holds jump and rides the arc.
        let script = |tolerance: i32| {
            let rules = Rules {
                gravity: 500,
                vertical_hit_tolerance: tolerance,
                damage: 50, // a landed cadence downs seat 1 in two shots — so dodging shows
                spawn_radius: 2 * POSITION_SCALE,
                spawn_jitter: 0,
                ..Default::default()
            };
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
            for _ in 0..16 {
                if m.phase() != MatchPhase::Live {
                    break;
                }
                step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true)), (1, jump_press())]);
            }
            m.pawns.iter().map(|p| (p.seat, p.pos, p.z, p.health, p.alive)).collect::<Vec<_>>()
        };
        let coupled = script(500); // JUMP_VELOCITY 1200 keeps seat 1 above 500 for most of the arc
        assert_eq!(coupled, script(500), "two identical z-coupled runs evolve to the identical sim state");
        let planar = script(0);
        let target_hp = |st: &[(SeatId, Vec2, i32, u16, bool)]| st.iter().find(|s| s.0 == 1).unwrap().3;
        assert!(
            target_hp(&coupled) > target_hp(&planar),
            "jumping above the tolerance dodged fire the planar run landed (coupled hp {} > planar hp {})",
            target_hp(&coupled),
            target_hp(&planar),
        );
    }

    #[test]
    fn a_held_jump_never_double_jumps_and_relaunches_only_after_landing() {
        // FM4: a jump requires the grounded state (z==0 && z_vel==0). Holding jump every
        // tick must NOT relaunch mid-air (no double/infinite jump) — the pawn rides the
        // single arc to an exact landing — and then jumps AGAIN the first grounded tick.
        let mut m = jump_match(500, 1);
        let mut zs = Vec::new();
        for _ in 0..7 {
            step_with(&mut m, &[(0, jump_press())]);
            zs.push(m.pawns[0].z);
        }
        assert_eq!(
            zs,
            vec![1200, 1900, 2100, 1800, 1000, 0, 1200],
            "held jump rides the arc to z==0 (no mid-air relaunch), then re-jumps the tick after landing"
        );

        // The airborne portion is byte-identical to a SINGLE jump: every held press
        // during flight was ignored by the grounded gate, contributing nothing.
        let mut single = jump_match(500, 1);
        step_with(&mut single, &[(0, jump_press())]);
        let mut single_zs = vec![single.pawns[0].z];
        while single.pawns[0].z > 0 {
            step_with(&mut single, &[(0, intent(Vec2::ZERO, EAST, false))]);
            single_zs.push(single.pawns[0].z);
        }
        assert_eq!(single_zs.as_slice(), &zs[..6], "a held jump's flight equals a single jump's — mid-air presses are inert");
    }

    #[test]
    fn negative_gravity_is_inert_like_disabled() {
        // FM2 (defensive): the vertical gate is `gravity > 0`, NOT `!= 0`, so a negative
        // gravity is treated as DISABLED — not as a velocity-amplifying runaway (a `-=`
        // of a negative gravity would ADD velocity every tick and launch a pawn off the
        // top of the world). A held jump under negative gravity must leave every z at 0,
        // exactly like gravity 0. This pins the strict `> 0` gate against a regression
        // that loosened it to `!= 0`, which would compile and pass every other test.
        let mut m = jump_match(-500, 1);
        for _ in 0..10 {
            step_with(&mut m, &[(0, jump_press())]);
        }
        assert!(
            m.pawns.iter().all(|p| p.z == 0 && p.z_vel == 0),
            "negative gravity is inert — no pawn leaves the ground"
        );
    }

    /// A two-seat match with the dash enabled at `dash_cooldown`, no spawn jitter, in
    /// the given `bounds` with the given `blockers` — so seat 0 starts at exactly
    /// (-spawn_radius, 0) and seat 1 at (+spawn_radius, 0), and a dash arc is computed
    /// against known geometry.
    fn dash_match(dash_cooldown: u16, bounds: Vec2, blockers: Vec<Blocker>, seed: u64) -> Match {
        let rules = Rules { dash_cooldown, spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        let cfg = MatchConfig { tick_hz: 30, max_ticks: 3600, bounds, seats: 2 };
        Match::new(MID.parse().unwrap(), cfg, rules, two_seats(), blockers, seed)
    }

    fn dash_intent(move_dir: Vec2) -> ActionIntent {
        ActionIntent { move_dir, aim: EAST, buttons: ActionButtons { fire: false, jump: false, ability: true, reload: false } }
    }

    #[test]
    fn a_dash_clamps_to_bounds_and_stops_at_a_wall() {
        // FM1: the dash burst must respect the SAME movement safety as a walk — it
        // clamps to the arena bounds and is refused by a blocker (both go through
        // slide), so it can neither leave the arena nor tunnel a wall (the god-move the
        // movement clamp exists to stop). DASH_DISTANCE 3000 dwarfs the 200-unit walk,
        // so an unclamped/untunneled dash would be unmistakable.
        let east = Vec2 { x: MOVE_INTENT_SCALE, y: 0 };

        // (a) Wall: seat 0 at (-2000,0) walks +200 to (-1800,0); the dash from there to
        // (1200,0) crosses a wall at x∈[-1000,-800], so the burst is refused and the
        // pawn holds at its post-walk position — it does NOT punch through.
        let wall = Blocker { min: Vec2 { x: -1000, y: -2000 }, max: Vec2 { x: -800, y: 2000 }, height: 0 };
        let mut m = dash_match(10, Vec2 { x: 50_000, y: 50_000 }, vec![wall], 1);
        assert_eq!(m.pawns[0].pos, Vec2 { x: -2000, y: 0 }, "seat 0 spawns at -spawn_radius");
        step_with(&mut m, &[(0, dash_intent(east))]);
        assert_eq!(m.pawns[0].pos, Vec2 { x: -1800, y: 0 }, "the dash is refused by the wall — only the walk applied, no tunnel");
        assert_eq!(m.pawns[0].dash_cooldown, 10, "consume-on-trigger: the wall refusing the burst is not a free retry");

        // (b) Bounds: in a ±2500 arena seat 1 starts at (2000,0), walks to (2200,0),
        // then the dash toward +x clamps to the boundary instead of escaping to 5200.
        let mut edge = dash_match(10, Vec2 { x: 2500, y: 2500 }, Vec::new(), 1);
        assert_eq!(edge.pawns[1].pos, Vec2 { x: 2000, y: 0 }, "seat 1 spawns at +spawn_radius");
        step_with(&mut edge, &[(1, dash_intent(east))]);
        assert_eq!(edge.pawns[1].pos, Vec2 { x: 2500, y: 0 }, "the dash clamps to the arena bound, never past it");
    }

    #[test]
    fn a_dash_is_refused_during_cooldown_and_disabled_at_zero() {
        let east = Vec2 { x: MOVE_INTENT_SCALE, y: 0 };

        // FM2a (cooldown gate): dash_cooldown 10. The opening dash bursts seat 0 from
        // (-2000,0) to (1200,0) [walk +200, dash +3000]; the very next tick's dash is
        // still on cooldown, so the pawn only walks (+200) — it does not burst again
        // (a second burst would land at 4400).
        let mut m = dash_match(10, Vec2 { x: 50_000, y: 50_000 }, Vec::new(), 1);
        step_with(&mut m, &[(0, dash_intent(east))]);
        assert_eq!(m.pawns[0].pos, Vec2 { x: 1200, y: 0 }, "the first dash bursts walk+DASH_DISTANCE");
        assert_eq!(m.pawns[0].dash_cooldown, 10, "the dash set its cooldown");
        step_with(&mut m, &[(0, dash_intent(east))]);
        assert_eq!(m.pawns[0].pos, Vec2 { x: 1400, y: 0 }, "the in-cooldown dash is refused — only the +200 walk applied");

        // FM2b (default-off byte-identity): with dash_cooldown 0 a held ability press
        // changes nothing — two matches differing ONLY in the ability bit reach
        // byte-identical state (close_match leaves dash_cooldown at its 0 default).
        let play = |ability: bool| {
            let mut m = close_match(1);
            let press = ActionIntent { move_dir: east, aim: EAST, buttons: ActionButtons { fire: true, jump: false, ability, reload: false } };
            for _ in 0..24 {
                if m.phase() != MatchPhase::Live {
                    break;
                }
                step_with(&mut m, &[(0, press), (1, press)]);
            }
            m.pawns.iter().map(|p| (p.seat, p.pos, p.health, p.score, p.alive)).collect::<Vec<_>>()
        };
        assert_eq!(play(true), play(false), "with dash_cooldown 0 the ability button changes nothing — byte-identical");
    }

    #[test]
    fn a_dash_reproduces_byte_for_byte_and_surfaces_only_own_cooldown() {
        // FM3: the dash displacement is pure integer and reproduces across runs; the
        // OWN dash cooldown is surfaced on SelfState, counting down to 0 exactly on the
        // tick the next dash is honored (the same start-of-tick off-by-one the fire
        // cooldown carries). An enemy's perception of the dasher carries NO dash
        // readiness — pinned structurally proto-side (VisibleEntity/BroadcastEntity
        // have no such field; widening either fails the wire-shape tests).
        let east = Vec2 { x: MOVE_INTENT_SCALE, y: 0 };
        let run = || {
            let mut m = dash_match(3, Vec2 { x: 50_000, y: 50_000 }, Vec::new(), 1);
            let mut trace = Vec::new();
            for _ in 0..5 {
                let ready = m.observe(0).own.dash_cooldown;
                step_with(&mut m, &[(0, dash_intent(east))]);
                trace.push((ready, m.pawns[0].pos));
            }
            trace
        };
        let a = run();
        assert_eq!(a, run(), "the dash arc + cooldown readout reproduce byte-for-byte across runs");

        // dash_cooldown reads 0 (ready) exactly when a dash fires: the opening tick,
        // then again after the 3-tick cooldown elapses. Each 0 readout coincides with a
        // walk+burst (+3200), the in-cooldown ticks with a walk-only step (+200).
        let readouts: Vec<u16> = a.iter().map(|(r, _)| *r).collect();
        assert_eq!(readouts, vec![0, 2, 1, 0, 2], "dash_cooldown counts 0 (ready) → 2 → 1 → 0 (ready) → 2");
        let xs: Vec<i32> = a.iter().map(|(_, p)| p.x).collect();
        assert_eq!(xs, vec![1200, 1400, 1600, 4800, 5000], "the two ready ticks burst +3200, the cooldown ticks walk +200");
    }

    #[test]
    fn a_zero_direction_dash_is_a_noop_and_keeps_the_dash_ready() {
        // FM4: an ability press with no movement direction has a DEFINED behavior — a
        // no-op that does NOT consume the cooldown (there is no direction to dash, so
        // the dash stays ready) rather than dashing a garbage vector or wasting the
        // cooldown. A directionful press the next tick still dashes, proving the
        // directionless press cost nothing.
        let east = Vec2 { x: MOVE_INTENT_SCALE, y: 0 };
        let mut m = dash_match(10, Vec2 { x: 50_000, y: 50_000 }, Vec::new(), 1);

        step_with(&mut m, &[(0, dash_intent(Vec2::ZERO))]);
        assert_eq!(m.pawns[0].pos, Vec2 { x: -2000, y: 0 }, "a zero-direction dash moves nothing");
        assert_eq!(m.pawns[0].dash_cooldown, 0, "a zero-direction dash does NOT consume the cooldown — the dash stays ready");

        step_with(&mut m, &[(0, dash_intent(east))]);
        assert_eq!(m.pawns[0].pos, Vec2 { x: 1200, y: 0 }, "the still-ready dash fires on a directionful press");
        assert_eq!(m.pawns[0].dash_cooldown, 10, "now the dash consumed its cooldown");
    }

    /// Build a 2-seat match with `wall_slide` set, no jitter, seat 0 at the origin and
    /// seat 1 parked far away (idle, keeping the match Live) — so a slide is computed
    /// from (0,0) against known wall geometry.
    fn slide_match(wall_slide: bool, blockers: Vec<Blocker>) -> Match {
        let rules = Rules { wall_slide, spawn_jitter: 0, ..Default::default() };
        let mut m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), blockers, 1);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 };
        m
    }

    #[test]
    fn wall_slide_slides_along_a_grazed_wall_and_a_corner_still_stops() {
        // FM1: with wall_slide on, a diagonal step whose full path is refused retries the
        // axis-separated components, so a pawn grazing a wall SLIDES along the unblocked
        // axis instead of dead-stopping — but it must not tunnel (the slid end stays on
        // the near side of the wall), and an inside corner (both axes refused) holds.
        let ne = Vec2 { x: 600, y: 800 }; // mag 1000 -> dx=120, dy=160 at max_speed 200
        // A tall vertical wall east of the origin: it refuses the eastward component but
        // never the pure-north one.
        let east_wall = Blocker { min: Vec2 { x: 100, y: -5000 }, max: Vec2 { x: 300, y: 5000 }, height: 0 };

        // Off: the diagonal into the wall dead-stops at the origin (the historical rule).
        let mut off = slide_match(false, vec![east_wall]);
        step_with(&mut off, &[(0, intent(ne, EAST, false))]);
        assert_eq!(off.pawns[0].pos, Vec2::ZERO, "wall_slide off: a grazing diagonal dead-stops");

        // On: the eastward component is refused but the northward one is clear, so the
        // pawn slides straight north (x stays 0 — well short of the wall's near edge 100).
        let mut on = slide_match(true, vec![east_wall]);
        step_with(&mut on, &[(0, intent(ne, EAST, false))]);
        assert_eq!(on.pawns[0].pos, Vec2 { x: 0, y: 160 }, "wall_slide on: the pawn slides along the wall");
        assert!(on.pawns[0].pos.x < east_wall.min.x, "the slid pawn never crosses the wall's near edge (no tunnel)");

        // Inside corner: a wall to the east AND one to the north refuse BOTH axis-
        // separated retries, so even with wall_slide on the pawn holds at the origin.
        let north_wall = Blocker { min: Vec2 { x: -5000, y: 100 }, max: Vec2 { x: 5000, y: 300 }, height: 0 };
        let mut corner = slide_match(true, vec![east_wall, north_wall]);
        step_with(&mut corner, &[(0, intent(ne, EAST, false))]);
        assert_eq!(corner.pawns[0].pos, Vec2::ZERO, "an inside corner (both axes refused) still full-stops");
    }

    #[test]
    fn wall_slide_only_changes_a_blocked_step() {
        // FM2 (default-off byte-identity): wall_slide changes movement ONLY when a step
        // is refused — an unobstructed move is identical on and off, and with it OFF a
        // blocked move dead-stops exactly as before. So the default (off) is byte-
        // identical to the pre-slide rule, and the flag is the sole load-bearing
        // difference on a block (the parity golden regenerated with zero outcome drift).
        let ne = Vec2 { x: 600, y: 800 };
        let east_wall = Blocker { min: Vec2 { x: 100, y: -5000 }, max: Vec2 { x: 300, y: 5000 }, height: 0 };

        // Unobstructed: no wall, so the full diagonal applies — identical on and off.
        let mut clear_off = slide_match(false, Vec::new());
        let mut clear_on = slide_match(true, Vec::new());
        step_with(&mut clear_off, &[(0, intent(ne, EAST, false))]);
        step_with(&mut clear_on, &[(0, intent(ne, EAST, false))]);
        assert_eq!(clear_off.pawns[0].pos, Vec2 { x: 120, y: 160 }, "an unobstructed diagonal advances fully");
        assert_eq!(clear_on.pawns[0].pos, clear_off.pawns[0].pos, "wall_slide does not touch an unobstructed step");

        // Obstructed: off dead-stops (the historical rule), on slides — the flag is the
        // only difference, and off reproduces the pre-slide behavior exactly.
        let mut blocked_off = slide_match(false, vec![east_wall]);
        let mut blocked_on = slide_match(true, vec![east_wall]);
        step_with(&mut blocked_off, &[(0, intent(ne, EAST, false))]);
        step_with(&mut blocked_on, &[(0, intent(ne, EAST, false))]);
        assert_eq!(blocked_off.pawns[0].pos, Vec2::ZERO, "wall_slide off: a blocked step dead-stops as before");
        assert_ne!(
            blocked_on.pawns[0].pos, blocked_off.pawns[0].pos,
            "wall_slide on is the sole load-bearing difference on a block"
        );
    }

    #[test]
    fn wall_slide_resolves_x_before_y_and_reproduces() {
        // FM3 (axis-order determinism + reproducibility): when BOTH axis-separated
        // retries are clear, the fixed X-first convention decides — the pawn slides on X,
        // not Y. A small blocker squarely on the diagonal refuses the full step while
        // neither axis-aligned path touches it, so the resolution order is the only thing
        // that picks the outcome. The result is pure-integer and reproduces exactly.
        let ne = Vec2 { x: 600, y: 800 }; // dx=120, dy=160
        // (60,80) is the midpoint of the (0,0)->(120,160) diagonal; this box straddles it
        // but lies clear of both the y=0 (X-only) and x=0 (Y-only) paths.
        let nub = Blocker { min: Vec2 { x: 50, y: 70 }, max: Vec2 { x: 70, y: 90 }, height: 0 };

        let mut a = slide_match(true, vec![nub]);
        step_with(&mut a, &[(0, intent(ne, EAST, false))]);
        assert_eq!(a.pawns[0].pos, Vec2 { x: 120, y: 0 }, "both retries clear -> X-first wins (slides on X, not Y)");

        // Reproducible: a second identical run lands byte-for-byte on the same slid point.
        let mut b = slide_match(true, vec![nub]);
        step_with(&mut b, &[(0, intent(ne, EAST, false))]);
        assert_eq!(b.pawns[0].pos, a.pawns[0].pos, "the slid position reproduces exactly");
    }

    #[test]
    fn wall_slide_dash_slides_and_a_slide_clamps_to_bounds() {
        // FM4 (shared-helper + bounds): slide() is used by BOTH the walk and the dash, so
        // wall_slide changes the dash too — a dash grazing a wall slides instead of being
        // refused outright; and the slid destination still clamps to the arena bounds.
        let ne = Vec2 { x: 600, y: 800 }; // walk dx,dy = 120,160 / dash burst = 1800,2400
        // A tall vertical wall east of the post-walk position: the walk (to x=120) clears
        // it, the dash burst (to x~1920) is refused on X, the north component is clear.
        let east_wall = Blocker { min: Vec2 { x: 300, y: -10_000 }, max: Vec2 { x: 500, y: 10_000 }, height: 0 };
        let dash = |wall_slide: bool| {
            let rules = Rules { wall_slide, dash_cooldown: 5, spawn_jitter: 0, ..Default::default() };
            let cfg = MatchConfig { tick_hz: 30, max_ticks: 3600, bounds: Vec2 { x: 50_000, y: 50_000 }, seats: 2 };
            let mut m = Match::new(MID.parse().unwrap(), cfg, rules, two_seats(), vec![east_wall], 1);
            m.pawns[0].pos = Vec2::ZERO;
            m.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 };
            m
        };

        // Off: the walk applies (to (120,160)), then the dash burst is refused by the
        // wall -> the pawn holds at the post-walk position.
        let mut off = dash(false);
        step_with(&mut off, &[(0, dash_intent(ne))]);
        assert_eq!(off.pawns[0].pos, Vec2 { x: 120, y: 160 }, "wall_slide off: the blocked dash burst is refused, only the walk applies");

        // On: the dash burst's eastward component is refused but the northward one is
        // clear, so the burst slides north from the post-walk position.
        let mut on = dash(true);
        step_with(&mut on, &[(0, dash_intent(ne))]);
        assert_eq!(on.pawns[0].pos, Vec2 { x: 120, y: 2560 }, "wall_slide on: the dash inherits the slide and bursts north");
        assert!(on.pawns[0].pos.x < east_wall.min.x, "the slid dash never crosses the wall's near edge");

        // Bounds: a wall_slide move toward the arena edge still clamps in-bounds (no wall).
        let cfg = MatchConfig { tick_hz: 30, max_ticks: 3600, bounds: Vec2 { x: 2500, y: 2500 }, seats: 2 };
        let rules = Rules { wall_slide: true, spawn_jitter: 0, ..Default::default() };
        let mut edge = Match::new(MID.parse().unwrap(), cfg, rules, two_seats(), Vec::new(), 1);
        edge.pawns[0].pos = Vec2 { x: 2400, y: 0 };
        edge.pawns[1].pos = Vec2 { x: -2400, y: 0 };
        step_with(&mut edge, &[(0, intent(Vec2 { x: MOVE_INTENT_SCALE, y: 0 }, EAST, false))]);
        assert_eq!(edge.pawns[0].pos, Vec2 { x: 2500, y: 0 }, "a wall_slide move still clamps to the arena bound");
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
    fn fine_lut_matches_an_exact_trig_reference() {
        // The finer-aim table is the integer core's only "trig", so it is pinned
        // against f64 cos/sin — test-only float; the runtime stays integer. Each of
        // the 64 directions must equal the rounded unit vector exactly, the four axes
        // must be exact, the quarter table must climb strictly from 0 to FINE_SCALE,
        // and every entry must sit a hair off the unit circle (tighter than the octant
        // diagonal it replaces).
        use std::f64::consts::TAU;
        let scale2 = FINE_SCALE as i64 * FINE_SCALE as i64;
        for (i, &(cx, cy)) in FINE_LUT.iter().enumerate() {
            let ang = TAU * i as f64 / FINE_DIRS as f64;
            let tc = (FINE_SCALE as f64 * ang.cos()).round() as i32;
            let ts = (FINE_SCALE as f64 * ang.sin()).round() as i32;
            assert_eq!((cx, cy), (tc, ts), "direction {i} diverged from the trig reference");
            let norm2 = cx as i64 * cx as i64 + cy as i64 * cy as i64;
            assert!((norm2 - scale2).abs() <= 50_000, "direction {i} off the unit circle by {}", norm2 - scale2);
        }
        assert_eq!(FINE_LUT[0], (FINE_SCALE, 0), "east");
        assert_eq!(FINE_LUT[FINE_QUARTER], (0, FINE_SCALE), "north");
        assert_eq!(FINE_LUT[2 * FINE_QUARTER], (-FINE_SCALE, 0), "west");
        assert_eq!(FINE_LUT[3 * FINE_QUARTER], (0, -FINE_SCALE), "south");
        assert_eq!(FINE_QSIN[0], 0);
        assert_eq!(FINE_QSIN[FINE_QUARTER], FINE_SCALE);
        for k in 1..=FINE_QUARTER {
            assert!(FINE_QSIN[k] > FINE_QSIN[k - 1], "the quarter sine must strictly increase at {k}");
        }
        // Quadrant symmetry, independent of rounding: reflecting a direction across the
        // X axis (i ↔ −i) negates sin and keeps cos; across the Y axis (i ↔ N/2−i)
        // negates cos and keeps sin. A future table edit that breaks the mirror — the
        // directional-bias failure mode — trips here.
        for i in 0..FINE_DIRS {
            let (cx, cy) = FINE_LUT[i];
            assert_eq!(FINE_LUT[(FINE_DIRS - i) % FINE_DIRS], (cx, -cy), "X-axis mirror broke at {i}");
            assert_eq!(FINE_LUT[(2 * FINE_QUARTER + FINE_DIRS - i) % FINE_DIRS], (-cx, cy), "Y-axis mirror broke at {i}");
        }
    }

    #[test]
    fn default_aim_is_octant_and_a_record_without_it_reads_octant() {
        // FM1: the default is the original 8-way behavior, so every pre-finer-aim
        // match and replay is byte-identical (the 75 existing combat/replay tests run
        // under it unchanged). A Rules serialized before the field existed must
        // deserialize to Octant — not whatever the enum's zero value happens to be.
        assert_eq!(AimMode::default(), AimMode::Octant);
        assert_eq!(Rules::default().aim_mode, AimMode::Octant);
        let mut obj = serde_json::to_value(Rules::default()).unwrap();
        obj.as_object_mut().unwrap().remove("aim_mode");
        let back: Rules = serde_json::from_value(obj).unwrap();
        assert_eq!(back.aim_mode, AimMode::Octant, "absent aim_mode defaults to Octant");
    }

    #[test]
    fn fine_aim_lands_a_shot_the_octant_snap_missed() {
        // The headline win, and the proof aim_mode is an outcome determinant: seat 0
        // aims 11.25° — a 64-way direction that snaps to due East as an octant — at a
        // target 11.25° off the East axis, ~20 m out. The octant beam points East and
        // the target's ~3.9 m lateral offset exceeds the 1.5 m hit radius, so the shot
        // misses; the finer beam points at the true aim, so the identical shot lands.
        let aim: Bam = 2048; // 11.25°
        assert_eq!(octant_index(aim), 0, "the aim snaps to the East octant");
        let target = Vec2 { x: 19_617, y: 3_902 }; // on the fine index-2 ray, ~20 m, ~3.9 m off East

        let fire_once = |mode: AimMode| {
            let mut m = close_match(1);
            m.rules.aim_mode = mode;
            m.pawns[0].pos = Vec2::ZERO;
            m.pawns[0].facing = aim;
            m.pawns[1].pos = target;
            let before = m.pawns[1].health;
            m.resolve_fire(0);
            before - m.pawns[1].health
        };
        assert_eq!(fire_once(AimMode::Octant), 0, "the octant snap misses the off-axis target");
        assert_eq!(fire_once(AimMode::Fine), Rules::default().damage, "finer aim lands the shot the octant missed");
    }

    #[test]
    fn fine_aim_hit_math_holds_at_extreme_scale() {
        // FM2: the finer beam's Q15 scale widens the dot/proj/perp products, so the
        // i128 widening must be preserved. (a) A real hit at a 1.5 Gmm range with a
        // billion-scale coordinate resolves exactly — i32/i64 intermediates would wrap
        // computing proj². (b) Seats at opposite i32 extremes fire without an overflow
        // panic; that far target is out of range, where the dist² pre-gate is the
        // widening that bites (a squared planar distance there exceeds i64).
        let cfg = MatchConfig { bounds: Vec2 { x: i32::MAX, y: i32::MAX }, ..config(2) };
        let rules = Rules { aim_mode: AimMode::Fine, weapon_range: 2_000_000_000, spawn_jitter: 0, ..Default::default() };
        let mut m = Match::new(MID.parse().unwrap(), cfg, rules, two_seats(), Vec::new(), 1);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].facing = EAST; // fine(EAST) = (FINE_SCALE, 0), dead-on the +X target
        m.pawns[1].pos = Vec2 { x: 1_500_000_000, y: 0 };
        let before = m.pawns[1].health;
        m.resolve_fire(0);
        assert_eq!(m.pawns[1].health, before - Rules::default().damage, "a billion-scale in-range shot lands, no wrap");

        let cfg2 = MatchConfig { bounds: Vec2 { x: i32::MAX, y: i32::MAX }, ..config(2) };
        let rules2 = Rules { aim_mode: AimMode::Fine, spawn_radius: i32::MAX, spawn_jitter: 0, ..Default::default() };
        let mut m2 = Match::new(MID.parse().unwrap(), cfg2, rules2, two_seats(), Vec::new(), 1);
        let h = m2.observe(1).own.health;
        step_with(&mut m2, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m2.observe(1).own.health, h, "out-of-range extreme fire: no hit, no overflow");
    }

    /// [`close_match`] in finer-aim mode — the same exact geometry, now resolving the
    /// hitscan beam through the 64-way table.
    fn fine_close_match(seed: u64) -> Match {
        let rules = Rules {
            aim_mode: AimMode::Fine,
            spawn_radius: 2 * POSITION_SCALE,
            spawn_jitter: 0,
            ..Default::default()
        };
        Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), seed)
    }

    fn play_fine(seed: u64) -> Match {
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker)];
        run_match(fine_close_match(seed), &mut policies)
    }

    #[test]
    fn a_fine_aim_match_replays_byte_for_byte_and_across_runs() {
        // FM3: the finer beam is a pure integer table lookup, so a fine-aim match is
        // deterministic — it re-runs from its action stream ALONE to the same result +
        // digest, and two independent same-seed runs are byte-identical (the basis for
        // grading and on-chain attestation). aim_mode rides in the record's Rules, so
        // the re-run resolves under the same resolution it was played on.
        let played = play_fine(1);
        assert_eq!(played.phase(), MatchPhase::Ended);
        let result = played.result().unwrap().clone();
        let replay = played.into_replay();
        let replayed = replay_match(fine_close_match(1), &replay);
        assert_eq!(replayed.result().unwrap(), &result, "fine-aim replay diverged from the live result");
        assert_eq!(replayed.into_replay().digest(), replay.digest(), "fine-aim replay digest diverged");

        let a = play_fine(7).into_replay();
        let b = play_fine(7).into_replay();
        assert_eq!(a.digest(), b.digest(), "two same-seed fine-aim runs diverged");
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
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

    /// Like [`play`], but in projectile weapon mode — the same two Seekers, now firing
    /// traveling shots, driven to a terminal result.
    fn play_projectiles(seed: u64) -> Match {
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker)];
        run_match(projectile_close_match(seed), &mut policies)
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
            height: 0,
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
    fn a_projectile_match_replays_byte_for_byte() {
        // FM1: projectiles are derived from the recorded fire actions, never recorded
        // themselves. A projectile match re-runs from its action stream ALONE on a
        // fresh same-seed match — the shots respawn and fly identically — to the same
        // result + digest. This is what keeps a traveling-shot match attestable.
        let played = play_projectiles(1);
        assert_eq!(played.phase(), MatchPhase::Ended);
        let result = played.result().unwrap().clone();
        let replay = played.into_replay();

        let replayed = replay_match(projectile_close_match(1), &replay);
        assert_eq!(replayed.phase(), MatchPhase::Ended);
        assert_eq!(replayed.result().unwrap(), &result, "projectile replay diverged from the live result");
        assert_eq!(replayed.into_replay().digest(), replay.digest(), "projectile replay digest diverged");
    }

    #[test]
    fn a_projectile_match_is_byte_identical_across_runs() {
        // FM1: the projectile path is float-free (octant velocity, integer swept
        // collision), so two independent same-seed runs are byte-identical.
        let a = play_projectiles(7).into_replay();
        let b = play_projectiles(7).into_replay();
        assert_eq!(a.digest(), b.digest());
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
    }

    #[test]
    fn a_finished_projectile_record_verifies() {
        // A projectile match played to the end re-runs from its record ALONE back to
        // the same result + committed hash — verify exercises the whole projectile sim.
        let rec = play_projectiles(1).to_record().unwrap();
        let verified = rec.verify().expect("a faithful projectile record verifies");
        assert_eq!(verified, rec.result, "verify returns the reproduced result");
    }

    #[test]
    fn flipping_weapon_mode_fails_to_reproduce() {
        // FM1: weapon_mode is a Rules determinant bound by RE-EXECUTION, not the digest
        // (the digest hashes only the action stream). A projectile match's record
        // re-run as hitscan resolves the same actions instantly instead of in flight,
        // so the outcome diverges and the record is rejected — it cannot be re-settled
        // under a weapon mode it was not played on.
        let mut rec = play_projectiles(1).to_record().unwrap();
        assert_eq!(rec.rules.weapon_mode, WeaponMode::Projectile);
        rec.rules.weapon_mode = WeaponMode::Hitscan;
        assert!(rec.verify().is_err(), "a record re-run under a different weapon mode must not reproduce");
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
            pickups: Vec::new(),
            rules_commit: Rules::default().canonical_encoding(),
            config: config(2),
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
    fn an_outcome_neutral_rules_tamper_breaks_the_hash() {
        // The binding's core win. action_deadline_micros is carried ONLY on the
        // Observation (a live agent's wall-clock answer budget) and never consulted by
        // the headless re-run, so tampering it leaves every outcome bit-identical.
        // Before Rules entered the digest this doctored record VERIFIED — the outcomes
        // reproduced and the hash ignored the tuning; now the hash commits the tuning,
        // so the re-run reproduces the SAME outcomes yet a DIFFERENT hash, caught as a
        // HashMismatch rather than slipped through.
        let mut rec = play(1).to_record().unwrap();
        rec.rules.action_deadline_micros ^= 1;
        match rec.verify() {
            Err(ReplayError::HashMismatch { .. }) => {}
            other => panic!("an outcome-neutral rules tamper must fail as HashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_tampered_stored_rules_commit_is_rejected() {
        // rules_commit is the one replay field verify RECONSTRUCTS from self.rules
        // rather than feeds into the re-run, so a record whose STORED rules_commit is
        // doctored (self.rules + result left honest) re-runs to the same outcomes AND
        // the same reconstructed hash — yet a hash-only consumer digesting the stored
        // replay gets a different hash. verify must reject it too, so the two never
        // disagree on the same record.
        let mut rec = play(1).to_record().unwrap();
        assert!(!rec.replay.rules_commit.is_empty(), "the record carries a rules commitment");
        rec.replay.rules_commit[0] ^= 0xff;
        assert_eq!(rec.verify(), Err(ReplayError::RulesCommitMismatch));
    }

    #[test]
    fn a_tampered_stored_config_is_rejected() {
        // config is the other replay field verify RECONSTRUCTS (the re-run is built
        // from self.config, not self.replay.config), so a doctored STORED replay.config
        // with an honest self.config + result re-runs to the same outcomes and the same
        // reconstructed hash — yet a hash-only consumer digesting the stored replay gets
        // a different hash. verify must reject it too. A determinant tamper (bounds) and
        // a non-determinant one (tick_hz, which the digest does not fold) both make the
        // STORED copy inconsistent with self.config, so both are caught by the exact
        // equality even though only the determinant would move a hash.
        let mut rec = play(1).to_record().unwrap();
        assert_eq!(rec.replay.config, rec.config, "the record's stored config matches its config");
        rec.replay.config.bounds.x ^= 1;
        assert_eq!(rec.verify(), Err(ReplayError::ConfigMismatch), "a doctored stored bound is rejected");
        let mut rec = play(1).to_record().unwrap();
        rec.replay.config.tick_hz ^= 1;
        assert_eq!(rec.verify(), Err(ReplayError::ConfigMismatch), "even a non-folded field must stay consistent");
    }

    #[test]
    fn rules_canonical_encoding_binds_every_field() {
        // Mirror of arena_proto::join_digest_binds_every_field: every sim-affecting
        // Rules field must land in the encoding, or a tampered value for an omitted
        // field slips through the digest unchanged — the exact gap this binding
        // closes. Flip EACH field and assert the bytes move.
        let base = Rules::default();
        assert_eq!(base.canonical_encoding(), base.canonical_encoding(), "encoding is not a pure function");
        // 13×i32 + 1×u32 + 10×u16 + 5×u8 = 81 bytes. A new sim field added to the
        // encoding moves this pin, forcing the field-flip set below to grow with it.
        assert_eq!(base.canonical_encoding().len(), 81, "the encoding width pins the covered field set");

        let cases: Vec<(&str, Rules)> = vec![
            ("max_speed", Rules { max_speed: base.max_speed + 1, ..base }),
            ("weapon_range", Rules { weapon_range: base.weapon_range + 1, ..base }),
            ("hit_radius", Rules { hit_radius: base.hit_radius + 1, ..base }),
            ("weapon_mode", Rules { weapon_mode: WeaponMode::Projectile, ..base }),
            ("projectile_speed", Rules { projectile_speed: base.projectile_speed + 1, ..base }),
            ("aim_mode", Rules { aim_mode: AimMode::Fine, ..base }),
            ("damage", Rules { damage: base.damage + 1, ..base }),
            ("fire_cooldown", Rules { fire_cooldown: base.fire_cooldown + 1, ..base }),
            ("mag_size", Rules { mag_size: base.mag_size + 1, ..base }),
            ("friendly_fire", Rules { friendly_fire: !base.friendly_fire, ..base }),
            ("perception_range", Rules { perception_range: base.perception_range + 1, ..base }),
            ("fov_octant_spread", Rules { fov_octant_spread: base.fov_octant_spread - 1, ..base }),
            ("start_health", Rules { start_health: base.start_health + 1, ..base }),
            ("spawn_radius", Rules { spawn_radius: base.spawn_radius + 1, ..base }),
            ("spawn_jitter", Rules { spawn_jitter: base.spawn_jitter + 1, ..base }),
            ("action_deadline_micros", Rules { action_deadline_micros: base.action_deadline_micros + 1, ..base }),
            ("pickup_radius", Rules { pickup_radius: base.pickup_radius + 1, ..base }),
            ("pickup_respawn_cooldown", Rules { pickup_respawn_cooldown: base.pickup_respawn_cooldown + 1, ..base }),
            ("melee_range", Rules { melee_range: base.melee_range + 1, ..base }),
            ("melee_damage", Rules { melee_damage: base.melee_damage + 1, ..base }),
            ("melee_cooldown", Rules { melee_cooldown: base.melee_cooldown + 1, ..base }),
            ("max_shield", Rules { max_shield: base.max_shield + 1, ..base }),
            ("gravity", Rules { gravity: base.gravity + 1, ..base }),
            ("dash_cooldown", Rules { dash_cooldown: base.dash_cooldown + 1, ..base }),
            ("wall_slide", Rules { wall_slide: !base.wall_slide, ..base }),
            ("perception_memory_ticks", Rules { perception_memory_ticks: base.perception_memory_ticks + 1, ..base }),
            ("vertical_hit_tolerance", Rules { vertical_hit_tolerance: base.vertical_hit_tolerance + 1, ..base }),
            ("knockback_velocity", Rules { knockback_velocity: base.knockback_velocity + 1, ..base }),
            ("knockback_horizontal", Rules { knockback_horizontal: base.knockback_horizontal + 1, ..base }),
        ];
        assert_eq!(cases.len(), 29, "every Rules field needs a flip case");
        for (field, mutated) in &cases {
            assert_ne!(base.canonical_encoding(), mutated.canonical_encoding(), "{field} must bind the encoding");
        }
        // The three WeaponMode bytes are distinct (Hitscan=0/Projectile=1/Melee=2),
        // so the digest tells the weapon apart — the Melee byte is part of the contract.
        let modes = [WeaponMode::Hitscan, WeaponMode::Projectile, WeaponMode::Melee];
        let encs: Vec<_> = modes.iter().map(|&m| Rules { weapon_mode: m, ..base }.canonical_encoding()).collect();
        assert_ne!(encs[0], encs[1]);
        assert_ne!(encs[0], encs[2]);
        assert_ne!(encs[1], encs[2]);
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
        r.replay.blockers[0] = Blocker { min: Vec2 { x: 10, y: 0 }, max: Vec2 { x: 0, y: 0 }, height: 0 };
        assert_eq!(r.verify(), Err(ReplayError::MalformedBlocker { index: 0 }), "inverted on x");

        let mut r = play_with_blockers(1, vec![off_line_blocker()]).to_record().unwrap();
        r.replay.blockers[0] = Blocker { min: Vec2 { x: 0, y: 10 }, max: Vec2 { x: 0, y: 0 }, height: 0 };
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
        let full_height = Blocker { min: Vec2 { x: -1, y: i32::MIN }, max: Vec2 { x: 1, y: i32::MAX }, height: 0 };
        assert!(segment_intersects_aabb(from, to, &full_height), "the extreme diagonal crosses a full-height slab");
        let off = Blocker { min: Vec2 { x: i32::MIN, y: i32::MAX - 1 }, max: Vec2 { x: i32::MIN + 1, y: i32::MAX }, height: 0 };
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

    fn health_pickup(x: i32, y: i32, amount: u16) -> PickupSpawn {
        PickupSpawn { kind: PickupKind::Health, position: Vec2 { x, y }, amount }
    }

    fn ammo_pickup(x: i32, y: i32, amount: u16) -> PickupSpawn {
        PickupSpawn { kind: PickupKind::Ammo, position: Vec2 { x, y }, amount }
    }

    fn shield_pickup(x: i32, y: i32, amount: u16) -> PickupSpawn {
        PickupSpawn { kind: PickupKind::Shield, position: Vec2 { x, y }, amount }
    }

    /// A 2-seat match with a configured pickup set, tight collection radius, seats
    /// not jittered so the geometry is exact.
    fn pickup_match(pickups: Vec<PickupSpawn>, rules: Rules) -> Match {
        Match::new_with_pickups(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), pickups, 1)
    }

    fn pickup_rules() -> Rules {
        Rules { spawn_jitter: 0, pickup_radius: 1000, pickup_respawn_cooldown: 3, ..Default::default() }
    }

    #[test]
    fn a_health_pickup_heals_a_wounded_pawn_then_goes_dormant() {
        let mut m = pickup_match(vec![health_pickup(0, 0, 40)], pickup_rules());
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].health = 50;
        m.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 }; // far, no combat
        // The active pickup is perceivable to the pawn standing on it.
        assert!(
            m.observe(0).visible.iter().any(|e| e.kind == arena_proto::EntityKind::Pickup),
            "an active pickup is perceivable"
        );
        step_with(&mut m, &[]);
        assert_eq!(m.pawns[0].health, 90, "the heal is applied (50 + 40)");
        assert!(
            !m.observe(0).visible.iter().any(|e| e.kind == arena_proto::EntityKind::Pickup),
            "a collected pickup is dormant and not perceivable"
        );
    }

    #[test]
    fn an_ammo_pickup_refills_and_respawns_after_its_cooldown() {
        let rules = Rules { perception_range: 50 * POSITION_SCALE, ..pickup_rules() };
        let mut m = pickup_match(vec![ammo_pickup(0, 0, 8)], rules);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].facing = EAST;
        m.pawns[0].ammo = 0; // emptied, so the refill is observable
        m.pawns[1].pos = Vec2 { x: -45 * POSITION_SCALE, y: 0 };
        step_with(&mut m, &[]);
        assert_eq!(m.pawns[0].ammo, 8, "the ammo pickup refilled an empty magazine");
        // Step off the pad so the respawned pickup is not instantly re-collected.
        m.pawns[0].pos = Vec2 { x: 20 * POSITION_SCALE, y: 0 };
        assert!(!m.observe(0).visible.iter().any(|e| e.kind == arena_proto::EntityKind::Pickup), "dormant");
        for _ in 0..3 {
            step_with(&mut m, &[]);
        }
        assert!(
            m.observe(0).visible.iter().any(|e| e.kind == arena_proto::EntityKind::Pickup),
            "the pickup respawned after its cooldown and is perceivable again"
        );
    }

    #[test]
    fn simultaneous_contact_resolves_to_exactly_one_collector_by_seat() {
        // FM3: two pawns reaching one pickup the same tick — only the lower seat
        // collects (it is consumed before the next pawn is checked); no double-grant.
        let mut m = pickup_match(vec![health_pickup(0, 0, 30)], pickup_rules());
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].health = 50;
        m.pawns[1].pos = Vec2::ZERO;
        m.pawns[1].health = 50;
        step_with(&mut m, &[]);
        assert_eq!(m.pawns[0].health, 80, "the lower seat collects the contested pickup");
        assert_eq!(m.pawns[1].health, 50, "the higher seat gets nothing — already consumed");
    }

    #[test]
    fn collecting_at_the_cap_is_a_no_op_without_overflow() {
        // FM3: a heal at full health clamps to the cap with no overflow — even with an
        // absurd amount that a plain add would wrap (overflow-checks are on in release).
        let mut m = pickup_match(vec![health_pickup(0, 0, u16::MAX)], pickup_rules());
        m.pawns[0].pos = Vec2::ZERO; // full health
        m.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 };
        let max = m.pawns[0].max_health;
        step_with(&mut m, &[]);
        assert_eq!(m.pawns[0].health, max, "a heal at the cap stays at the cap, no overflow");
    }

    #[test]
    fn pickup_perception_obeys_the_same_parity_bound_as_a_pawn() {
        // FM2: an active pickup is perceivable ONLY within range + cone + LOS, carries
        // no hidden state, and an out-of-bound one never leaks its position.
        let near = ammo_pickup(5000, 0, 5); // dead ahead, in range
        let behind = health_pickup(-5000, 0, 5); // in range but behind the facing
        let far = health_pickup(0, 100 * POSITION_SCALE, 5); // out of perception range
        let rules = Rules {
            perception_range: 30 * POSITION_SCALE,
            fov_octant_spread: 1,
            ..pickup_rules()
        };
        let mut m = pickup_match(vec![near, behind, far], rules);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].facing = EAST;
        m.pawns[1].pos = Vec2 { x: 49 * POSITION_SCALE, y: 0 };
        let obs = m.observe(0);
        let seen: Vec<&arena_proto::VisibleEntity> =
            obs.visible.iter().filter(|e| e.kind == arena_proto::EntityKind::Pickup).collect();
        assert_eq!(seen.len(), 1, "only the in-range, in-cone, clear-LOS pickup is perceivable");
        let p = seen[0];
        assert_eq!(p.entity_id, PICKUP_ID_BASE, "the near pickup, by its stable id");
        assert_eq!(p.position, Vec2 { x: 5000, y: 0 }, "its real position");
        assert_eq!(p.team, 0, "a pickup is reported neutral");
        assert_eq!(p.facing, 0, "a pickup carries no facing");
        // Neither the behind nor the far pickup's position leaks anywhere.
        assert!(
            obs.visible.iter().all(|e| e.position != Vec2 { x: -5000, y: 0 } && e.position != Vec2 { x: 0, y: 100 * POSITION_SCALE }),
            "an out-of-cone or out-of-range pickup's position must not leak"
        );
    }

    /// Run a 2-seat Seeker-vs-Seeker match (close, in range at spawn) with a pickup
    /// set, to a terminal record.
    fn play_with_pickups(seed: u64, pickups: Vec<PickupSpawn>) -> Match {
        let rules = Rules { spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        let m = Match::new_with_pickups(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), pickups, seed);
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker)];
        run_match(m, &mut policies)
    }

    #[test]
    fn a_pickup_match_replays_byte_for_byte_and_across_runs() {
        // FM1: the pickup collect/respawn timeline is a pure function of the seed,
        // rules, and recorded actions — a pickup match re-runs from its record ALONE
        // to the same result + digest, and two same-seed runs are byte-identical.
        let pickups = vec![health_pickup(-2000, 0, 30), ammo_pickup(0, 0, 10)];
        let played = play_with_pickups(1, pickups.clone());
        assert_eq!(played.phase(), MatchPhase::Ended);
        let record = played.to_record().unwrap();
        assert!(!record.replay.pickups.is_empty(), "the configured pickups rode into the record");
        assert!(record.verify().is_ok(), "a pickup match re-runs to its own committed result");

        let a = play_with_pickups(7, pickups.clone()).into_replay();
        let b = play_with_pickups(7, pickups).into_replay();
        assert_eq!(a.digest(), b.digest(), "two same-seed pickup runs diverged");
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
    }

    #[test]
    fn the_recorded_pickup_layout_binds_the_committed_hash() {
        // FM1: the item layout is committed (v3), so moving or dropping a pickup —
        // even one no pawn collected — breaks the committed hash, like a blocker.
        let record = play_with_pickups(1, vec![health_pickup(-2000, 0, 30), ammo_pickup(10 * POSITION_SCALE, 0, 5)])
            .to_record()
            .unwrap();
        assert!(record.verify().is_ok());
        let mut moved = record.clone();
        moved.replay.pickups[1].position.x += 1;
        assert!(moved.verify().is_err(), "a moved pickup must break the committed hash");
        let mut dropped = record.clone();
        dropped.replay.pickups.pop();
        assert!(dropped.verify().is_err(), "dropping a pickup must break the committed hash");
    }

    #[test]
    fn verify_rejects_an_over_budget_pickup_list() {
        // FM4: the verifier bounds the pickup count (per-tick collection is
        // O(seats·pickups)); the budget itself is processed, one past it is rejected.
        let record = play_with_pickups(1, vec![health_pickup(-2000, 0, 10)]).to_record().unwrap();
        let filler = ammo_pickup(0, 0, 1);
        let mut over = record.clone();
        over.replay.pickups.resize(MAX_REPLAY_PICKUPS + 1, filler);
        assert_eq!(
            over.verify(),
            Err(ReplayError::TooManyPickups { pickups: MAX_REPLAY_PICKUPS + 1, max: MAX_REPLAY_PICKUPS }),
            "one past the budget is rejected before the re-run"
        );
        let mut at = record.clone();
        at.replay.pickups.resize(MAX_REPLAY_PICKUPS, filler);
        assert!(
            !matches!(at.verify(), Err(ReplayError::TooManyPickups { .. })),
            "the budget itself is processed, not size-rejected — the bound is non-vacuous"
        );
    }

    #[test]
    fn damage_pawn_drains_shield_then_spills_to_health() {
        // FM1: damage drains shield first, then spills the remainder to health; the
        // return is the EFFECTIVE HP removed (shield + health), capped at what the pawn
        // had — the score basis. Exercised directly on the one shared damage path.
        let mut m = new_match(1);
        // Overflow spill: shield 10 vs 25 dmg → 10 absorbed, 15 to health, 25 effective.
        m.pawns[1].shield = 10;
        m.pawns[1].health = 100;
        assert_eq!(m.damage_pawn(1, 25), 25, "effective = shield absorbed + health spill");
        assert_eq!(m.pawns[1].shield, 0, "shield fully drained");
        assert_eq!(m.pawns[1].health, 85, "the 15 overflow spilled to health");
        assert!(m.pawns[1].alive);
        // Under-shield: a hit smaller than the pool costs no health.
        m.pawns[1].shield = 50;
        m.pawns[1].health = 100;
        assert_eq!(m.damage_pawn(1, 25), 25);
        assert_eq!(m.pawns[1].shield, 25, "only the shield drained");
        assert_eq!(m.pawns[1].health, 100, "no health lost under the shield");
        // Exact-deplete: a hit equal to the pool zeroes shield, leaves health intact.
        m.pawns[1].shield = 25;
        m.pawns[1].health = 100;
        assert_eq!(m.damage_pawn(1, 25), 25);
        assert_eq!(m.pawns[1].shield, 0);
        assert_eq!(m.pawns[1].health, 100, "an exactly-depleting hit costs no health");
        // Lethal spill: shield 10 + health 5 vs 25 → downed; overkill (10) not in the
        // effective return, mirroring the prior damage.min(health) clamp.
        m.pawns[1].shield = 10;
        m.pawns[1].health = 5;
        assert_eq!(m.damage_pawn(1, 25), 15, "effective HP removed caps at what the pawn had");
        assert_eq!(m.pawns[1].shield, 0);
        assert_eq!(m.pawns[1].health, 0);
        assert!(!m.pawns[1].alive, "health reached 0 → downed");
    }

    fn knockback_match(gravity: i32, knockback_velocity: i32) -> Match {
        let rules = Rules { gravity, knockback_velocity, ..Default::default() };
        Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1)
    }

    #[test]
    fn knockback_pops_a_grounded_survivor_upward() {
        // FM-gating: a damaging hit on a surviving, grounded pawn adds exactly
        // knockback_velocity to its z_vel — the variable-fall source — and never touches
        // the shooter's z_vel.
        let mut m = knockback_match(60, 800);
        assert_eq!(m.pawns[1].z_vel, 0, "the target starts grounded");
        assert_eq!(m.damage_pawn(1, 25), 25, "the hit dealt damage");
        assert!(m.pawns[1].alive, "the target survived");
        assert_eq!(m.pawns[1].z_vel, 800, "a survivor is popped up by exactly knockback_velocity");
        assert_eq!(m.pawns[0].z_vel, 0, "the shooter never recoils");
    }

    #[test]
    fn knockback_is_off_by_default() {
        // FM1: with the default rules (gravity 0, knockback 0) a hit imparts no impulse —
        // the 2D match is byte-identical, the z stays 0.
        let mut m = new_match(1);
        m.damage_pawn(1, 25);
        assert_eq!(m.pawns[1].z_vel, 0, "no knockback by default");
    }

    #[test]
    fn knockback_requires_gravity_on() {
        // FM-gating: knockback is meaningless without gravity (z would never come back
        // down), so gravity 0 suppresses it even with a knockback velocity set — keeping a
        // gravity-off match byte-identical to 2D.
        let mut m = knockback_match(0, 800);
        m.damage_pawn(1, 25);
        assert_eq!(m.pawns[1].z_vel, 0, "gravity off → no launch even with knockback set");
    }

    #[test]
    fn knockback_requires_a_positive_velocity() {
        // The complement: gravity on but knockback 0 (the off switch) imparts nothing.
        let mut m = knockback_match(60, 0);
        m.damage_pawn(1, 25);
        assert_eq!(m.pawns[1].z_vel, 0, "knockback 0 → no launch even under gravity");
    }

    #[test]
    fn knockback_does_not_launch_a_downed_target() {
        // FM-gating: a hit that DOWNS the pawn must not launch the corpse — gravity
        // integration skips a dead pawn, so a non-zero z_vel on it would be dead churn (and
        // a phantom flying body).
        let mut m = knockback_match(60, 800);
        m.pawns[1].health = 10; // a 25-damage hit is lethal
        assert_eq!(m.damage_pawn(1, 25), 10, "effective caps at the 10 HP it had");
        assert!(!m.pawns[1].alive, "the hit downed it");
        assert_eq!(m.pawns[1].z_vel, 0, "a corpse is never launched");
    }

    #[test]
    fn knockback_imparts_nothing_on_a_zero_damage_hit() {
        // FM-gating: the impulse rides REAL damage — a 0-effective hit (the value a miss
        // would carry, though a miss never reaches the sink) leaves z_vel untouched.
        let mut m = knockback_match(60, 800);
        assert_eq!(m.damage_pawn(1, 0), 0, "no damage dealt");
        assert_eq!(m.pawns[1].z_vel, 0, "no damage → no knockback");
    }

    #[test]
    fn knockback_stacks_onto_an_airborne_target() {
        // FM2: a target already rising (mid-jump) takes the impulse ON TOP of its current
        // z_vel, so a hit launches it higher than a self-jump — the mechanic that makes a
        // landing harder than a jump.
        let mut m = knockback_match(60, 800);
        m.pawns[1].z = 500;
        m.pawns[1].z_vel = 1200; // already ascending from a jump
        m.damage_pawn(1, 25);
        assert_eq!(m.pawns[1].z_vel, 2000, "knockback stacks onto the existing ascent");
    }

    #[test]
    fn knockback_saturates_without_overflow() {
        // FM2: a max knockback added to a max-stacked z_vel must not panic — the impulse
        // is saturating, capping at i32::MAX rather than wrapping to a downward velocity.
        let mut m = knockback_match(60, i32::MAX);
        m.pawns[1].z_vel = i32::MAX;
        m.pawns[1].health = 100; // survives the small hit
        m.damage_pawn(1, 10);
        assert_eq!(m.pawns[1].z_vel, i32::MAX, "saturates at the ceiling, no wrap, no panic");
    }

    #[test]
    fn knockback_launches_a_friendly_fire_target() {
        // FM-gating: knockback follows DAMAGE, not team — a friendly-fire hit deals real
        // damage, so it knocks the ally back too (it scores nothing, but it hurts). Pinned
        // through the shared sink, where the team distinction has already been resolved.
        let mut m = knockback_match(60, 800);
        m.pawns[1].team = m.pawns[0].team; // same team — a friendly hit
        m.damage_pawn(1, 25);
        assert_eq!(m.pawns[1].z_vel, 800, "a friendly-fire hit still imparts knockback");
    }

    #[test]
    fn knockback_through_a_real_fire_launches_then_falls() {
        // End-to-end: a real hitscan shot launches the grounded target, which then rides
        // the gravity arc up and back down — the full author→launch→fall path, not just the
        // sink in isolation. The z integration runs BEFORE the fire each tick, so the
        // impulse lands this tick and integrates into altitude on the next.
        let mut m = close_match(1);
        m.rules.gravity = 60;
        m.rules.knockback_velocity = 800;
        assert_eq!((m.pawns[1].z, m.pawns[1].z_vel), (0, 0), "the target starts grounded");
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]); // seat 0 fires at seat 1
        assert!(m.pawns[1].alive, "one shot does not down a full-health pawn");
        assert_eq!(m.pawns[1].z_vel, 800, "the hit imparted the upward impulse");
        assert_eq!(m.pawns[1].z, 0, "z integrates next tick — gravity ran before the fire this tick");
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, false))]); // ride the arc
        assert!(m.pawns[1].z > 0, "the knocked-back pawn is now airborne");
    }

    #[test]
    fn a_shield_absorbs_a_real_shot_and_can_save_a_pawn() {
        // FM1: end-to-end through a real fire. Control: a low-health pawn with no shield
        // is downed by one shot. With more shield than the shot, the same pawn survives
        // (health untouched) and the shield drains by exactly the damage — so the shield
        // is load-bearing, not vacuous.
        let dmg = Rules::default().damage;
        let mut control = close_match(1);
        control.pawns[1].health = dmg - 5;
        control.pawns[1].shield = 0;
        step_with(&mut control, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert!(!control.observe(1).own.alive, "no shield: one shot downs the low-health pawn");

        let mut m = close_match(1);
        m.pawns[1].health = dmg - 5;
        m.pawns[1].shield = dmg + 10;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        let own = m.observe(1).own;
        assert!(own.alive, "the shield absorbed the otherwise-lethal shot");
        assert_eq!(own.health, dmg - 5, "health untouched under the shield");
        assert_eq!(own.shield, 10, "the shield drained by exactly the shot's damage");
    }

    #[test]
    fn a_shield_pickup_fills_the_pool_capped_at_max_shield() {
        // FM3/FM4: a Shield pickup adds to the pool with the same atomic, saturating,
        // capped collect as health/ammo. One fills the empty pool; collecting past
        // max_shield saturates with no overflow.
        let rules = Rules { max_shield: 100, ..pickup_rules() };
        let mut one = pickup_match(vec![shield_pickup(0, 0, 60)], rules);
        one.pawns[0].pos = Vec2::ZERO;
        one.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 }; // far, no combat
        assert_eq!(one.pawns[0].shield, 0, "a pawn starts with no shield");
        step_with(&mut one, &[]);
        assert_eq!(one.pawns[0].shield, 60, "the shield pickup filled the pool");
        // Two 60-shield pickups on one pad, collected the same tick: the pool saturates
        // at the 100 cap (not 120) — capped, no overflow.
        let mut capped = pickup_match(vec![shield_pickup(0, 0, 60), shield_pickup(0, 0, 60)], rules);
        capped.pawns[0].pos = Vec2::ZERO;
        capped.pawns[1].pos = Vec2 { x: 40 * POSITION_SCALE, y: 0 };
        step_with(&mut capped, &[]);
        assert_eq!(capped.pawns[0].shield, 100, "collecting past max_shield is capped");
    }

    #[test]
    fn friendly_fire_drains_an_allys_shield_first() {
        // FM4: a friendly_fire hit drains shield by the SAME rule (one shared damage
        // path) — the ally's shield absorbs first, health is spared under it, and the
        // shooter is still never credited for a team hit.
        let mut m = ally_match(true, WeaponMode::Hitscan);
        let dmg = Rules::default().damage;
        m.pawns[1].shield = dmg + 5;
        let full = m.pawns[1].health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.pawns[1].shield, 5, "the friendly hit drained the ally's shield");
        assert_eq!(m.pawns[1].health, full, "health spared under the shield");
        assert_eq!(m.pawns[0].score, 0, "a friendly hit is never rewarded");
    }

    #[test]
    fn shield_is_never_perceived_by_an_enemy_or_a_spectator() {
        // FM2: a pawn's shield is private-HUD — own-state only. Give seat 1 a distinctive
        // shield, then prove it shows in seat 1's OWN observation but in NEITHER seat 0's
        // perception of it NOR the public broadcast (the same x-ray bound as ammo).
        let mut m = close_match(1);
        m.pawns[1].shield = 4242;
        assert_eq!(m.observe(1).own.shield, 4242, "a pawn sees its own shield");
        let obs0 = serde_json::to_value(m.observe(0)).unwrap();
        assert!(
            obs0["visible"].as_array().unwrap().iter().any(|e| e["entity_id"] == 1),
            "seat 0 perceives seat 1 in this close match"
        );
        for e in obs0["visible"].as_array().unwrap() {
            assert!(e.get("shield").is_none(), "a perceived enemy must carry no shield field");
        }
        let bc = serde_json::to_value(m.broadcast()).unwrap();
        for e in bc["entities"].as_array().unwrap() {
            assert!(e.get("shield").is_none(), "the broadcast must expose no pawn's shield");
        }
        // The distinctive value appears nowhere a non-owner can read.
        assert!(!obs0.to_string().contains("4242"), "the enemy's shield must not leak into seat 0's view");
        assert!(!bc.to_string().contains("4242"), "the enemy's shield must not leak into the broadcast");
    }

    #[test]
    fn a_max_shield_zero_match_never_materializes_shield() {
        // FM4: with max_shield == 0 (the default) no pawn can hold shield, so damage
        // resolves health-only exactly as before. The full match plays out with every
        // pawn at shield 0 throughout — the runtime half of the default-0 byte-identity
        // guarantee (the parity golden's zero outcome drift is the digest-level half).
        assert_eq!(Rules::default().max_shield, 0, "shield is disabled by default");
        let played = play(1);
        assert_eq!(played.phase(), MatchPhase::Ended);
        assert!(played.pawns.iter().all(|p| p.shield == 0), "no shield materializes when disabled");
    }

    #[test]
    fn a_shield_match_replays_and_a_shield_spawn_binds_the_digest() {
        // FM3: shield is LIVE state (derived from pickups + actions, never recorded), so
        // a shield match re-runs from its record alone bit-for-bit; and a Shield spawn is
        // committed in the digest (its kind byte), so dropping it breaks the hash.
        let rules = Rules { spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, max_shield: 100, ..Default::default() };
        let m = Match::new_with_pickups(
            MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), vec![shield_pickup(0, 0, 50)], 1,
        );
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker)];
        let record = run_match(m, &mut policies).to_record().unwrap();
        assert_eq!(record.replay.pickups[0].kind, PickupKind::Shield, "the shield spawn rode into the record");
        assert!(record.verify().is_ok(), "a shield match re-derives its shield timeline and re-runs to its own hash");
        let mut dropped = record.clone();
        dropped.replay.pickups.pop();
        assert!(dropped.verify().is_err(), "dropping the shield spawn breaks the committed hash");
    }

    #[test]
    fn the_default_no_pickup_match_is_unchanged() {
        // FM5: Match::new delegates to new_with_pickups with none, so a no-pickup match
        // is identical and no pickup ever appears in any observation.
        let a = Match::new(MID.parse().unwrap(), config(2), Rules::default(), two_seats(), Vec::new(), 1).into_replay();
        let b = Match::new_with_pickups(MID.parse().unwrap(), config(2), Rules::default(), two_seats(), Vec::new(), Vec::new(), 1)
            .into_replay();
        assert_eq!(a.digest(), b.digest(), "new == new_with_pickups(empty)");
        assert!(a.pickups.is_empty(), "a default match records no pickups");
        let played = play(1);
        assert!(
            played.observe(0).visible.iter().all(|e| e.kind != arena_proto::EntityKind::Pickup),
            "a default match never surfaces a pickup"
        );
    }

    /// A 2v2 roster: seats {0,1} on team 0, seats {2,3} on team 1 — the shape the
    /// matchmaker's team formation produces, used to exercise the team win condition
    /// and team placement directly.
    fn four_seats_two_teams() -> Vec<SeatInfo> {
        vec![
            SeatInfo { seat: 0, team: 0, controller: "0xa".into() },
            SeatInfo { seat: 1, team: 0, controller: "0xb".into() },
            SeatInfo { seat: 2, team: 1, controller: "0xc".into() },
            SeatInfo { seat: 3, team: 1, controller: "0xd".into() },
        ]
    }

    #[test]
    fn team_placement_groups_teammates_and_ranks_teams() {
        // FM4: placement is by TEAM — teammates SHARE a placement (never contend as
        // rivals), even with different individual scores and even when one is dead.
        // Both teams have a survivor (a timeout), so teams rank by total score:
        // team 0 (10) over team 1 (6). The OLD per-seat rule would rank these four
        // seats 1/4/2/3 by (alive, score, seat); by team they are 1/1/2/2.
        let mut m = Match::new(
            MID.parse().unwrap(),
            config(4),
            Rules::default(),
            four_seats_two_teams(),
            Vec::new(),
            1,
        );
        for (seat, alive, score) in [(0, true, 10), (1, false, 0), (2, true, 4), (3, false, 2)] {
            m.pawns[seat].alive = alive;
            m.pawns[seat].score = score;
        }
        let placements: Vec<u16> = m.outcomes().iter().map(|o| o.placement).collect();
        assert_eq!(placements, vec![1, 1, 2, 2], "teammates share one team placement");
    }

    #[test]
    fn two_whole_teams_tied_on_score_both_take_first() {
        // FM2 (the draw clause): two whole teams time out with one survivor each and
        // an equal TOTAL score, so neither team out-ranks the other — both share
        // first place (a draw the settlement layer reads as `settleDraw`). Team
        // totals are equal (8 == 8) though no individual seat's score matches, so a
        // per-seat rule would split them; by team they tie.
        let mut m = Match::new(
            MID.parse().unwrap(),
            config(4),
            Rules::default(),
            four_seats_two_teams(),
            Vec::new(),
            1,
        );
        for (seat, alive, score) in [(0, true, 5), (1, false, 3), (2, true, 6), (3, false, 2)] {
            m.pawns[seat].alive = alive;
            m.pawns[seat].score = score;
        }
        let placements: Vec<u16> = m.outcomes().iter().map(|o| o.placement).collect();
        assert_eq!(placements, vec![1, 1, 1, 1], "tied teams share first place");
    }

    #[test]
    fn a_2v2_ends_only_when_a_whole_team_is_down() {
        // FM2: the win condition keys on alive TEAMS, not alive players. Downing one
        // of team 1 leaves both teams represented, so the match stays Live with three
        // of four players up; only when team 1's LAST pawn falls does it end — and
        // the whole surviving team shares first place.
        let mut m = Match::new(
            MID.parse().unwrap(),
            config(4),
            Rules::default(),
            four_seats_two_teams(),
            Vec::new(),
            1,
        );
        assert_eq!(m.phase(), MatchPhase::Live);
        m.pawns[2].alive = false;
        m.pawns[2].health = 0;
        m.step(&BTreeMap::new());
        assert_eq!(m.phase(), MatchPhase::Live, "team 1 still has a survivor");
        m.pawns[3].alive = false;
        m.pawns[3].health = 0;
        m.step(&BTreeMap::new());
        assert_eq!(m.phase(), MatchPhase::Ended, "team 1 wiped → the match is over");
        let firsts: Vec<SeatId> = m
            .result()
            .expect("ended")
            .outcomes
            .iter()
            .filter(|o| o.placement == 1)
            .map(|o| o.seat)
            .collect();
        assert_eq!(firsts, vec![0, 1], "the whole surviving team shares first place");
    }

    #[test]
    fn team_placement_reduces_to_per_seat_for_singletons() {
        // FM3: with each seat its own team (FFA, the default), team placement IS the
        // per-seat alive>score>seat rule, so FFA outcomes stay byte-identical. seat 1
        // (alive, top score) first, seat 0 (alive) second, seat 2 (dead) third.
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "0xa".into() },
            SeatInfo { seat: 1, team: 1, controller: "0xb".into() },
            SeatInfo { seat: 2, team: 2, controller: "0xc".into() },
        ];
        let mut m = Match::new(MID.parse().unwrap(), config(3), Rules::default(), seats, Vec::new(), 1);
        for (seat, alive, score) in [(0, true, 5), (1, true, 9), (2, false, 3)] {
            m.pawns[seat].alive = alive;
            m.pawns[seat].score = score;
        }
        let placements: Vec<u16> = m.outcomes().iter().map(|o| o.placement).collect();
        assert_eq!(placements, vec![2, 1, 3], "singleton teams rank exactly per-seat");
    }

    /// A 3-seat match for the friendly-fire tests: seats 0 and 1 are ALLIES
    /// (team 0); seat 2 is a lone enemy (team 1) parked far to the WEST so the match
    /// stays Live (≥2 teams) without ever entering seat 0's eastward line of fire.
    /// Seat 0 sits at the origin facing EAST, with ally seat 1 four metres dead
    /// ahead. Positions are set by hand for an exact, replay-free assertion.
    fn ally_match(friendly_fire: bool, weapon_mode: WeaponMode) -> Match {
        let rules = Rules {
            friendly_fire,
            weapon_mode,
            spawn_radius: 2 * POSITION_SCALE,
            spawn_jitter: 0,
            ..Default::default()
        };
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "0xaa".into() },
            SeatInfo { seat: 1, team: 0, controller: "0xbb".into() },
            SeatInfo { seat: 2, team: 1, controller: "0xcc".into() },
        ];
        let mut m = Match::new(MID.parse().unwrap(), config(3), rules, seats, Vec::new(), 1);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].facing = EAST;
        m.pawns[1].pos = Vec2 { x: 4 * POSITION_SCALE, y: 0 };
        m.pawns[2].pos = Vec2 { x: -40 * POSITION_SCALE, y: 0 };
        m
    }

    #[test]
    fn friendly_fire_off_a_beam_spares_an_ally() {
        // FM1: the default (off) hardcodes the pre-flag rule — a hitscan beam never
        // touches a same-team pawn, so the ally is unharmed and the shooter unscored.
        let mut m = ally_match(false, WeaponMode::Hitscan);
        let full = m.pawns[1].health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.pawns[1].health, full, "an ally takes no beam damage with FF off");
        assert_eq!(m.pawns[0].score, 0, "no hit, no score");
    }

    #[test]
    fn friendly_fire_off_a_projectile_spares_an_ally() {
        // FM1: same default on the projectile path — the shot flies through the ally.
        let mut m = ally_match(false, WeaponMode::Projectile);
        let full = m.pawns[1].health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        for _ in 0..5 {
            step_with(&mut m, &[]);
        }
        assert_eq!(m.pawns[1].health, full, "an ally takes no projectile damage with FF off");
        assert_eq!(m.pawns[0].score, 0, "no hit, no score");
    }

    #[test]
    fn friendly_fire_on_a_beam_damages_an_ally_without_scoring() {
        // FM2 + FM3: with the flag on, a beam hits the nearest body even if it is an
        // ally — but a team hit deals damage WITHOUT crediting the shooter, and the
        // shooter is never in its own beam.
        let mut m = ally_match(true, WeaponMode::Hitscan);
        let full = m.pawns[1].health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.pawns[1].health, full - Rules::default().damage, "the ally takes the hit");
        assert_eq!(m.pawns[0].score, 0, "a friendly hit is never rewarded");
        assert_eq!(m.pawns[0].health, m.pawns[0].max_health, "the shooter never hits itself");
    }

    #[test]
    fn friendly_fire_on_a_projectile_damages_an_ally_without_scoring() {
        // FM2 + FM3: the projectile path honors the flag identically — and the
        // shooter-self skip (t.seat == proj.shooter) keeps the shot's own firer safe
        // as it launches from the shooter's position.
        let mut m = ally_match(true, WeaponMode::Projectile);
        let full = m.pawns[1].health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        for _ in 0..5 {
            step_with(&mut m, &[]);
        }
        assert_eq!(m.pawns[1].health, full - Rules::default().damage, "the ally takes the shot");
        assert_eq!(m.pawns[0].score, 0, "a friendly hit is never rewarded");
        assert_eq!(m.pawns[0].health, m.pawns[0].max_health, "the shot never hits its own firer");
    }

    #[test]
    fn friendly_fire_on_stays_deterministic_and_verifies() {
        // FM3: friendly fire is a pure conditional skip over the existing integer,
        // seat-ordered combat — a same-seed FF-on match is byte-identical across runs
        // and re-runs from its record to the same result. Seed-derived spawns (NOT
        // hand-set, so the record's re-run reproduces them) put seat 0 a short hop
        // west of its ally; firing along its spawn facing lands a real friendly hit.
        let play = || {
            let cfg = MatchConfig { max_ticks: 30, ..config(3) };
            let rules = Rules {
                friendly_fire: true,
                spawn_radius: 2 * POSITION_SCALE,
                spawn_jitter: 0,
                ..Default::default()
            };
            let seats = vec![
                SeatInfo { seat: 0, team: 0, controller: "0xaa".into() },
                SeatInfo { seat: 1, team: 0, controller: "0xbb".into() },
                SeatInfo { seat: 2, team: 1, controller: "0xcc".into() },
            ];
            let mut m = Match::new(MID.parse().unwrap(), cfg, rules, seats, Vec::new(), 9);
            let aim = m.observe(0).own.facing; // toward the arena centre, where the ally sits
            while m.phase() == MatchPhase::Live {
                step_with(&mut m, &[(0, intent(Vec2::ZERO, aim, true))]);
            }
            m
        };
        let a = play();
        let b = play();
        assert_eq!(a.phase(), MatchPhase::Ended);
        let ally_down = a.observe(1).own.health < Rules::default().start_health;
        assert!(ally_down, "the scenario must actually exercise a friendly hit");
        let ra = a.to_record().unwrap();
        assert!(ra.verify().is_ok(), "an FF-on match re-runs to its committed result");
        let rb = b.to_record().unwrap();
        assert_eq!(ra.replay.digest(), rb.replay.digest(), "two FF-on runs diverged");
        assert_eq!(serde_json::to_string(&ra).unwrap(), serde_json::to_string(&rb).unwrap());
    }

    #[test]
    fn friendly_fire_does_not_alter_perception() {
        // FM4: friendly_fire is a damage rule, not a perception rule — observe() reads
        // it nowhere, so flipping it leaves every seat's observation byte-identical.
        let off = ally_match(false, WeaponMode::Hitscan);
        let on = ally_match(true, WeaponMode::Hitscan);
        for seat in 0..3u8 {
            assert_eq!(off.observe(seat), on.observe(seat), "the flag must not touch perception");
        }
    }

    #[test]
    fn arena_map_default_and_unknown_keys_resolve_empty() {
        // FM4: the empty/default key AND any unrecognised key degrade safe to the
        // empty arena — no geometry, no panic, no guessing.
        assert_eq!(arena_map(""), ArenaMap::empty());
        assert_eq!(arena_map("does-not-exist"), ArenaMap::empty());
        let empty = arena_map("");
        assert!(empty.blockers.is_empty() && empty.pickups.is_empty(), "default is no geometry");
    }

    #[test]
    fn arena_map_reference_is_non_empty_and_deterministic() {
        // FM4: the reference arena carries real geometry, and arena_map is a pure
        // function — the same key always yields the identical map.
        let a = arena_map("reference");
        assert!(!a.blockers.is_empty() && !a.pickups.is_empty(), "the reference arena has geometry");
        assert_eq!(a, arena_map("reference"), "arena_map must be deterministic for a key");
    }

    #[test]
    fn reference_arena_loads_byte_identical_from_embedded_json() {
        // FM3 (hard equality): dogfooding reference_arena() through include_str! must
        // reproduce the EXACT prior hardcoded geometry (POSITION_SCALE-resolved ints),
        // or arena_map("reference") shifts value and the matchmaker + any record drift.
        let m = POSITION_SCALE;
        let expected = ArenaMap {
            blockers: vec![Blocker { min: Vec2 { x: -3 * m, y: -3 * m }, max: Vec2 { x: 3 * m, y: 3 * m }, height: 0 }],
            pickups: vec![
                PickupSpawn { kind: PickupKind::Health, position: Vec2 { x: -20 * m, y: 0 }, amount: 50 },
                PickupSpawn { kind: PickupKind::Health, position: Vec2 { x: 20 * m, y: 0 }, amount: 50 },
            ],
        };
        assert_eq!(arena_map("reference"), expected, "the embedded JSON reference arena matches the prior hardcoded value byte-for-byte");
    }

    #[test]
    fn a_reference_arena_match_round_trips_from_its_record() {
        // FM2 + FM3: a match built from a named arena's map carries that EXACT
        // geometry into its record and re-runs from the record ALONE to its
        // committed result — the formation->replay parity the matchmaker relies on.
        let map = arena_map("reference");
        let rules = Rules { spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        let m = Match::new_with_pickups(
            MID.parse().unwrap(),
            config(2),
            rules,
            two_seats(),
            map.blockers.clone(),
            map.pickups.clone(),
            1,
        );
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker)];
        let played = run_match(m, &mut policies);
        assert_eq!(played.phase(), MatchPhase::Ended);
        let record = played.to_record().unwrap();
        assert_eq!(record.replay.blockers, map.blockers, "the arena's blockers rode into the record");
        assert_eq!(record.replay.pickups, map.pickups, "the arena's pickups rode into the record");
        assert!(record.verify().is_ok(), "a reference-arena match re-runs to its committed result");
    }

    #[test]
    fn arena_map_json_round_trips() {
        // Serialize -> from_json reproduces the map exactly: the data-driven format
        // is the map's own serde form, so an authored map and a loaded one agree.
        let map = ArenaMap {
            blockers: vec![Blocker { min: Vec2 { x: -3000, y: -3000 }, max: Vec2 { x: 3000, y: 3000 }, height: 0 }],
            pickups: vec![health_pickup(-20000, 0, 50), ammo_pickup(20000, 0, 30)],
        };
        let json = serde_json::to_string(&map).unwrap();
        assert_eq!(ArenaMap::from_json(&json).unwrap(), map, "the map round-trips through JSON");
    }

    #[test]
    fn from_json_loads_a_partial_or_empty_map() {
        // Each field is serde(default): a blockers-only, pickups-only, or fully empty
        // map all load (the absent array is empty) — authoring one half is valid.
        assert_eq!(ArenaMap::from_json("{}").unwrap(), ArenaMap::empty(), "an empty object is the empty arena");
        let blockers_only = ArenaMap::from_json(r#"{"blockers":[{"min":{"x":-1,"y":-1},"max":{"x":1,"y":1}}]}"#).unwrap();
        assert_eq!(blockers_only.blockers.len(), 1);
        assert!(blockers_only.pickups.is_empty(), "an omitted pickups array defaults to empty");
        let pickups_only = ArenaMap::from_json(r#"{"pickups":[{"kind":"health","position":{"x":0,"y":0},"amount":10}]}"#).unwrap();
        assert!(pickups_only.blockers.is_empty(), "an omitted blockers array defaults to empty");
        assert_eq!(pickups_only.pickups.len(), 1);
    }

    #[test]
    fn from_json_rejects_an_unknown_field_and_malformed_json() {
        // deny_unknown_fields: a typo'd key (`blocker` for `blockers`) fails loudly
        // instead of silently parsing to an empty arena (which would ship broken).
        assert!(matches!(ArenaMap::from_json(r#"{"blocker":[]}"#), Err(ArenaMapError::Parse(_))), "an unknown field is rejected");
        assert!(matches!(ArenaMap::from_json("{ not json"), Err(ArenaMapError::Parse(_))), "syntactically invalid JSON is rejected");
        assert!(matches!(ArenaMap::from_json(r#"{"blockers":42}"#), Err(ArenaMapError::Parse(_))), "a wrong field type is rejected");
    }

    #[test]
    fn from_json_rejects_a_degenerate_blocker_but_allows_a_zero_area_point() {
        // An inverted AABB (min > max on an axis) breaks the SAT LOS test + the
        // movement clamp; reject it. A zero-AREA blocker (min == max) is a harmless
        // point and is allowed.
        for bad in [
            ArenaMap { blockers: vec![Blocker { min: Vec2 { x: 10, y: 0 }, max: Vec2 { x: 0, y: 0 }, height: 0 }], pickups: vec![] },
            ArenaMap { blockers: vec![Blocker { min: Vec2 { x: 0, y: 10 }, max: Vec2 { x: 0, y: 0 }, height: 0 }], pickups: vec![] },
        ] {
            let json = serde_json::to_string(&bad).unwrap();
            assert_eq!(ArenaMap::from_json(&json), Err(ArenaMapError::DegenerateBlocker { index: 0 }), "an inverted AABB is rejected");
        }
        let point = ArenaMap { blockers: vec![Blocker { min: Vec2 { x: 5, y: 5 }, max: Vec2 { x: 5, y: 5 }, height: 0 }], pickups: vec![] };
        let json = serde_json::to_string(&point).unwrap();
        assert!(ArenaMap::from_json(&json).is_ok(), "a zero-area point blocker (min == max) is allowed");
    }

    #[test]
    fn from_json_rejects_a_zero_amount_pickup() {
        // A pickup that grants nothing (amount == 0) is a no-op item — malformed
        // authoring, rejected with its index.
        let bad = ArenaMap { blockers: vec![], pickups: vec![health_pickup(0, 0, 50), ammo_pickup(0, 0, 0)] };
        let json = serde_json::to_string(&bad).unwrap();
        assert_eq!(ArenaMap::from_json(&json), Err(ArenaMapError::EmptyPickup { index: 1 }), "a zero-amount pickup is rejected at its index");
    }

    #[test]
    fn from_json_enforces_the_verify_caps_so_a_loadable_map_always_verifies() {
        // The loader caps at exactly the bounds MatchRecord::verify enforces, so a map
        // the loader accepts can never be rejected downstream. At the cap: ok; over: rejected.
        let blocker = Blocker { min: Vec2 { x: 0, y: 0 }, max: Vec2 { x: 1, y: 1 }, height: 0 };
        let at_cap = ArenaMap { blockers: vec![blocker; MAX_REPLAY_BLOCKERS], pickups: vec![] };
        assert!(ArenaMap::from_json(&serde_json::to_string(&at_cap).unwrap()).is_ok(), "exactly MAX_REPLAY_BLOCKERS loads");
        let over = ArenaMap { blockers: vec![blocker; MAX_REPLAY_BLOCKERS + 1], pickups: vec![] };
        assert_eq!(
            ArenaMap::from_json(&serde_json::to_string(&over).unwrap()),
            Err(ArenaMapError::TooManyBlockers { count: MAX_REPLAY_BLOCKERS + 1, max: MAX_REPLAY_BLOCKERS }),
            "one blocker over the verify cap is rejected"
        );
        let pk = health_pickup(0, 0, 1);
        let pk_over = ArenaMap { blockers: vec![], pickups: vec![pk; MAX_REPLAY_PICKUPS + 1] };
        assert_eq!(
            ArenaMap::from_json(&serde_json::to_string(&pk_over).unwrap()),
            Err(ArenaMapError::TooManyPickups { count: MAX_REPLAY_PICKUPS + 1, max: MAX_REPLAY_PICKUPS }),
            "one pickup over the verify cap is rejected"
        );
    }

    #[test]
    fn arena_map_error_display_covers_every_variant() {
        let cases = [
            ArenaMapError::Parse("boom".into()),
            ArenaMapError::TooManyBlockers { count: 2000, max: MAX_REPLAY_BLOCKERS },
            ArenaMapError::TooManyPickups { count: 500, max: MAX_REPLAY_PICKUPS },
            ArenaMapError::DegenerateBlocker { index: 3 },
            ArenaMapError::EmptyPickup { index: 4 },
        ];
        for e in cases {
            assert!(!e.to_string().is_empty(), "every ArenaMapError renders a message");
        }
    }

    /// A 2-seat match in melee mode, no jitter, seats placed by the caller.
    fn melee_match(seats: u8) -> Match {
        let rules = Rules { weapon_mode: WeaponMode::Melee, spawn_jitter: 0, ..Default::default() };
        let roster: Vec<SeatInfo> = (0..seats)
            .map(|s| SeatInfo { seat: s, team: s as u16, controller: format!("0x{s:02x}") })
            .collect();
        Match::new(MID.parse().unwrap(), config(seats), rules, roster, Vec::new(), 1)
    }

    #[test]
    fn melee_strikes_an_enemy_in_range_and_arc() {
        // A swing damages an enemy within melee_range and the frontal arc, by
        // melee_damage (not the ranged `damage`), and the shooter scores it.
        let mut m = melee_match(2);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].facing = EAST;
        m.pawns[1].pos = Vec2 { x: POSITION_SCALE, y: 0 }; // 1 m dead ahead, inside the 2 m reach
        let hp = m.pawns[1].health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.pawns[1].health, hp - m.rules.melee_damage, "the in-range, in-arc enemy takes melee_damage");
        assert_eq!(m.pawns[0].score, m.rules.melee_damage as i32, "the shooter scores the melee hit");
    }

    #[test]
    fn melee_misses_out_of_range_and_behind_the_arc() {
        // Out of reach OR outside the frontal arc → no hit (the close-quarters arc is
        // not a 360 ring and not infinite reach).
        let mut far = melee_match(2);
        far.pawns[0].pos = Vec2::ZERO;
        far.pawns[0].facing = EAST;
        far.pawns[1].pos = Vec2 { x: 3 * POSITION_SCALE, y: 0 }; // 3 m > 2 m reach
        let hp = far.pawns[1].health;
        step_with(&mut far, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(far.pawns[1].health, hp, "an enemy beyond melee_range is not struck");

        let mut behind = melee_match(2);
        behind.pawns[0].pos = Vec2::ZERO;
        behind.pawns[0].facing = EAST;
        behind.pawns[1].pos = Vec2 { x: -POSITION_SCALE, y: 0 }; // in reach but directly behind
        let hp = behind.pawns[1].health;
        step_with(&mut behind, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(behind.pawns[1].health, hp, "an enemy behind the facing arc is not struck");
    }

    #[test]
    fn melee_cleaves_every_enemy_in_the_arc_deterministically() {
        // Unlike the nearest-only beam, one swing strikes EVERY enemy in range + arc;
        // seat-ordered + atomic so the same-tick multi-hit is reproducible.
        let strike = || {
            let mut m = melee_match(3);
            m.pawns[0].pos = Vec2::ZERO;
            m.pawns[0].facing = EAST;
            m.pawns[1].pos = Vec2 { x: POSITION_SCALE, y: 100 }; // both ~1 m ahead, in arc
            m.pawns[2].pos = Vec2 { x: POSITION_SCALE, y: -100 };
            step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
            (m.pawns[1].health, m.pawns[2].health, m.pawns[0].score)
        };
        let dmg = Rules { weapon_mode: WeaponMode::Melee, ..Default::default() }.melee_damage;
        let (h1, h2, score) = strike();
        assert_eq!(h1, 100 - dmg, "the first enemy in the arc is cleaved");
        assert_eq!(h2, 100 - dmg, "the second enemy in the arc is cleaved by the same swing");
        assert_eq!(score, 2 * dmg as i32, "the shooter scores both cleaved enemies");
        assert_eq!(strike(), (h1, h2, score), "the cleave is deterministic across identical runs");
    }

    #[test]
    fn melee_needs_no_ammo() {
        // Melee is the always-available fallback: an empty magazine still swings, and
        // ammo is neither required nor decremented (no underflow).
        let mut m = melee_match(2);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].facing = EAST;
        m.pawns[0].ammo = 0;
        m.pawns[1].pos = Vec2 { x: POSITION_SCALE, y: 0 };
        let hp = m.pawns[1].health;
        step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
        assert_eq!(m.pawns[1].health, hp - m.rules.melee_damage, "a melee swing lands with an empty magazine");
        assert_eq!(m.pawns[0].ammo, 0, "melee consumes no ammo (and does not underflow)");
    }

    #[test]
    fn melee_cooldown_gates_repeat_swings() {
        // The first swing connects and arms melee_cooldown; an immediate second swing
        // is refused until the cooldown elapses.
        let mut m = melee_match(2);
        m.pawns[0].pos = Vec2::ZERO;
        m.pawns[0].facing = EAST;
        m.pawns[1].pos = Vec2 { x: POSITION_SCALE, y: 0 };
        m.pawns[1].health = m.rules.melee_damage + 10; // survives exactly one swing
        let fire = (0u8, intent(Vec2::ZERO, EAST, true));
        step_with(&mut m, &[fire]);
        assert_eq!(m.pawns[1].health, 10, "the first swing lands");
        step_with(&mut m, &[fire]);
        assert_eq!(m.pawns[1].health, 10, "a second swing within melee_cooldown is refused");
    }

    #[test]
    fn a_hitscan_match_outcome_ignores_the_melee_fields() {
        // The melee fields are read ONLY in melee mode: a Hitscan match's OUTCOME is
        // identical whatever they are set to (only the replay_hash, which binds them,
        // differs — the hard cutover, pinned by the regenerated parity golden).
        let baseline_match = play(1);
        let baseline = baseline_match.result().unwrap();
        let rules = Rules {
            spawn_radius: 2 * POSITION_SCALE,
            spawn_jitter: 0,
            melee_range: 99 * POSITION_SCALE,
            melee_damage: u16::MAX,
            melee_cooldown: 1,
            ..Default::default()
        };
        let m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker)];
        let melee_match = run_match(m, &mut policies);
        let melee_fields = melee_match.result().unwrap();
        assert_eq!(baseline.outcomes, melee_fields.outcomes, "extreme melee fields do not change a Hitscan outcome");
        assert_eq!(baseline.final_tick, melee_fields.final_tick, "nor the final tick");
    }

    #[test]
    fn a_melee_match_self_verifies_and_a_flipped_mode_does_not() {
        // A melee match seals into a self-verifying record; flipping a HITSCAN record's
        // weapon_mode to Melee fails verify (the tuning is digest-bound — the binds-rules
        // guard catches the stored rules_commit no longer matching the rules).
        let rules = Rules { weapon_mode: WeaponMode::Melee, spawn_radius: 2 * POSITION_SCALE, spawn_jitter: 0, ..Default::default() };
        let m = Match::new(MID.parse().unwrap(), config(2), rules, two_seats(), Vec::new(), 1);
        let mut policies: Vec<Box<dyn Policy>> = vec![Box::new(Seeker), Box::new(Seeker)];
        let played = run_match(m, &mut policies);
        assert_eq!(played.phase(), MatchPhase::Ended, "a melee Seeker duel reaches a terminal state");
        let melee_record = played.to_record().unwrap();
        assert!(melee_record.verify().is_ok(), "a melee match re-runs to its own committed result");

        let mut flipped = play(1).to_record().unwrap();
        flipped.rules.weapon_mode = WeaponMode::Melee;
        assert!(flipped.verify().is_err(), "a hitscan record reverified as melee must not verify");
    }

    #[test]
    fn melee_respects_friendly_fire() {
        // An ally in range + arc is spared with friendly_fire off, and struck (but never
        // scored) with it on — the same rule resolve_fire applies.
        let ally_swing = |ff: bool| {
            let rules = Rules { weapon_mode: WeaponMode::Melee, friendly_fire: ff, spawn_jitter: 0, ..Default::default() };
            let roster = vec![
                SeatInfo { seat: 0, team: 0, controller: "0x00".into() },
                SeatInfo { seat: 1, team: 0, controller: "0x01".into() }, // same team
            ];
            let mut m = Match::new(MID.parse().unwrap(), config(2), rules, roster, Vec::new(), 1);
            m.pawns[0].pos = Vec2::ZERO;
            m.pawns[0].facing = EAST;
            m.pawns[1].pos = Vec2 { x: POSITION_SCALE, y: 0 };
            step_with(&mut m, &[(0, intent(Vec2::ZERO, EAST, true))]);
            (m.pawns[1].health, m.pawns[0].score)
        };
        let dmg = Rules { weapon_mode: WeaponMode::Melee, ..Default::default() }.melee_damage;
        assert_eq!(ally_swing(false), (100, 0), "with friendly_fire off, an ally is not struck");
        assert_eq!(ally_swing(true), (100 - dmg, 0), "with friendly_fire on, an ally is struck but the hit never scores");
    }
}
