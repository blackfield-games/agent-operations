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
}
