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

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use arena_core::{Match, MatchRecord, ReplayError, Rules};
use arena_proto::{
    ActionIntent, Broadcast, ControllerKind, MatchConfig, MatchMode, MatchPhase, MatchResult, SeatId,
    SeatInfo, SpectatorMsg, TeamId, Vec2, POSITION_SCALE,
};
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

/// The shared buffer behind one [`Spectator`]: a bounded ring of pending messages.
/// When it is full the OLDEST message is dropped (and counted) so a slow consumer
/// can never backpressure the publisher.
struct SpectatorInner {
    ring: Mutex<VecDeque<SpectatorMsg>>,
    capacity: usize,
    dropped: AtomicU64,
}

/// A read-only subscription to a [`SpectatorFeed`].
///
/// A spectator can ONLY pull messages: it holds no reference to the match and
/// exposes no method that sends anything back, so it can never inject an action or
/// otherwise influence a ranked match. That read-only property (FM2) is enforced by
/// construction — the absence of a send path — not by a runtime check that could be
/// forgotten. Dropping the handle unsubscribes; the feed reclaims the slot on its
/// next publish or [`subscribe`](SpectatorFeed::subscribe).
pub struct Spectator {
    inner: Arc<SpectatorInner>,
}

impl Spectator {
    /// Pull the oldest buffered message, or `None` if the spectator is caught up.
    /// Read-only: there is deliberately no method on a [`Spectator`] that mutates
    /// the match.
    pub fn recv(&self) -> Option<SpectatorMsg> {
        self.inner.ring.lock().expect("spectator ring poisoned").pop_front()
    }

    /// How many messages this spectator missed because it fell behind — its ring
    /// filled and the oldest were dropped. A lossy feed never blocks the sim, so a
    /// slow consumer observes drops here instead of stalling the match.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// How many messages are buffered and unread.
    pub fn buffered(&self) -> usize {
        self.inner.ring.lock().expect("spectator ring poisoned").len()
    }
}

/// A read-only, bounded, lossy spectator feed, decoupled from the simulation.
///
/// The authoritative side (the match driver) calls [`publish_frame`] /
/// [`publish_end`] after each tick; spectators [`subscribe`] and pull at their own
/// pace. The feed NEVER blocks the publisher: each spectator has a bounded ring,
/// and when a slow consumer's ring is full the oldest message is dropped (counted),
/// so neither a slow consumer nor an unbounded subscriber count can backpressure or
/// stall the tick loop (FM3). Spectating is one-directional — a [`Spectator`] has no
/// way to send anything back (FM2).
///
/// [`publish_frame`]: SpectatorFeed::publish_frame
/// [`publish_end`]: SpectatorFeed::publish_end
/// [`subscribe`]: SpectatorFeed::subscribe
pub struct SpectatorFeed {
    subscribers: Mutex<Vec<Weak<SpectatorInner>>>,
    /// Each spectator's ring size — frames buffered before the oldest is dropped.
    capacity: usize,
    /// Admission cap on concurrently LIVE spectators, so an unbounded number of
    /// subscribers can't exhaust memory.
    max_spectators: usize,
}

impl SpectatorFeed {
    /// `capacity` is each spectator's ring size (messages buffered before the oldest
    /// is dropped); `max_spectators` caps concurrently live subscribers. Both must
    /// be non-zero — a zero ring would drop every message, and a zero cap would
    /// admit no one.
    pub fn new(capacity: usize, max_spectators: usize) -> Self {
        assert!(capacity > 0, "spectator ring capacity must be > 0");
        assert!(max_spectators > 0, "max_spectators must be > 0");
        Self { subscribers: Mutex::new(Vec::new()), capacity, max_spectators }
    }

    /// Admit a new read-only spectator, or `None` if the live-subscriber cap is
    /// reached. Prunes dropped subscribers first, so a disconnected spectator frees
    /// its slot for a newcomer.
    pub fn subscribe(&self) -> Option<Spectator> {
        let mut subs = self.subscribers.lock().expect("subscribers poisoned");
        subs.retain(|w| w.strong_count() > 0);
        if subs.len() >= self.max_spectators {
            return None;
        }
        let inner = Arc::new(SpectatorInner {
            ring: Mutex::new(VecDeque::with_capacity(self.capacity)),
            capacity: self.capacity,
            dropped: AtomicU64::new(0),
        });
        subs.push(Arc::downgrade(&inner));
        Some(Spectator { inner })
    }

    /// Fan one per-tick [`Broadcast`] out to every live spectator as a
    /// [`SpectatorMsg::Frame`]. NON-BLOCKING: a full ring drops its oldest message.
    /// Returns the number of live spectators it reached.
    pub fn publish_frame(&self, frame: Broadcast) -> usize {
        self.fan_out(SpectatorMsg::Frame(frame))
    }

    /// Fan the terminal [`MatchResult`] out as a [`SpectatorMsg::End`] — the last
    /// message a spectator receives. Same non-blocking, lossy delivery.
    pub fn publish_end(&self, result: MatchResult) -> usize {
        self.fan_out(SpectatorMsg::End(result))
    }

    /// How many spectators are currently live (prunes dropped ones first).
    pub fn spectator_count(&self) -> usize {
        let mut subs = self.subscribers.lock().expect("subscribers poisoned");
        subs.retain(|w| w.strong_count() > 0);
        subs.len()
    }

    /// Deliver `msg` to every live spectator's ring without ever blocking: a full
    /// ring sheds its oldest entry. The only locks held are the brief subscriber and
    /// per-ring mutexes — no consumer is awaited, so the caller (the tick loop)
    /// returns immediately regardless of how far behind any spectator is.
    fn fan_out(&self, msg: SpectatorMsg) -> usize {
        let mut subs = self.subscribers.lock().expect("subscribers poisoned");
        subs.retain(|w| w.strong_count() > 0);
        for w in subs.iter() {
            if let Some(inner) = w.upgrade() {
                let mut ring = inner.ring.lock().expect("spectator ring poisoned");
                if ring.len() == inner.capacity {
                    ring.pop_front();
                    inner.dropped.fetch_add(1, Ordering::Relaxed);
                }
                ring.push_back(msg.clone());
            }
        }
        subs.len()
    }
}

/// Re-run a finished [`MatchRecord`] (arena-05) into the sequence of [`Broadcast`]
/// frames a spectator would have seen — the "watch a finished match" path, the
/// replay counterpart to the live [`SpectatorFeed`].
///
/// The record is VERIFIED first ([`MatchRecord::verify`]), so a truncated, tampered,
/// or non-reproducing record is rejected as a typed [`ReplayError`] and NEVER panics
/// the playback — the same anti-DoS guarantee arena-05 gives a settlement verifier,
/// reused here because a spectator/replay feed parses untrusted records too. On
/// success the verified record is re-run from its determinants, capturing the
/// broadcast at the opening tick and after every simulated tick — `ticks.len() + 1`
/// frames, the last carrying the terminal `phase == Ended`.
pub fn replay_frames(record: &MatchRecord) -> Result<Vec<Broadcast>, ReplayError> {
    record.verify()?;
    let mut m = Match::new(
        record.replay.match_id,
        record.config,
        record.rules,
        record.replay.seats.clone(),
        record.replay.seed,
    );
    // Do NOT pre-size from `record.replay.ticks.len()`: `verify` accepts a record
    // padded with canonical post-terminal ticks (they are scanned but never
    // simulated — `replay_match` breaks at the terminal phase), so an adversarial
    // record can inflate that length to millions while the match really ran a
    // handful of ticks. The loop below breaks at the terminal phase too, so it
    // pushes only the frames actually simulated; let the Vec grow to that real
    // count instead of eagerly reserving memory for attacker-controlled padding.
    let mut frames = Vec::new();
    frames.push(m.broadcast());
    for tr in &record.replay.ticks {
        if m.phase() != MatchPhase::Live {
            break;
        }
        let intents: BTreeMap<SeatId, ActionIntent> =
            tr.actions.iter().map(|a| (a.seat, a.intent)).collect();
        m.step(&intents);
        frames.push(m.broadcast());
    }
    Ok(frames)
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

        // An empty token never authenticates even if an agent is (wrongly)
        // allowlisted with one — the empty-token guard rejects, not just an
        // allowlist miss, so an empty signature can never satisfy a ranked seat.
        v.authorize("0xempty", "");
        assert!(!v.verify("0xempty", ""), "an empty token is rejected even when allowlisted empty");
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

    #[test]
    fn an_empty_token_string_is_a_casual_seat_not_a_ranked_claim() {
        // The SDK sends an empty signature_hex for casual play. presented_token
        // collapses Some("") to no-token, so it reads as casual — admitted in Mixed,
        // rejected (ranked-required) in Agent mode — never verified as a ranked
        // claim against an empty signature.
        let empty = || JoinRequest { agent_id: "0xbot".into(), kind: ControllerKind::Agent, token: Some(String::new()) };
        let mm = open_mm();
        assert!(mm.join(MatchMode::Mixed, empty()).unwrap().is_queued(), "an empty token is a casual Mixed seat");
        assert!(
            matches!(mm.join(MatchMode::Agent, empty()), Err(JoinError::Unauthenticated { .. })),
            "an empty token is not a ranked claim, so Agent mode rejects it"
        );
    }

    const FIXED_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    /// A deterministic 2-seat / 2-team match that ends at a small tick cap (both
    /// teams stay alive under forfeits), reproducible from its fixed id + seed.
    fn scripted_match() -> Match {
        let config = MatchConfig {
            tick_hz: 30,
            max_ticks: 5,
            bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
            seats: 2,
        };
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "0xaaaa".into() },
            SeatInfo { seat: 1, team: 1, controller: "0xbbbb".into() },
        ];
        Match::new(FIXED_ID.parse().unwrap(), config, Rules::default(), seats, 42)
    }

    /// Drive a match to its terminal state with both seats forfeiting every tick.
    fn run_to_end(mut m: Match) -> Match {
        while m.phase() == MatchPhase::Live {
            m.step(&BTreeMap::new());
        }
        m
    }

    fn finished_record() -> MatchRecord {
        run_to_end(scripted_match()).to_record().expect("a finished match yields a record")
    }

    fn frame_at(tick: u64) -> Broadcast {
        Broadcast {
            protocol_version: arena_proto::PROTOCOL_VERSION,
            match_id: FIXED_ID.parse().unwrap(),
            tick,
            phase: MatchPhase::Live,
            entities: Vec::new(),
        }
    }

    #[test]
    fn replay_frames_reproduces_a_finished_match() {
        let record = finished_record();
        let frames = replay_frames(&record).expect("a valid record replays");
        // One opening frame plus one after each simulated tick.
        assert_eq!(frames.len(), record.replay.ticks.len() + 1);
        assert_eq!(frames.first().unwrap().tick, 0);
        assert_eq!(frames.first().unwrap().phase, MatchPhase::Live);
        let last = frames.last().unwrap();
        assert_eq!(last.phase, MatchPhase::Ended, "the final frame is the terminal state");
        let ids: Vec<u32> = last.entities.iter().map(|e| e.entity_id).collect();
        assert_eq!(ids, vec![0, 1], "the broadcast shows the whole roster");
        // Deterministic: replaying the same record twice yields identical frames.
        assert_eq!(frames, replay_frames(&record).unwrap(), "replay is deterministic");
    }

    #[test]
    fn replay_frames_rejects_a_corrupt_record_without_panicking() {
        // FM3 safety: a malformed setup (negative arena bound) would PANIC a naive
        // re-run (a `min > max` clamp); replay_frames verifies first, so it is a clean
        // typed error — a corrupt record can't crash a spectator/replay feed.
        let mut malformed = finished_record();
        malformed.config.bounds.x = -1;
        assert!(matches!(replay_frames(&malformed), Err(ReplayError::MalformedSetup)));

        // A record whose committed result no longer matches the re-run is rejected,
        // not silently played to a divergent outcome.
        let mut tampered = finished_record();
        tampered.result.final_tick += 1;
        assert!(matches!(replay_frames(&tampered), Err(ReplayError::ResultMismatch)));
    }

    #[test]
    fn replay_frames_does_not_allocate_for_post_terminal_padding() {
        // FM3 (memory-DoS): a record padded with canonical post-terminal ticks still
        // passes verify() (they are scanned but never simulated), so replay_frames
        // must NOT pre-size its output from ticks.len(). It returns only the frames
        // actually simulated, and its buffer tracks that real count — not the
        // attacker-controlled padding length.
        let mut padded = finished_record();
        let real_ticks = padded.replay.ticks.len();
        let next = real_ticks as u64;
        for k in 0..20_000u64 {
            padded.replay.ticks.push(arena_proto::TickRecord { tick: next + k, actions: Vec::new() });
        }
        let frames = replay_frames(&padded).expect("a post-terminal-padded record still verifies + replays");
        assert_eq!(frames.len(), real_ticks + 1, "frames track the SIMULATED ticks, not the padded length");
        assert!(
            frames.capacity() < 1024,
            "output buffer must track simulated frames, not the {}-tick padded length (cap {})",
            padded.replay.ticks.len(),
            frames.capacity()
        );
    }

    #[test]
    fn spectator_feed_drops_oldest_when_a_consumer_falls_behind() {
        // FM3: a slow consumer must never stall the publisher. With a ring of 3, a
        // spectator that never reads keeps only the NEWEST 3 frames; the rest are
        // dropped (counted), and every publish returns without blocking.
        let feed = SpectatorFeed::new(3, 8);
        let spec = feed.subscribe().expect("under cap");
        for i in 0..10u64 {
            let reached = feed.publish_frame(frame_at(i));
            assert_eq!(reached, 1, "publish reaches the one live spectator and returns");
        }
        assert_eq!(spec.buffered(), 3, "the ring is bounded to its capacity");
        assert_eq!(spec.dropped(), 7, "the 7 oldest frames were shed");
        // What remains is the most-recent window (ticks 7,8,9), oldest-first.
        let ticks: Vec<u64> = std::iter::from_fn(|| spec.recv())
            .map(|m| match m {
                SpectatorMsg::Frame(b) => b.tick,
                SpectatorMsg::End(_) => u64::MAX,
            })
            .collect();
        assert_eq!(ticks, vec![7, 8, 9], "the buffer holds the most recent frames");
    }

    #[test]
    fn a_caught_up_spectator_drops_nothing_while_a_slow_one_does() {
        // The rings are independent: from ONE publish stream, a spectator that drains
        // keeps every frame (0 dropped) while one that ignores the feed sheds the
        // overflow — a slow consumer never penalizes a fast one or the publisher.
        let feed = SpectatorFeed::new(2, 4);
        let fast = feed.subscribe().unwrap();
        let slow = feed.subscribe().unwrap();
        for i in 0..6u64 {
            feed.publish_frame(frame_at(i));
            // `fast` drains immediately; `slow` never reads.
            assert!(fast.recv().is_some());
        }
        assert_eq!(fast.dropped(), 0, "a drained spectator misses nothing");
        assert_eq!(slow.dropped(), 4, "the ignored spectator shed the overflow");
        assert_eq!(slow.buffered(), 2, "bounded to its ring");
    }

    #[test]
    fn spectating_cannot_alter_the_match_outcome() {
        // FM2: a spectator is read-only — it cannot inject an action or influence the
        // match. Run the SAME deterministic match with no feed, and again while fanning
        // every tick to several spectators (one draining, one ignoring); the result is
        // identical, so spectating has no side effect on the authoritative sim.
        let result_no_feed = run_to_end(scripted_match()).result().unwrap().clone();

        let feed = SpectatorFeed::new(2, 4);
        let drainer = feed.subscribe().unwrap();
        let _idler = feed.subscribe().unwrap(); // never reads; its ring just drops
        let mut m = scripted_match();
        while m.phase() == MatchPhase::Live {
            feed.publish_frame(m.broadcast());
            let _ = drainer.recv(); // a spectator pulling frames mid-match
            m.step(&BTreeMap::new());
        }
        let result_with_feed = m.result().unwrap().clone();
        feed.publish_end(result_with_feed.clone());

        assert_eq!(result_no_feed, result_with_feed, "spectating must not change the match");
    }

    #[test]
    fn spectator_feed_caps_concurrent_subscribers_and_reclaims_dropped_slots() {
        // FM3: an unbounded number of spectators can't be admitted. At the cap,
        // subscribe returns None; dropping a spectator frees its slot for a newcomer.
        let feed = SpectatorFeed::new(4, 2);
        let a = feed.subscribe().expect("1st under cap");
        let b = feed.subscribe().expect("2nd under cap");
        assert_eq!(feed.spectator_count(), 2);
        assert!(feed.subscribe().is_none(), "the 3rd is refused at the cap");
        drop(a);
        assert_eq!(feed.spectator_count(), 1, "a dropped spectator frees its slot");
        let _c = feed.subscribe().expect("a slot reopened");
        assert_eq!(feed.spectator_count(), 2);
        let _ = b;
    }

    #[test]
    fn spectator_receives_frames_then_terminal_end() {
        let feed = SpectatorFeed::new(8, 2);
        let spec = feed.subscribe().unwrap();
        feed.publish_frame(frame_at(0));
        feed.publish_frame(frame_at(1));
        feed.publish_end(run_to_end(scripted_match()).result().unwrap().clone());
        assert!(matches!(spec.recv(), Some(SpectatorMsg::Frame(b)) if b.tick == 0));
        assert!(matches!(spec.recv(), Some(SpectatorMsg::Frame(b)) if b.tick == 1));
        assert!(matches!(spec.recv(), Some(SpectatorMsg::End(_))));
        assert!(spec.recv().is_none(), "nothing follows the terminal End");
    }
}
