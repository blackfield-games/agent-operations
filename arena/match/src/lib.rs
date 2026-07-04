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
//! - **Ranked seats are authenticated.** A ranked agent seat must present a
//!   signature the [`IdentityVerifier`] accepts *before* it is queued, so an
//!   unauthenticated agent can never reach a settle-able ranked match.
//!   [`SignatureVerifier`] is the real gate: it recovers the secp256k1
//!   signer from the arena-01 `join_digest` over *this connection's* challenge
//!   nonce and admits a ranked seat only when the recovered address equals the
//!   claimed `agent_id` — key possession, not assertion. (Whether that address is
//!   a *registered* on-chain agent is a separate eligibility check that composes on
//!   top, a later contracts task; [`StubIdentityVerifier`] stands in for that
//!   allowlist in tests.)
//! - **Formation is atomic.** Per-mode queues live behind one lock, so a join that
//!   completes a match pulls its whole roster under that lock — no concurrent join
//!   can double-seat a participant or start a match a seat short.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use arena_core::{
    arena_map, ranked_delta, ranked_field_delta, Match, MatchRecord, RatingDelta, ReplayError,
    Rules, SeatDelta, SplitMix64, DEFAULT_RATING,
};
use arena_proto::{
    verify_join_signature, ActionIntent, Broadcast, ControllerKind, MatchConfig, MatchMode,
    MatchPhase, MatchResult, SeatId, SeatInfo, SpectatorMsg, TeamId, Vec2, POSITION_SCALE,
    PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Verifies that a ranked seat controls the identity it claims, over the
/// connection's server-issued challenge.
///
/// `signature_hex` is the arena-01 `AgentMsg::Join.signature_hex`; `nonce` is the
/// challenge the Gateway minted for *this* connection (never a client-supplied
/// value). [`SignatureVerifier`] is the real implementation — it recovers the
/// secp256k1 signer from the [`join_digest`](arena_proto::join_digest) and accepts
/// only when the recovered address equals `agent_id`. [`StubIdentityVerifier`]
/// stands in for the on-chain *registration* check (a later contracts task) in the
/// matchmaker's policy tests, enforcing the same accept/reject boundary against an
/// allowlist so the ranked gate is exercised without crypto.
pub trait IdentityVerifier {
    /// `true` iff `signature_hex` proves control of `agent_id` over `nonce`. An
    /// empty, missing, or wrong signature — or one over a different nonce, version,
    /// or identity — must return `false`; admitting an unproven identity to a
    /// ranked seat is the exact failure this trait exists to prevent.
    fn verify(&self, agent_id: &str, nonce: &[u8], signature_hex: &str) -> bool;
}

/// The production [`IdentityVerifier`]: recover the join-digest signer and admit a
/// ranked seat only when the recovered address equals the claimed `agent_id`.
///
/// Delegates to [`verify_join_signature`] over the build's [`PROTOCOL_VERSION`] and
/// the connection's challenge `nonce`, so a ranked seat forms only for a connection
/// that holds the key behind the address it claims — the arena counterpart of the
/// render mesh's earner `Hello` recovery. A forged, wrong-key, cross-version, or
/// replayed-nonce signature recovers a different (or no) address and is rejected.
/// This proves key possession; on-chain registration eligibility is a separate
/// check that wraps this one.
#[derive(Debug, Default, Clone, Copy)]
pub struct SignatureVerifier;

impl IdentityVerifier for SignatureVerifier {
    fn verify(&self, agent_id: &str, nonce: &[u8], signature_hex: &str) -> bool {
        verify_join_signature(PROTOCOL_VERSION, agent_id, nonce, signature_hex).is_ok()
    }
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
    // Ignores `nonce`: the stub stands in for the on-chain *registration* lookup
    // (allowlist membership), not the signature recovery — the challenge binding is
    // [`SignatureVerifier`]'s job. Policy tests use the stub to exercise admission
    // without crypto.
    fn verify(&self, agent_id: &str, _nonce: &[u8], token: &str) -> bool {
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
    /// Seats per team. `1` (the default) is free-for-all — each seat its own team,
    /// byte-identical to the pre-team formation. A larger value forms symmetric
    /// teams (2 ⇒ 2v2, 3 ⇒ 3v3); it must divide `seats_per_match` into at least two
    /// whole teams, which [`Matchmaker::new`] enforces. The sim never reads this —
    /// it only sees the resulting `team` on each roster seat.
    pub team_size: u8,
    pub tick_hz: u16,
    pub max_ticks: u64,
    pub bounds: Vec2,
    /// The builtin arena key whose static geometry — vision blockers + world
    /// pickups — every formed match plays under, resolved through [`arena_map`].
    /// The default `""` is the empty arena
    /// (no occlusion, no items), so an unconfigured matchmaker forms matches
    /// byte-identical to the pre-map-loading behaviour; an unknown key degrades to
    /// that same empty arena rather than failing a formation.
    pub arena: &'static str,
    /// The widest rating gap at which the ranked matchmaker will immediately pair the
    /// longest-waiting agent with its nearest opponent. A finite value makes ranked
    /// agents *wait in a pool* for a close-enough match instead of pairing the first
    /// two that arrive — the gate that makes nearest-rated pairing meaningful (with
    /// eager pairing the pool never exceeds one match, so there is nothing to choose).
    /// A long-waiting agent is never starved: once the pool reaches
    /// `RANKED_FORCE_POOL_MULTIPLE × seats` it is matched regardless (see
    /// [`select_ranked`]). The default [`i32::MAX`] imposes no effective gate — for any
    /// realistic rating spread the gap is far below it, so ranked pairs as soon as two
    /// agents wait, identical to pre-ladder formation — so a ranked deployment sets a
    /// finite tolerance (the magnitude is a balance decision, like the K-factor). Only
    /// the Agent (ranked) queue reads it.
    pub ranked_rating_tolerance: i32,
    /// The most formed-but-unsettled ranked registrations the matchmaker holds before
    /// it evicts the OLDEST to bound memory. A registration leaks if its match's result
    /// never reports back; this caps that leak. A backstop, not a normal-operation
    /// limit — under prompt settling `pending_ranked` stays far below it and nothing is
    /// ever evicted, so a healthy deployment is byte-identical to the pre-cap behaviour;
    /// eviction only fires once the registry is already pathologically deep. `0` opts
    /// out (unbounded — the exact pre-cap behaviour). Only the Agent (ranked) queue
    /// grows this map. Mirrors the mesh registration-bucket cap.
    pub max_pending_ranked: usize,
    /// The server-authoritative combat [`Rules`] every formed match runs under — the
    /// perception cone, memory window, weapon model, and the rest of the tuning the sim
    /// clamps and resolves against. The matchmaker OWNS this (it is operator config set
    /// at construction, never a joining seat's wire input), so a ranked/matchmade match
    /// carries exactly the tuning a hand-seated direct match does. The default
    /// [`Rules::default`] is the historical hardcoded ruleset, so an unconfigured
    /// matchmaker forms matches byte-identical to the pre-rules-knob path; a deployment
    /// that wants a perception-memory window (or an FOV cone, …) sets it here and every
    /// formed match plays under it.
    pub rules: Rules,
}

impl Default for MatchParams {
    fn default() -> Self {
        Self {
            seats_per_match: 2,
            team_size: 1,
            tick_hz: 30,
            max_ticks: 3600,
            bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
            arena: "",
            ranked_rating_tolerance: i32::MAX,
            max_pending_ranked: 4096,
            rules: Rules::default(),
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

/// Domain separator folded into the match seed for team assignment, so the team
/// shuffle and the spawn draws (both seeded from the same id) don't share a PRNG
/// sequence. Any fixed non-zero constant works; this one spells "team_v01".
const TEAM_ASSIGN_DOMAIN: u64 = 0x7465_616d_5f76_3031;

/// Assign each of `seats` seats to one of `seats / team_size` teams — balanced
/// (exactly `team_size` seats per team) and reproducible from `match_id` alone,
/// the team analogue of [`seed_for_match`]. `team_size == 1` is free-for-all: each
/// seat is its own team (`team == seat`), the byte-identical default (the replay
/// digest folds each seat's team, so a reshuffle here would shift every existing
/// hash for no gameplay gain on singleton teams). For `team_size > 1` a balanced
/// team-label multiset `[0,0,…,1,1,…]` is Fisher-Yates-shuffled by a [`SplitMix64`]
/// seeded from the match id, so the assignment is balanced by construction, cannot
/// be steered by queue arrival order, yet replays identically from the id.
///
/// Caller guarantees (via [`Matchmaker::new`]) that `team_size >= 1` and divides
/// `seats` into at least two whole teams.
fn assign_teams(match_id: Uuid, seats: usize, team_size: usize) -> Vec<TeamId> {
    if team_size <= 1 {
        return (0..seats).map(|i| i as TeamId).collect();
    }
    let num_teams = seats / team_size;
    let mut labels: Vec<TeamId> =
        (0..num_teams).flat_map(|t| std::iter::repeat_n(t as TeamId, team_size)).collect();
    let mut rng = SplitMix64::new(seed_for_match(match_id) ^ TEAM_ASSIGN_DOMAIN);
    // Fisher-Yates over the balanced labels: a permutation preserves the per-team
    // counts, so the result stays exactly `team_size` seats per team. The small
    // modulo bias is immaterial — balance is structural, not statistical.
    for i in (1..labels.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        labels.swap(i, j);
    }
    labels
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

/// Schema version of [`LadderSnapshot`]. Bumped whenever the snapshot's shape or
/// the meaning of its fields changes, so a snapshot written by an older (or newer)
/// build is DETECTED and rejected by [`Matchmaker::from_snapshot`] rather than
/// silently restored into wrong ratings. The ABI-drift-gate analog for the
/// persisted ladder.
pub const LADDER_SNAPSHOT_VERSION: u32 = 1;

/// A portable, versioned snapshot of the matchmaker's ranked rating ladder, so the
/// accumulated ratings survive a process restart — the offline analog of the mesh
/// coordinator's SQLite crash-recovery. Captured by [`Matchmaker::snapshot`] and
/// restored by [`Matchmaker::from_snapshot`].
///
/// Only the rating ladder is persisted; `pending_ranked` (matches that have FORMED
/// but not yet settled) is deliberately NOT, because the in-flight match's PLAY is
/// not persisted either — a fresh process never saw it, so settling it from restored
/// rating context would commit a result it cannot trust. After a restore, a result
/// for a pre-restart match settles to the same clean no-op as any unregistered match
/// ([`Matchmaker::apply_ranked_result`] returns `None`); the durable, authoritative
/// settled-reputation record is the on-chain `AgentRegistry`/`MatchSettlement`, not
/// this matchmaking cache.
///
/// The ratings are pure `i32`, so a `snapshot → serialize → deserialize → restore`
/// round-trip reproduces every rating EXACTLY (no float, platform-stable). The type
/// is `serde`-(de)serializable but format-agnostic: the consumer (operator tooling /
/// the harness) chooses JSON, bincode, etc., the same library-seam discipline as the
/// `Settle`/`Spender` transports. A malformed or truncated blob fails at the
/// consumer's deserialize boundary as a clean `Result` error (serde never panics);
/// a version mismatch is caught by [`Matchmaker::from_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LadderSnapshot {
    /// The schema version this snapshot was written under; checked against
    /// [`LADDER_SNAPSHOT_VERSION`] on restore.
    pub version: u32,
    /// agent identity → integer ranked rating, the ladder verbatim.
    pub ratings: BTreeMap<String, i32>,
}

/// Why restoring a [`LadderSnapshot`] failed. Restore never panics and never
/// silently seeds wrong/zero ratings — a rejected snapshot leaves the caller to
/// decide (start fresh, or refuse to boot), which is safer than mis-ranking agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// The snapshot's `version` is not [`LADDER_SNAPSHOT_VERSION`] — an older or
    /// newer schema whose fields could mean something different, so it is rejected
    /// rather than misinterpreted.
    Version { found: u32, expected: u32 },
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Version { found, expected } => write!(
                f,
                "ladder snapshot version mismatch: found {found}, expected {expected}"
            ),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// All of the matchmaker's mutable state, behind its one lock: the per-mode waiting
/// queues and the ranked rating ladder. Holding them under a single lock means a
/// join that pairs by rating reads the book and the queue atomically — there is no
/// window where a rating could move between selecting an opponent and seating it.
#[derive(Default)]
struct State {
    queues: Queues,
    /// The in-memory ranked rating ladder: agent identity → integer rating. A
    /// ranked (Agent-mode) agent is seeded at [`DEFAULT_RATING`] on its first join
    /// and moves only when a terminal ranked match applies its rating delta.
    /// `BTreeMap`, not `HashMap`, so iteration is order-deterministic — the pairing
    /// and replay-determinism properties depend on it.
    ratings: BTreeMap<String, i32>,
    /// Ranked matches that have FORMED but not yet been settled into the ladder,
    /// `match_id → the full seat-ordered roster's agent identities + an insertion seq`.
    /// Every Agent-mode match registers here at formation (every seat is an
    /// authenticated ranked agent); a 1v1 settles via
    /// [`Matchmaker::apply_ranked_result`] and a 3+/team field via
    /// [`Matchmaker::apply_ranked_field_result`], each removing its entry and applying
    /// the delta. This is what makes the rating update safe: a casual/human/unknown
    /// match was never registered, so applying its result is a no-op (FM1), and
    /// resolving a match removes its entry, so a replayed result cannot double-apply
    /// (FM3). The registry is bounded by `MatchParams::max_pending_ranked` — a match
    /// whose result never reports back would otherwise leak its entry forever, so over
    /// the cap the OLDEST (lowest `seq`) registration is evicted.
    pending_ranked: BTreeMap<Uuid, PendingRanked>,
    /// Monotonic stamp assigned to each `pending_ranked` insertion. `BTreeMap` is
    /// `Uuid`-keyed (not insertion-ordered), so this is what lets the cap evict the
    /// genuinely OLDEST registration deterministically. Only ever increments
    /// (saturating); never reused.
    next_pending_seq: u64,
    /// How many `pending_ranked` registrations the cap has evicted — surfaced via
    /// [`Matchmaker::ranked_evictions`] for observability. A rising count means matches
    /// are forming faster than their results report back (the leak the cap bounds).
    ranked_evictions: usize,
}

/// A formed-but-unsettled ranked registration: the full roster's agents in seat order
/// (so a multi-seat settle sources each seat's rating in canonical order) plus the
/// insertion sequence the cap uses to evict the OLDEST entry — the one most likely
/// abandoned (its result never came back) — when the registry is over its bound.
struct PendingRanked {
    seq: u64,
    agents: Vec<String>,
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
    state: Mutex<State>,
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
        assert!(params.team_size >= 1, "team_size must be at least 1, got 0");
        assert!(
            params.seats_per_match.is_multiple_of(params.team_size),
            "seats_per_match ({}) must divide evenly into teams of team_size ({})",
            params.seats_per_match,
            params.team_size
        );
        // The same single-team reason as seats_per_match >= 2: a roster that forms
        // one team ends on its first tick, so a match needs at least two teams.
        assert!(
            params.seats_per_match / params.team_size >= 2,
            "a match needs at least 2 teams, got {} (seats {} / team_size {})",
            params.seats_per_match / params.team_size,
            params.seats_per_match,
            params.team_size
        );
        Self { state: Mutex::new(State::default()), verifier, params }
    }

    /// Restore a matchmaker whose ranked rating ladder is seeded from a prior
    /// [`snapshot`](Self::snapshot), so accumulated ratings survive a restart.
    /// `verifier` and `params` are supplied fresh — they are runtime config, not
    /// persisted; only the rating ladder is restored. `pending_ranked` starts empty
    /// by design (see [`LadderSnapshot`]), so a result for a match that formed before
    /// the restart settles to a clean no-op rather than from stale rating context.
    ///
    /// Returns [`SnapshotError::Version`] if the snapshot's schema version is not
    /// [`LADDER_SNAPSHOT_VERSION`] — never panicking and never silently seeding wrong
    /// ratings. The same `params` validity asserts as [`new`](Self::new) apply.
    pub fn from_snapshot(
        verifier: V,
        params: MatchParams,
        snapshot: LadderSnapshot,
    ) -> Result<Self, SnapshotError> {
        if snapshot.version != LADDER_SNAPSHOT_VERSION {
            return Err(SnapshotError::Version {
                found: snapshot.version,
                expected: LADDER_SNAPSHOT_VERSION,
            });
        }
        let mm = Self::new(verifier, params);
        mm.state.lock().expect("matchmaker mutex poisoned").ratings = snapshot.ratings;
        Ok(mm)
    }

    /// Admit `req` to `mode`'s queue, forming a match if it now can. Returns
    /// [`JoinOutcome::Formed`] to the join that completes a match (whose roster is
    /// removed from the queue atomically), [`JoinOutcome::Queued`] otherwise, or a
    /// [`JoinError`] if the request is inadmissible — rejected before it is queued.
    ///
    /// `nonce` is the challenge the Gateway issued for *this* connection; it is
    /// taken as a parameter from server connection state, never from the
    /// client-supplied `req`, so a ranked signature is always checked against the
    /// freshly-issued challenge and a captured Join can't be replayed on another
    /// connection. It is consulted only for a ranked agent claim; human and casual
    /// seats ignore it.
    pub fn join(
        &self,
        mode: MatchMode,
        nonce: &[u8],
        req: JoinRequest,
    ) -> Result<JoinOutcome, JoinError> {
        self.admit(mode, nonce, &req)?;
        let seated = Seated { agent_id: req.agent_id, kind: req.kind };
        let roster = {
            let mut st = self.state.lock().expect("matchmaker mutex poisoned");
            // A ranked (Agent-mode) seat enters the ladder at the seed rating on its
            // first join, so every waiting ranked agent has a rating to pair on.
            if mode == MatchMode::Agent {
                st.ratings.entry(seated.agent_id.clone()).or_insert(DEFAULT_RATING);
            }
            // Split-borrow the disjoint State fields: the queue is taken mutably to
            // push + drain the roster, the ladder shared so ranked pairing can read it.
            let st = &mut *st;
            let queue = st.queues.for_mode(mode);
            queue.push(seated);
            try_form(
                mode,
                queue,
                &st.ratings,
                self.params.seats_per_match as usize,
                self.params.ranked_rating_tolerance,
            )
        };
        Ok(match roster {
            Some(roster) => JoinOutcome::Formed(Box::new(self.build(mode, roster))),
            None => JoinOutcome::Queued,
        })
    }

    /// Kind + identity admission, run *before* the queue lock so a rejected join
    /// never enters a queue — verifying at formation instead would let an
    /// unauthenticated agent sit in the ranked queue until it was selected.
    fn admit(&self, mode: MatchMode, nonce: &[u8], req: &JoinRequest) -> Result<(), JoinError> {
        match (mode, req.kind) {
            (MatchMode::Human, ControllerKind::Agent) | (MatchMode::Agent, ControllerKind::Human) => {
                return Err(JoinError::WrongKindForMode { mode, kind: req.kind });
            }
            _ => {}
        }
        if req.kind == ControllerKind::Agent {
            match req.presented_token() {
                Some(token) if self.verifier.verify(&req.agent_id, nonce, token) => {}
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
    /// arrival order, assign them to balanced teams derived from the freshly minted
    /// match id (free-for-all when `team_size == 1`, so the match doesn't end the
    /// instant it starts), and seed the sim from that same id.
    fn build(&self, mode: MatchMode, roster: Vec<Seated>) -> Match {
        // The mode's composition is guaranteed by selection; assert it before the
        // match starts so a selection bug fails loud rather than starting a match
        // that defeats its own mode (e.g. an all-human "Mixed" match).
        assert!(composition_ok(mode, &roster), "formed a {mode:?} match with an invalid composition");
        let match_id = Uuid::new_v4();
        let teams = assign_teams(match_id, roster.len(), self.params.team_size as usize);
        let seats: Vec<SeatInfo> = roster
            .iter()
            .enumerate()
            .map(|(i, s)| SeatInfo { seat: i as SeatId, team: teams[i], controller: s.agent_id.clone() })
            .collect();
        // Register every ranked match (Agent mode — every seat is an authenticated
        // ranked agent) so its terminal result can later settle into the ladder: a 1v1
        // via apply_ranked_result (ranked_delta), a 3+/team field via
        // apply_ranked_field_result (ranked_field_delta). A casual/human/Mixed match has
        // no rated field to move (FM1) and is never registered. The FULL seat-ordered
        // roster is stored (not just two seats), so a multi-seat settle can source each
        // seat's rating in canonical order. The brief re-lock keeps the heavy Match::new
        // off the formation lock, as build already does. Keyed by the freshly minted id,
        // so the result carrying that id resolves exactly this match.
        if mode == MatchMode::Agent {
            let agents: Vec<String> = seats.iter().map(|s| s.controller.clone()).collect();
            let mut st = self.state.lock().expect("matchmaker mutex poisoned");
            let seq = st.next_pending_seq;
            st.next_pending_seq = st.next_pending_seq.saturating_add(1);
            st.pending_ranked.insert(match_id, PendingRanked { seq, agents });
            // Bound the registry: a match whose result never reports back would otherwise
            // leak its entry forever. Over the cap, evict the OLDEST (lowest seq) — the one
            // most likely abandoned. Inert until the cap is reached (and `cap == 0` opts
            // out), so a healthy, prompt-settling deployment never evicts and stays
            // byte-identical to the pre-cap behaviour.
            let cap = self.params.max_pending_ranked;
            if cap != 0 && st.pending_ranked.len() > cap {
                let oldest = st.pending_ranked.iter().min_by_key(|(_, p)| p.seq).map(|(id, _)| *id);
                if let Some(id) = oldest {
                    st.pending_ranked.remove(&id);
                    st.ranked_evictions = st.ranked_evictions.saturating_add(1);
                }
            }
        }
        let config = MatchConfig {
            tick_hz: self.params.tick_hz,
            max_ticks: self.params.max_ticks,
            bounds: self.params.bounds,
            seats: seats.len() as u8,
        };
        // Load the configured arena's static geometry (vision blockers + world
        // pickups) at formation; the empty/default key is byte-identical to the
        // pre-map-loading path, and an unknown key degrades to the empty arena.
        let map = arena_map(self.params.arena);
        // Form under the matchmaker's configured Rules — the server-authoritative tuning
        // carried on MatchParams — so a matchmade match runs the same perception cone,
        // memory window, and weapon model a hand-seated direct match does. The default is
        // Rules::default(), byte-identical to the formerly hardcoded ruleset.
        Match::new_with_pickups(
            match_id,
            config,
            self.params.rules,
            seats,
            map.blockers,
            map.pickups,
            seed_for_match(match_id),
        )
    }

    /// How many participants are waiting in `mode`'s queue — for observability and
    /// tests.
    pub fn waiting(&self, mode: MatchMode) -> usize {
        let mut st = self.state.lock().expect("matchmaker mutex poisoned");
        st.queues.for_mode(mode).len()
    }

    /// Capture the current ranked rating ladder as a portable, versioned
    /// [`LadderSnapshot`] for persistence. Only the ratings are captured (see
    /// [`LadderSnapshot`] for why `pending_ranked` is excluded); a restore with
    /// [`from_snapshot`](Self::from_snapshot) reproduces every rating exactly.
    pub fn snapshot(&self) -> LadderSnapshot {
        let st = self.state.lock().expect("matchmaker mutex poisoned");
        LadderSnapshot { version: LADDER_SNAPSHOT_VERSION, ratings: st.ratings.clone() }
    }

    /// The current ranked rating of `agent_id`, or `None` if it has never joined a
    /// ranked (Agent-mode) queue. A freshly seeded agent reads back
    /// [`DEFAULT_RATING`] until a terminal ranked match moves it.
    pub fn rating(&self, agent_id: &str) -> Option<i32> {
        self.state.lock().expect("matchmaker mutex poisoned").ratings.get(agent_id).copied()
    }

    /// Settle a terminal ranked match's `result` into the ladder, returning the
    /// zero-sum [`RatingDelta`] applied (or `None` if nothing moved).
    ///
    /// The match must have been formed by this matchmaker as a ranked 1v1 (Agent
    /// mode, 2 seats) — identified by `result.match_id`. A result for a casual, human,
    /// team, or unknown match is a no-op (it was never registered, FM1), as is a
    /// second call for the same match (its registration is removed on the first, so a
    /// replayed result cannot double-apply, FM3). On a hit, each seat's rating moves
    /// by exactly the [`ranked_delta`] the core computes from the two pre-match
    /// ratings and the outcome — `delta.a` to the seat the result orders first,
    /// `delta.b == -delta.a` to the other — so the ladder conserves total rating.
    /// `k` is the owner-set K-factor (operator-gated magnitude); the caller supplies
    /// it rather than the matchmaker baking it in. For a multi-seat (FFA / 3+ / team)
    /// ranked match, settle with [`apply_ranked_field_result`](Self::apply_ranked_field_result)
    /// instead — this 1v1 route returns `None` for anything but a two-seat result.
    pub fn apply_ranked_result(&self, result: &MatchResult, k: i32) -> Option<RatingDelta> {
        // Treat the result as UNTRUSTED and validate-then-commit: a malformed result
        // (not exactly two distinct seats, or a seat outside the registered roster) is
        // a clean no-op that LEAVES the match registered, so a later well-formed result
        // can still settle it — a bad report never silently burns the registration.
        let [oa, ob] = result.outcomes.as_slice() else { return None };
        if oa.seat == ob.seat {
            return None;
        }
        let mut st = self.state.lock().expect("matchmaker mutex poisoned");
        // Peek (do not remove yet): `ranked_delta` orders the two seats by ascending
        // seat id (canonical), so pair each outcome's seat to the agent the roster
        // seated there. Bail — registration intact — if either seat is out of range.
        let pending = st.pending_ranked.get(&result.match_id)?;
        // This is the 1v1 route: it settles ONLY a 2-seat registration. Every Agent
        // match registers now (not just 1v1s), so a 3+/team match shares this registry;
        // settling one here would apply a partial 2-seat delta and burn its registration,
        // leaving the rest of the field unsettled. Bail (registration intact) so it
        // settles through `apply_ranked_field_result` instead.
        if pending.agents.len() != 2 {
            return None;
        }
        let (Some(agent_a), Some(agent_b)) =
            (pending.agents.get(oa.seat as usize), pending.agents.get(ob.seat as usize))
        else {
            return None;
        };
        let (agent_a, agent_b) = (agent_a.clone(), agent_b.clone());
        let ra = st.ratings.get(&agent_a).copied().unwrap_or(DEFAULT_RATING);
        let rb = st.ratings.get(&agent_b).copied().unwrap_or(DEFAULT_RATING);
        let delta = ranked_delta(result, ra, rb, k)?;
        // Commit: the apply is well-formed, so consume the registration and write the
        // book. delta.a lands on the seat the result orders first, delta.b == -delta.a
        // on the other, keeping the ladder zero-sum.
        st.pending_ranked.remove(&result.match_id);
        st.ratings.insert(agent_a, ra.saturating_add(delta.a));
        st.ratings.insert(agent_b, rb.saturating_add(delta.b));
        Some(delta)
    }

    /// Settle a terminal multi-seat (FFA / 3+ / team) ranked match's `result` into the
    /// ladder, returning the zero-sum per-seat [`SeatDelta`]s applied (or `None` if
    /// nothing moved). The multi-seat generalization of [`apply_ranked_result`]: that
    /// settles a 1v1 via [`ranked_delta`], this settles a full placement field via the
    /// core's [`ranked_field_delta`].
    ///
    /// The match must have been formed by this matchmaker as a ranked match (Agent
    /// mode) — identified by `result.match_id`. Each seat's pre-match rating is sourced
    /// from the live ladder in the result's canonical ascending-seat order, so
    /// `ratings[i]` pairs to `result.outcomes[i].seat` exactly as [`ranked_field_delta`]
    /// requires; the field's per-seat deltas are then applied back to each seat's agent.
    /// The field sums to exactly `0`, so the ladder conserves total reputation across
    /// the whole roster, not just two seats.
    ///
    /// Treats the result as UNTRUSTED, validate-then-commit: the registration is
    /// consumed and the ladder written ONLY when the result covers the full registered
    /// roster — exactly one outcome per registered seat, every seat in range and
    /// distinct (with the exact count, distinct in-range seats are a permutation of the
    /// roster). A result for a casual/human/unknown match (never registered, FM1), or a
    /// malformed one — wrong seat count, an out-of-roster seat, or a duplicated seat
    /// that would double-apply and break zero-sum (FM2) — is a no-op that LEAVES the
    /// match registered, so a later well-formed result can still settle it. A second
    /// call for the same match is a no-op (the registration is removed on the first, so
    /// a replay cannot double-apply, FM3). On a well-formed two-seat result this settles
    /// the ladder byte-identically to [`apply_ranked_result`] — [`ranked_field_delta`]
    /// agrees with [`ranked_delta`] at n=2 (FM4). `k` is the owner-set K-factor.
    pub fn apply_ranked_field_result(&self, result: &MatchResult, k: i32) -> Option<Vec<SeatDelta>> {
        let mut st = self.state.lock().expect("matchmaker mutex poisoned");
        // Peek (do not remove yet): bail with the registration intact on any malformed
        // result, so a bad report never silently burns it.
        let pending = st.pending_ranked.get(&result.match_id)?;
        let n = pending.agents.len();
        // Full-roster coverage: exactly one outcome per registered seat. A short or long
        // result is malformed — settling it would apply a partial (non-zero-sum) field.
        if result.outcomes.len() != n {
            return None;
        }
        // Map each outcome's seat to the agent the roster seated there, in the result's
        // canonical order, and source that agent's live rating — so ratings[i] pairs to
        // outcomes[i] exactly as ranked_field_delta requires, and agents[i] is the agent
        // its delta lands on. Reject an out-of-roster seat (get == None) or a duplicate:
        // with the exact-count check above, n distinct in-range seats are a permutation
        // of the roster, so a duplicate (which would double-apply, breaking zero-sum) is
        // provably caught here.
        let mut seen = vec![false; n];
        let mut agents: Vec<String> = Vec::with_capacity(n);
        let mut ratings: Vec<i32> = Vec::with_capacity(n);
        for o in &result.outcomes {
            let idx = o.seat as usize;
            let agent = pending.agents.get(idx)?;
            if std::mem::replace(&mut seen[idx], true) {
                return None;
            }
            ratings.push(st.ratings.get(agent).copied().unwrap_or(DEFAULT_RATING));
            agents.push(agent.clone());
        }
        // The deltas come back in outcome order (canonical), so deltas[i], agents[i], and
        // ratings[i] all describe the same seat — apply each to its agent. None here only
        // for a degenerate field (<2 seats), which the coverage check already precludes.
        let deltas = ranked_field_delta(result, &ratings, k)?;
        // Commit: consume the registration and write each seat's rating. Each base is the
        // rating sourced above, so base + delta matches the value ranked_field_delta
        // computed from; the field is zero-sum, so the ladder total is conserved.
        st.pending_ranked.remove(&result.match_id);
        for ((delta, agent), base) in deltas.iter().zip(agents).zip(ratings) {
            st.ratings.insert(agent, base.saturating_add(delta.delta));
        }
        Some(deltas)
    }

    /// How many ranked matches have formed but not yet been settled into the ladder —
    /// the depth of the pending-result registry. For observability and tests (a real
    /// deployment would alarm on this growing without bound, signalling matches whose
    /// results are never reported back).
    pub fn unsettled_ranked(&self) -> usize {
        self.state.lock().expect("matchmaker mutex poisoned").pending_ranked.len()
    }

    /// How many formed-but-unsettled ranked registrations the cap has evicted to bound
    /// the registry. Zero under healthy operation (results report back before the cap is
    /// reached); a rising count signals matches forming faster than they settle — the
    /// leak [`MatchParams::max_pending_ranked`] bounds. For observability and tests.
    pub fn ranked_evictions(&self) -> usize {
        self.state.lock().expect("matchmaker mutex poisoned").ranked_evictions
    }
}

/// Pull a full roster from `queue` if `mode`'s composition can be met, removing
/// exactly those participants; otherwise leave the queue untouched and return
/// `None`. Called under the matchmaker lock, so selection + removal is atomic.
fn try_form(
    mode: MatchMode,
    queue: &mut Vec<Seated>,
    ratings: &BTreeMap<String, i32>,
    seats: usize,
    ranked_tolerance: i32,
) -> Option<Vec<Seated>> {
    if seats == 0 || queue.len() < seats {
        return None;
    }
    let mut picks = match mode {
        // The Human queue is single-kind by admission, so the first `seats` waiting
        // (FIFO) already satisfy the composition.
        MatchMode::Human => (0..seats).collect::<Vec<usize>>(),
        // The Agent queue is ranked: pair by rating within tolerance, not arrival
        // order — and may return None to leave the agents waiting for a closer match.
        MatchMode::Agent => select_ranked(queue, ratings, seats, ranked_tolerance)?,
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

/// Once this multiple of the seat count is waiting in the ranked pool, the
/// longest-waiting agent is matched with its nearest opponent regardless of the
/// rating tolerance — the structural anti-starvation fallback, which also bounds the
/// pool size. The forced match is still anchored on the oldest, so it is precisely
/// the agent that has waited longest that the fallback seats.
const RANKED_FORCE_POOL_MULTIPLE: usize = 2;

/// Select `seats` ranked agents from `queue`, or `None` to leave them waiting.
///
/// Anchors on the longest-waiting agent (the queue head) and fills the seats with
/// the agents NEAREST it in rating: FIFO decides *who waits least*, rating decides
/// *who they face*. The cluster forms only if its widest gap from the anchor is
/// within `tolerance` — otherwise the agents wait in the pool for a closer match,
/// which is what lets the pool exceed one match and makes "nearest" a real choice.
///
/// Anchoring on the head, plus the force cap, is the anti-starvation fallback: a lone
/// outlier rating is never skipped by tighter pairs forming around it, because once
/// it reaches the head it anchors the next match, and once the pool reaches
/// [`RANKED_FORCE_POOL_MULTIPLE`]`× seats` that match forms regardless of tolerance.
/// So "prefer the nearest-rated opponent", "no rating is starved", and a bounded pool
/// all hold. A rating tie breaks by arrival order (queue index), so the whole
/// selection is a pure function of `(ratings, queue order, tolerance)` with no
/// `HashMap` iteration — a replayed join sequence pairs identically.
fn select_ranked(
    queue: &[Seated],
    ratings: &BTreeMap<String, i32>,
    seats: usize,
    tolerance: i32,
) -> Option<Vec<usize>> {
    if seats == 0 || queue.len() < seats {
        return None;
    }
    let rating_of = |i: usize| ratings.get(&queue[i].agent_id).copied().unwrap_or(DEFAULT_RATING);
    let anchor = rating_of(0);
    let mut others: Vec<usize> = (1..queue.len()).collect();
    others.sort_by_key(|&i| (rating_of(i).abs_diff(anchor), i));
    let picks: Vec<usize> = std::iter::once(0).chain(others.into_iter().take(seats - 1)).collect();
    let max_gap = picks.iter().map(|&i| rating_of(i).abs_diff(anchor)).max().unwrap_or(0);
    let within_tolerance = i64::from(max_gap) <= i64::from(tolerance);
    let pool_forces = queue.len() >= seats.saturating_mul(RANKED_FORCE_POOL_MULTIPLE);
    (within_tolerance || pool_forces).then_some(picks)
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

/// Re-run a verified [`MatchRecord`] from its determinants, invoking `on_frame` with
/// each [`Broadcast`] in order — the opening tick plus one after every SIMULATED tick,
/// the last carrying `phase == Ended`. The shared spine behind both [`replay_frames`]
/// (which collects the frames) and [`replay_to_feed`] (which streams them to
/// spectators), so the two can never diverge on what a replay produces.
///
/// VERIFIES first ([`MatchRecord::verify`]), so a truncated, tampered, or
/// non-reproducing record is rejected as a typed [`ReplayError`] and NEVER panics the
/// playback — the same anti-DoS guarantee arena-05 gives a settlement verifier, reused
/// because a replay/spectator path parses untrusted records too. `verify` bounds cost
/// AND proves the record reaches a terminal result, which it RETURNS — so a caller that
/// needs the outcome (the feed's terminal `publish_end`) takes it from here rather than
/// re-deriving it, and `publish_end` can never fire on a rejected record (the `?`
/// returns first).
fn replay_into(
    record: &MatchRecord,
    mut on_frame: impl FnMut(Broadcast),
) -> Result<MatchResult, ReplayError> {
    let result = record.verify()?;
    // new_with_pickups, not new: a recorded match's world pickups are a determinant of
    // its broadcasts, so rebuilding without them would diverge the spectator feed from
    // the real match (the core verify() re-run already loads them).
    let mut m = Match::new_with_pickups(
        record.replay.match_id,
        record.config,
        record.rules,
        record.replay.seats.clone(),
        record.replay.blockers.clone(),
        record.replay.pickups.clone(),
        record.replay.seed,
    );
    // Burn the pre-live Starting countdown silently before the opening frame: a
    // countdown is invisible to the scored tick stream (and the digest), so the feed
    // opens at Live tick 0 exactly as a no-countdown match. A no-countdown record is
    // already Live, so this is a no-op.
    while m.phase() == MatchPhase::Starting {
        m.step(&BTreeMap::new());
    }
    // The opening-tick broadcast, then one after every SIMULATED tick. The loop breaks
    // at the terminal phase, so a record `verify` accepts with canonical post-terminal
    // tick padding (scanned but never simulated — `replay_match` breaks there too)
    // streams ONLY the frames actually played: an adversarial record claiming millions
    // of ticks can't turn this into an unbounded publish loop.
    on_frame(m.broadcast());
    for tr in &record.replay.ticks {
        if m.phase() != MatchPhase::Live {
            break;
        }
        let intents: BTreeMap<SeatId, ActionIntent> =
            tr.actions.iter().map(|a| (a.seat, a.intent)).collect();
        m.step(&intents);
        on_frame(m.broadcast());
    }
    Ok(result)
}

/// Re-run a finished [`MatchRecord`] (arena-05) into the sequence of [`Broadcast`]
/// frames a spectator would have seen — the "watch a finished match" path, the replay
/// counterpart to the live [`SpectatorFeed`]. At most `ticks.len() + 1` frames: the
/// broadcast at the opening tick plus one after every simulated tick, the last carrying
/// the terminal `phase == Ended`. A non-verifying record is a typed [`ReplayError`],
/// never a panic (see [`replay_into`]).
pub fn replay_frames(record: &MatchRecord) -> Result<Vec<Broadcast>, ReplayError> {
    let mut frames = Vec::new();
    replay_into(record, |frame| frames.push(frame))?;
    Ok(frames)
}

/// Stream a finished [`MatchRecord`] to a live [`SpectatorFeed`] — the producer the
/// feed was built for. Re-runs the verified record and
/// [`publish_frame`](SpectatorFeed::publish_frame)s each [`Broadcast`] in the exact
/// order [`replay_frames`] returns, then [`publish_end`](SpectatorFeed::publish_end)s
/// the terminal [`MatchResult`] EXACTLY ONCE, after the final frame — so a subscribed
/// spectator streams the match frame-by-frame and learns who won. Returns the number of
/// frames published (`== replay_frames(record)?.len()`).
///
/// Publishing is non-blocking and lossy by the feed's design: a slow spectator drops
/// its oldest frames (counted) rather than stalling the replay, so a stalled consumer
/// can never backpressure the publisher. The terminal result comes from
/// [`verify`](MatchRecord::verify) (which a verified record guarantees is terminal), so
/// `publish_end` always fires once with the real outcome and never on a rejected record.
pub fn replay_to_feed(record: &MatchRecord, feed: &SpectatorFeed) -> Result<usize, ReplayError> {
    let mut published = 0usize;
    let result = replay_into(record, |frame| {
        feed.publish_frame(frame);
        published += 1;
    })?;
    feed.publish_end(result);
    Ok(published)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_proto::MatchPhase;
    use arena_proto::{ActionButtons, PickupKind, PickupSpawn, SeatOutcome};
    use k256::ecdsa::{RecoveryId, Signature, SigningKey};

    #[test]
    fn stub_verifier_admits_only_the_authorized_token() {
        let mut v = StubIdentityVerifier::new();
        v.authorize("0xagent", "goodsig");

        // The stub ignores the nonce (it stands in for the registration lookup), so
        // these pass an empty challenge; SignatureVerifier's tests exercise the nonce.
        assert!(v.verify("0xagent", b"", "goodsig"), "the authorized identity + token is accepted");
        assert!(!v.verify("0xagent", b"", "badsig"), "a wrong token is rejected");
        assert!(!v.verify("0xagent", b"", ""), "an empty token is rejected");
        assert!(!v.verify("0xunknown", b"", "goodsig"), "an unregistered agent is rejected");
        assert!(!v.verify("0xunknown", b"", ""), "an unknown agent with no token is rejected");

        // An empty token never authenticates even if an agent is (wrongly)
        // allowlisted with one — the empty-token guard rejects, not just an
        // allowlist miss, so an empty signature can never satisfy a ranked seat.
        v.authorize("0xempty", "");
        assert!(!v.verify("0xempty", b"", ""), "an empty token is rejected even when allowlisted empty");
    }

    #[test]
    fn stub_verifier_default_authorizes_no_one() {
        // A fresh verifier is closed by default — no identity is ranked-admissible
        // until explicitly authorized, so a forgotten allowlist fails safe.
        let v = StubIdentityVerifier::new();
        assert!(!v.verify("0xagent", b"", "anything"));
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
        ranked_mm_tol(authorized, i32::MAX)
    }

    fn ranked_mm_tol(authorized: &[(&str, &str)], tolerance: i32) -> Matchmaker<StubIdentityVerifier> {
        let mut v = StubIdentityVerifier::new();
        for (id, token) in authorized {
            v.authorize(*id, *token);
        }
        Matchmaker::new(v, MatchParams { ranked_rating_tolerance: tolerance, ..MatchParams::default() })
    }

    fn controllers(m: &Match) -> Vec<String> {
        let mut who: Vec<String> = m.seats().iter().map(|s| s.controller.clone()).collect();
        who.sort();
        who
    }

    #[test]
    fn human_mode_forms_an_all_human_match() {
        let mm = open_mm();
        assert!(mm.join(MatchMode::Human, b"", JoinRequest::human("alice")).unwrap().is_queued());
        let m = mm
            .join(MatchMode::Human, b"", JoinRequest::human("bob"))
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
        let r = mm.join(MatchMode::Human, b"", JoinRequest::casual_agent("0xbot"));
        assert!(matches!(
            r,
            Err(JoinError::WrongKindForMode { mode: MatchMode::Human, kind: ControllerKind::Agent })
        ));
        assert_eq!(mm.waiting(MatchMode::Human), 0, "a rejected join never enters the queue");
    }

    #[test]
    fn agent_mode_rejects_a_human_seat() {
        let mm = open_mm();
        let r = mm.join(MatchMode::Agent, b"", JoinRequest::human("alice"));
        assert!(matches!(
            r,
            Err(JoinError::WrongKindForMode { mode: MatchMode::Agent, kind: ControllerKind::Human })
        ));
    }

    #[test]
    fn agent_mode_forms_from_authorized_agents() {
        let mm = ranked_mm(&[("0xa", "siga"), ("0xb", "sigb")]);
        assert!(mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xa", "siga")).unwrap().is_queued());
        let m = mm
            .join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xb", "sigb"))
            .unwrap()
            .into_formed()
            .expect("two authorized agents form a ranked match");
        assert_eq!(controllers(&m), ["0xa", "0xb"]);
        assert_eq!(m.phase(), MatchPhase::Live);
    }

    #[test]
    fn a_ranked_agent_seeds_at_the_default_rating() {
        // FM3 (seed): a fresh ranked agent enters the ladder at exactly DEFAULT_RATING
        // on its first Agent-mode join, and reads back that value until a match moves
        // it. The ladder is ranked-only — a human seat is never rated.
        let mm = ranked_mm(&[("0xa", "siga")]);
        assert_eq!(mm.rating("0xa"), None, "no rating until the agent joins ranked");
        assert!(mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xa", "siga")).unwrap().is_queued());
        assert_eq!(mm.rating("0xa"), Some(DEFAULT_RATING), "seeded at the default on the first ranked join");

        let open = open_mm();
        open.join(MatchMode::Human, b"", JoinRequest::human("alice")).unwrap();
        assert_eq!(open.rating("alice"), None, "a human seat is never laddered");
    }

    #[test]
    fn mixed_needs_both_kinds_to_form() {
        // FM1: a Mixed match must not start all-one-kind. Humans alone never form…
        let humans = open_mm();
        assert!(humans.join(MatchMode::Mixed, b"", JoinRequest::human("h1")).unwrap().is_queued());
        assert!(humans.join(MatchMode::Mixed, b"", JoinRequest::human("h2")).unwrap().is_queued());
        assert_eq!(humans.waiting(MatchMode::Mixed), 2, "no Mixed match from humans alone");
        // …and casual agents alone never form.
        let agents = open_mm();
        assert!(agents.join(MatchMode::Mixed, b"", JoinRequest::casual_agent("a1")).unwrap().is_queued());
        assert!(agents.join(MatchMode::Mixed, b"", JoinRequest::casual_agent("a2")).unwrap().is_queued());
        assert_eq!(agents.waiting(MatchMode::Mixed), 2, "no Mixed match from agents alone");
    }

    #[test]
    fn mixed_always_includes_an_agent_even_when_humans_dominate() {
        // Selection takes one-of-each first, so even a queue stacked with humans
        // forms a Mixed match around the single agent — never an all-human one.
        let mm = open_mm();
        assert!(mm.join(MatchMode::Mixed, b"", JoinRequest::human("h1")).unwrap().is_queued());
        assert!(mm.join(MatchMode::Mixed, b"", JoinRequest::human("h2")).unwrap().is_queued());
        let m = mm
            .join(MatchMode::Mixed, b"", JoinRequest::casual_agent("a1"))
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

    fn ranked_queue(ids: &[&str]) -> Vec<Seated> {
        ids.iter().map(|id| Seated { agent_id: (*id).into(), kind: ControllerKind::Agent }).collect()
    }

    fn rating_book(entries: &[(&str, i32)]) -> BTreeMap<String, i32> {
        entries.iter().map(|(id, r)| (id.to_string(), *r)).collect()
    }

    #[test]
    fn ranked_pairing_faces_the_head_with_its_nearest_rated_opponent() {
        // FM2 (pairing quality): the queue head plays its NEAREST-rated opponent, not
        // the next arrival. Head A(1500) faces C(1520) over B(1900), though B waited
        // longer — "prefer the nearest-rated opponent", the point of not-just-FIFO.
        let queue = ranked_queue(&["A", "B", "C"]);
        let ratings = rating_book(&[("A", 1500), ("B", 1900), ("C", 1520)]);
        assert_eq!(
            select_ranked(&queue, &ratings, 2, i32::MAX).unwrap(),
            vec![0, 2],
            "the head is paired by rating distance, not arrival order"
        );
    }

    #[test]
    fn ranked_pairing_defers_a_pair_outside_the_tolerance() {
        // FM2 (tolerance gate): with a finite tolerance and no close-enough opponent in
        // a sub-cap pool, no match forms — the agents wait for a nearer rating. This is
        // what lets the ranked pool exceed one match, so "nearest" becomes a real
        // choice. A(1500)'s only partner B(1900) is 400 apart, tolerance 100 ⇒ None.
        let pair = ranked_queue(&["A", "B"]);
        assert!(
            select_ranked(&pair, &rating_book(&[("A", 1500), ("B", 1900)]), 2, 100).is_none(),
            "a too-far pair waits rather than forming"
        );
        // A within-tolerance pair forms under the same tolerance.
        assert_eq!(
            select_ranked(&pair, &rating_book(&[("A", 1500), ("B", 1560)]), 2, 100).unwrap(),
            vec![0, 1],
            "a within-tolerance pair forms"
        );
    }

    #[test]
    fn ranked_pairing_forces_a_match_at_the_pool_cap_anchored_on_the_oldest() {
        // FM2 (aging / no-starvation): a lone outlier is never starved. Below the cap
        // with no within-tolerance partner it waits; once the pool reaches the force
        // cap (RANKED_FORCE_POOL_MULTIPLE × seats = 4) the longest-waiting head forms
        // with its nearest regardless of tolerance — and the head, the oldest, is in it.
        let ratings = rating_book(&[("X", 3000), ("M1", 1500), ("M2", 1500), ("M3", 1500)]);
        let under_cap = ranked_queue(&["X", "M1", "M2"]);
        assert!(
            select_ranked(&under_cap, &ratings, 2, 0).is_none(),
            "under the cap, a far outlier waits for a closer match"
        );
        let at_cap = ranked_queue(&["X", "M1", "M2", "M3"]);
        let picks = select_ranked(&at_cap, &ratings, 2, 0).expect("the pool cap forces a match");
        assert!(picks.contains(&0), "the forced match seats the longest-waiting head — never starved");
        assert_eq!(picks.len(), 2, "exactly a full roster is taken");
    }

    #[test]
    fn ranked_pairing_is_deterministic_on_rating_ties() {
        // FM4 (determinism): equidistant opponents break by arrival order, so the
        // selection is a pure function of (ratings, queue order, tolerance) — no
        // HashMap surprise. All seeded equal ⇒ head A pairs with B (before C), always.
        let queue = ranked_queue(&["A", "B", "C"]);
        let equal = rating_book(&[("A", DEFAULT_RATING), ("B", DEFAULT_RATING), ("C", DEFAULT_RATING)]);
        assert_eq!(select_ranked(&queue, &equal, 2, i32::MAX).unwrap(), vec![0, 1]);
        // A rating the book has never seen defaults to DEFAULT_RATING in the metric —
        // the selector must not panic on a miss (the matchmaker seeds before queuing,
        // but the pure function stays total).
        assert_eq!(
            select_ranked(&queue, &BTreeMap::new(), 2, i32::MAX).unwrap(),
            vec![0, 1],
            "an unseeded rating defaults rather than panicking"
        );
    }

    #[test]
    fn ranked_pairing_needs_a_full_roster() {
        let queue = ranked_queue(&["A"]);
        assert!(
            select_ranked(&queue, &rating_book(&[("A", 1500)]), 2, i32::MAX).is_none(),
            "one waiter cannot form a 2-seat match"
        );
    }

    /// Form a ranked 2-seat Agent match between two authorized agents (`a` at seat 0,
    /// `b` at seat 1), returning the matchmaker and the formed match so its id can
    /// settle a result. Extra agents may be pre-authorized for follow-on rounds.
    fn ranked_pair(a: &str, b: &str, extra: &[&str]) -> (Matchmaker<StubIdentityVerifier>, Match) {
        let mut authorized: Vec<(&str, &str)> = vec![(a, "t"), (b, "t")];
        authorized.extend(extra.iter().map(|id| (*id, "t")));
        let mm = ranked_mm(&authorized);
        assert!(mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent(a, "t")).unwrap().is_queued());
        let m = mm
            .join(MatchMode::Agent, b"", JoinRequest::ranked_agent(b, "t"))
            .unwrap()
            .into_formed()
            .expect("two ranked agents form a 1v1");
        (mm, m)
    }

    /// A decisive [`MatchResult`] for `match_id`: `winner_seat` placed first, the
    /// other second, so [`settlement`] reads a clean `Win { winner_seat }`.
    fn decisive_result(match_id: Uuid, winner_seat: SeatId) -> MatchResult {
        let outcome = |seat: SeatId| SeatOutcome {
            seat,
            team: seat as TeamId,
            placement: if seat == winner_seat { 1 } else { 2 },
            score: 0,
            alive_at_end: seat == winner_seat,
        };
        MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id,
            final_tick: 10,
            outcomes: vec![outcome(0), outcome(1)],
            replay_hash: String::new(),
        }
    }

    #[test]
    fn a_ranked_result_moves_both_seats_by_exactly_the_core_delta() {
        // FM3: a terminal ranked match settles into the ladder — both seats start at
        // the seed and move by EXACTLY ranked_delta (no off-by-one, no divergence from
        // the core), zero-sum.
        let (mm, m) = ranked_pair("0xa", "0xb", &[]);
        assert_eq!(mm.rating("0xa"), Some(DEFAULT_RATING));
        assert_eq!(mm.rating("0xb"), Some(DEFAULT_RATING));
        assert_eq!(mm.unsettled_ranked(), 1, "the formed ranked match is registered");

        let result = decisive_result(m.match_id(), 0); // 0xa (seat 0) wins
        let k = 32;
        let applied = mm.apply_ranked_result(&result, k).expect("a registered ranked match settles");

        let expected = ranked_delta(&result, DEFAULT_RATING, DEFAULT_RATING, k).unwrap();
        assert_eq!(applied, expected, "the matchmaker applies the core delta verbatim");
        assert_eq!(mm.rating("0xa"), Some(DEFAULT_RATING + expected.a), "the winner gains exactly delta.a");
        assert_eq!(mm.rating("0xb"), Some(DEFAULT_RATING + expected.b), "the loser moves exactly delta.b");
        assert_eq!(expected.a, -expected.b, "zero-sum: total ladder rating is conserved");
        assert!(expected.a > 0, "an even-match win raises the winner");
        assert_eq!(mm.unsettled_ranked(), 0, "settling consumes the registration");
    }

    #[test]
    fn a_ranked_result_settles_at_most_once() {
        // FM3 (no double-apply): the registration is removed on the first settle, so a
        // replayed or duplicate result for the same match is a no-op — the ladder holds.
        let (mm, m) = ranked_pair("0xa", "0xb", &[]);
        let result = decisive_result(m.match_id(), 0);
        assert!(mm.apply_ranked_result(&result, 32).is_some(), "the first settle applies");
        let after = (mm.rating("0xa"), mm.rating("0xb"));
        assert!(mm.apply_ranked_result(&result, 32).is_none(), "a second settle of the same match is a no-op");
        assert_eq!((mm.rating("0xa"), mm.rating("0xb")), after, "the replay left ratings unchanged");
        assert_eq!(mm.unsettled_ranked(), 0);
    }

    // -- multi-seat (FFA / 3+ / team) ranked settlement -------------------------

    /// A multi-seat [`MatchResult`] for `match_id`: each `(seat, placement)` becomes a
    /// [`SeatOutcome`] (placement 1 = best, ties share a rank). FFA, so `team == seat`.
    /// Outcomes keep the given order (callers pass canonical ascending seat).
    fn field_result(match_id: Uuid, placements: &[(SeatId, u16)]) -> MatchResult {
        MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id,
            final_tick: 10,
            outcomes: placements
                .iter()
                .map(|&(seat, placement)| SeatOutcome {
                    seat,
                    team: seat as TeamId,
                    placement,
                    score: 0,
                    alive_at_end: placement == 1,
                })
                .collect(),
            replay_hash: String::new(),
        }
    }

    /// Form an N-seat FFA ranked match whose seats carry DISTINCT pre-match ratings.
    /// The ratings are restored via a snapshot (a fresh join would seed them all equal),
    /// then the agents join to form the match with those ratings intact — `join` seeds
    /// only an ABSENT agent, so the restored ladder survives. Returns the matchmaker and
    /// the formed match.
    fn ranked_field_seeded(seats: &[(&str, i32)]) -> (Matchmaker<StubIdentityVerifier>, Match) {
        let mut v = StubIdentityVerifier::new();
        for &(id, _) in seats {
            v.authorize(id, "t");
        }
        let snapshot = LadderSnapshot {
            version: LADDER_SNAPSHOT_VERSION,
            ratings: seats.iter().map(|&(id, r)| (id.to_string(), r)).collect(),
        };
        let params = MatchParams {
            seats_per_match: seats.len() as u8,
            ranked_rating_tolerance: i32::MAX,
            ..MatchParams::default()
        };
        let mm = Matchmaker::from_snapshot(v, params, snapshot).expect("a current-version snapshot restores");
        let mut formed = None;
        for &(id, _) in seats {
            if let Some(m) = mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent(id, "t")).unwrap().into_formed() {
                formed = Some(m);
            }
        }
        (mm, formed.expect("the full roster forms an N-seat ranked match"))
    }

    /// A seat -> agent map read from a formed match, indexed by seat id (the selector's
    /// internal seating order is opaque, so the test reads it back rather than assuming).
    fn agents_by_seat(m: &Match) -> Vec<String> {
        let mut roster = m.seats().to_vec();
        roster.sort_by_key(|s| s.seat);
        roster.iter().map(|s| s.controller.clone()).collect()
    }

    #[test]
    fn a_multiseat_result_settles_each_seat_against_the_field_zero_sum() {
        // FM1 (seat->rating mapping) + FM2 (zero-sum): a 3-seat FFA ranked match settles
        // every seat by EXACTLY core ranked_field_delta over the seats' LIVE ratings,
        // sourced in canonical seat order. Distinct ratings + an UPSET placement (the
        // 1300 underdog wins, the 1800 favourite comes last) make every seat's delta
        // distinct and sign-dependent on its own (rating, placement) pair, so a
        // seat->agent swap would settle the wrong reputation; the field stays zero-sum.
        let (mm, m) = ranked_field_seeded(&[("0xa", 1500), ("0xb", 1800), ("0xc", 1300)]);
        assert_eq!(mm.unsettled_ranked(), 1, "the formed 3-seat ranked match is registered");
        let agent_at = agents_by_seat(&m);
        let rating_at: Vec<i32> = agent_at.iter().map(|a| mm.rating(a).unwrap()).collect();

        // seat 0 second, seat 1 (favourite) last, seat 2 (underdog) first.
        let result = field_result(m.match_id(), &[(0, 2), (1, 3), (2, 1)]);
        let k = 32;
        let expected = ranked_field_delta(&result, &rating_at, k).expect("a 3-seat field aligned to its ratings");
        // The fixture is chosen so every seat moves by a distinct, non-zero delta — a
        // seat whose delta were 0 would make its per-seat assertion below trivially true.
        let mut distinct: Vec<i32> = expected.iter().map(|d| d.delta).collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 3, "every seat's delta is distinct: {expected:?}");
        assert!(expected.iter().all(|d| d.delta != 0), "every seat actually moves: {expected:?}");

        let applied = mm.apply_ranked_field_result(&result, k).expect("a registered multi-seat match settles");
        assert_eq!(applied, expected, "the matchmaker feeds the core the seats' ratings in canonical order");

        for (i, agent) in agent_at.iter().enumerate() {
            assert_eq!(
                mm.rating(agent),
                Some(rating_at[i] + expected[i].delta),
                "seat {i}'s agent moved by exactly its own field delta — a swap would fail here"
            );
        }
        assert_eq!(expected.iter().map(|d| i64::from(d.delta)).sum::<i64>(), 0, "the field is zero-sum");
        let before: i64 = rating_at.iter().map(|&r| i64::from(r)).sum();
        let after: i64 = agent_at.iter().map(|a| i64::from(mm.rating(a).unwrap())).sum();
        assert_eq!(before, after, "total ladder reputation is conserved across the settle");
        assert_eq!(mm.unsettled_ranked(), 0, "settling consumes the registration");
    }

    #[test]
    fn a_multiseat_settle_maps_by_seat_id_not_outcome_position() {
        // FM1 (indexing): the settle pairs each rating and delta by the outcome's SEAT
        // ID, not its position in the list. Feed a well-formed field whose outcomes are
        // NOT in ascending-seat order and confirm each agent still moves by its own
        // seat's delta — an implementation that indexed by position would mis-settle.
        let (mm, m) = ranked_field_seeded(&[("0xa", 1500), ("0xb", 1800), ("0xc", 1300)]);
        let agent_at = agents_by_seat(&m);
        let rating_at: Vec<i32> = agent_at.iter().map(|a| mm.rating(a).unwrap()).collect();

        // Outcomes deliberately out of canonical order: seat 2 listed first, then 0, 1.
        let result = field_result(m.match_id(), &[(2, 1), (0, 2), (1, 3)]);
        let applied =
            mm.apply_ranked_field_result(&result, 32).expect("a well-formed (if unsorted) field settles");

        for sd in &applied {
            let seat = sd.seat as usize;
            assert_eq!(
                mm.rating(&agent_at[seat]),
                Some(rating_at[seat] + sd.delta),
                "seat {seat}'s agent moved by its own delta regardless of outcome position"
            );
        }
        let before: i64 = rating_at.iter().map(|&r| i64::from(r)).sum();
        let after: i64 = agent_at.iter().map(|a| i64::from(mm.rating(a).unwrap())).sum();
        assert_eq!(before, after, "still zero-sum under a permuted outcome order");
    }

    #[test]
    fn a_multiseat_result_settles_at_most_once() {
        // FM3 (no double-apply): the registration is consumed on the first settle, so a
        // replayed multi-seat result is a clean no-op and every rating holds.
        let (mm, m) = ranked_field_seeded(&[("0xa", 1500), ("0xb", 1500), ("0xc", 1500)]);
        let result = field_result(m.match_id(), &[(0, 1), (1, 2), (2, 3)]);
        assert!(mm.apply_ranked_field_result(&result, 32).is_some(), "the first settle applies");
        let after: Vec<_> = ["0xa", "0xb", "0xc"].iter().map(|a| mm.rating(a)).collect();
        assert!(mm.apply_ranked_field_result(&result, 32).is_none(), "a replay of the same match is a no-op");
        let again: Vec<_> = ["0xa", "0xb", "0xc"].iter().map(|a| mm.rating(a)).collect();
        assert_eq!(after, again, "the replay left every rating unchanged");
        assert_eq!(mm.unsettled_ranked(), 0);
    }

    #[test]
    fn a_malformed_multiseat_result_is_a_noop_that_leaves_the_match_registered() {
        // FM2 (no partial apply / validate-then-commit): a result that does not cover the
        // full roster — wrong seat count, an out-of-roster seat, or a duplicated seat
        // (which would double-apply and break zero-sum) — settles NOTHING and LEAVES the
        // match registered, so a later well-formed result can still settle it.
        let (mm, m) = ranked_field_seeded(&[("0xa", 1500), ("0xb", 1600), ("0xc", 1400)]);
        let mid = m.match_id();
        let book = || ["0xa", "0xb", "0xc"].iter().map(|a| mm.rating(a)).collect::<Vec<_>>();
        let before = book();

        assert!(mm.apply_ranked_field_result(&field_result(mid, &[(0, 1), (1, 2)]), 32).is_none(), "too few outcomes is a partial field");
        assert!(mm.apply_ranked_field_result(&field_result(mid, &[(0, 1), (1, 2), (3, 3)]), 32).is_none(), "a seat outside the roster bails");
        assert!(mm.apply_ranked_field_result(&field_result(mid, &[(0, 1), (0, 2), (2, 3)]), 32).is_none(), "a duplicated seat bails");

        assert_eq!(book(), before, "no malformed result moved the ladder");
        assert_eq!(mm.unsettled_ranked(), 1, "the match is still registered after every malformed report");
        assert!(mm.apply_ranked_field_result(&field_result(mid, &[(0, 1), (1, 2), (2, 3)]), 32).is_some(), "a well-formed result still settles it");
        assert_eq!(mm.unsettled_ranked(), 0);
    }

    #[test]
    fn a_2seat_result_settles_identically_through_both_paths() {
        // FM4 (no 1v1 regression): a 2-seat ranked result routed through the multi-seat
        // field path settles the ladder IDENTICALLY to the existing 1v1 ranked_delta
        // route (ranked_field_delta agrees with ranked_delta at n=2). Two matchmakers
        // seeded the same, the same decisive result, settled by each path — the ladders
        // must match and the field's two seat-deltas equal the RatingDelta's a/b.
        let (mm_legacy, m_legacy) = ranked_field_seeded(&[("0xa", 1500), ("0xb", 1600)]);
        let (mm_field, m_field) = ranked_field_seeded(&[("0xa", 1500), ("0xb", 1600)]);
        let k = 32;

        let legacy = mm_legacy
            .apply_ranked_result(&decisive_result(m_legacy.match_id(), 0), k)
            .expect("the 1v1 route settles");
        let deltas = mm_field
            .apply_ranked_field_result(&decisive_result(m_field.match_id(), 0), k)
            .expect("the field route settles");

        assert_eq!(deltas.len(), 2, "a 2-seat field has two seat deltas");
        assert_eq!(deltas[0].delta, legacy.a, "seat 0's field delta equals ranked_delta.a");
        assert_eq!(deltas[1].delta, legacy.b, "seat 1's field delta equals ranked_delta.b");
        assert!(mm_field.rating("0xa").unwrap() > 1500, "the underdog winner actually moved (not a double no-op)");
        assert_eq!(
            (mm_legacy.rating("0xa"), mm_legacy.rating("0xb")),
            (mm_field.rating("0xa"), mm_field.rating("0xb")),
            "both routes leave byte-identical ladders"
        );
    }

    /// A ranked FFA matchmaker over `seats` seats with a custom `max_pending_ranked`
    /// cap (open tolerance), for the multi-seat eviction test.
    fn ranked_field_mm_cap(authorized: &[&str], seats: u8, cap: usize) -> Matchmaker<StubIdentityVerifier> {
        let mut v = StubIdentityVerifier::new();
        for id in authorized {
            v.authorize(*id, "t");
        }
        Matchmaker::new(
            v,
            MatchParams { seats_per_match: seats, ranked_rating_tolerance: i32::MAX, max_pending_ranked: cap, ..MatchParams::default() },
        )
    }

    /// Form one N-seat ranked match from `ids` on a shared `mm` (the roster leaves the
    /// queue on forming, so the next call forms a fresh `match_id`).
    fn form_ranked_field(mm: &Matchmaker<StubIdentityVerifier>, ids: &[&str]) -> Match {
        let mut formed = None;
        for id in ids {
            if let Some(m) = mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent(*id, "t")).unwrap().into_formed() {
                formed = Some(m);
            }
        }
        formed.expect("the full roster forms an N-seat ranked match")
    }

    #[test]
    fn multiseat_registrations_are_bounded_by_the_same_cap_evicting_the_oldest() {
        // FM3 (eviction): a multi-seat registration is bounded by the SAME
        // max_pending_ranked + oldest-eviction the 1v1 path uses. With cap 2 and 3-seat
        // matches, the third formation evicts the first; the evicted oldest is a clean
        // no-op, the two survivors still settle.
        let mm = ranked_field_mm_cap(&["0xa", "0xb", "0xc"], 3, 2);
        let first = form_ranked_field(&mm, &["0xa", "0xb", "0xc"]); // oldest (seq 0)
        let second = form_ranked_field(&mm, &["0xa", "0xb", "0xc"]);
        let third = form_ranked_field(&mm, &["0xa", "0xb", "0xc"]); // this formation evicts `first`
        assert_eq!(mm.unsettled_ranked(), 2, "the registry never exceeds the cap");
        assert_eq!(mm.ranked_evictions(), 1, "one multi-seat registration was evicted");

        let full = |id: Uuid| field_result(id, &[(0, 1), (1, 2), (2, 3)]);
        assert!(mm.apply_ranked_field_result(&full(first.match_id()), 32).is_none(), "the evicted oldest is a clean no-op");
        assert!(mm.apply_ranked_field_result(&full(third.match_id()), 32).is_some(), "a surviving recent match still settles");
        assert!(mm.apply_ranked_field_result(&full(second.match_id()), 32).is_some(), "the other survivor settles too");
        assert_eq!(mm.unsettled_ranked(), 0);
    }

    #[test]
    fn the_1v1_route_refuses_a_multiseat_registration_leaving_it_for_the_field_path() {
        // Every Agent match registers now, so a 3+/team match shares the registry the 1v1
        // route reads. apply_ranked_result MUST refuse it: a 2-outcome result against a
        // 3-seat roster would partial-settle two seats and burn the registration, leaving
        // the rest of the field unsettled. It bails with the registration intact so the
        // field path settles the full roster — and a genuine 1v1 still settles via the
        // 1v1 route.
        let (mm, m) = ranked_field_seeded(&[("0xa", 1500), ("0xb", 1600), ("0xc", 1400)]);
        let mid = m.match_id();
        assert!(
            mm.apply_ranked_result(&decisive_result(mid, 0), 32).is_none(),
            "the 1v1 route refuses a 3-seat registration"
        );
        assert_eq!(mm.unsettled_ranked(), 1, "the registration is intact — not burned by the 1v1 route");
        assert_eq!(
            ["0xa", "0xb", "0xc"].map(|a| mm.rating(a).unwrap()),
            [1500, 1600, 1400],
            "no rating moved"
        );
        assert!(
            mm.apply_ranked_field_result(&field_result(mid, &[(0, 1), (1, 2), (2, 3)]), 32).is_some(),
            "the field path settles the full 3-seat roster"
        );
        assert_eq!(mm.unsettled_ranked(), 0);

        let (mm2, m2) = ranked_field_seeded(&[("0xa", 1500), ("0xb", 1600)]);
        assert!(
            mm2.apply_ranked_result(&decisive_result(m2.match_id(), 0), 32).is_some(),
            "a genuine 1v1 still settles through the 1v1 route"
        );
    }

    // -- ladder snapshot persistence --------------------------------------------

    /// Just the `StubIdentityVerifier` (not a whole matchmaker), for `from_snapshot`,
    /// which takes a fresh verifier alongside the restored ladder.
    fn ranked_verifier(authorized: &[(&str, &str)]) -> StubIdentityVerifier {
        let mut v = StubIdentityVerifier::new();
        for (id, token) in authorized {
            v.authorize(*id, *token);
        }
        v
    }

    #[test]
    fn snapshot_round_trips_byte_identically_through_serde() {
        // FM1: snapshot -> serialize -> deserialize -> restore reproduces the ladder
        // EXACTLY. Ratings are i32, so equality is exact and the serialized form is
        // byte-stable — re-serializing the restored ladder yields the identical bytes.
        let (mm, m) = ranked_pair("0xa", "0xb", &[]);
        mm.apply_ranked_result(&decisive_result(m.match_id(), 0), 32).unwrap(); // move off the seed
        let snap = mm.snapshot();
        assert_eq!(snap.version, LADDER_SNAPSHOT_VERSION);

        let bytes = serde_json::to_vec(&snap).expect("snapshot serializes");
        let decoded: LadderSnapshot = serde_json::from_slice(&bytes).expect("snapshot deserializes");
        assert_eq!(decoded, snap, "the snapshot round-trips losslessly");

        let restored = Matchmaker::from_snapshot(
            ranked_verifier(&[("0xa", "t"), ("0xb", "t")]),
            MatchParams::default(),
            decoded,
        )
        .expect("a current-version snapshot restores");
        assert_eq!(restored.rating("0xa"), mm.rating("0xa"), "0xa's rating survives the restart exactly");
        assert_eq!(restored.rating("0xb"), mm.rating("0xb"), "0xb's rating survives the restart exactly");
        assert_eq!(
            serde_json::to_vec(&restored.snapshot()).unwrap(),
            bytes,
            "re-snapshotting the restored ladder is byte-identical"
        );
    }

    #[test]
    fn a_post_restore_settle_matches_the_never_restarted_path() {
        // FM1: a snapshot -> restore -> settle yields a delta IDENTICAL to the
        // never-restarted matchmaker settling the same match from the same ladder, so
        // restoring carries the full rating context (not a reset to the seed, which
        // would erase every agent's history). Move the ladder well off the seed, then
        // run the SAME next match on both the original (continuing) and the restored mm.
        let authorized = [("0xa", "t"), ("0xb", "t")];
        let mm = ranked_mm(&authorized);
        for _ in 0..4 {
            let m = form_ranked(&mm, "0xa", "0xb");
            mm.apply_ranked_result(&decisive_result(m.match_id(), 0), 32).unwrap(); // 0xa keeps winning
        }
        assert_ne!(mm.rating("0xa"), Some(DEFAULT_RATING), "precondition: the ladder moved off the seed");

        let restored = Matchmaker::from_snapshot(
            ranked_verifier(&authorized),
            MatchParams::default(),
            mm.snapshot(),
        )
        .unwrap();
        assert_eq!(restored.rating("0xa"), mm.rating("0xa"), "restored ratings match the original");
        assert_eq!(restored.rating("0xb"), mm.rating("0xb"));

        // The same next match, settled on both, applies the same delta and lands the
        // same ladder — proving the restored ratings drive settlement identically.
        let m_orig = form_ranked(&mm, "0xa", "0xb");
        let m_rest = form_ranked(&restored, "0xa", "0xb");
        let d_orig = mm.apply_ranked_result(&decisive_result(m_orig.match_id(), 0), 32).unwrap();
        let d_rest = restored.apply_ranked_result(&decisive_result(m_rest.match_id(), 0), 32).unwrap();
        assert_eq!(d_rest, d_orig, "the restored matchmaker applies the same delta as the never-restarted one");
        assert_eq!(restored.rating("0xa"), mm.rating("0xa"), "and lands the same ladder");
        assert_eq!(restored.rating("0xb"), mm.rating("0xb"));
    }

    #[test]
    fn a_corrupt_snapshot_fails_to_deserialize_without_panicking() {
        // FM2: a malformed, truncated, or wrong-typed blob fails at the serde boundary
        // as a clean Result error — never a panic, never a silent wrong/zero ladder.
        let snap = ranked_mm(&[("0xa", "t")]).snapshot();
        let bytes = serde_json::to_vec(&snap).unwrap();
        assert!(
            serde_json::from_slice::<LadderSnapshot>(&bytes[..bytes.len() / 2]).is_err(),
            "a truncated snapshot is a clean Err"
        );
        assert!(serde_json::from_slice::<LadderSnapshot>(b"not a snapshot").is_err(), "non-JSON is a clean Err");
        assert!(
            serde_json::from_str::<LadderSnapshot>(r#"{"version":1,"ratings":{"0xa":"high"}}"#).is_err(),
            "a non-i32 rating is a clean Err, never a silent wrong rating"
        );
    }

    #[test]
    fn from_snapshot_rejects_a_version_mismatch() {
        // FM3: a snapshot from a different schema version (older OR newer) is detected
        // via the version tag and rejected, not misread into wrong ratings.
        for bad in [0, LADDER_SNAPSHOT_VERSION + 1, 999] {
            let snap = LadderSnapshot { version: bad, ratings: BTreeMap::from([("0xa".to_string(), 1700)]) };
            let err = Matchmaker::from_snapshot(ranked_verifier(&[("0xa", "t")]), MatchParams::default(), snap)
                .err()
                .expect("a version mismatch is rejected");
            assert_eq!(err, SnapshotError::Version { found: bad, expected: LADDER_SNAPSHOT_VERSION });
        }
        // The current version restores fine — the boundary holds both ways.
        let ok = LadderSnapshot {
            version: LADDER_SNAPSHOT_VERSION,
            ratings: BTreeMap::from([("0xa".to_string(), 1700)]),
        };
        let mm = Matchmaker::from_snapshot(ranked_verifier(&[("0xa", "t")]), MatchParams::default(), ok)
            .expect("the current version restores");
        assert_eq!(mm.rating("0xa"), Some(1700), "the restored rating is exact");
    }

    #[test]
    fn a_pre_restart_match_settles_to_a_noop_after_restore() {
        // FM4 (chosen design): pending_ranked is intentionally NOT persisted, so a
        // result for a match that FORMED before the restart settles to a clean no-op on
        // the restored matchmaker — its play wasn't persisted either, so the fresh
        // process can't trust the result (the chain is the authoritative settle record).
        let (mm, m) = ranked_pair("0xa", "0xb", &[]);
        assert_eq!(mm.unsettled_ranked(), 1, "the match is registered before the restart");

        let restored = Matchmaker::from_snapshot(
            ranked_verifier(&[("0xa", "t"), ("0xb", "t")]),
            MatchParams::default(),
            mm.snapshot(),
        )
        .unwrap();
        assert_eq!(restored.unsettled_ranked(), 0, "no pending registration survives the restart");
        let before = (restored.rating("0xa"), restored.rating("0xb"));
        assert!(
            restored.apply_ranked_result(&decisive_result(m.match_id(), 0), 32).is_none(),
            "a pre-restart match's result is a clean no-op after restore"
        );
        assert_eq!(
            (restored.rating("0xa"), restored.rating("0xb")),
            before,
            "the no-op left the restored ladder untouched"
        );
    }

    /// A ranked matchmaker with a custom `max_pending_ranked` cap (and an open
    /// tolerance), for the eviction tests.
    fn ranked_mm_cap(authorized: &[(&str, &str)], cap: usize) -> Matchmaker<StubIdentityVerifier> {
        let mut v = StubIdentityVerifier::new();
        for (id, token) in authorized {
            v.authorize(*id, *token);
        }
        Matchmaker::new(v, MatchParams { max_pending_ranked: cap, ..MatchParams::default() })
    }

    /// Form one ranked 1v1 between two already-authorized agents in `mm`, returning the
    /// formed match. Reusable across many formations: the pair leaves the queue on
    /// forming, so the next call forms a fresh `match_id`.
    fn form_ranked(mm: &Matchmaker<StubIdentityVerifier>, a: &str, b: &str) -> Match {
        assert!(mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent(a, "t")).unwrap().is_queued());
        mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent(b, "t"))
            .unwrap()
            .into_formed()
            .expect("two ranked agents form a 1v1")
    }

    #[test]
    fn pending_ranked_is_bounded_by_the_cap() {
        // FM2: a stream of formations whose results never report back must not grow the
        // registry without bound — it caps at max_pending_ranked, evicting the overflow.
        let cap = 3;
        let mm = ranked_mm_cap(&[("0xa", "t"), ("0xb", "t")], cap);
        for _ in 0..(cap + 5) {
            form_ranked(&mm, "0xa", "0xb");
        }
        assert_eq!(mm.unsettled_ranked(), cap, "the registry never exceeds the cap");
        assert_eq!(mm.ranked_evictions(), 5, "every formation past the cap evicts exactly one");
    }

    #[test]
    fn eviction_drops_the_oldest_registration_keeping_recent_ones() {
        // FM1 + FM4: when capped, the OLDEST (first-formed) registration is evicted — the
        // match most likely abandoned — while just-formed matches still settle. Eviction
        // is age-based (insertion seq), NOT Uuid-order, so the first-formed loses
        // regardless of its random id — the determinism the seq stamp buys.
        let cap = 2;
        let mm = ranked_mm_cap(&[("0xa", "t"), ("0xb", "t")], cap);
        let first = form_ranked(&mm, "0xa", "0xb"); // oldest (seq 0)
        let second = form_ranked(&mm, "0xa", "0xb");
        let third = form_ranked(&mm, "0xa", "0xb"); // this formation evicts `first`
        assert_eq!(mm.unsettled_ranked(), cap);
        assert_eq!(mm.ranked_evictions(), 1);

        // The evicted oldest no longer settles — a clean no-op, no panic, no rating move.
        assert!(
            mm.apply_ranked_result(&decisive_result(first.match_id(), 0), 32).is_none(),
            "the evicted oldest match is a clean no-op"
        );
        // The two surviving recent matches still settle normally.
        assert!(
            mm.apply_ranked_result(&decisive_result(third.match_id(), 0), 32).is_some(),
            "a surviving recent match still settles"
        );
        assert!(
            mm.apply_ranked_result(&decisive_result(second.match_id(), 0), 32).is_some(),
            "the other survivor settles too"
        );
        assert_eq!(mm.unsettled_ranked(), 0);
    }

    #[test]
    fn eviction_does_not_fire_below_the_cap() {
        // Byte-identical default: at or under the cap nothing is evicted and every match
        // settles, exactly as before the cap existed.
        let cap = 8;
        let mm = ranked_mm_cap(&[("0xa", "t"), ("0xb", "t")], cap);
        let matches: Vec<Match> = (0..cap).map(|_| form_ranked(&mm, "0xa", "0xb")).collect();
        assert_eq!(mm.unsettled_ranked(), cap, "exactly at the cap, nothing evicted");
        assert_eq!(mm.ranked_evictions(), 0);
        for m in &matches {
            assert!(
                mm.apply_ranked_result(&decisive_result(m.match_id(), 0), 32).is_some(),
                "every below-cap match settles"
            );
        }
        assert_eq!(mm.unsettled_ranked(), 0);
    }

    #[test]
    fn an_unbounded_cap_never_evicts() {
        // cap == 0 opts out: the exact pre-cap behaviour — the registry grows with each
        // unreported formation, nothing is ever evicted.
        let mm = ranked_mm_cap(&[("0xa", "t"), ("0xb", "t")], 0);
        for _ in 0..50 {
            form_ranked(&mm, "0xa", "0xb");
        }
        assert_eq!(mm.unsettled_ranked(), 50, "cap 0 = unbounded, nothing evicted");
        assert_eq!(mm.ranked_evictions(), 0);
    }

    #[test]
    fn a_casual_or_human_result_never_touches_the_ladder() {
        // FM1: only a registered ranked (Agent 1v1) match moves ratings. A human match
        // and a casual Mixed match are never registered, so applying their results is a
        // no-op, and neither seats a laddered rating.
        let mm = open_mm();
        mm.join(MatchMode::Human, b"", JoinRequest::human("alice")).unwrap();
        let human = mm.join(MatchMode::Human, b"", JoinRequest::human("bob")).unwrap().into_formed().unwrap();

        mm.join(MatchMode::Mixed, b"", JoinRequest::human("h1")).unwrap();
        let mixed = mm
            .join(MatchMode::Mixed, b"", JoinRequest::casual_agent("0xcasual"))
            .unwrap()
            .into_formed()
            .unwrap();

        assert_eq!(mm.unsettled_ranked(), 0, "neither a human nor a casual match is registered as ranked");
        assert!(mm.apply_ranked_result(&decisive_result(human.match_id(), 0), 32).is_none(), "a human result settles nothing");
        assert!(mm.apply_ranked_result(&decisive_result(mixed.match_id(), 0), 32).is_none(), "a casual result settles nothing");
        assert_eq!(mm.rating("alice"), None, "no human is laddered");
        assert_eq!(mm.rating("0xcasual"), None, "a casual cross-play agent is not laddered");

        // A result for a match this matchmaker never formed is likewise a no-op.
        assert!(mm.apply_ranked_result(&decisive_result(Uuid::new_v4(), 0), 32).is_none(), "an unknown match settles nothing");
    }

    #[test]
    fn the_loser_seat_drives_the_sign_of_the_delta() {
        // FM3 (mapping): the delta is paired to the right seats — when seat 1 wins, it
        // is seat 1's agent that rises and seat 0's that falls (the mirror of the WinA
        // case), so the matchmaker never credits the wrong agent.
        let (mm, m) = ranked_pair("0xa", "0xb", &[]);
        mm.apply_ranked_result(&decisive_result(m.match_id(), 1), 32).unwrap(); // seat 1 (0xb) wins
        assert!(mm.rating("0xb").unwrap() > DEFAULT_RATING, "the seat-1 winner rose");
        assert!(mm.rating("0xa").unwrap() < DEFAULT_RATING, "the seat-0 loser fell");
        assert_eq!(
            mm.rating("0xa").unwrap() - DEFAULT_RATING,
            -(mm.rating("0xb").unwrap() - DEFAULT_RATING),
            "the two moves are exact negatives"
        );
    }

    #[test]
    fn a_drawn_ranked_match_settles_but_moves_nobody() {
        // A draw between equals applies a zero delta yet still CONSUMES the
        // registration — the match is settled, just with no rating change.
        let (mm, m) = ranked_pair("0xa", "0xb", &[]);
        let mut draw = decisive_result(m.match_id(), 0);
        draw.outcomes[1].placement = 1; // both at placement 1 ⇒ Settlement::Draw
        assert_eq!(mm.apply_ranked_result(&draw, 32), Some(RatingDelta { a: 0, b: 0 }), "an even draw moves nobody");
        assert_eq!(mm.rating("0xa"), Some(DEFAULT_RATING));
        assert_eq!(mm.rating("0xb"), Some(DEFAULT_RATING));
        assert_eq!(mm.unsettled_ranked(), 0, "a draw still consumes the registration");
    }

    #[test]
    fn a_malformed_result_is_a_no_op_that_leaves_the_match_settleable() {
        // Cross-review hardening (validate-then-commit): apply_ranked_result treats the
        // result as untrusted. A malformed result — wrong outcome count, duplicate
        // seats, or an out-of-range seat — is a clean no-op that does NOT consume the
        // registration, so a later well-formed result still settles the match (a bad
        // report never silently burns it, and never corrupts a rating).
        let (mm, m) = ranked_pair("0xa", "0xb", &[]);
        let mid = m.match_id();

        let mut one = decisive_result(mid, 0);
        one.outcomes.truncate(1);
        assert!(mm.apply_ranked_result(&one, 32).is_none(), "a one-outcome result settles nothing");
        assert_eq!(mm.unsettled_ranked(), 1, "the registration survives a wrong-count result");

        let mut dup = decisive_result(mid, 0);
        dup.outcomes[1].seat = 0; // both seats 0 — would otherwise corrupt one agent
        assert!(mm.apply_ranked_result(&dup, 32).is_none(), "duplicate-seat outcomes settle nothing");

        let mut oor = decisive_result(mid, 0);
        oor.outcomes[1].seat = 7; // a seat outside the 2-agent roster
        assert!(mm.apply_ranked_result(&oor, 32).is_none(), "an out-of-range seat settles nothing");

        assert_eq!(
            (mm.rating("0xa"), mm.rating("0xb")),
            (Some(DEFAULT_RATING), Some(DEFAULT_RATING)),
            "no malformed attempt moved a rating"
        );
        assert_eq!(mm.unsettled_ranked(), 1, "still registered after every malformed attempt");

        // The well-formed result still settles it — the registration was never burned.
        let delta = mm.apply_ranked_result(&decisive_result(mid, 0), 32).expect("a well-formed result still settles");
        assert!(delta.a > 0, "the winner finally rose");
        assert_eq!(mm.unsettled_ranked(), 0, "now consumed");
    }

    #[test]
    fn pairing_reflects_ratings_a_settled_match_moved() {
        // FM2 + FM3 end to end, under a finite tolerance that builds a real pool:
        // settling a result moves the book, and the NEXT formation pairs by those
        // UPDATED ratings, not arrival order. After 0xa beats 0xb (a→1516, b→1484, 32
        // apart > tolerance 20), the two re-queue but do NOT rematch — the gate holds
        // them in the pool — and a third agent 0xc (1500) completes 0xb's match as the
        // nearer rating, leaving 0xa. Pure arrival order would have rematched 0xb+0xa.
        let mm = ranked_mm_tol(&[("0xa", "t"), ("0xb", "t"), ("0xc", "t")], 20);
        // Round 1: both seed at 1500 (gap 0 ≤ 20), so they pair and play; 0xa wins.
        assert!(mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xa", "t")).unwrap().is_queued());
        let m1 = mm
            .join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xb", "t"))
            .unwrap()
            .into_formed()
            .expect("two seed-rated agents pair within tolerance");
        mm.apply_ranked_result(&decisive_result(m1.match_id(), 0), 32).unwrap();
        assert_eq!(mm.rating("0xa"), Some(DEFAULT_RATING + 16), "a rose by K/2 on the even win");
        assert_eq!(mm.rating("0xb"), Some(DEFAULT_RATING - 16), "b fell by K/2");

        // Round 2: b(1484) then a(1516) re-queue — 32 apart, beyond tolerance 20, pool
        // below the cap — so the gate defers their rematch and both wait.
        assert!(mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xb", "t")).unwrap().is_queued());
        assert!(
            mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xa", "t")).unwrap().is_queued(),
            "a and b are too far apart to rematch — the pool grows instead of forming"
        );
        assert_eq!(mm.waiting(MatchMode::Agent), 2, "both wait under the tolerance gate");

        // 0xc(1500) joins: nearest to head 0xb(1484) is 0xc (gap 16 ≤ 20), not 0xa
        // (gap 32). So 0xb pairs with 0xc; 0xa, the farther rating, keeps waiting.
        let m2 = mm
            .join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xc", "t"))
            .unwrap()
            .into_formed()
            .expect("the nearer third agent completes the head's match");
        assert_eq!(controllers(&m2), ["0xb", "0xc"], "the head paired with the nearer rating, not the earlier arrival");
        assert_eq!(mm.waiting(MatchMode::Agent), 1, "0xa, the farther rating, keeps waiting");
    }

    #[test]
    fn the_ladder_and_pairings_are_deterministic_across_replays() {
        // FM4: the book + pairing are order-deterministic (BTreeMap, no HashMap), so
        // the SAME join+result sequence yields identical pairings AND identical final
        // ratings — independent of the random match ids, which steer only spawns/teams,
        // never the ladder. Run the whole scripted sequence twice and compare.
        let run = || {
            let mm = ranked_mm_tol(&[("0xa", "t"), ("0xb", "t"), ("0xc", "t"), ("0xd", "t")], 20);
            let mut rosters: Vec<Vec<String>> = Vec::new();
            for id in ["0xa", "0xb", "0xc", "0xd", "0xa", "0xc"] {
                if let Some(m) =
                    mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent(id, "t")).unwrap().into_formed()
                {
                    rosters.push(controllers(&m));
                    // The head (seat 0) wins each time, a fixed scripted outcome.
                    mm.apply_ranked_result(&decisive_result(m.match_id(), 0), 32).unwrap();
                }
            }
            let book: Vec<(&str, i32)> =
                ["0xa", "0xb", "0xc", "0xd"].into_iter().filter_map(|id| mm.rating(id).map(|r| (id, r))).collect();
            (rosters, book)
        };
        let (rosters_1, book_1) = run();
        let (rosters_2, book_2) = run();
        assert_eq!(rosters_1, rosters_2, "the same join sequence forms the same pairings every run");
        assert_eq!(book_1, book_2, "the same join+result sequence yields the same final ratings every run");
        assert_eq!(rosters_1.len(), 3, "the scripted sequence formed three matches (the test is non-vacuous)");
    }

    #[test]
    fn a_formed_match_starts_on_the_real_core() {
        let mm = open_mm();
        mm.join(MatchMode::Human, b"", JoinRequest::human("p0")).unwrap();
        let m = mm.join(MatchMode::Human, b"", JoinRequest::human("p1")).unwrap().into_formed().unwrap();
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
                    let outcome = mm.join(MatchMode::Human, b"", JoinRequest::human(format!("p{i}"))).unwrap();
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
    fn assign_teams_ffa_is_the_identity_mapping() {
        // FM3: team_size 1 is free-for-all — each seat its own team, the exact
        // pre-team mapping the replay digest folds, so every existing FFA record is
        // byte-identical. The id is irrelevant here (no shuffle is drawn).
        for seats in [2usize, 3, 4, 8] {
            let teams = assign_teams(Uuid::new_v4(), seats, 1);
            let expected: Vec<TeamId> = (0..seats as TeamId).collect();
            assert_eq!(teams, expected, "team_size 1 maps seat i to team i for {seats} seats");
        }
    }

    fn team_counts(teams: &[TeamId]) -> BTreeMap<TeamId, usize> {
        let mut counts: BTreeMap<TeamId, usize> = BTreeMap::new();
        for t in teams {
            *counts.entry(*t).or_default() += 1;
        }
        counts
    }

    #[test]
    fn assign_teams_is_balanced_and_reproducible_from_the_id() {
        // FM1: a 2v2 splits four seats into exactly two teams of two, the same way
        // every time for a given id — reproducible from the id alone (no wall-clock,
        // no HashMap order) — and the result is a permutation of the balanced labels.
        let id = Uuid::new_v4();
        let a = assign_teams(id, 4, 2);
        assert_eq!(a, assign_teams(id, 4, 2), "the same id yields the same teams");
        let counts = team_counts(&a);
        assert_eq!(counts.len(), 2, "exactly two teams: {counts:?}");
        assert!(counts.values().all(|&c| c == 2), "each team has exactly two seats: {counts:?}");
    }

    #[test]
    fn assign_teams_balances_a_3v3() {
        // FM1: balance holds for larger teams — six seats into two teams of three.
        let counts = team_counts(&assign_teams(Uuid::new_v4(), 6, 3));
        assert_eq!(counts.len(), 2, "two teams: {counts:?}");
        assert!(counts.values().all(|&c| c == 3), "each team has exactly three seats: {counts:?}");
    }

    #[test]
    fn assign_teams_varies_with_the_match_id() {
        // FM1: the id actually steers the split (it is not a fixed function of seat
        // index), so two ids generally divide the same roster differently — the
        // property that defeats arrival-order team-stacking. A fixed scheme would
        // make every id agree; we assert at least one of many differs.
        let first = assign_teams(Uuid::from_u128(1), 8, 2);
        let differs = (2u128..256).any(|n| assign_teams(Uuid::from_u128(n), 8, 2) != first);
        assert!(differs, "team assignment must depend on the match id");
    }

    #[test]
    #[should_panic(expected = "divide evenly")]
    fn a_matchmaker_rejects_an_indivisible_team_size() {
        // FM1: five seats cannot form whole teams of two.
        Matchmaker::new(
            StubIdentityVerifier::new(),
            MatchParams { seats_per_match: 5, team_size: 2, ..MatchParams::default() },
        );
    }

    #[test]
    #[should_panic(expected = "at least 2 teams")]
    fn a_matchmaker_rejects_a_single_team_config() {
        // FM1: seats == team_size is one team, which ends on the first tick — the
        // team analogue of the sub-two-seat guard.
        Matchmaker::new(
            StubIdentityVerifier::new(),
            MatchParams { seats_per_match: 2, team_size: 2, ..MatchParams::default() },
        );
    }

    #[test]
    #[should_panic(expected = "at least 1")]
    fn a_matchmaker_rejects_a_zero_team_size() {
        Matchmaker::new(
            StubIdentityVerifier::new(),
            MatchParams { seats_per_match: 4, team_size: 0, ..MatchParams::default() },
        );
    }

    #[test]
    fn a_formed_2v2_seats_two_balanced_teams() {
        // FM1 end-to-end: a team_size-2 matchmaker forms a four-seat match split into
        // two teams of two, exactly the id-derived assignment (reproducible from the
        // minted id alone).
        let mm = Matchmaker::new(
            StubIdentityVerifier::new(),
            MatchParams { seats_per_match: 4, team_size: 2, ..MatchParams::default() },
        );
        for p in ["p0", "p1", "p2"] {
            assert!(mm.join(MatchMode::Human, b"", JoinRequest::human(p)).unwrap().is_queued());
        }
        let m = mm
            .join(MatchMode::Human, b"", JoinRequest::human("p3"))
            .unwrap()
            .into_formed()
            .expect("the fourth human completes the 2v2");
        assert_eq!(m.config().seats, 4);
        let counts = team_counts(&m.seats().iter().map(|s| s.team).collect::<Vec<_>>());
        assert_eq!(counts.len(), 2, "two teams: {counts:?}");
        assert!(counts.values().all(|&c| c == 2), "two seats each: {counts:?}");
        let got: Vec<TeamId> = m.seats().iter().map(|s| s.team).collect();
        assert_eq!(got, assign_teams(m.match_id(), 4, 2), "the formed teams are the id-derived split");
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
        rejected(mm.join(MatchMode::Agent, b"", JoinRequest::casual_agent("0xa")), "0xa");
        // A wrong token for a known agent.
        rejected(mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xa", "badsig")), "0xa");
        // A valid-looking token for an unregistered agent.
        rejected(mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xb", "goodsig")), "0xb");
        assert_eq!(mm.waiting(MatchMode::Agent), 0, "no unauthenticated agent entered the ranked queue");

        // The authorized identity is admitted.
        assert!(mm.join(MatchMode::Agent, b"", JoinRequest::ranked_agent("0xa", "goodsig")).unwrap().is_queued());
        assert_eq!(mm.waiting(MatchMode::Agent), 1);
    }

    #[test]
    fn mixed_rejects_a_forged_ranked_token_but_admits_casual() {
        // Mixed allows a casual (token-less) agent for cross-play, but a PRESENTED
        // token is a ranked claim — a forged one is rejected, so a bad ranked claim
        // cannot slip in disguised as casual play.
        let mm = ranked_mm(&[("0xgood", "goodsig")]);
        assert!(mm.join(MatchMode::Mixed, b"", JoinRequest::casual_agent("0xcasual")).unwrap().is_queued());
        let forged = mm.join(MatchMode::Mixed, b"", JoinRequest::ranked_agent("0xevil", "forged"));
        assert!(matches!(forged, Err(JoinError::Unauthenticated { .. })));
        assert!(mm.join(MatchMode::Mixed, b"", JoinRequest::ranked_agent("0xgood", "goodsig")).unwrap().is_queued());
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
        assert!(mm.join(MatchMode::Mixed, b"", empty()).unwrap().is_queued(), "an empty token is a casual Mixed seat");
        assert!(
            matches!(mm.join(MatchMode::Agent, b"", empty()), Err(JoinError::Unauthenticated { .. })),
            "an empty token is not a ranked claim, so Agent mode rejects it"
        );
    }

    // --- Ranked admission under real signature recovery (SignatureVerifier) ---

    /// A deterministic signing key (the dev vector mesh + proto share).
    fn agent_key() -> SigningKey {
        let bytes =
            hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318").unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    /// A second, distinct key whose address differs from `agent_key`.
    fn other_key() -> SigningKey {
        let bytes =
            hex::decode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    fn agent_addr(sk: &SigningKey) -> String {
        arena_proto::address_from_verifying_key(sk.verifying_key())
    }

    /// Sign a join the way the agent SDK will: `[r||s||v]` hex over
    /// `join_digest(version, agent_id, nonce)`.
    fn sign_join(sk: &SigningKey, version: u32, agent_id: &str, nonce: &[u8]) -> String {
        let digest = arena_proto::join_digest(version, agent_id, nonce);
        let (sig, recid): (Signature, RecoveryId) = sk.sign_prehash_recoverable(&digest).unwrap();
        let mut out = sig.to_bytes().to_vec();
        out.push(recid.to_byte());
        hex::encode(out)
    }

    fn signed_mm() -> Matchmaker<SignatureVerifier> {
        Matchmaker::new(SignatureVerifier, MatchParams::default())
    }

    #[test]
    fn signature_verifier_admits_the_key_holder_over_the_connection_nonce() {
        let sk = agent_key();
        let addr = agent_addr(&sk);
        let nonce: &[u8] = b"connection-1-challenge";
        let sig = sign_join(&sk, PROTOCOL_VERSION, &addr, nonce);
        assert!(SignatureVerifier.verify(&addr, nonce, &sig), "the key holder is admitted");
    }

    #[test]
    fn signature_verifier_rejects_a_wrong_key() {
        // A valid signature by a DIFFERENT key claiming the victim's address recovers
        // the attacker's address, not the claim, so it is not ranked.
        let victim = agent_addr(&agent_key());
        let nonce: &[u8] = b"connection-1-challenge";
        let forged = sign_join(&other_key(), PROTOCOL_VERSION, &victim, nonce);
        assert!(!SignatureVerifier.verify(&victim, nonce, &forged));
    }

    #[test]
    fn signature_verifier_rejects_a_replayed_nonce() {
        // A signature captured over connection A's challenge fails against connection
        // B's — the nonce binds the proof to one connection.
        let sk = agent_key();
        let addr = agent_addr(&sk);
        let sig = sign_join(&sk, PROTOCOL_VERSION, &addr, b"connection-A");
        assert!(!SignatureVerifier.verify(&addr, b"connection-B", &sig), "replay rejected");
        assert!(SignatureVerifier.verify(&addr, b"connection-A", &sig), "valid over its own nonce");
    }

    #[test]
    fn signature_verifier_rejects_a_tampered_version() {
        // A proof made under a different protocol_version does not verify under the
        // build's PROTOCOL_VERSION (the digest binds the version).
        let sk = agent_key();
        let addr = agent_addr(&sk);
        let nonce: &[u8] = b"connection-1";
        let sig = sign_join(&sk, PROTOCOL_VERSION + 1, &addr, nonce);
        assert!(!SignatureVerifier.verify(&addr, nonce, &sig));
    }

    #[test]
    fn signature_verifier_rejects_an_empty_signature_without_erroring() {
        // An empty signature is never ranked and never panics; the matchmaker treats
        // it as a casual seat upstream.
        let addr = agent_addr(&agent_key());
        assert!(!SignatureVerifier.verify(&addr, b"connection-1", ""));
    }

    #[test]
    fn agent_mode_forms_a_ranked_match_from_signed_seats() {
        // End to end on the real verifier: two key holders each sign their OWN
        // connection challenge and form a ranked Agent match.
        let (a, b) = (agent_key(), other_key());
        let (addr_a, addr_b) = (agent_addr(&a), agent_addr(&b));
        let (nonce_a, nonce_b): (&[u8], &[u8]) = (b"chal-a", b"chal-b");
        let sig_a = sign_join(&a, PROTOCOL_VERSION, &addr_a, nonce_a);
        let sig_b = sign_join(&b, PROTOCOL_VERSION, &addr_b, nonce_b);
        let mm = signed_mm();
        assert!(mm
            .join(MatchMode::Agent, nonce_a, JoinRequest::ranked_agent(addr_a.as_str(), sig_a))
            .unwrap()
            .is_queued());
        let m = mm
            .join(MatchMode::Agent, nonce_b, JoinRequest::ranked_agent(addr_b.as_str(), sig_b))
            .unwrap()
            .into_formed()
            .expect("two signed agents form a ranked match");
        let mut expected = vec![addr_a, addr_b];
        expected.sort();
        assert_eq!(controllers(&m), expected, "the recovered identities seat the match");
        assert_eq!(m.phase(), MatchPhase::Live);
    }

    #[test]
    fn agent_mode_rejects_a_wrong_key_signature() {
        // FM1: a present-but-unverified signature must not seat a ranked agent. The
        // attacker holds other_key but claims the victim's address.
        let mm = signed_mm();
        let victim = agent_addr(&agent_key());
        let nonce: &[u8] = b"chal";
        let forged = sign_join(&other_key(), PROTOCOL_VERSION, &victim, nonce);
        assert!(matches!(
            mm.join(MatchMode::Agent, nonce, JoinRequest::ranked_agent(victim.as_str(), forged)),
            Err(JoinError::Unauthenticated { .. })
        ));
        assert_eq!(mm.waiting(MatchMode::Agent), 0, "the forged claim never entered the ranked queue");
    }

    #[test]
    fn agent_mode_rejects_a_signature_replayed_from_another_connection() {
        // FM2: a signature captured on connection A, replayed on connection B (a
        // fresh challenge), is verified against B's nonce, fails recovery, and is
        // rejected — never seating a ranked agent on a stale proof.
        let mm = signed_mm();
        let sk = agent_key();
        let addr = agent_addr(&sk);
        let sig_for_a = sign_join(&sk, PROTOCOL_VERSION, &addr, b"connection-A");
        assert!(matches!(
            mm.join(
                MatchMode::Agent,
                b"connection-B",
                JoinRequest::ranked_agent(addr.as_str(), sig_for_a.clone())
            ),
            Err(JoinError::Unauthenticated { .. })
        ));
        // Sanity: on its own connection the same proof is admitted.
        assert!(mm
            .join(MatchMode::Agent, b"connection-A", JoinRequest::ranked_agent(addr.as_str(), sig_for_a))
            .unwrap()
            .is_queued());
    }

    #[test]
    fn an_empty_signature_is_casual_not_ranked_under_real_verification() {
        // FM4: an empty signature_hex is unranked — admitted as casual in Mixed,
        // rejected (ranked-required) in Agent — never an error, never ranked, even
        // under the real recovering verifier.
        let mm = signed_mm();
        let casual = || JoinRequest::casual_agent("0xobserver");
        assert!(
            mm.join(MatchMode::Mixed, b"chal", casual()).unwrap().is_queued(),
            "an empty signature is a casual Mixed seat"
        );
        assert!(
            matches!(mm.join(MatchMode::Agent, b"chal", casual()), Err(JoinError::Unauthenticated { .. })),
            "an empty signature is not a ranked claim"
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
        Match::new(FIXED_ID.parse().unwrap(), config, Rules::default(), seats, Vec::new(), 42)
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
    fn replay_streams_a_countdown_record_identically_to_no_countdown() {
        // A pre-live countdown is invisible to the scored stream, so a spectator feed
        // opens at Live tick 0 exactly as a no-countdown match: the countdown burns
        // silently before the first frame, never truncating the feed to the opening
        // Starting frame nor shifting the Live tick numbering.
        let countdown_record = |starting_ticks: u32| -> MatchRecord {
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
            let rules = Rules { starting_ticks, ..Default::default() };
            let mut m = Match::new(FIXED_ID.parse().unwrap(), config, rules, seats, Vec::new(), 42);
            while m.phase() != MatchPhase::Ended {
                m.step(&BTreeMap::new());
            }
            m.to_record().expect("a finished match yields a record")
        };

        let counted = replay_frames(&countdown_record(4)).expect("a countdown record replays");
        let plain = replay_frames(&countdown_record(0)).expect("a no-countdown record replays");
        assert_eq!(counted, plain, "the countdown is invisible to the spectator feed");
        assert_eq!(counted.first().unwrap().tick, 0, "the feed opens at Live tick 0");
        assert_eq!(
            counted.first().unwrap().phase,
            MatchPhase::Live,
            "the opening frame is Live, not the pre-live Starting state",
        );
        assert_eq!(counted.last().unwrap().phase, MatchPhase::Ended);
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
            padded.replay.ticks.push(arena_proto::TickRecord { tick: next + k, actions: Vec::new(), forfeits: Vec::new() });
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
    fn replay_frames_rejects_an_over_budget_record() {
        // The arena-core verifier cost-budget protects the spectator/replay endpoint:
        // replay_frames calls verify() first, so a record padded past MAX_REPLAY_TICKS
        // is a typed rejection — it never re-runs an attacker-sized tick stream. (The
        // 20k-padding test above stays under the cap, so it still replays.)
        let mut over = finished_record();
        let next = over.replay.ticks.len() as u64;
        for k in 0..=arena_core::MAX_REPLAY_TICKS as u64 {
            over.replay.ticks.push(arena_proto::TickRecord { tick: next + k, actions: Vec::new(), forfeits: Vec::new() });
        }
        assert!(over.replay.ticks.len() > arena_core::MAX_REPLAY_TICKS);
        assert!(matches!(replay_frames(&over), Err(ReplayError::TooManyTicks { .. })));
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

    #[test]
    fn replay_to_feed_streams_the_exact_replay_frames_then_one_terminal_end() {
        // FM1 + FM2: a spectator whose ring holds the whole match receives EXACTLY the
        // Broadcast sequence replay_frames returns, in order, then a SINGLE terminal End
        // carrying the match's MatchResult -- and nothing after. The publisher itself
        // drops/reorders/duplicates nothing (distinct from the feed's slow-consumer
        // lossiness, exercised below).
        let record = finished_record();
        let expected = replay_frames(&record).expect("a valid record replays");
        let result = record.verify().expect("a finished record verifies to its terminal result");

        // Ring sized for every frame + the End, so a non-reading spectator sheds nothing.
        let feed = SpectatorFeed::new(expected.len() + 1, 2);
        let spec = feed.subscribe().unwrap();
        let published = replay_to_feed(&record, &feed).expect("the record streams to the feed");
        assert_eq!(published, expected.len(), "one publish per replay_frames frame");

        let mut streamed = Vec::new();
        let mut ended: Option<MatchResult> = None;
        let mut ends = 0u32;
        while let Some(msg) = spec.recv() {
            match msg {
                SpectatorMsg::Frame(b) => {
                    assert_eq!(ends, 0, "every frame precedes the terminal End");
                    streamed.push(b);
                }
                SpectatorMsg::End(r) => {
                    ends += 1;
                    ended = Some(r);
                }
            }
        }
        assert_eq!(streamed, expected, "the feed carries the exact replay_frames sequence, in order");
        assert_eq!(ends, 1, "publish_end fires exactly once");
        assert_eq!(ended, Some(result), "the terminal End carries the match's MatchResult");
        assert_eq!(spec.dropped(), 0, "a spectator sized for the match drops nothing");
    }

    #[test]
    fn replay_to_feed_completes_past_a_slow_spectator_that_sheds_overflow() {
        // FM3: the feed is lossy + non-blocking, so a slow consumer must NEVER stall the
        // publisher. With a ring far smaller than the match, a spectator that never reads
        // sheds its overflow (counted), yet replay_to_feed still RUNS TO COMPLETION and
        // streams every frame -- consumer speed is decoupled from the replay.
        let record = finished_record();
        let frame_count = replay_frames(&record).unwrap().len();
        assert!(frame_count >= 3, "the match must outlast the slow ring of 2");
        let total_msgs = frame_count + 1; // every frame plus the terminal End

        let feed = SpectatorFeed::new(2, 4);
        let slow = feed.subscribe().unwrap(); // never reads
        let published = replay_to_feed(&record, &feed).expect("completes despite the stalled consumer");

        assert_eq!(published, frame_count, "the publisher streamed every frame to completion");
        assert_eq!(slow.buffered(), 2, "the slow ring stays bounded to its capacity");
        assert_eq!(
            slow.dropped(),
            (total_msgs - 2) as u64,
            "the slow consumer shed its overflow instead of backpressuring the publisher"
        );
    }

    #[test]
    fn replay_to_feed_streams_only_simulated_frames_for_a_padded_record() {
        // FM4 (inherited from replay_frames): a record padded with canonical
        // post-terminal ticks passes verify() (they are scanned but never simulated), so
        // replay_to_feed must stream ONLY the real frames -- the padding can't turn the
        // feed into an unbounded publish loop.
        let mut padded = finished_record();
        let real_frames = replay_frames(&padded).unwrap().len();
        let next = padded.replay.ticks.len() as u64;
        for k in 0..20_000u64 {
            padded.replay.ticks.push(arena_proto::TickRecord { tick: next + k, actions: Vec::new(), forfeits: Vec::new() });
        }

        // Ring sized for the REAL frames + End: an extra padding publish would overflow it.
        let feed = SpectatorFeed::new(real_frames + 1, 2);
        let spec = feed.subscribe().unwrap();
        let published = replay_to_feed(&padded, &feed).expect("a padded record still verifies + streams");

        assert_eq!(published, real_frames, "only the simulated frames are published, not the 20k padding");
        let frames = std::iter::from_fn(|| spec.recv())
            .filter(|m| matches!(m, SpectatorMsg::Frame(_)))
            .count();
        assert_eq!(frames, real_frames, "the spectator sees only the real frames");
        assert_eq!(spec.dropped(), 0, "no padding frame was ever published, so nothing was shed");
    }

    /// Form a 2-seat Human match on `arena` and hand back the built [`Match`].
    fn form_on(arena: &'static str) -> Match {
        let mm = Matchmaker::new(StubIdentityVerifier::new(), MatchParams { arena, ..MatchParams::default() });
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        mm.join(MatchMode::Human, b"", JoinRequest::human("b")).unwrap().into_formed().unwrap()
    }

    #[test]
    fn a_default_arena_forms_a_match_with_no_geometry() {
        // FM1: an unconfigured matchmaker (arena "") forms a match byte-identical to
        // the pre-map-loading path — no blockers, no pickups.
        let replay = form_on("").into_replay();
        assert!(replay.blockers.is_empty(), "the default arena has no blockers");
        assert!(replay.pickups.is_empty(), "the default arena has no pickups");
    }

    #[test]
    fn a_named_arena_loads_its_geometry_into_the_formed_match() {
        // FM3: build() routes the configured arena's map through new_with_pickups, so
        // a formed match carries that arena's EXACT blockers + pickups — not the
        // empty set the old Match::new path dropped them to.
        let map = arena_map("reference");
        let replay = form_on("reference").into_replay();
        assert_eq!(replay.blockers, map.blockers, "the reference blockers reached the match");
        assert_eq!(replay.pickups, map.pickups, "the reference pickups reached the match");
    }

    #[test]
    fn an_unknown_arena_forms_a_match_with_no_geometry() {
        // FM4: an unrecognised arena key degrades safe to the empty arena — never a
        // panic, never a stray map.
        let replay = form_on("no-such-arena").into_replay();
        assert!(replay.blockers.is_empty() && replay.pickups.is_empty(), "an unknown arena is empty");
    }

    /// Form a 2-seat Human match under `rules` and hand back the built [`Match`] — the
    /// rules twin of [`form_on`], for confirming the matchmaker forms under its configured
    /// tuning rather than a hardcoded default.
    fn form_under(rules: Rules) -> Match {
        let mm = Matchmaker::new(StubIdentityVerifier::new(), MatchParams { rules, ..MatchParams::default() });
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        mm.join(MatchMode::Human, b"", JoinRequest::human("b")).unwrap().into_formed().unwrap()
    }

    #[test]
    fn an_unconfigured_matchmaker_forms_under_default_rules() {
        // FM1: the default MatchParams.rules is Rules::default(), so a matchmaker no one
        // tuned forms a match byte-identical to the pre-rules-knob path — memory off,
        // full-circle FOV, every combat constant at its historical default.
        assert_eq!(form_under(Rules::default()).rules(), Rules::default());
    }

    #[test]
    fn configured_matchparams_rules_reach_the_formed_match() {
        // FM2: build() forms under self.params.rules, NOT a hardcoded Rules::default(), so a
        // perception-memory window configured on the matchmaker reaches the sim the match
        // runs — the seat memory an agent reads as an in_line_of_sight=false echo. This is
        // what lets the --mode/ranked path carry the window the direct path already threads;
        // the WHOLE ruleset crosses, not just the one field.
        let rules = Rules { perception_memory_ticks: 30, ..Rules::default() };
        let formed = form_under(rules);
        assert_eq!(formed.rules().perception_memory_ticks, 30, "the configured window reached the formed match");
        assert_eq!(formed.rules(), rules, "the WHOLE configured ruleset reached the match, not just one field");
    }

    #[test]
    fn replay_frames_rebuilds_pickups_so_the_feed_matches_the_live_match() {
        // FM2: world pickups are a broadcast determinant — seat 0 survives this match
        // ONLY by repeatedly healing on a pickup it stands on while seat 1 shoots it.
        // replay_frames must rebuild those pickups, or the re-run kills seat 0 early
        // and the spectator feed diverges from (and runs shorter than) the real match.
        let rules = Rules {
            spawn_radius: 5 * POSITION_SCALE,
            spawn_jitter: 0,
            fire_cooldown: 0,
            damage: 25,
            start_health: 100,
            weapon_range: 50 * POSITION_SCALE,
            pickup_respawn_cooldown: 1,
            ..Default::default()
        };
        let seats = vec![
            SeatInfo { seat: 0, team: 0, controller: "p0".into() },
            SeatInfo { seat: 1, team: 1, controller: "p1".into() },
        ];
        let pickups = vec![PickupSpawn {
            kind: PickupKind::Health,
            position: Vec2 { x: -5 * POSITION_SCALE, y: 0 },
            amount: 100,
        }];
        let config = MatchConfig {
            tick_hz: 30,
            max_ticks: 10,
            bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
            seats: 2,
        };
        let mut m = Match::new_with_pickups(Uuid::new_v4(), config, rules, seats, Vec::new(), pickups, 1);

        // Seat 1 fires WEST (0x8000, toward seat 0) every tick; seat 0 forfeits its
        // action and just stands on the pickup, healing as it respawns.
        let fire_west = ActionIntent {
            move_dir: Vec2 { x: 0, y: 0 },
            aim: 0x8000,
            buttons: ActionButtons { fire: true, jump: false, ability: false, reload: false },
        };
        let mut live = vec![m.broadcast()];
        while m.phase() == MatchPhase::Live {
            let intents = BTreeMap::from([(1u8, fire_west)]);
            m.step(&intents);
            live.push(m.broadcast());
        }

        let seat0_alive = |b: &Broadcast| b.entities.iter().find(|e| e.entity_id == 0).unwrap().alive;
        assert!(
            seat0_alive(live.last().unwrap()),
            "seat 0 must survive via heals — otherwise the test cannot catch the bug"
        );

        let record = m.to_record().unwrap();
        assert!(!record.replay.pickups.is_empty(), "the record carries the pickup");
        let replayed = replay_frames(&record).expect("a valid pickup record replays");
        assert_eq!(live, replayed, "the spectator feed must reproduce the live match's pickup heals");
    }
}
