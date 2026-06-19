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
}
