//! Agent Gateway wire protocol — the single contract a player-as-pawn and an
//! agent-as-player share.
//!
//! Blackfield is one combat core playable by humans and autonomous agents on
//! equal footing. Humans drive a pawn through a `PlayerController`; agents drive
//! an identical pawn through an `AgentController` bridged over this protocol. The
//! simulation cannot tell the two apart and does not privilege either — so the
//! types here are the security boundary, not a convenience. Two implementations
//! conform to this one contract: the headless Rust reference arena (this
//! workspace) and, later, the UE5 Lyra dedicated server, so an agent written
//! against the reference plays the shipped game unchanged.
//!
//! Three invariants are encoded in the types, not left to a sender's goodwill:
//!
//! - **Parity-bounded observation.** An [`Observation`] carries the seat's own
//!   state in full plus only the entities it could perceive that tick. There is
//!   no field for full world state, so an omniscient agent cannot be built on
//!   this protocol even by a buggy or malicious server.
//! - **Explicit versioning.** The handshake and every per-tick envelope carry
//!   [`PROTOCOL_VERSION`], and the version is fixed at the handshake — a mismatch
//!   is rejected before a match starts — so the Rust arena and the UE5 server
//!   cannot silently diverge as they evolve.
//! - **Canonical, deterministic encoding.** All spatial quantities are integer
//!   fixed-point (no floats), so the same match produces byte-identical replay
//!   bytes and the same hash on any platform — the basis for on-chain
//!   attestation and reproducible grading.
//!
//! Like `mesh/proto`, this is plain serde JSON: the wire form is the serde
//! representation and transport lives in the arena/UE5 crates.

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use uuid::Uuid;

/// The Gateway protocol version. Bumped on any wire-incompatible change to the
/// envelopes, observation, action, or replay record. Both peers send it in the
/// handshake ([`AgentMsg::Join`] / [`GatewayMsg::Welcome`]) and a mismatch is
/// rejected before a match starts, so an agent that conforms to one
/// implementation can never silently desync against another.
pub const PROTOCOL_VERSION: u32 = 1;

/// Fixed-point scale for positions and lengths: wire units per metre. A position
/// of `1500` is 1.5 m. Integer fixed-point (not floats) keeps the wire form
/// canonical and byte-stable across platforms — float formatting and float math
/// both vary by target, which would break replay hashing and on-chain
/// attestation.
pub const POSITION_SCALE: i32 = 1000;

/// Fixed-point scale for velocities: wire units per metre per tick. Same
/// rationale as [`POSITION_SCALE`].
pub const VELOCITY_SCALE: i32 = 1000;

/// A binary angle measure (BAM): the full turn is `0..=u16::MAX`, so `0` is
/// 0 rad, `0x4000` is π/2, `0x8000` is π. Angles wrap by construction (`u16`
/// overflow == 2π wraparound) and carry no float, so aim and facing are exact
/// and replay-stable. Convert with [`bam_to_radians`] for display only — never
/// on the wire.
pub type Bam = u16;

/// Convert a [`Bam`] to radians. Display/debug helper only; the wire form is
/// always the integer BAM so determinism is never at the mercy of float
/// formatting.
pub fn bam_to_radians(bam: Bam) -> f64 {
    (bam as f64) * (std::f64::consts::TAU / 65536.0)
}

/// A team/faction id. `0` is the neutral/unaffiliated team. Two seats on the
/// same team are allies (co-op or team PvP); the matchmaking mode decides who
/// fills the seats, not the protocol.
pub type TeamId = u16;

/// A 2D integer fixed-point point on the arena plane (see [`POSITION_SCALE`]).
/// The combat core is plane-plus-height; the vertical axis rides on the entity
/// records that need it, keeping the common case two integers wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: i32,
    pub y: i32,
}

impl Vec2 {
    pub const ZERO: Vec2 = Vec2 { x: 0, y: 0 };
}

/// Identifies a single match. Server-minted (v4) so a peer can neither forge a
/// match id nor collide with a live one.
pub type MatchId = Uuid;

/// A seat in a match — the slot one controller (human or agent) fills. Arenas
/// seat a bounded roster, so `u8` is ample and keeps the wire form compact; the
/// matchmaking mode, not the protocol, decides whether a seat is filled by a
/// human or an agent.
pub type SeatId = u8;

/// The server-authoritative match lifecycle. A match advances strictly
/// `Lobby → Starting → Live → Ended` and never moves backward; an agent reads
/// the current phase off every [`Observation`] and the terminal transition is
/// carried by [`GatewayMsg::End`]. Actions are simulated only in [`Live`].
///
/// [`Live`]: MatchPhase::Live
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchPhase {
    /// Seats are filling; no simulation yet.
    Lobby,
    /// Roster locked, spawn/countdown running; actions are not yet simulated.
    Starting,
    /// The match is live; observations stream and actions are simulated.
    Live,
    /// The match is over; no further actions are accepted.
    Ended,
}

/// How a match's seats are filled — the matchmaking dimension, not a gameplay
/// fork. One combat core serves all three: `Human` is human-only (clean ranked
/// PvP), `Agent` is agent-only (ranked A2A), and `Mixed` puts both kinds on the
/// same battlefield. The mode decides *who* fills the seats and what composition
/// is valid; the simulation treats every seat identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Every seat is human-controlled.
    Human,
    /// Every seat is agent-controlled (ranked A2A).
    Agent,
    /// At least one human and at least one agent share the match.
    Mixed,
}

/// What kind of controller fills a seat. A human drives a pawn through a
/// `PlayerController`, an agent through an `AgentController` over the Gateway; the
/// core cannot tell them apart, so this is a matchmaking/identity label only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerKind {
    Human,
    Agent,
}

/// The peer announced a [`PROTOCOL_VERSION`] this build cannot speak. Returned by
/// [`check_version`] and surfaced as a handshake rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionMismatch {
    pub ours: u32,
    pub theirs: u32,
}

impl std::fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "gateway protocol version mismatch: ours={}, theirs={}",
            self.ours, self.theirs
        )
    }
}

impl std::error::Error for VersionMismatch {}

/// Reject a handshake whose announced version is not exactly ours. The Gateway
/// is a moving contract with two independent implementations (the Rust arena and
/// the UE5 server) that evolve on their own cadence; an agent that conforms to
/// one version must never be silently simulated under another — that is how a
/// "passing" agent ends up cheating or desyncing in the field. So the check is
/// exact-match, run at the handshake, before any match state exists.
pub fn check_version(theirs: u32) -> Result<(), VersionMismatch> {
    if theirs == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(VersionMismatch {
            ours: PROTOCOL_VERSION,
            theirs,
        })
    }
}

/// What an observed entity is — enough to reason about it without leaking any
/// of its hidden internal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    /// A pawn — a seat's controllable body.
    Player,
    /// A traveling shot in flight. Produced by a projectile-weapon-mode match as a
    /// parity-bounded [`VisibleEntity`]: it carries only its perceivable facets
    /// (position, travel `facing`) and is reported as the neutral team, so it leaks
    /// neither the shooter's nor the target's identity. A pure-hitscan match never
    /// produces one.
    Projectile,
    /// A collectible world item (a health or ammo pickup). Produced by a match
    /// configured with [`PickupSpawn`]s as a parity-bounded [`VisibleEntity`]: it
    /// carries only its perceivable facets (id, position) and is reported as the
    /// neutral team, so its effect sub-kind, amount, and respawn timer never leak. A
    /// dormant (collected, not-yet-respawned) pickup produces none.
    Pickup,
}

/// The receiving seat's OWN pawn state, in full. A controller always knows its
/// own health, ammo, weapon cooldown, position and facing — this is the one place
/// full internal state appears in an [`Observation`], and it is always the
/// receiver's own, so it grants no perception advantage. The private-HUD fields
/// here (`ammo`, `cooldown`) are the receiver's own and so appear ONLY in this
/// type, never in a [`VisibleEntity`] or [`BroadcastEntity`] — reading another
/// pawn's ammo or fire timing would be a tactical x-ray.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfState {
    pub seat: SeatId,
    pub team: TeamId,
    /// Ground-plane position (see [`POSITION_SCALE`]).
    pub position: Vec2,
    /// Elevation in [`POSITION_SCALE`] units; `0` on a planar arena.
    pub z: i32,
    pub facing: Bam,
    /// Per-tick planar velocity (see [`VELOCITY_SCALE`]).
    pub velocity: Vec2,
    pub health: u16,
    pub max_health: u16,
    pub ammo: u16,
    /// Ticks until this pawn may fire again, as the next action sees it: `0` means
    /// a `fire` submitted for THIS observation's tick is honored (subject to
    /// `ammo`). It already accounts for the start-of-tick cooldown decrement the
    /// sim applies before resolving fire, so the fire-ready predicate is simply
    /// `cooldown == 0` — the controller never has to model the off-by-one. The
    /// HUD-equivalent of a human's "weapon ready" indicator, so an agent times its
    /// shots on the same information a human player has.
    pub cooldown: u16,
    pub alive: bool,
}

/// One entity the seat can perceive at the current tick.
///
/// This type is the parity bound made structural. It carries only what a player
/// standing in the seat's pawn could observe: identity, team, kind, the
/// last-perceived position/facing, and whether the entity is in line of sight
/// *right now*. There is deliberately NO field for another entity's health,
/// ammo, cooldowns, or intent — the bound is the *absence* of those fields, so an
/// agent built on this protocol cannot read hidden state even if the server that
/// fills it is buggy or hostile. Widening this struct with internal state is a
/// security regression and is pinned against by a wire-shape test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleEntity {
    pub entity_id: u32,
    pub kind: EntityKind,
    pub team: TeamId,
    /// Last perceived ground-plane position (see [`POSITION_SCALE`]).
    pub position: Vec2,
    /// Last perceived elevation (see [`POSITION_SCALE`]).
    pub z: i32,
    pub facing: Bam,
    /// `true` if in the seat's line of sight this tick. The reference core occludes
    /// by EXCLUSION — an entity whose sightline crosses a vision [`Blocker`] is
    /// dropped from the visible set entirely (it never widens perception), so every
    /// entry it emits is in sight and this field is `true`. The `false` value is
    /// reserved for a future perception-memory model that would surface a
    /// last-known, since-lost position; the first-cut core produces no such entry.
    pub in_line_of_sight: bool,
}

/// A parity-bounded, player-perspective snapshot for one seat at one tick — the
/// security boundary of the Gateway.
///
/// It carries the seat's own state in full ([`own`](Observation::own)) plus ONLY
/// the entities that seat could perceive this tick
/// ([`visible`](Observation::visible)). There is intentionally no field for full
/// world state — no global pawn table, no all-entities list — so an omniscient
/// agent cannot be constructed on this protocol by anyone, including a server
/// that wants to. `visible` is in ascending `entity_id` order so the snapshot is
/// canonical and replay-stable. `deadline_micros` bounds how long the agent may
/// take to answer before it forfeits the tick, so a slow agent never stalls the
/// match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub protocol_version: u32,
    pub match_id: MatchId,
    pub seat: SeatId,
    pub tick: u64,
    pub phase: MatchPhase,
    /// Microseconds the agent has to return an [`Action`] for this tick before
    /// the tick is forfeited on its behalf. Server-set; the bounded-latency
    /// invariant.
    pub deadline_micros: u32,
    pub own: SelfState,
    pub visible: Vec<VisibleEntity>,
}

/// One entity in a [`Broadcast`] — the PUBLIC, on-stage state of a single pawn as
/// a non-participant spectator sees it.
///
/// This is the spectator counterpart to [`VisibleEntity`], and the difference is
/// deliberate. A `VisibleEntity` is parity-bounded — only what one seat can perceive
/// this tick — and carries no health or score. A `BroadcastEntity` is the
/// caster-camera view: it is reported for EVERY pawn (a spectator watching the
/// rendered match sees the whole battlefield), and it carries the health bar and
/// scoreboard a broadcast shows — `health`/`max_health`/`score`/`alive`. But it
/// stops at what is *on screen*: there is deliberately NO `ammo` or `cooldown`
/// field, so the feed is not a tactical x-ray of every player's private HUD. That
/// exclusion is the broadcast's security line — a spectator must learn no more than
/// a viewer of the stream would — and is pinned by a wire-shape test; widening it
/// with a private field is a regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BroadcastEntity {
    pub entity_id: u32,
    pub kind: EntityKind,
    pub team: TeamId,
    /// Ground-plane position (see [`POSITION_SCALE`]).
    pub position: Vec2,
    /// Elevation in [`POSITION_SCALE`] units; `0` on a planar arena.
    pub z: i32,
    pub facing: Bam,
    pub health: u16,
    pub max_health: u16,
    /// Cumulative damage dealt — the match score, as shown on the broadcast
    /// scoreboard.
    pub score: i32,
    pub alive: bool,
}

/// A spectator's whole-battlefield snapshot at one tick — the broadcast (caster)
/// view, NOT any seat's [`Observation`].
///
/// Where an [`Observation`] is one seat's parity-bounded, player-perspective slice
/// (its own state plus only what it perceives), a `Broadcast` is the
/// omniscient-over-PUBLIC-state view a spectator/caster gets: every pawn's on-stage
/// state ([`BroadcastEntity`]), in ascending `entity_id` order so the frame is
/// canonical. It is intentionally a SEPARATE type from `Observation` — a spectator
/// feed must never be wired from a seat's private observation, which would either
/// leak one seat's hidden state to every viewer or, conversely, withhold the
/// whole-map view a broadcast needs. It carries no `deadline_micros`, no `own`, and
/// no per-seat `visible`: a spectator is not a participant, cannot act, and has no
/// tick to answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Broadcast {
    pub protocol_version: u32,
    pub match_id: MatchId,
    pub tick: u64,
    pub phase: MatchPhase,
    /// Every pawn's public on-stage state, ascending by `entity_id` (canonical).
    pub entities: Vec<BroadcastEntity>,
}

/// The length of a full-speed move request: a [`ActionIntent::move_dir`] whose
/// magnitude is `MOVE_INTENT_SCALE` asks for full speed, and the server clamps
/// anything longer. Integer fixed-point, so the clamp is exact and identical on
/// every implementation.
pub const MOVE_INTENT_SCALE: i32 = 1000;

/// The buttons an agent may press this tick. Edge-vs-level semantics (a fresh
/// press vs a held button) are the sim's to define; the protocol carries the
/// booleans canonically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionButtons {
    pub fire: bool,
    pub jump: bool,
    pub ability: bool,
    pub reload: bool,
}

/// The control intent for a single tick: a planar move direction, an aim, and
/// the buttons. Every continuous quantity is integer fixed-point, so the request
/// is canonical and clamp-checkable with no float ambiguity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionIntent {
    /// Desired planar move direction as a fixed-point vector; magnitude in
    /// `0..=MOVE_INTENT_SCALE` expresses fraction of max speed. The server clamps
    /// the magnitude — this is a request, never trusted as the resulting
    /// velocity.
    pub move_dir: Vec2,
    pub aim: Bam,
    pub buttons: ActionButtons,
}

impl ActionIntent {
    /// The canonical anti-god-mode move clamp, shared by every implementation so
    /// the Rust arena and the UE5 server apply the *same* rule a human's input
    /// goes through. Returns a copy whose `move_dir` magnitude is at most
    /// [`MOVE_INTENT_SCALE`]; an in-range request is returned unchanged. Pure
    /// integer math (no float, integer sqrt), so the clamp is deterministic and
    /// can never round *up* past the cap.
    pub fn clamped(&self) -> ActionIntent {
        let x = self.move_dir.x as i64;
        let y = self.move_dir.y as i64;
        // Sum in u64: each i64 square of an i32 is ≤ i32::MIN² (4.6e18, in range),
        // but their i64 SUM overflows at move_dir == {i32::MIN, i32::MIN}
        // (i64::MAX + 1). Widening the add keeps the clamp panic-free and correct
        // on every attacker-controlled input — without it a release build wraps to
        // a tiny magnitude, skips the clamp, and lets that vector through at
        // god-mode speed.
        let mag_sq = (x * x) as u64 + (y * y) as u64;
        let max = MOVE_INTENT_SCALE as i64;
        let max_sq = (max * max) as u64;
        let move_dir = if mag_sq <= max_sq {
            self.move_dir
        } else {
            let mag = isqrt_u64(mag_sq).max(1) as i64;
            Vec2 {
                x: (x * max / mag) as i32,
                y: (y * max / mag) as i32,
            }
        };
        ActionIntent {
            move_dir,
            aim: self.aim,
            buttons: self.buttons,
        }
    }
}

/// Integer square root (floor) via Newton's method — deterministic on every
/// platform, unlike `(n as f64).sqrt()`. Used by the move clamp so action
/// normalization never depends on float behavior.
fn isqrt_u64(n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// One agent action, bound to the tick it answers.
///
/// `tick` is the [`Observation`] tick this action responds to, so the server can
/// discard a stale or late action rather than apply it to a newer tick. The
/// action is a REQUEST: the server validates and clamps every field through the
/// same rules a human's input goes through, and no field here is trusted as
/// authoritative state. [`validate`](Action::validate) is the cheap structural
/// gate at the Gateway boundary; semantic clamping (speed via
/// [`ActionIntent::clamped`], fire rate, ability cooldowns) is the sim's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub protocol_version: u32,
    pub match_id: MatchId,
    pub seat: SeatId,
    /// The observation tick this action answers.
    pub tick: u64,
    pub intent: ActionIntent,
}

/// Why [`Action::validate`] rejected an action envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionError {
    /// The action's [`PROTOCOL_VERSION`] does not match this build's.
    Version(VersionMismatch),
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionError::Version(m) => write!(f, "invalid action: {m}"),
        }
    }
}

impl std::error::Error for ActionError {}

impl Action {
    /// Structural validation at the Gateway boundary: the protocol version must
    /// match exactly, because an action framed under a different version cannot
    /// be interpreted safely. A well-formed envelope is NOT a trusted action —
    /// the sim still applies every semantic clamp (speed, fire rate, cooldowns)
    /// before anything affects match state.
    pub fn validate(&self) -> Result<(), ActionError> {
        check_version(self.protocol_version).map_err(ActionError::Version)
    }
}

impl ActionButtons {
    /// Pack the four buttons into a byte (bit 0 = fire, 1 = jump, 2 = ability,
    /// 3 = reload). Used by the replay digest so the canonical encoding of an
    /// action is fixed-width and order-free.
    pub fn bits(self) -> u8 {
        (self.fire as u8) | (self.jump as u8) << 1 | (self.ability as u8) << 2 | (self.reload as u8) << 3
    }
}

/// Who held a seat for a match — the controller identity, recorded so a replay
/// names its players. The identity string is an on-chain agent address for a
/// ranked agent seat, or a human/label otherwise; the protocol does not
/// interpret it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatInfo {
    pub seat: SeatId,
    pub team: TeamId,
    pub controller: String,
}

/// One seat's accepted (post-clamp) intent for one tick. A seat that forfeited
/// the tick (no action within the deadline) simply has no entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatAction {
    pub seat: SeatId,
    pub intent: ActionIntent,
}

/// One simulated tick's canonical record: the accepted actions that drove it,
/// ascending by seat. The position of a [`TickRecord`] in
/// [`ReplayRecord::ticks`] is its tick order, so the stream is fully ordered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickRecord {
    pub tick: u64,
    pub actions: Vec<SeatAction>,
}

/// A static axis-aligned vision-blocking volume on the ground plane — the
/// integer footprint of an occluder (a wall, a pillar) the line-of-sight test
/// reasons against. Stored as its two opposite corners in position units (see
/// [`POSITION_SCALE`]): `min` is the low corner and `max` the high corner on each
/// axis, so the well-formed invariant is `min.x <= max.x && min.y <= max.y`. An
/// inverted box is rejected before any simulation; a zero-extent box on one axis
/// is a legitimate thin wall.
///
/// Corners are stored directly (not as centre ± half-extent) so the segment-vs-box
/// test consumes them with zero arithmetic — no `centre + half` step that could
/// overflow `i32` — keeping the integer geometry panic-free at any (operator-set)
/// arena coordinate.
///
/// First cut: a blocker occludes VISION only — it is NOT physical, so movement and
/// hitscan pass through it. It is a server-authoritative match determinant: a
/// match's blocker set decides what each seat can perceive, so it is bound into
/// the [`ReplayRecord`] and committed by [`digest`](ReplayRecord::digest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    /// The low corner: the minimum on each axis. `min.x <= max.x && min.y <= max.y`.
    pub min: Vec2,
    /// The high corner: the maximum on each axis.
    pub max: Vec2,
}

/// What a collectible world pickup grants the pawn that reaches it. The reference
/// core caps every effect to the pawn's own ceiling (a heal never exceeds
/// `max_health`, an ammo refill never exceeds the magazine), so a pickup is always
/// a non-overflowing, server-authoritative effect. The sub-kind is a server-side
/// match determinant; the parity-bounded [`VisibleEntity`] a perceiver sees carries
/// only [`EntityKind::Pickup`], not this — an agent learns a pickup's effect by
/// collecting it, the same empirical posture as the weapon model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickupKind {
    /// Heals the collector, clamped to `max_health`.
    Health,
    /// Refills the collector's ammo, clamped to the magazine size.
    Ammo,
}

/// One configured pickup spawn point — a static match determinant (the producer
/// authors the world's items, never an agent). It is `(kind, position, amount)`;
/// the live collectible/dormant STATE is derived from this plus the action stream
/// and never recorded, so the replay re-runs it bit-for-bit. Bound into the
/// [`ReplayRecord`] and committed by [`digest`](ReplayRecord::digest), so a tampered
/// item layout yields a different hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickupSpawn {
    pub kind: PickupKind,
    /// Where the pickup sits (and respawns), in position units (see [`POSITION_SCALE`]).
    pub position: Vec2,
    /// The effect magnitude — health restored, or ammo refilled — clamped to the
    /// pawn's ceiling at collection so it never overflows.
    pub amount: u16,
}

/// The full deterministic record of a match — the PRNG seed, the roster, the
/// static vision blockers, and the ordered per-tick accepted-action stream —
/// sufficient to re-run the match bit-for-bit and reproduce its [`MatchResult`].
///
/// For the record to be a stable commitment the producer MUST build it in
/// canonical order: `seats` and each tick's `actions` ascending by seat, `ticks`
/// in tick order. [`digest`](ReplayRecord::digest) then commits the whole record
/// to one 32-byte hash that is identical on every platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayRecord {
    pub protocol_version: u32,
    pub match_id: MatchId,
    /// The seed the sim ran from; replay re-runs from this exact value.
    pub seed: u64,
    pub seats: Vec<SeatInfo>,
    /// Static vision blockers the match ran under, in declared order. A match
    /// determinant of perception (not of movement/combat in this first cut), so
    /// it is committed by [`digest`](ReplayRecord::digest) — a tampered blocker
    /// set yields a different hash. `serde(default)` so a record written before
    /// this field existed deserializes to the no-occluder behavior it ran under.
    #[serde(default)]
    pub blockers: Vec<Blocker>,
    /// Configured pickup spawn points the match ran under, in declared order. A
    /// match determinant of combat (a collected health/ammo pickup changes who
    /// survives), so it is committed by [`digest`](ReplayRecord::digest) — a
    /// tampered item layout yields a different hash, the same as `blockers`.
    /// `serde(default)` so a record written before this field existed deserializes
    /// to the no-pickup behavior it ran under.
    #[serde(default)]
    pub pickups: Vec<PickupSpawn>,
    pub ticks: Vec<TickRecord>,
}

impl ReplayRecord {
    /// The canonical commitment to this match: `keccak256` over a domain-tagged,
    /// length-prefixed, big-endian, integer-only encoding of every field — no
    /// JSON, no float, no map iteration, so the same match yields the same 32
    /// bytes on every platform. This is what an on-chain attestation signs and
    /// what a grader compares; it mirrors the `mesh/proto` digest discipline.
    /// Lowercase-hex of this is the wire form carried in
    /// [`MatchResult::replay_hash`].
    pub fn digest(&self) -> [u8; 32] {
        let mut h = Keccak256::new();
        // v2 folded in the static `blockers` set; v3 folds in the static `pickups`
        // set. The bump is honest about each encoding change. Blockers occlude
        // vision only, so they cannot be bound by re-execution alone and committing
        // them here is what pins a match's perception geometry to its hash; pickups
        // DO alter the re-run outcome (a collected heal changes who survives), but an
        // UNCOLLECTED pickup does not, so binding the full item layout here keeps the
        // digest a complete, self-identifying commitment to the match world — the
        // same role `blockers` plays.
        h.update(b"blackfield/arena/replay/v3");
        h.update(self.protocol_version.to_be_bytes());
        h.update(self.match_id.as_bytes());
        h.update(self.seed.to_be_bytes());
        h.update((self.seats.len() as u32).to_be_bytes());
        for s in &self.seats {
            h.update([s.seat]);
            h.update(s.team.to_be_bytes());
            h.update((s.controller.len() as u32).to_be_bytes());
            h.update(s.controller.as_bytes());
        }
        h.update((self.blockers.len() as u32).to_be_bytes());
        for b in &self.blockers {
            h.update(b.min.x.to_be_bytes());
            h.update(b.min.y.to_be_bytes());
            h.update(b.max.x.to_be_bytes());
            h.update(b.max.y.to_be_bytes());
        }
        h.update((self.pickups.len() as u32).to_be_bytes());
        for p in &self.pickups {
            // An explicit kind byte (not the enum discriminant) so the wire mapping
            // is the fixed contract a second implementation reproduces.
            h.update([match p.kind {
                PickupKind::Health => 0u8,
                PickupKind::Ammo => 1u8,
            }]);
            h.update(p.position.x.to_be_bytes());
            h.update(p.position.y.to_be_bytes());
            h.update(p.amount.to_be_bytes());
        }
        h.update((self.ticks.len() as u32).to_be_bytes());
        for t in &self.ticks {
            h.update(t.tick.to_be_bytes());
            h.update((t.actions.len() as u32).to_be_bytes());
            for a in &t.actions {
                h.update([a.seat]);
                h.update(a.intent.move_dir.x.to_be_bytes());
                h.update(a.intent.move_dir.y.to_be_bytes());
                h.update(a.intent.aim.to_be_bytes());
                h.update([a.intent.buttons.bits()]);
            }
        }
        h.finalize().into()
    }
}

/// How one seat finished — ordered, canonical, and enough to settle a ranked
/// match. `placement` is 1-based (1 = best); tied seats share a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatOutcome {
    pub seat: SeatId,
    pub team: TeamId,
    pub placement: u16,
    pub score: i32,
    pub alive_at_end: bool,
}

/// The terminal result of a match — the canonical, attestable summary settlement
/// and grading read. `replay_hash` is the lowercase-hex
/// [`ReplayRecord::digest`], so the result commits to the exact match that
/// produced it; anyone with the replay can recompute the hash and re-run the
/// match to verify both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchResult {
    pub protocol_version: u32,
    pub match_id: MatchId,
    pub final_tick: u64,
    /// Per-seat outcomes, ascending by seat (canonical order).
    pub outcomes: Vec<SeatOutcome>,
    /// Lowercase hex of [`ReplayRecord::digest`].
    pub replay_hash: String,
}

/// The rules a match is played under, sent to every seat at [`GatewayMsg::Start`]
/// so an agent knows the tick rate, the time limit, and the arena bounds it must
/// stay within. Read-only; the server is authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchConfig {
    /// Simulation ticks per second.
    pub tick_hz: u16,
    /// Hard cap on match length in ticks; the match ends at this tick if no
    /// earlier win condition fires.
    pub max_ticks: u64,
    /// Arena half-extent per axis (see [`POSITION_SCALE`]): play stays within
    /// `[-bounds, +bounds]`.
    pub bounds: Vec2,
    /// Number of seats in the match.
    pub seats: u8,
}

/// Server → agent messages — the Gateway side of the protocol, consumed by an
/// agent's `AgentController` (or the reference harness). Internally tagged on
/// `type`, exactly like `mesh/proto`'s `CoordinatorMsg`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayMsg {
    /// The first frame on a connection: a single-use random challenge the agent
    /// MUST fold into its [`join_digest`] (hex of the raw nonce bytes), binding
    /// the identity proof to *this* connection so a captured [`AgentMsg::Join`]
    /// replayed on a fresh connection — which gets a different challenge — fails
    /// signature recovery. Connection-scoped: held only for the handshake, like
    /// mesh's `CoordinatorMsg::Challenge`.
    Challenge { nonce: String },
    /// Handshake accepted: the seat is admitted. Confirms the
    /// [`PROTOCOL_VERSION`] the server speaks — the agent MUST
    /// [`check_version`] it — and the seat it was assigned.
    Welcome {
        protocol_version: u32,
        match_id: MatchId,
        seat: SeatId,
    },
    /// Handshake refused (version mismatch, full match, or unauthenticated
    /// ranked seat). Terminal for this connection.
    Reject { reason: String },
    /// The match is starting; here are the rules.
    Start { match_id: MatchId, config: MatchConfig },
    /// A per-tick parity-bounded observation; the agent answers with
    /// [`AgentMsg::Act`] before its `deadline_micros` elapses.
    Observe(Observation),
    /// The match is over; the canonical, attestable result.
    End(MatchResult),
}

/// Agent → server messages — the controller side of the protocol. Internally
/// tagged on `type`, exactly like `mesh/proto`'s `EarnerMsg`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    /// Handshake: request a seat. Carries the agent's [`PROTOCOL_VERSION`] (the
    /// server [`check_version`]s it and replies [`GatewayMsg::Welcome`] or
    /// [`GatewayMsg::Reject`]) and its claimed identity. For a ranked agent seat
    /// `agent_id` is an on-chain agent address, proven by `signature_hex` over
    /// the join digest; a casual/unranked seat may send an empty `signature_hex`
    /// and matchmaking decides whether that seat is allowed.
    Join {
        protocol_version: u32,
        agent_id: String,
        signature_hex: String,
    },
    /// The action answering the observation tick it names.
    Act(Action),
    /// Leave or forfeit the match.
    Leave { reason: String },
}

/// Server → spectator messages — the read-only spectator side of the protocol,
/// consumed by a caster UI or the eventual UE5 spectator client. Internally tagged
/// on `type`, like [`GatewayMsg`].
///
/// The spectator protocol is one-directional BY CONSTRUCTION: this is the only
/// spectator message enum, and there is no spectator → server counterpart — no
/// `Act`, no input of any kind. A spectator connection can only RECEIVE a stream of
/// [`Frame`](SpectatorMsg::Frame)s and a terminal [`End`](SpectatorMsg::End); it has
/// no message it can send that the server would interpret, so a spectator can never
/// inject an action or otherwise influence a ranked match. Read-only here is the
/// *absence* of an inbound type, not a runtime check that could be forgotten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpectatorMsg {
    /// One per-tick whole-battlefield broadcast frame.
    Frame(Broadcast),
    /// The match is over; the canonical, attestable result — the same value seats
    /// receive at [`GatewayMsg::End`].
    End(MatchResult),
}

/// Canonical digest an agent signs at [`AgentMsg::Join`] to prove control of the
/// key behind `agent_id` — the arena analogue of mesh's hello digest. The server
/// recovers the signer from this digest plus the Join's `signature_hex` and
/// admits a *ranked* seat only if the recovered address matches the claimed
/// `agent_id`, tying a ranked seat to a verified on-chain agent identity (and
/// thus to reputation + settlement).
///
/// Bytes = `keccak256( DOMAIN || protocol_version_be || len(agent_id) ||
/// agent_id || len(nonce) || nonce )`, every `len` a big-endian `u32`. The
/// `DOMAIN` tag separates it from the replay digest and from mesh's hello digest
/// (so no signature can cross protocols); the length prefixes make the field
/// boundaries unambiguous; the server-issued challenge `nonce` is folded *in*
/// (not merely sent alongside), so the signature and the freshness are
/// inseparable and a captured Join cannot be replayed against a different
/// challenge. The `protocol_version` binds the identity proof to the version it
/// was made under. Both sides MUST build it identically.
pub fn join_digest(protocol_version: u32, agent_id: &str, nonce: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"blackfield/arena/join/v1");
    h.update(protocol_version.to_be_bytes());
    h.update((agent_id.len() as u32).to_be_bytes());
    h.update(agent_id.as_bytes());
    h.update((nonce.len() as u32).to_be_bytes());
    h.update(nonce);
    h.finalize().into()
}

/// Why a [`AgentMsg::Join`]'s `signature_hex` failed to prove control of the
/// claimed `agent_id`. Mirrors mesh's earner `VerifyError` so the arena Gateway's
/// ranked-admission gate reports the same shape of failure as the render mesh.
#[derive(Debug, PartialEq, Eq)]
pub enum JoinVerifyError {
    /// `signature_hex` was not valid hex or not the required 65 bytes (`[r||s||v]`).
    BadSignatureEncoding,
    /// Parsed but high-S (non-canonical / malleable per EIP-2). Rejected so a
    /// given join proof has exactly one valid 65-byte encoding.
    NonCanonicalSignature,
    /// No public key could be recovered from the signature over the digest.
    Unrecoverable,
    /// A key was recovered, but its address is not the claimed `agent_id` — the
    /// signer does not control the identity it claims, so the seat is not ranked.
    AddressMismatch,
}

/// Ethereum-style address (0x-prefixed, lowercase) from a verifying key:
/// keccak256(uncompressed_pubkey[1..])[12..]. The same derivation mesh uses, so a
/// session key recovers to the identical address whether it signs a render result
/// or an arena join — one agent address spans both subsystems.
pub fn address_from_verifying_key(vk: &VerifyingKey) -> String {
    let point = vk.to_encoded_point(false);
    let hash = Keccak256::digest(&point.as_bytes()[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

/// Recover the signer of a [`AgentMsg::Join`] and assert it controls the claimed
/// `agent_id` — the arena analogue of mesh's `verify_hello_signature`.
///
/// `signature_hex` is a recoverable secp256k1 `[r||s||v]` over
/// [`join_digest(protocol_version, agent_id, nonce)`](join_digest); on `Ok` the
/// recovered address equals `agent_id` (case-insensitive), so the connection
/// provably holds the key behind the identity it claims and the Gateway may form a
/// *ranked* seat. `agent_id` feeds the digest *and* is the recovery target, so a
/// captured signature can't be reattached to a different claimed address — a
/// forger would have to recover its own (different) address.
///
/// `nonce` is the server-issued, per-connection challenge; folding it into the
/// digest means a Join captured off the wire and replayed on a fresh connection —
/// which gets a different challenge — recovers a key that no longer matches and is
/// rejected. The caller passes the nonce IT issued for this connection, never a
/// client-supplied value.
///
/// This proves key *possession* only; whether `agent_id` is a registered, eligible
/// on-chain agent is a separate check (a later contracts task) that composes on top
/// of this one.
pub fn verify_join_signature(
    protocol_version: u32,
    agent_id: &str,
    nonce: &[u8],
    signature_hex: &str,
) -> Result<(), JoinVerifyError> {
    let raw = hex::decode(signature_hex.strip_prefix("0x").unwrap_or(signature_hex))
        .map_err(|_| JoinVerifyError::BadSignatureEncoding)?;
    if raw.len() != 65 {
        return Err(JoinVerifyError::BadSignatureEncoding);
    }
    let sig = Signature::from_slice(&raw[..64]).map_err(|_| JoinVerifyError::BadSignatureEncoding)?;
    // Enforce low-S (EIP-2): `normalize_s` returns `Some` only for the high-S
    // (malleable) half, so reject it and a join proof has one canonical encoding.
    if sig.normalize_s().is_some() {
        return Err(JoinVerifyError::NonCanonicalSignature);
    }
    let recid = RecoveryId::from_byte(raw[64]).ok_or(JoinVerifyError::BadSignatureEncoding)?;
    let digest = join_digest(protocol_version, agent_id, nonce);
    let vk = VerifyingKey::recover_from_prehash(&digest, &sig, recid)
        .map_err(|_| JoinVerifyError::Unrecoverable)?;
    if address_from_verifying_key(&vk).eq_ignore_ascii_case(agent_id) {
        Ok(())
    } else {
        Err(JoinVerifyError::AddressMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{RecoveryId, Signature, SigningKey};

    /// Produce a hex `[r||s||v]` join signature, mirroring the agent SDK's signer.
    fn sign_join(sk: &SigningKey, protocol_version: u32, agent_id: &str, nonce: &[u8]) -> String {
        let digest = join_digest(protocol_version, agent_id, nonce);
        let (sig, recid): (Signature, RecoveryId) = sk.sign_prehash_recoverable(&digest).unwrap();
        let mut out = sig.to_bytes().to_vec();
        out.push(recid.to_byte());
        hex::encode(out)
    }

    fn dev_key() -> SigningKey {
        let bytes =
            hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318").unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    /// A second, distinct dev key whose address differs from `dev_key`.
    fn other_key() -> SigningKey {
        let bytes =
            hex::decode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    fn dev_address() -> String {
        address_from_verifying_key(dev_key().verifying_key())
    }

    const CHAL: &[u8] = b"arena-challenge-nonce";

    #[test]
    fn verify_join_signature_accepts_the_signing_key() {
        // The arena analogue of mesh's hello recovery: the agent signs join_digest
        // with its session key and verify recovers exactly the claimed agent_id, so
        // a ranked seat is tied to a held key.
        let sk = dev_key();
        let addr = dev_address();
        let sig = sign_join(&sk, PROTOCOL_VERSION, &addr, CHAL);
        assert_eq!(verify_join_signature(PROTOCOL_VERSION, &addr, CHAL, &sig), Ok(()));
    }

    #[test]
    fn verify_join_signature_rejects_a_forged_signer() {
        // An attacker signs a Join *claiming* dev's address but with its own key.
        // Recovery over the claimed-address digest yields the attacker's address,
        // not dev's, so key possession fails — an identity can't be spoofed by
        // someone who doesn't hold its key.
        let forged = sign_join(&other_key(), PROTOCOL_VERSION, &dev_address(), CHAL);
        assert_eq!(
            verify_join_signature(PROTOCOL_VERSION, &dev_address(), CHAL, &forged),
            Err(JoinVerifyError::AddressMismatch)
        );
    }

    #[test]
    fn verify_join_signature_rejects_a_replayed_challenge_nonce() {
        // A signature captured over THIS connection's challenge, replayed against a
        // different connection's challenge, recovers a different key and is rejected
        // — the nonce binds the proof to one connection (anti-replay).
        let sk = dev_key();
        let addr = dev_address();
        let sig = sign_join(&sk, PROTOCOL_VERSION, &addr, b"connection-A-nonce");
        assert_eq!(
            verify_join_signature(PROTOCOL_VERSION, &addr, b"connection-B-nonce", &sig),
            Err(JoinVerifyError::AddressMismatch)
        );
        // Sanity: the same signature still verifies against its own challenge.
        assert_eq!(
            verify_join_signature(PROTOCOL_VERSION, &addr, b"connection-A-nonce", &sig),
            Ok(())
        );
    }

    #[test]
    fn verify_join_signature_rejects_a_tampered_protocol_version() {
        // A proof made under one protocol_version must not verify under another:
        // the digest binds the version, so recovery over the verifier's version
        // yields a different key. Prevents cross-version replay of an identity proof.
        let sk = dev_key();
        let addr = dev_address();
        let sig = sign_join(&sk, PROTOCOL_VERSION + 1, &addr, CHAL);
        assert_eq!(
            verify_join_signature(PROTOCOL_VERSION, &addr, CHAL, &sig),
            Err(JoinVerifyError::AddressMismatch)
        );
    }

    #[test]
    fn verify_join_signature_rejects_a_tampered_agent_id() {
        // A signature over agent A's digest, presented as a claim to agent B's
        // identity, recovers A's key (≠ B), so the claim to B is rejected.
        let sk = dev_key();
        let signed_addr = dev_address();
        let sig = sign_join(&sk, PROTOCOL_VERSION, &signed_addr, CHAL);
        let other_addr = address_from_verifying_key(other_key().verifying_key());
        assert_ne!(signed_addr, other_addr);
        assert_eq!(
            verify_join_signature(PROTOCOL_VERSION, &other_addr, CHAL, &sig),
            Err(JoinVerifyError::AddressMismatch)
        );
    }

    #[test]
    fn verify_join_signature_rejects_malformed_encoding() {
        let addr = dev_address();
        // Non-hex (after the optional 0x strip).
        assert_eq!(
            verify_join_signature(PROTOCOL_VERSION, &addr, CHAL, "0xnothex"),
            Err(JoinVerifyError::BadSignatureEncoding)
        );
        // Valid hex but the wrong length (64, not the required 65 bytes).
        assert_eq!(
            verify_join_signature(PROTOCOL_VERSION, &addr, CHAL, &"00".repeat(64)),
            Err(JoinVerifyError::BadSignatureEncoding)
        );
    }

    /// 256-bit big-endian `a - b` (a >= b) — flip a low-S signature to its
    /// malleable high-S counterpart `s' = n - s` without pulling in scalar types.
    fn be_sub(a: &[u8; 32], b: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let mut d = a[i] as i16 - b[i] as i16 - borrow;
            if d < 0 {
                d += 256;
                borrow = 1;
            } else {
                borrow = 0;
            }
            out[i] = d as u8;
        }
        out
    }

    #[test]
    fn verify_join_signature_rejects_high_s() {
        // secp256k1 group order n.
        const N: [u8; 32] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C,
            0xD0, 0x36, 0x41, 0x41,
        ];
        let sk = dev_key();
        let addr = dev_address();
        let low_hex = sign_join(&sk, PROTOCOL_VERSION, &addr, CHAL);
        assert_eq!(verify_join_signature(PROTOCOL_VERSION, &addr, CHAL, &low_hex), Ok(()));

        // Malleate to the high-S half (s' = n - s, r unchanged): a second valid
        // encoding of the same proof, rejected so each join proof is canonical.
        let raw = hex::decode(&low_hex).unwrap();
        let s_high = be_sub(&N, &raw[32..64]);
        let mut malleated = Vec::with_capacity(65);
        malleated.extend_from_slice(&raw[0..32]);
        malleated.extend_from_slice(&s_high);
        malleated.push(raw[64]);
        assert!(
            Signature::from_slice(&malleated[..64]).unwrap().normalize_s().is_some(),
            "constructed s must be the high-S half"
        );
        assert_eq!(
            verify_join_signature(PROTOCOL_VERSION, &addr, CHAL, &hex::encode(&malleated)),
            Err(JoinVerifyError::NonCanonicalSignature)
        );
    }

    #[test]
    fn join_digest_binds_every_field() {
        // The signature commits to the whole Join AND the challenge nonce, so a
        // captured signature can't be reattached to a different identity/version
        // nor replayed against a fresh challenge.
        let n = b"nonce";
        let base = join_digest(PROTOCOL_VERSION, "0xabc", n);
        assert_ne!(base, join_digest(PROTOCOL_VERSION + 1, "0xabc", n), "version");
        assert_ne!(base, join_digest(PROTOCOL_VERSION, "0xabd", n), "agent_id");
        assert_ne!(base, join_digest(PROTOCOL_VERSION, "0xabc", b"other"), "nonce");
        assert_ne!(base, join_digest(PROTOCOL_VERSION, "0xabc", b""), "empty nonce");
        // Length-delimited: shifting a byte across the agent_id/nonce boundary
        // must not alias to the same digest.
        assert_ne!(
            join_digest(PROTOCOL_VERSION, "0xab", b"cd"),
            join_digest(PROTOCOL_VERSION, "0xabc", b"d"),
            "field boundaries must be unambiguous"
        );
    }

    #[test]
    fn version_handshake_accepts_match_and_rejects_drift() {
        assert!(check_version(PROTOCOL_VERSION).is_ok());
        // A peer one version ahead or behind is rejected with both versions
        // named, so the handshake can report the incompatibility precisely.
        let err = check_version(PROTOCOL_VERSION + 1).unwrap_err();
        assert_eq!(err, VersionMismatch { ours: PROTOCOL_VERSION, theirs: PROTOCOL_VERSION + 1 });
        assert!(check_version(0).is_err());
    }

    #[test]
    fn match_phase_tags_are_stable() {
        let cases = [
            (MatchPhase::Lobby, "lobby"),
            (MatchPhase::Starting, "starting"),
            (MatchPhase::Live, "live"),
            (MatchPhase::Ended, "ended"),
        ];
        for (variant, tag) in cases {
            let serialized = serde_json::to_value(variant).unwrap();
            assert_eq!(serialized, serde_json::json!(tag), "MatchPhase::{variant:?} tag drifted");
            let round: MatchPhase = serde_json::from_value(serialized).unwrap();
            assert_eq!(round, variant, "MatchPhase::{variant:?} did not round-trip");
        }
    }

    #[test]
    fn match_mode_and_controller_kind_tags_are_stable() {
        // Matchmaking vocabulary shared with the match service and (later) the
        // engine; pin the snake_case wire spelling so a rename breaks loud.
        for (variant, tag) in [
            (MatchMode::Human, "human"),
            (MatchMode::Agent, "agent"),
            (MatchMode::Mixed, "mixed"),
        ] {
            assert_eq!(serde_json::to_value(variant).unwrap(), serde_json::json!(tag));
            let round: MatchMode = serde_json::from_value(serde_json::json!(tag)).unwrap();
            assert_eq!(round, variant);
        }
        for (variant, tag) in [(ControllerKind::Human, "human"), (ControllerKind::Agent, "agent")] {
            assert_eq!(serde_json::to_value(variant).unwrap(), serde_json::json!(tag));
            let round: ControllerKind = serde_json::from_value(serde_json::json!(tag)).unwrap();
            assert_eq!(round, variant);
        }
    }

    #[test]
    fn vec2_wire_shape_is_stable() {
        let canonical = serde_json::json!({ "x": 1500, "y": -250 });
        let parsed: Vec2 = serde_json::from_value(canonical.clone()).unwrap();
        assert_eq!(parsed, Vec2 { x: 1500, y: -250 });
        assert_eq!(serde_json::to_value(parsed).unwrap(), canonical, "Vec2 wire shape drifted");
    }

    #[test]
    fn bam_quarter_turn_is_half_pi() {
        // 0x4000 is a quarter turn; the helper is display-only but must be right.
        let r = bam_to_radians(0x4000);
        assert!((r - std::f64::consts::FRAC_PI_2).abs() < 1e-9, "got {r}");
    }

    const FIXED_MATCH: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn object_keys(v: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn visible_entity_exposes_no_hidden_state() {
        // FM1: the parity bound is the ABSENCE of hidden-state fields. Pin the
        // exact perceivable key set so adding another entity's health / ammo /
        // intent / cooldown — anything a player couldn't see — fails CI.
        let e = VisibleEntity {
            entity_id: 7,
            kind: EntityKind::Player,
            team: 2,
            position: Vec2 { x: 1000, y: -2000 },
            z: 0,
            facing: 0x4000,
            in_line_of_sight: true,
        };
        let json = serde_json::to_value(e).unwrap();
        assert_eq!(
            object_keys(&json),
            ["entity_id", "facing", "in_line_of_sight", "kind", "position", "team", "z"],
            "VisibleEntity gained or lost a field — the perceivable set is a security contract"
        );
        for forbidden in ["health", "ammo", "max_health", "intent", "cooldown", "velocity"] {
            assert!(
                json.get(forbidden).is_none(),
                "VisibleEntity must not leak hidden state field `{forbidden}`"
            );
        }
    }

    #[test]
    fn self_state_is_only_the_observers_own_full_state() {
        // FM2: `own` is the ONE place full internal state appears, and it is always
        // the receiver's own. Pin its exact key set so a field that would smuggle
        // another seat's private data — or global/RNG state — into `own` fails CI.
        let s = SelfState {
            seat: 0,
            team: 1,
            position: Vec2 { x: 0, y: 0 },
            z: 0,
            facing: 0x4000,
            velocity: Vec2 { x: 0, y: 0 },
            health: 100,
            max_health: 100,
            ammo: 30,
            cooldown: 5,
            alive: true,
        };
        let json = serde_json::to_value(s).unwrap();
        assert_eq!(
            object_keys(&json),
            ["alive", "ammo", "cooldown", "facing", "health", "max_health", "position", "seat", "team", "velocity", "z"],
            "SelfState gained or lost a field — the observable self-surface is a security contract"
        );
        for forbidden in ["enemies", "visible", "all_pawns", "rng_state", "world", "world_state"] {
            assert!(
                json.get(forbidden).is_none(),
                "SelfState must not carry global / other-seat field `{forbidden}`"
            );
        }
    }

    #[test]
    fn observation_carries_no_full_world_field() {
        // FM1: an Observation may carry the seat's own state + only what it
        // perceives. Pin the exact top-level key set so no global pawn table /
        // all-entities / world-state field can be added without failing CI.
        let obs = sample_observation();
        let json = serde_json::to_value(&obs).unwrap();
        assert_eq!(
            object_keys(&json),
            ["deadline_micros", "match_id", "own", "phase", "protocol_version", "seat", "tick", "visible"],
            "Observation gained or lost a top-level field — omniscient state must never appear"
        );
        for forbidden in ["all_pawns", "entities", "world", "world_state", "pawns"] {
            assert!(
                json.get(forbidden).is_none(),
                "Observation must not carry full-world field `{forbidden}`"
            );
        }
    }

    #[test]
    fn observation_wire_shape_is_stable() {
        let canonical = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "match_id": FIXED_MATCH,
            "seat": 0,
            "tick": 128,
            "phase": "live",
            "deadline_micros": 50_000,
            "own": {
                "seat": 0, "team": 1,
                "position": { "x": 0, "y": 0 }, "z": 0,
                "facing": 16384,
                "velocity": { "x": 0, "y": 0 },
                "health": 100, "max_health": 100, "ammo": 30, "cooldown": 5, "alive": true
            },
            "visible": [{
                "entity_id": 7, "kind": "player", "team": 2,
                "position": { "x": 1000, "y": -2000 }, "z": 0,
                "facing": 16384, "in_line_of_sight": true
            }]
        });
        let parsed: Observation = serde_json::from_value(canonical.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), canonical, "Observation wire shape drifted");
    }

    #[test]
    fn broadcast_entity_exposes_no_private_hud_state() {
        // FM1: the broadcast is the on-stage view, not a tactical x-ray. Pin the
        // exact public key set so adding a private-HUD field (ammo, cooldown, intent)
        // — anything a viewer of the stream could not see — fails CI. It DOES carry
        // health + score (the broadcast health bar + scoreboard); that is the
        // deliberate difference from the parity-bounded VisibleEntity, which has
        // neither.
        let e = BroadcastEntity {
            entity_id: 3,
            kind: EntityKind::Player,
            team: 1,
            position: Vec2 { x: 1000, y: -2000 },
            z: 0,
            facing: 0x4000,
            health: 80,
            max_health: 100,
            score: 42,
            alive: true,
        };
        let json = serde_json::to_value(e).unwrap();
        assert_eq!(
            object_keys(&json),
            ["alive", "entity_id", "facing", "health", "kind", "max_health", "position", "score", "team", "z"],
            "BroadcastEntity gained or lost a field — the public broadcast surface is a security contract"
        );
        for forbidden in ["ammo", "cooldown", "intent", "velocity"] {
            assert!(
                json.get(forbidden).is_none(),
                "BroadcastEntity must not leak private HUD field `{forbidden}`"
            );
        }
    }

    #[test]
    fn broadcast_is_a_whole_field_snapshot_distinct_from_observation() {
        // FM1: a Broadcast is a SEPARATE type from Observation — no seat, no `own`,
        // no per-seat `visible`, no deadline. Pin the top-level key set so the
        // spectator frame can never be silently reshaped into — or sourced from — a
        // seat's parity-bounded observation.
        let b = sample_broadcast();
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(
            object_keys(&json),
            ["entities", "match_id", "phase", "protocol_version", "tick"],
            "Broadcast gained or lost a top-level field"
        );
        for forbidden in ["seat", "own", "visible", "deadline_micros"] {
            assert!(
                json.get(forbidden).is_none(),
                "Broadcast must not carry seat-observation field `{forbidden}`"
            );
        }
    }

    #[test]
    fn broadcast_wire_shape_is_stable() {
        let canonical = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "match_id": FIXED_MATCH,
            "tick": 64,
            "phase": "live",
            "entities": [{
                "entity_id": 0, "kind": "player", "team": 1,
                "position": { "x": -1000, "y": 500 }, "z": 0,
                "facing": 16384, "health": 80, "max_health": 100, "score": 42, "alive": true
            }]
        });
        let parsed: Broadcast = serde_json::from_value(canonical.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), canonical, "Broadcast wire shape drifted");
    }

    #[test]
    fn spectator_msg_wire_shapes_are_stable() {
        // Frame — newtype variant: the Broadcast fields flatten next to "type".
        let mut frame = serde_json::to_value(sample_broadcast()).unwrap();
        frame.as_object_mut().unwrap().insert("type".into(), "frame".into());
        assert_round::<SpectatorMsg>(&frame, "SpectatorMsg::Frame");

        // End — newtype variant: the MatchResult fields flatten next to "type", the
        // same terminal result a seat gets at GatewayMsg::End.
        let result = MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id: FIXED_MATCH.parse().unwrap(),
            final_tick: 2,
            outcomes: vec![SeatOutcome { seat: 0, team: 1, placement: 1, score: 3, alive_at_end: true }],
            replay_hash: hex::encode(sample_replay().digest()),
        };
        let mut end = serde_json::to_value(&result).unwrap();
        end.as_object_mut().unwrap().insert("type".into(), "end".into());
        assert_round::<SpectatorMsg>(&end, "SpectatorMsg::End");
    }

    #[test]
    fn action_wire_shape_is_stable() {
        let canonical = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "match_id": FIXED_MATCH,
            "seat": 3,
            "tick": 128,
            "intent": {
                "move_dir": { "x": 600, "y": 800 },
                "aim": 16384,
                "buttons": { "fire": true, "jump": false, "ability": false, "reload": false }
            }
        });
        let parsed: Action = serde_json::from_value(canonical.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), canonical, "Action wire shape drifted");
    }

    #[test]
    fn action_validate_rejects_version_drift() {
        let mut a = sample_action();
        assert!(a.validate().is_ok());
        a.protocol_version = PROTOCOL_VERSION + 1;
        assert_eq!(
            a.validate(),
            Err(ActionError::Version(VersionMismatch { ours: PROTOCOL_VERSION, theirs: PROTOCOL_VERSION + 1 }))
        );
    }

    #[test]
    fn move_clamp_caps_overlong_and_leaves_inrange_untouched() {
        let buttons = ActionButtons { fire: false, jump: false, ability: false, reload: false };
        // A 3-4-5 vector of magnitude 5000 clamps to magnitude 1000: (600, 800).
        let overlong = ActionIntent { move_dir: Vec2 { x: 3000, y: 4000 }, aim: 0, buttons };
        assert_eq!(overlong.clamped().move_dir, Vec2 { x: 600, y: 800 });
        // An at-cap request (magnitude exactly 1000) is returned unchanged.
        let at_cap = ActionIntent { move_dir: Vec2 { x: 600, y: 800 }, aim: 0, buttons };
        assert_eq!(at_cap.clamped().move_dir, Vec2 { x: 600, y: 800 });
        // A short request is untouched.
        let short = ActionIntent { move_dir: Vec2 { x: 300, y: -400 }, aim: 0, buttons };
        assert_eq!(short.clamped().move_dir, Vec2 { x: 300, y: -400 });
        // Aim and buttons pass through.
        let a = ActionIntent { move_dir: Vec2 { x: 9999, y: 0 }, aim: 12345, buttons: ActionButtons { fire: true, ..buttons } };
        assert_eq!(a.clamped().aim, 12345);
        assert!(a.clamped().buttons.fire);
    }

    #[test]
    fn move_clamp_never_exceeds_max() {
        let buttons = ActionButtons { fire: false, jump: false, ability: false, reload: false };
        let max_sq = (MOVE_INTENT_SCALE as i64).pow(2);
        // The {i32::MIN, i32::MIN} corner is the worst case: its i64 magnitude
        // sum is exactly i64::MAX + 1, which overflows without the widened add.
        for (x, y) in [
            (i32::MAX, i32::MAX),
            (i32::MIN, i32::MIN),
            (i32::MIN, i32::MAX),
            (1001, 0),
            (1000, 1000),
            (-30000, 12000),
            (0, i32::MIN + 1),
        ] {
            let c = ActionIntent { move_dir: Vec2 { x, y }, aim: 0, buttons }.clamped();
            let mag_sq = (c.move_dir.x as i64).pow(2) + (c.move_dir.y as i64).pow(2);
            assert!(mag_sq <= max_sq, "clamp let ({x},{y}) through at mag^2={mag_sq} > {max_sq}");
        }
    }

    #[test]
    fn isqrt_is_floor() {
        assert_eq!(isqrt_u64(0), 0);
        assert_eq!(isqrt_u64(1), 1);
        assert_eq!(isqrt_u64(24), 4);
        assert_eq!(isqrt_u64(25), 5);
        assert_eq!(isqrt_u64(26), 5);
        assert_eq!(isqrt_u64(u64::MAX), 4294967295);
    }

    #[test]
    fn action_buttons_bits_pack() {
        assert_eq!(ActionButtons { fire: false, jump: false, ability: false, reload: false }.bits(), 0);
        assert_eq!(ActionButtons { fire: true, jump: false, ability: false, reload: false }.bits(), 1);
        assert_eq!(ActionButtons { fire: false, jump: true, ability: false, reload: false }.bits(), 2);
        assert_eq!(ActionButtons { fire: false, jump: false, ability: true, reload: false }.bits(), 4);
        assert_eq!(ActionButtons { fire: false, jump: false, ability: false, reload: true }.bits(), 8);
        assert_eq!(ActionButtons { fire: true, jump: true, ability: true, reload: true }.bits(), 15);
    }

    #[test]
    fn replay_digest_is_deterministic_and_serde_byte_stable() {
        // FM3: the same match must produce the same bytes and the same hash on
        // every run. The digest is over an integer-only canonical encoding (no
        // float, no map), and serde output is stable because every container is
        // an ordered Vec — so two encodings are byte-identical.
        let rec = sample_replay();
        assert_eq!(rec.digest(), rec.digest(), "digest is not a pure function");
        let a = serde_json::to_string(&rec).unwrap();
        let b = serde_json::to_string(&rec.clone()).unwrap();
        assert_eq!(a, b, "serde encoding is not byte-stable");
    }

    #[test]
    fn replay_digest_golden() {
        // A fixed record must hash to a fixed value. Any change to the canonical
        // encoding (field order, prefixing, domain tag, the v2 blockers / v3 pickups
        // sections) flips this — the hard byte-stability pin for on-chain attestation.
        assert_eq!(
            hex::encode(sample_replay().digest()),
            "7e4f0d7d72e68f5a5883c31dfad94aec3db40a3ad4e1e751215820469f3cbf94"
        );
    }

    #[test]
    fn replay_digest_is_order_and_field_sensitive() {
        let base = sample_replay().digest();
        // Different seed -> different commitment.
        let mut r = sample_replay();
        r.seed ^= 1;
        assert_ne!(base, r.digest(), "seed must bind");
        // Reordered ticks -> different commitment (tick order is canonical).
        let mut r = sample_replay();
        r.ticks.reverse();
        assert_ne!(base, r.digest(), "tick order must bind");
        // A changed action -> different commitment.
        let mut r = sample_replay();
        r.ticks[0].actions[0].intent.aim ^= 1;
        assert_ne!(base, r.digest(), "an action must bind");
        // A changed controller identity -> different commitment.
        let mut r = sample_replay();
        r.seats[0].controller.push('x');
        assert_ne!(base, r.digest(), "roster must bind");
        // An added vision blocker -> different commitment: the perception geometry
        // is bound even though it does not alter the re-run outcome (FM1).
        let mut r = sample_replay();
        r.blockers.push(Blocker { min: Vec2 { x: 1, y: 2 }, max: Vec2 { x: 3, y: 4 } });
        assert_ne!(base, r.digest(), "a blocker must bind");
        // A moved blocker corner -> different commitment.
        let mut r = sample_replay();
        r.blockers.push(Blocker { min: Vec2 { x: 1, y: 2 }, max: Vec2 { x: 3, y: 4 } });
        let with_blocker = r.digest();
        r.blockers[0].max.x += 1;
        assert_ne!(with_blocker, r.digest(), "a blocker's geometry must bind");
        // An added pickup -> different commitment: the item layout is bound (v3), the
        // same role blockers play — an uncollected pickup is invisible to re-execution.
        let mut r = sample_replay();
        r.pickups.push(PickupSpawn { kind: PickupKind::Health, position: Vec2 { x: 5, y: 6 }, amount: 25 });
        let with_pickup = r.digest();
        assert_ne!(base, with_pickup, "a pickup must bind");
        // Its kind, position, and amount each bind.
        let mut r2 = r.clone();
        r2.pickups[0].kind = PickupKind::Ammo;
        assert_ne!(with_pickup, r2.digest(), "a pickup's kind must bind");
        let mut r2 = r.clone();
        r2.pickups[0].position.x += 1;
        assert_ne!(with_pickup, r2.digest(), "a pickup's position must bind");
        let mut r2 = r.clone();
        r2.pickups[0].amount += 1;
        assert_ne!(with_pickup, r2.digest(), "a pickup's amount must bind");
    }

    #[test]
    fn replay_record_wire_shape_is_stable() {
        let rec = sample_replay();
        let json = serde_json::to_value(&rec).unwrap();
        let round: ReplayRecord = serde_json::from_value(json).unwrap();
        assert_eq!(round, rec, "ReplayRecord did not round-trip");
    }

    #[test]
    fn match_result_wire_shape_is_stable_and_commits_to_replay() {
        let rec = sample_replay();
        let canonical = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "match_id": FIXED_MATCH,
            "final_tick": 2,
            "outcomes": [
                { "seat": 0, "team": 1, "placement": 1, "score": 3, "alive_at_end": true },
                { "seat": 1, "team": 2, "placement": 2, "score": 1, "alive_at_end": false }
            ],
            "replay_hash": hex::encode(rec.digest())
        });
        let parsed: MatchResult = serde_json::from_value(canonical.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), canonical, "MatchResult wire shape drifted");
        assert_eq!(parsed.replay_hash, hex::encode(rec.digest()), "result must commit to the replay digest");
    }

    #[test]
    fn match_config_wire_shape_is_stable() {
        let canonical = serde_json::json!({
            "tick_hz": 30, "max_ticks": 3600, "bounds": { "x": 50_000, "y": 50_000 }, "seats": 8
        });
        let parsed: MatchConfig = serde_json::from_value(canonical.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), canonical, "MatchConfig wire shape drifted");
    }

    #[test]
    fn gateway_msg_wire_shapes_are_stable() {
        // Challenge — struct variant; the anti-replay nonce is a hex string.
        let challenge = serde_json::json!({ "type": "challenge", "nonce": "0a1b2c3d4e5f60718293a4b5c6d7e8f9" });
        assert_round::<GatewayMsg>(&challenge, "GatewayMsg::Challenge");

        // Welcome — struct variant.
        let welcome = serde_json::json!({
            "type": "welcome", "protocol_version": PROTOCOL_VERSION, "match_id": FIXED_MATCH, "seat": 2
        });
        assert_round::<GatewayMsg>(&welcome, "GatewayMsg::Welcome");

        // Reject — struct variant.
        let reject = serde_json::json!({ "type": "reject", "reason": "version mismatch" });
        assert_round::<GatewayMsg>(&reject, "GatewayMsg::Reject");

        // Start — struct variant carrying the config.
        let start = serde_json::json!({
            "type": "start", "match_id": FIXED_MATCH,
            "config": { "tick_hz": 30, "max_ticks": 3600, "bounds": { "x": 50_000, "y": 50_000 }, "seats": 8 }
        });
        assert_round::<GatewayMsg>(&start, "GatewayMsg::Start");

        // Observe — newtype variant: the Observation fields flatten next to "type".
        let mut observe = serde_json::to_value(sample_observation()).unwrap();
        observe.as_object_mut().unwrap().insert("type".into(), "observe".into());
        assert_round::<GatewayMsg>(&observe, "GatewayMsg::Observe");

        // End — newtype variant: the MatchResult fields flatten next to "type".
        let result = MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id: FIXED_MATCH.parse().unwrap(),
            final_tick: 2,
            outcomes: vec![SeatOutcome { seat: 0, team: 1, placement: 1, score: 3, alive_at_end: true }],
            replay_hash: hex::encode(sample_replay().digest()),
        };
        let mut end = serde_json::to_value(&result).unwrap();
        end.as_object_mut().unwrap().insert("type".into(), "end".into());
        assert_round::<GatewayMsg>(&end, "GatewayMsg::End");
    }

    #[test]
    fn agent_msg_wire_shapes_are_stable() {
        // Join — struct variant.
        let join = serde_json::json!({
            "type": "join", "protocol_version": PROTOCOL_VERSION,
            "agent_id": "0xabcdef1234567890abcdef1234567890abcdef12", "signature_hex": "0xcafe"
        });
        assert_round::<AgentMsg>(&join, "AgentMsg::Join");

        // Act — newtype variant: the Action fields flatten next to "type".
        let mut act = serde_json::to_value(sample_action()).unwrap();
        act.as_object_mut().unwrap().insert("type".into(), "act".into());
        assert_round::<AgentMsg>(&act, "AgentMsg::Act");

        // Leave — struct variant.
        let leave = serde_json::json!({ "type": "leave", "reason": "forfeit" });
        assert_round::<AgentMsg>(&leave, "AgentMsg::Leave");
    }

    #[test]
    fn join_version_drives_welcome_or_reject() {
        // The handshake: a matching version is welcomed onto a seat; a drifted
        // version is rejected before any match state exists.
        let decide = |v: u32| -> GatewayMsg {
            match check_version(v) {
                Ok(()) => GatewayMsg::Welcome {
                    protocol_version: PROTOCOL_VERSION,
                    match_id: FIXED_MATCH.parse().unwrap(),
                    seat: 0,
                },
                Err(m) => GatewayMsg::Reject { reason: m.to_string() },
            }
        };
        assert!(matches!(decide(PROTOCOL_VERSION), GatewayMsg::Welcome { .. }));
        assert!(matches!(decide(PROTOCOL_VERSION + 1), GatewayMsg::Reject { .. }));
    }

    fn assert_round<T: Serialize + for<'de> Deserialize<'de>>(canonical: &serde_json::Value, what: &str) {
        let parsed: T = serde_json::from_value(canonical.clone()).unwrap();
        assert_eq!(&serde_json::to_value(&parsed).unwrap(), canonical, "{what} wire shape drifted");
    }

    fn sample_replay() -> ReplayRecord {
        let buttons = ActionButtons { fire: true, jump: false, ability: false, reload: false };
        ReplayRecord {
            protocol_version: PROTOCOL_VERSION,
            match_id: FIXED_MATCH.parse().unwrap(),
            seed: 0xdead_beef,
            seats: vec![
                SeatInfo { seat: 0, team: 1, controller: "0xaaaa".into() },
                SeatInfo { seat: 1, team: 2, controller: "0xbbbb".into() },
            ],
            blockers: Vec::new(),
            pickups: Vec::new(),
            ticks: vec![
                TickRecord {
                    tick: 0,
                    actions: vec![SeatAction {
                        seat: 0,
                        intent: ActionIntent { move_dir: Vec2 { x: 600, y: 800 }, aim: 0x4000, buttons },
                    }],
                },
                TickRecord {
                    tick: 1,
                    actions: vec![SeatAction {
                        seat: 1,
                        intent: ActionIntent { move_dir: Vec2 { x: -100, y: 0 }, aim: 0, buttons },
                    }],
                },
            ],
        }
    }

    fn sample_action() -> Action {
        Action {
            protocol_version: PROTOCOL_VERSION,
            match_id: FIXED_MATCH.parse().unwrap(),
            seat: 3,
            tick: 128,
            intent: ActionIntent {
                move_dir: Vec2 { x: 600, y: 800 },
                aim: 0x4000,
                buttons: ActionButtons { fire: true, jump: false, ability: false, reload: false },
            },
        }
    }

    fn sample_observation() -> Observation {
        Observation {
            protocol_version: PROTOCOL_VERSION,
            match_id: FIXED_MATCH.parse().unwrap(),
            seat: 0,
            tick: 128,
            phase: MatchPhase::Live,
            deadline_micros: 50_000,
            own: SelfState {
                seat: 0,
                team: 1,
                position: Vec2::ZERO,
                z: 0,
                facing: 0x4000,
                velocity: Vec2::ZERO,
                health: 100,
                max_health: 100,
                ammo: 30,
                cooldown: 5,
                alive: true,
            },
            visible: vec![VisibleEntity {
                entity_id: 7,
                kind: EntityKind::Player,
                team: 2,
                position: Vec2 { x: 1000, y: -2000 },
                z: 0,
                facing: 0x4000,
                in_line_of_sight: true,
            }],
        }
    }

    fn sample_broadcast() -> Broadcast {
        Broadcast {
            protocol_version: PROTOCOL_VERSION,
            match_id: FIXED_MATCH.parse().unwrap(),
            tick: 64,
            phase: MatchPhase::Live,
            entities: vec![BroadcastEntity {
                entity_id: 0,
                kind: EntityKind::Player,
                team: 1,
                position: Vec2 { x: -1000, y: 500 },
                z: 0,
                facing: 0x4000,
                health: 80,
                max_health: 100,
                score: 42,
                alive: true,
            }],
        }
    }
}
