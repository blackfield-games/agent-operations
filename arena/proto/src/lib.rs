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
//! - **Explicit versioning.** Every top-level envelope carries
//!   [`PROTOCOL_VERSION`]; the handshake rejects a mismatch, so the Rust arena
//!   and the UE5 server cannot silently diverge as they evolve.
//! - **Canonical, deterministic encoding.** All spatial quantities are integer
//!   fixed-point (no floats), so the same match produces byte-identical replay
//!   bytes and the same hash on any platform — the basis for on-chain
//!   attestation and reproducible grading.
//!
//! Like `mesh/proto`, this is plain serde JSON: the wire form is the serde
//! representation and transport lives in the arena/UE5 crates.

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
    Player,
    Projectile,
    Pickup,
}

/// The receiving seat's OWN pawn state, in full. A controller always knows its
/// own health, ammo, position and facing — this is the one place full internal
/// state appears in an [`Observation`], and it is always the receiver's own, so
/// it grants no perception advantage.
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
    /// `true` if in the seat's line of sight this tick; `false` if this is a
    /// last-known position the seat has since lost sight of.
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
        let mag_sq = (x * x + y * y) as u64;
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

/// The full deterministic record of a match — the PRNG seed, the roster, and the
/// ordered per-tick accepted-action stream — sufficient to re-run the match
/// bit-for-bit and reproduce its [`MatchResult`].
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
        h.update(b"blackfield/arena/replay/v1");
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

#[cfg(test)]
mod tests {
    use super::*;

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
                "health": 100, "max_health": 100, "ammo": 30, "alive": true
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
        for (x, y) in [(i32::MAX, i32::MAX), (1001, 0), (1000, 1000), (-30000, 12000), (0, i32::MIN + 1)] {
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
        // encoding (field order, prefixing, domain tag) flips this — the
        // hard byte-stability pin for on-chain attestation.
        assert_eq!(
            hex::encode(sample_replay().digest()),
            "35f5283a7492c4e72534fd6e40dad73996571ffe8d17972f0bf8cd9497bca005"
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
}
