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
