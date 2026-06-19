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
use std::sync::Mutex;

use arena_core::{Match, Rules};
use arena_proto::{ControllerKind, MatchConfig, MatchMode, SeatId, SeatInfo, TeamId, Vec2, POSITION_SCALE};
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

    /// The presented token, treating an empty string as no token (the SDK sends an
    /// empty `signature_hex` for a casual seat, which must not read as a ranked
    /// claim).
    fn presented_token(&self) -> Option<&str> {
        self.token.as_deref().filter(|t| !t.is_empty())
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
    /// Seats per formed match — at least 2 (a PvP match needs two seats to be a
    /// contest, and `Mixed` needs room for one of each kind). [`Matchmaker::new`]
    /// rejects a smaller value rather than queue joins into a match that can never
    /// form or ends the instant it starts.
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

/// One admitted participant waiting for a seat. The token is gone by here — it
/// was verified at [`Matchmaker::join`] before queuing — so the queue holds only
/// what the roster needs.
#[derive(Debug, Clone)]
struct Seated {
    agent_id: String,
    kind: ControllerKind,
}

/// The per-mode waiting queues, all behind the matchmaker's one lock.
#[derive(Default)]
struct Queues {
    human: Vec<Seated>,
    agent: Vec<Seated>,
    mixed: Vec<Seated>,
}

impl Queues {
    fn for_mode(&mut self, mode: MatchMode) -> &mut Vec<Seated> {
        match mode {
            MatchMode::Human => &mut self.human,
            MatchMode::Agent => &mut self.agent,
            MatchMode::Mixed => &mut self.mixed,
        }
    }
}

/// Forms matches from waiting participants by [`MatchMode`], over the arena-02
/// core. Every mode shares one combat core; the mode only governs which
/// controller kinds may fill the seats and what composition is valid.
///
/// The per-mode queues live behind one [`Mutex`], so a [`join`](Matchmaker::join)
/// that completes a match selects and removes its whole roster under that lock —
/// concurrent joins are serialized, so none can double-seat a participant or start
/// a match a seat short. Identity is checked *before* the lock, so an
/// inadmissible join never touches a queue.
pub struct Matchmaker<V> {
    queues: Mutex<Queues>,
    verifier: V,
    params: MatchParams,
}

impl<V: IdentityVerifier> Matchmaker<V> {
    pub fn new(verifier: V, params: MatchParams) -> Self {
        // Fail fast on a degenerate config: a 0-seat matchmaker would queue every
        // join forever, and a 1-seat match is a single-team match the core ends on
        // its first tick. A match needs at least two seats.
        assert!(
            params.seats_per_match >= 2,
            "a match needs at least 2 seats, got {}",
            params.seats_per_match
        );
        Self { queues: Mutex::new(Queues::default()), verifier, params }
    }

    /// Admit `req` to `mode`'s queue, forming a match if it now can. Returns
    /// [`JoinOutcome::Formed`] to the join that completes a match (whose roster is
    /// removed from the queue atomically), [`JoinOutcome::Queued`] otherwise, or a
    /// [`JoinError`] if the request is inadmissible — rejected before it is queued.
    pub fn join(&self, mode: MatchMode, req: JoinRequest) -> Result<JoinOutcome, JoinError> {
        self.admit(mode, &req)?;
        let seated = Seated { agent_id: req.agent_id, kind: req.kind };
        let roster = {
            let mut q = self.queues.lock().expect("matchmaker mutex poisoned");
            let queue = q.for_mode(mode);
            queue.push(seated);
            try_form(mode, queue, self.params.seats_per_match as usize)
        };
        Ok(match roster {
            Some(roster) => JoinOutcome::Formed(Box::new(self.build(mode, roster))),
            None => JoinOutcome::Queued,
        })
    }

    /// Kind + identity admission, run *before* the queue lock so a rejected join
    /// never enters a queue — verifying at formation instead would let an
    /// unauthenticated agent sit in the ranked queue until it was selected.
    fn admit(&self, mode: MatchMode, req: &JoinRequest) -> Result<(), JoinError> {
        match (mode, req.kind) {
            (MatchMode::Human, ControllerKind::Agent) | (MatchMode::Agent, ControllerKind::Human) => {
                return Err(JoinError::WrongKindForMode { mode, kind: req.kind });
            }
            _ => {}
        }
        if req.kind == ControllerKind::Agent {
            match req.presented_token() {
                Some(token) if self.verifier.verify(&req.agent_id, token) => {}
                Some(_) => return Err(JoinError::Unauthenticated { agent_id: req.agent_id.clone() }),
                // Agent mode is ranked: every agent seat must authenticate. Mixed
                // admits a token-less agent as a casual cross-play seat.
                None if mode == MatchMode::Agent => {
                    return Err(JoinError::Unauthenticated { agent_id: req.agent_id.clone() });
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Build the arena-02 match from a formed roster: seat the participants in
    /// arrival order, each its own team (a free-for-all, so the match doesn't
    /// end the instant it starts), and seed it from the freshly minted match id.
    fn build(&self, mode: MatchMode, roster: Vec<Seated>) -> Match {
        // The mode's composition is guaranteed by selection; assert it before the
        // match starts so a selection bug fails loud rather than starting a match
        // that defeats its own mode (e.g. an all-human "Mixed" match).
        assert!(composition_ok(mode, &roster), "formed a {mode:?} match with an invalid composition");
        let match_id = Uuid::new_v4();
        let seats: Vec<SeatInfo> = roster
            .iter()
            .enumerate()
            .map(|(i, s)| SeatInfo { seat: i as SeatId, team: i as TeamId, controller: s.agent_id.clone() })
            .collect();
        let config = MatchConfig {
            tick_hz: self.params.tick_hz,
            max_ticks: self.params.max_ticks,
            bounds: self.params.bounds,
            seats: seats.len() as u8,
        };
        Match::new(match_id, config, Rules::default(), seats, seed_for_match(match_id))
    }

    /// How many participants are waiting in `mode`'s queue — for observability and
    /// tests.
    pub fn waiting(&self, mode: MatchMode) -> usize {
        let mut q = self.queues.lock().expect("matchmaker mutex poisoned");
        q.for_mode(mode).len()
    }
}

/// Pull a full roster from `queue` if `mode`'s composition can be met, removing
/// exactly those participants; otherwise leave the queue untouched and return
/// `None`. Called under the matchmaker lock, so selection + removal is atomic.
fn try_form(mode: MatchMode, queue: &mut Vec<Seated>, seats: usize) -> Option<Vec<Seated>> {
    if seats == 0 || queue.len() < seats {
        return None;
    }
    let mut picks = match mode {
        // Human/Agent queues are single-kind by admission, so the first `seats`
        // waiting (FIFO) already satisfy the composition.
        MatchMode::Human | MatchMode::Agent => (0..seats).collect::<Vec<usize>>(),
        MatchMode::Mixed => select_mixed(queue, seats)?,
    };
    picks.sort_unstable();
    // Remove from the back so earlier indices stay valid; that yields the picks in
    // descending order, so reverse to restore arrival (FIFO) order — deterministic
    // seating, independent of which join triggered formation.
    let mut roster: Vec<Seated> = picks.iter().rev().map(|&i| queue.remove(i)).collect();
    roster.reverse();
    Some(roster)
}

/// Choose `seats` participants for a Mixed match guaranteed to hold at least one
/// human AND at least one agent, so a Mixed match can NEVER form all-one-kind —
/// the headline cross-play property. Takes the earliest-waiting human and agent
/// first, then fills the rest in arrival order. `None` if either kind is absent
/// (or there is no room for one of each).
fn select_mixed(queue: &[Seated], seats: usize) -> Option<Vec<usize>> {
    if seats < 2 {
        return None;
    }
    let first_human = queue.iter().position(|s| s.kind == ControllerKind::Human)?;
    let first_agent = queue.iter().position(|s| s.kind == ControllerKind::Agent)?;
    let mut picks = vec![first_human, first_agent];
    for i in 0..queue.len() {
        if picks.len() == seats {
            break;
        }
        if i != first_human && i != first_agent {
            picks.push(i);
        }
    }
    Some(picks)
}

/// Does `roster` satisfy `mode`'s seat-composition rule? The selection guarantees
/// it; this is the assertion checked before a formed match starts, so a mode whose
/// composition is unenforced can never slip through and defeat the mode.
fn composition_ok(mode: MatchMode, roster: &[Seated]) -> bool {
    let agents = roster.iter().filter(|s| s.kind == ControllerKind::Agent).count();
    let humans = roster.len() - agents;
    match mode {
        MatchMode::Human => agents == 0 && humans > 0,
        MatchMode::Agent => humans == 0 && agents > 0,
        MatchMode::Mixed => humans > 0 && agents > 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_proto::MatchPhase;

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

    fn open_mm() -> Matchmaker<StubIdentityVerifier> {
        Matchmaker::new(StubIdentityVerifier::new(), MatchParams::default())
    }

    fn ranked_mm(authorized: &[(&str, &str)]) -> Matchmaker<StubIdentityVerifier> {
        let mut v = StubIdentityVerifier::new();
        for (id, token) in authorized {
            v.authorize(*id, *token);
        }
        Matchmaker::new(v, MatchParams::default())
    }

    fn controllers(m: &Match) -> Vec<String> {
        let mut who: Vec<String> = m.seats().iter().map(|s| s.controller.clone()).collect();
        who.sort();
        who
    }

    #[test]
    fn human_mode_forms_an_all_human_match() {
        let mm = open_mm();
        assert!(mm.join(MatchMode::Human, JoinRequest::human("alice")).unwrap().is_queued());
        let m = mm
            .join(MatchMode::Human, JoinRequest::human("bob"))
            .unwrap()
            .into_formed()
            .expect("the second human completes the 2-seat match");
        assert_eq!(m.phase(), MatchPhase::Live);
        assert_eq!(controllers(&m), ["alice", "bob"]);
        assert_eq!(mm.waiting(MatchMode::Human), 0, "the roster left the queue");
    }

    #[test]
    fn human_mode_rejects_an_agent_seat() {
        // FM1: a Human ranked match must never admit an agent seat.
        let mm = open_mm();
        let r = mm.join(MatchMode::Human, JoinRequest::casual_agent("0xbot"));
        assert!(matches!(
            r,
            Err(JoinError::WrongKindForMode { mode: MatchMode::Human, kind: ControllerKind::Agent })
        ));
        assert_eq!(mm.waiting(MatchMode::Human), 0, "a rejected join never enters the queue");
    }

    #[test]
    fn agent_mode_rejects_a_human_seat() {
        let mm = open_mm();
        let r = mm.join(MatchMode::Agent, JoinRequest::human("alice"));
        assert!(matches!(
            r,
            Err(JoinError::WrongKindForMode { mode: MatchMode::Agent, kind: ControllerKind::Human })
        ));
    }

    #[test]
    fn agent_mode_forms_from_authorized_agents() {
        let mm = ranked_mm(&[("0xa", "siga"), ("0xb", "sigb")]);
        assert!(mm.join(MatchMode::Agent, JoinRequest::ranked_agent("0xa", "siga")).unwrap().is_queued());
        let m = mm
            .join(MatchMode::Agent, JoinRequest::ranked_agent("0xb", "sigb"))
            .unwrap()
            .into_formed()
            .expect("two authorized agents form a ranked match");
        assert_eq!(controllers(&m), ["0xa", "0xb"]);
        assert_eq!(m.phase(), MatchPhase::Live);
    }

    #[test]
    fn mixed_needs_both_kinds_to_form() {
        // FM1: a Mixed match must not start all-one-kind. Humans alone never form…
        let humans = open_mm();
        assert!(humans.join(MatchMode::Mixed, JoinRequest::human("h1")).unwrap().is_queued());
        assert!(humans.join(MatchMode::Mixed, JoinRequest::human("h2")).unwrap().is_queued());
        assert_eq!(humans.waiting(MatchMode::Mixed), 2, "no Mixed match from humans alone");
        // …and casual agents alone never form.
        let agents = open_mm();
        assert!(agents.join(MatchMode::Mixed, JoinRequest::casual_agent("a1")).unwrap().is_queued());
        assert!(agents.join(MatchMode::Mixed, JoinRequest::casual_agent("a2")).unwrap().is_queued());
        assert_eq!(agents.waiting(MatchMode::Mixed), 2, "no Mixed match from agents alone");
    }

    #[test]
    fn mixed_always_includes_an_agent_even_when_humans_dominate() {
        // Selection takes one-of-each first, so even a queue stacked with humans
        // forms a Mixed match around the single agent — never an all-human one.
        let mm = open_mm();
        assert!(mm.join(MatchMode::Mixed, JoinRequest::human("h1")).unwrap().is_queued());
        assert!(mm.join(MatchMode::Mixed, JoinRequest::human("h2")).unwrap().is_queued());
        let m = mm
            .join(MatchMode::Mixed, JoinRequest::casual_agent("a1"))
            .unwrap()
            .into_formed()
            .expect("the agent completes a Mixed match");
        let who = controllers(&m);
        assert!(who.contains(&"a1".to_string()), "the agent must be seated, never an all-human Mixed match");
        assert!(who.contains(&"h1".to_string()), "the earliest waiting human is seated (FIFO)");
        assert_eq!(who.len(), 2);
        assert_eq!(mm.waiting(MatchMode::Mixed), 1, "the surplus human keeps waiting");
    }

    #[test]
    fn composition_ok_enforces_each_mode() {
        let human = Seated { agent_id: "h".into(), kind: ControllerKind::Human };
        let agent = Seated { agent_id: "a".into(), kind: ControllerKind::Agent };
        assert!(composition_ok(MatchMode::Human, &[human.clone(), human.clone()]));
        assert!(!composition_ok(MatchMode::Human, &[human.clone(), agent.clone()]));
        assert!(!composition_ok(MatchMode::Human, &[]));
        assert!(composition_ok(MatchMode::Agent, &[agent.clone(), agent.clone()]));
        assert!(!composition_ok(MatchMode::Agent, &[agent.clone(), human.clone()]));
        assert!(composition_ok(MatchMode::Mixed, &[human.clone(), agent.clone()]));
        assert!(!composition_ok(MatchMode::Mixed, &[human.clone(), human.clone()]));
        assert!(!composition_ok(MatchMode::Mixed, &[agent.clone(), agent.clone()]));
    }

    #[test]
    fn a_formed_match_starts_on_the_real_core() {
        let mm = open_mm();
        mm.join(MatchMode::Human, JoinRequest::human("p0")).unwrap();
        let m = mm.join(MatchMode::Human, JoinRequest::human("p1")).unwrap().into_formed().unwrap();
        // A real arena-02 match: Live at tick 0, two seats on distinct teams (a
        // free-for-all, so it does not end instantly), reproducible from its id.
        assert_eq!(m.phase(), MatchPhase::Live);
        assert_eq!(m.tick(), 0);
        assert_eq!(m.config().seats, 2);
        assert_eq!(m.seats()[0].team, 0);
        assert_eq!(m.seats()[1].team, 1);
        assert_eq!(m.seed(), seed_for_match(m.match_id()), "the seed derives from the minted id");
        assert_eq!(m.observe(0).own.seat, 0, "the core yields a real parity-bounded observation");
    }

    #[test]
    #[should_panic(expected = "at least 2 seats")]
    fn a_matchmaker_rejects_a_sub_two_seat_config() {
        // A 0-seat matchmaker would queue forever and a 1-seat match ends instantly;
        // both are caller bugs, so construction fails loud rather than degrading.
        Matchmaker::new(
            StubIdentityVerifier::new(),
            MatchParams { seats_per_match: 1, ..MatchParams::default() },
        );
    }

    #[test]
    fn concurrent_joins_form_matches_atomically() {
        // FM2: under concurrent joins, formation must not double-seat one
        // participant or start a match a seat short. Many humans join at once; the
        // one lock serializes join+form, so each participant is seated exactly once.
        use std::sync::Arc;
        use std::thread;

        const N: usize = 64;
        let mm = Arc::new(open_mm());
        let formed: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..N)
            .map(|i| {
                let mm = Arc::clone(&mm);
                let formed = Arc::clone(&formed);
                thread::spawn(move || {
                    let outcome = mm.join(MatchMode::Human, JoinRequest::human(format!("p{i}"))).unwrap();
                    if let Some(m) = outcome.into_formed() {
                        let roster = m.seats().iter().map(|s| s.controller.clone()).collect::<Vec<_>>();
                        formed.lock().unwrap().push(roster);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let formed = Arc::try_unwrap(formed).unwrap().into_inner().unwrap();
        let mut seated: Vec<String> = formed.iter().flatten().cloned().collect();
        seated.sort();
        let mut expected: Vec<String> = (0..N).map(|i| format!("p{i}")).collect();
        expected.sort();
        assert_eq!(seated, expected, "every participant seated exactly once — no double-seat, no lost seat");
        assert_eq!(formed.len(), N / 2, "N participants form N/2 two-seat matches");
        assert!(formed.iter().all(|r| r.len() == 2), "every formed match is full");
        assert_eq!(mm.waiting(MatchMode::Human), 0, "no participant stranded in the queue");
    }

    #[test]
    fn agent_mode_rejects_unauthenticated_and_admits_authorized() {
        // FM3: the ranked Agent queue must reject a missing/invalid/foreign
        // identity token before it can reach a settle-able match.
        let mm = ranked_mm(&[("0xa", "goodsig")]);
        let rejected = |r: Result<JoinOutcome, JoinError>, who: &str| match r {
            Err(JoinError::Unauthenticated { agent_id }) => assert_eq!(agent_id, who),
            _ => panic!("expected Unauthenticated for {who}"),
        };
        // No token — every Agent-mode seat is ranked.
        rejected(mm.join(MatchMode::Agent, JoinRequest::casual_agent("0xa")), "0xa");
        // A wrong token for a known agent.
        rejected(mm.join(MatchMode::Agent, JoinRequest::ranked_agent("0xa", "badsig")), "0xa");
        // A valid-looking token for an unregistered agent.
        rejected(mm.join(MatchMode::Agent, JoinRequest::ranked_agent("0xb", "goodsig")), "0xb");
        assert_eq!(mm.waiting(MatchMode::Agent), 0, "no unauthenticated agent entered the ranked queue");

        // The authorized identity is admitted.
        assert!(mm.join(MatchMode::Agent, JoinRequest::ranked_agent("0xa", "goodsig")).unwrap().is_queued());
        assert_eq!(mm.waiting(MatchMode::Agent), 1);
    }

    #[test]
    fn mixed_rejects_a_forged_ranked_token_but_admits_casual() {
        // Mixed allows a casual (token-less) agent for cross-play, but a PRESENTED
        // token is a ranked claim — a forged one is rejected, so a bad ranked claim
        // cannot slip in disguised as casual play.
        let mm = ranked_mm(&[("0xgood", "goodsig")]);
        assert!(mm.join(MatchMode::Mixed, JoinRequest::casual_agent("0xcasual")).unwrap().is_queued());
        let forged = mm.join(MatchMode::Mixed, JoinRequest::ranked_agent("0xevil", "forged"));
        assert!(matches!(forged, Err(JoinError::Unauthenticated { .. })));
        assert!(mm.join(MatchMode::Mixed, JoinRequest::ranked_agent("0xgood", "goodsig")).unwrap().is_queued());
        // Two agents, no human → no match formed; the forged join never queued.
        assert_eq!(mm.waiting(MatchMode::Mixed), 2);
    }
}
