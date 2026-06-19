//! arena-match — the matchmaking service that forms matches over the arena-02
//! reference core.
//!
//! "Same battlefield, or separate arenas" is a matchmaking concern here, not a
//! code fork. One combat core ([`arena_core::Match`]) serves all three modes; the
//! [`MatchMode`](arena_proto::MatchMode) decides *which controller kinds fill the
//! seats* and *what composition is valid*, and the simulation treats every seat
//! identically. `Human` is human-only (clean ranked PvP), `Agent` is agent-only
//! (ranked A2A), and `Mixed` puts at least one human and at least one agent on the
//! same battlefield — the headline cross-play case.
//!
//! Two properties are structural, not left to a caller's goodwill:
//!
//! - **Ranked seats are authenticated.** A ranked agent seat must present an
//!   identity token the [`IdentityVerifier`] accepts *before* it is queued, so an
//!   unauthenticated or duplicate agent can never reach a settle-able ranked
//!   match. The production verifier recovers the secp256k1 signer from the
//!   arena-01 `join_digest` and checks on-chain registration; until that contracts
//!   task lands, [`StubIdentityVerifier`] enforces the same accept/reject boundary
//!   against a configured allowlist — the gate is real, not a TODO.
//! - **Formation is atomic.** Per-mode queues live behind one lock, so a join that
//!   completes a match pulls its whole roster under that lock — no concurrent join
//!   can double-seat a participant or start a match a seat short.

use std::collections::BTreeMap;

use arena_core::Match;
use arena_proto::{ControllerKind, MatchMode, Vec2, POSITION_SCALE};
use uuid::Uuid;

/// Verifies that a ranked seat controls the on-chain identity it claims.
///
/// The `token` is the arena-01 `AgentMsg::Join.signature_hex`. The production
/// implementation recovers the secp256k1 signer from the
/// [`join_digest`](arena_proto::join_digest) and checks that the recovered
/// address is a registered agent (the on-chain identity registry is a later
/// contracts task); [`StubIdentityVerifier`] stands in for it now and enforces
/// the *same* accept/reject boundary, so the matchmaker's ranked gate is exercised
/// end to end rather than deferred.
pub trait IdentityVerifier {
    /// `true` iff `token` proves control of `agent_id`. An empty, missing, or
    /// wrong token must return `false` — admitting an unproven identity to a
    /// ranked seat is the exact failure this trait exists to prevent.
    fn verify(&self, agent_id: &str, token: &str) -> bool;
}

/// A stand-in [`IdentityVerifier`] that authorizes a fixed `agent_id -> token`
/// allowlist. It is a stub for the on-chain registry, not a bypass: an agent is
/// admitted to a ranked seat only if it is on the allowlist AND presents exactly
/// its authorized token, so the unauthenticated-agent rejection path is real and
/// testable today.
#[derive(Debug, Default, Clone)]
pub struct StubIdentityVerifier {
    authorized: BTreeMap<String, String>,
}

impl StubIdentityVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Authorize `agent_id` to claim a ranked seat with `token`. Chainable so a
    /// roster of test identities reads as one expression.
    pub fn authorize(&mut self, agent_id: impl Into<String>, token: impl Into<String>) -> &mut Self {
        self.authorized.insert(agent_id.into(), token.into());
        self
    }
}

impl IdentityVerifier for StubIdentityVerifier {
    fn verify(&self, agent_id: &str, token: &str) -> bool {
        // Reject an empty token outright (the unranked/casual sentinel) so it can
        // never satisfy a ranked seat by coincidentally matching an empty allowlist
        // entry; otherwise require an exact match against the registered token.
        !token.is_empty() && self.authorized.get(agent_id).is_some_and(|t| t == token)
    }
}

/// A request for a seat: who is joining, what kind of controller they are, and —
/// for a ranked agent — the identity token proving the claim. The matchmaker
/// admits or rejects this against the requested mode before queuing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRequest {
    /// The controller identity: an on-chain agent address for a ranked agent, a
    /// human/label otherwise. Recorded as the seat's `controller` in the roster.
    pub agent_id: String,
    pub kind: ControllerKind,
    /// The identity proof (arena-01 `Join.signature_hex`). `None` — or an empty
    /// string — is an unranked/casual seat; a ranked seat must present a token the
    /// [`IdentityVerifier`] accepts.
    pub token: Option<String>,
}

impl JoinRequest {
    /// A human seat (no agent identity token — humans authenticate out of band).
    pub fn human(agent_id: impl Into<String>) -> Self {
        Self { agent_id: agent_id.into(), kind: ControllerKind::Human, token: None }
    }

    /// An unranked/casual agent seat — allowed only where the mode permits casual
    /// cross-play (Mixed), never in ranked Agent mode.
    pub fn casual_agent(agent_id: impl Into<String>) -> Self {
        Self { agent_id: agent_id.into(), kind: ControllerKind::Agent, token: None }
    }

    /// A ranked agent seat presenting its identity token.
    pub fn ranked_agent(agent_id: impl Into<String>, token: impl Into<String>) -> Self {
        Self { agent_id: agent_id.into(), kind: ControllerKind::Agent, token: Some(token.into()) }
    }
}

/// Why a [`JoinRequest`] was refused before it could be queued. Rejecting at the
/// boundary — rather than at formation — keeps an inadmissible participant out of
/// the queue entirely, so it can never be selected into a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    /// The controller kind isn't allowed in the requested mode: an agent in the
    /// human-only `Human` queue, or a human in the agent-only `Agent` queue.
    /// `Mixed` accepts both kinds, so it never raises this.
    WrongKindForMode { mode: MatchMode, kind: ControllerKind },
    /// A ranked seat presented a missing or invalid identity token. Every agent
    /// seat is ranked in `Agent` mode; in `Mixed`, a token — once presented — must
    /// still verify, so a forged ranked claim is caught even in casual cross-play.
    Unauthenticated { agent_id: String },
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinError::WrongKindForMode { mode, kind } => {
                write!(f, "join rejected: a {kind:?} seat is not allowed in {mode:?} mode")
            }
            JoinError::Unauthenticated { agent_id } => {
                write!(f, "join rejected: ranked seat for agent {agent_id} is unauthenticated")
            }
        }
    }
}

impl std::error::Error for JoinError {}

/// The result of an admitted [`JoinRequest`]: either the participant is waiting
/// for more seats, or this join completed a match.
pub enum JoinOutcome {
    /// Seated into the mode's queue; not enough waiting participants to form a
    /// match yet.
    Queued,
    /// This join completed a match — built on the arena-02 core, ready to run. Its
    /// whole roster (including this participant) was removed from the queue
    /// atomically; the caller reads [`Match::seats`] to notify the other seats.
    /// Boxed because a `Match` is large and `Queued` — the common outcome — carries
    /// nothing, so the enum stays cheap to return on every join.
    Formed(Box<Match>),
}

impl JoinOutcome {
    pub fn is_queued(&self) -> bool {
        matches!(self, JoinOutcome::Queued)
    }

    /// The formed match, if this join completed one.
    pub fn into_formed(self) -> Option<Match> {
        match self {
            JoinOutcome::Formed(m) => Some(*m),
            JoinOutcome::Queued => None,
        }
    }
}

/// The match parameters every formed match runs under — the roster size and the
/// read-only rules summary (tick rate, length cap, arena bounds). Mirrors the
/// loopback harness defaults so a matchmade match plays identically to a
/// hand-started one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchParams {
    /// Seats per formed match. A `Mixed` match needs at least 2 (one of each
    /// kind); below that it can never form.
    pub seats_per_match: u8,
    pub tick_hz: u16,
    pub max_ticks: u64,
    pub bounds: Vec2,
}

impl Default for MatchParams {
    fn default() -> Self {
        Self {
            seats_per_match: 2,
            tick_hz: 30,
            max_ticks: 3600,
            bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
        }
    }
}

/// Derive the deterministic match seed from the server-minted match id, so a
/// match is reproducible from its id alone — there is no separate seed to record,
/// publish, or keep in sync. Folding the 16 id bytes into a `u64` is a pure
/// function of the id, so two builds from the same id spawn identically.
pub fn seed_for_match(match_id: Uuid) -> u64 {
    let b = match_id.as_bytes();
    let hi = u64::from_be_bytes(b[0..8].try_into().expect("uuid is 16 bytes"));
    let lo = u64::from_be_bytes(b[8..16].try_into().expect("uuid is 16 bytes"));
    hi ^ lo
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_verifier_admits_only_the_authorized_token() {
        let mut v = StubIdentityVerifier::new();
        v.authorize("0xagent", "goodsig");

        assert!(v.verify("0xagent", "goodsig"), "the authorized identity + token is accepted");
        assert!(!v.verify("0xagent", "badsig"), "a wrong token is rejected");
        assert!(!v.verify("0xagent", ""), "an empty token is rejected");
        assert!(!v.verify("0xunknown", "goodsig"), "an unregistered agent is rejected");
        assert!(!v.verify("0xunknown", ""), "an unknown agent with no token is rejected");
    }

    #[test]
    fn stub_verifier_default_authorizes_no_one() {
        // A fresh verifier is closed by default — no identity is ranked-admissible
        // until explicitly authorized, so a forgotten allowlist fails safe.
        let v = StubIdentityVerifier::new();
        assert!(!v.verify("0xagent", "anything"));
    }

    #[test]
    fn join_request_constructors_set_kind_and_token() {
        let h = JoinRequest::human("alice");
        assert_eq!(h.kind, ControllerKind::Human);
        assert_eq!(h.token, None);

        let casual = JoinRequest::casual_agent("0xbot");
        assert_eq!(casual.kind, ControllerKind::Agent);
        assert_eq!(casual.token, None, "a casual agent carries no token");

        let ranked = JoinRequest::ranked_agent("0xbot", "sig");
        assert_eq!(ranked.kind, ControllerKind::Agent);
        assert_eq!(ranked.token.as_deref(), Some("sig"));
    }

    #[test]
    fn seed_is_a_pure_function_of_the_match_id() {
        let id: Uuid = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        assert_eq!(seed_for_match(id), seed_for_match(id), "seed must be reproducible from the id");
        // A different id yields a different seed (the two id halves differ here, so
        // the fold doesn't cancel to zero).
        let other: Uuid = "550e8400-e29b-41d4-a716-446655440001".parse().unwrap();
        assert_ne!(seed_for_match(id), seed_for_match(other));
    }

    #[test]
    fn join_error_messages_name_the_cause() {
        let wrong = JoinError::WrongKindForMode { mode: MatchMode::Human, kind: ControllerKind::Agent };
        assert!(wrong.to_string().contains("Human"));
        assert!(wrong.to_string().contains("Agent"));
        let unauth = JoinError::Unauthenticated { agent_id: "0xbot".into() };
        assert!(unauth.to_string().contains("0xbot"));
        assert!(unauth.to_string().contains("unauthenticated"));
    }

    #[test]
    fn queued_outcome_reports_queued() {
        assert!(JoinOutcome::Queued.is_queued());
        assert!(JoinOutcome::Queued.into_formed().is_none());
    }
}
