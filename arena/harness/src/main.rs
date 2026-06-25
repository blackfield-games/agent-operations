//! arena-harness — a line-delimited JSON loopback gateway over the arena-02
//! reference core, so an external (e.g. Python) agent can play a real match
//! against the real simulation without an engine or a network.
//!
//! This is transport glue, NOT a second source of gameplay: every rule lives in
//! `arena-core` and is reached only through the existing `observe` / `ingest` /
//! `step`. The harness mints no gameplay state of its own. A real networked
//! gateway is one connection per seat; this multiplexes all seats over one stdio
//! pipe with a thin `{ "seat": u8, "frame": <arena-01 msg> }` envelope, so the
//! `frame` payload is pure arena-01 and an agent SDK written against it needs no
//! harness-specific code.
//!
//! Determinism: the match id and seed come from argv (a random server-minted id
//! would make the replay hash non-reproducible), so the same flags produce a
//! byte-identical `MatchResult` every run — the property the integration test pins.
//!
//! Protocol, per seat, exactly as arena-01 defines it:
//!   server -> Challenge ; agent -> Join ; server -> Welcome, Start ;
//!   then each tick  server -> Observe ; agent -> Act|Leave ;
//!   and at the end  server -> End(MatchResult).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use arena_core::{
    ranked_delta, ranked_field_delta, settlement, Match, Rules, SeatDelta, Settlement, DEFAULT_RATING,
};
use arena_proto::{
    check_version, verify_join_signature, ActionIntent, AgentMsg, GatewayMsg, JoinVerifyError,
    MatchConfig, MatchPhase, MatchResult, ReplayRecord, SeatId, SeatInfo, Vec2, POSITION_SCALE,
    PROTOCOL_VERSION,
};
use uuid::Uuid;

/// A fixed, valid v4-shaped id used when `--match-id` is omitted, so a bare
/// invocation is still fully deterministic.
const DEFAULT_MATCH_ID: &str = "00000000-0000-4000-8000-000000000000";

struct Args {
    match_id: Uuid,
    seed: u64,
    seats: u8,
    max_ticks: u64,
    /// Drive the off-chain settlement path through a [`MockSettler`] after the
    /// match (logging to stderr), mirroring mesh's `--relay-dev-mock`. Off by
    /// default so the loopback's stdout — and its replay determinism — is
    /// byte-identical; the live Base settler is operator-gated.
    settle_dev_mock: bool,
}

fn parse_args() -> Args {
    let mut match_id = DEFAULT_MATCH_ID.to_string();
    let mut seed: u64 = 0;
    let mut seats: u8 = 2;
    let mut max_ticks: u64 = 3600;
    let mut settle_dev_mock = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--match-id" => match_id = it.next().expect("--match-id needs a value"),
            "--seed" => seed = it.next().expect("--seed needs a value").parse().expect("seed is a u64"),
            "--seats" => seats = it.next().expect("--seats needs a value").parse().expect("seats is a u8"),
            "--max-ticks" => {
                max_ticks = it.next().expect("--max-ticks needs a value").parse().expect("max-ticks is a u64")
            }
            "--settle-dev-mock" => settle_dev_mock = true,
            other => panic!("unknown argument: {other}"),
        }
    }
    Args {
        match_id: Uuid::parse_str(&match_id).expect("--match-id is a valid UUID"),
        seed,
        seats,
        max_ticks,
        settle_dev_mock,
    }
}

/// A deterministic per-seat challenge nonce. Unranked play ignores it (no
/// signature), but it stays fixed per (match, seat) so the handshake adds no
/// nondeterminism.
fn nonce_for(match_id: Uuid, seat: SeatId) -> String {
    let mut bytes = match_id.as_bytes().to_vec();
    bytes.push(seat);
    hex::encode(bytes)
}

/// The harness's ranked-admission gate — the loopback twin of `arena-match`'s
/// production identity verifier and the networked Gateway. An EMPTY `signature_hex`
/// is an unranked seat (the baseline's default — admitted with no proof); a
/// non-empty one MUST recover to `agent_id` over `nonce` through the same
/// [`verify_join_signature`] the contract-backed admission uses. So the loopback
/// admits unranked play AND validly-signed ranked play, and refuses only a
/// PRESENTED-but-invalid signature — it never silently seats a forged identity.
fn admit_join(agent_id: &str, nonce: &[u8], signature_hex: &str) -> Result<(), JoinVerifyError> {
    if signature_hex.is_empty() {
        return Ok(());
    }
    verify_join_signature(PROTOCOL_VERSION, agent_id, nonce, signature_hex)
}

fn emit(out: &mut impl Write, seat: SeatId, msg: &GatewayMsg) {
    let frame = serde_json::to_value(msg).expect("serialize gateway message");
    let envelope = serde_json::json!({ "seat": seat, "frame": frame });
    writeln!(out, "{}", serde_json::to_string(&envelope).expect("serialize envelope"))
        .expect("write frame");
}

fn read_agent(line: &str) -> (SeatId, AgentMsg) {
    let v: serde_json::Value = serde_json::from_str(line).expect("parse transport envelope");
    let seat = v.get("seat").and_then(serde_json::Value::as_u64).expect("envelope seat") as SeatId;
    let msg: AgentMsg =
        serde_json::from_value(v.get("frame").expect("envelope frame").clone()).expect("parse agent message");
    (seat, msg)
}

fn next_line(lines: &mut impl Iterator<Item = io::Result<String>>) -> String {
    loop {
        match lines.next() {
            Some(Ok(l)) if !l.trim().is_empty() => return l,
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("read error on agent stream: {e}"),
            None => panic!("agent stream ended before the match did"),
        }
    }
}

/// Why a settlement submission recorded no fresh resolution.
#[derive(Debug)]
enum SettleError {
    /// This `match_id` is already resolved — the on-chain `MatchSettlement` fence
    /// (every resolution requires `Status.Open`) reverted `MatchNotOpen`. An
    /// *idempotent* outcome: a crash/retry after the match ended re-submitted a
    /// settlement that already landed. The caller treats it as benign (the terminal
    /// state already holds) instead of double-applying reputation or escrow.
    AlreadyResolved,
    /// The result is not a 1v1 ranked pair (returned by [`settle_match`]). `settle`/
    /// `settleDraw` take exactly two agents (`agentA` vs `agentB`), so any other seat
    /// count has no `settle`/`settleDraw` form — a 3+ field settles through
    /// [`settle_field_match`]/`settleField` instead — and the 1v1 seam refuses it rather
    /// than commit a resolution the contract can't accept.
    NotRankedPair,
    /// The match is not a multi-seat (FFA / 3+) ranked field. The symmetric guard to
    /// [`NotRankedPair`](SettleError::NotRankedPair) on the field seam
    /// ([`settle_field_match`]): a 1v1 pair (or a degenerate single/empty result)
    /// settles through [`settle_match`] in the single-delta `settle`/`settleDraw`
    /// shape, so the field path refuses it rather than emit a per-seat vector for a
    /// result the 1v1 path owns (and the contract's `settleField` itself rejects a
    /// sub-2 field).
    NotRankedField,
    /// The supplied per-seat ratings do not align 1:1 with the result's seats. The
    /// field delta pairs `ratings[i]` to `outcomes[i].seat` positionally, so a
    /// wrong-length rating vector would mis-pair seats to ratings — refused before any
    /// emit rather than settled against a misaligned vector.
    RatingsMismatch,
}

/// The off-chain → on-chain settlement boundary for a finished match. Mirrors the
/// three `MatchSettlement` resolutions — `settle` (decisive winner), `settleDraw`,
/// and `cancelMatch` — and, except for a cancel, commits the canonical
/// [`ReplayRecord`] digest of the exact match being settled.
///
/// The trait is transport-agnostic: it takes plain data, never a key, RPC URL, or
/// signer, so [`MockSettler`] drives the whole flow offline. The live Base
/// implementation (an RPC provider plus an authorized attester key with gas and
/// real-fund custody) is operator-gated and not built here — it slots in behind
/// this trait, the same Relay/Spender split mesh uses.
/// The signed reputation a settlement applies to the FIRST-ordered party — the
/// winner for [`settle`](Settle::settle), the lower-seat participant (`agentA`) for
/// [`settle_draw`](Settle::settle_draw) — with the counterparty receiving the
/// contract-applied negation, so the on-chain `recordMatchResult(+d)` /
/// `recordMatchResult(-d)` pair stays zero-sum (the core guarantees `b == -a`).
///
/// `None` defers to the contract's own FIXED `reputationDelta` — the pre-ladder
/// behaviour — so a settlement with no ranked context is byte-identical to before.
/// `Some(d)` carries the variable Elo delta [`ranked_delta`] computed from the two
/// participants' ratings: a favoured win earns less, an upset more.
type ReputationDelta = Option<i32>;

/// One seat's line in a settled multi-seat (FFA / 3+) ranked field: the agent identity
/// (the seat's roster `controller`, the harness stand-in for the on-chain address) and
/// its signed zero-sum reputation delta. The on-chain `settleField(agents[], deltas[])`
/// consumes a field as two parallel arrays in this canonical ascending-seat order, so a
/// reordering here would credit the wrong agent on-chain — the entries are built in the
/// exact order [`ranked_field_delta`] returns and never re-sorted.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FieldEntry {
    agent: String,
    delta: i32,
}

trait Settle {
    fn settle(&self, match_id: Uuid, winner: &str, reputation: ReputationDelta, replay_digest: [u8; 32]) -> Result<(), SettleError>;
    fn settle_draw(&self, match_id: Uuid, reputation: ReputationDelta, replay_digest: [u8; 32]) -> Result<(), SettleError>;
    /// Settle a multi-seat (FFA / 3+) ranked result to reputation: the zero-sum per-seat
    /// `entries` in canonical ascending-seat order, mirroring the on-chain `settleField`.
    /// No winner — placement is folded into the per-seat deltas. Reputation-only (no
    /// escrow), matching the contract slice.
    fn settle_field(&self, match_id: Uuid, entries: Vec<FieldEntry>, replay_digest: [u8; 32]) -> Result<(), SettleError>;
    fn cancel(&self, match_id: Uuid) -> Result<(), SettleError>;
}

/// One recorded resolution, mirroring the terminal state the contract would hold.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolution {
    Win { winner: String, reputation: ReputationDelta, replay_digest: [u8; 32] },
    Draw { reputation: ReputationDelta, replay_digest: [u8; 32] },
    /// A multi-seat (FFA / 3+) field: the zero-sum per-seat lines in canonical
    /// ascending-seat order, the harness mirror of the on-chain `settleField`.
    Field { entries: Vec<FieldEntry>, replay_digest: [u8; 32] },
    Cancelled,
}

/// In-process [`Settle`] for tests and the `--settle-dev-mock` path. Never touches
/// a chain: it records each resolution and models the contract's per-`match_id`
/// fence — a second resolution of ANY kind returns [`SettleError::AlreadyResolved`],
/// the same logic the on-chain `Status.Open` check applies.
///
/// The map is in-memory, so this guards a retry WITHIN one run, not across a
/// process crash: the authoritative, crash-durable double-settle guard is the
/// on-chain fence (a real submitter that crashed mid-settle re-submits and the
/// chain rejects it). The mock also does NOT re-implement the contract's other
/// input checks (winner-is-a-participant, distinct agents) — those stay the
/// contract's job; the mock models only the idempotency boundary.
#[derive(Default)]
struct MockSettler {
    resolved: RefCell<BTreeMap<Uuid, Resolution>>,
}

impl MockSettler {
    /// Apply the per-`match_id` fence, then record. Reads the map (the fence) and
    /// writes it, so a replay of any resolution is rejected exactly as the on-chain
    /// `Status.Open` check rejects a second settle/draw/cancel.
    fn record(&self, match_id: Uuid, resolution: Resolution) -> Result<(), SettleError> {
        let mut resolved = self.resolved.borrow_mut();
        if resolved.contains_key(&match_id) {
            return Err(SettleError::AlreadyResolved);
        }
        resolved.insert(match_id, resolution);
        Ok(())
    }

    fn resolution(&self, match_id: Uuid) -> Option<Resolution> {
        self.resolved.borrow().get(&match_id).cloned()
    }
}

impl Settle for MockSettler {
    fn settle(&self, match_id: Uuid, winner: &str, reputation: ReputationDelta, replay_digest: [u8; 32]) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Win { winner: winner.to_string(), reputation, replay_digest })
    }

    fn settle_draw(&self, match_id: Uuid, reputation: ReputationDelta, replay_digest: [u8; 32]) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Draw { reputation, replay_digest })
    }

    fn settle_field(&self, match_id: Uuid, entries: Vec<FieldEntry>, replay_digest: [u8; 32]) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Field { entries, replay_digest })
    }

    fn cancel(&self, match_id: Uuid) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Cancelled)
    }
}

/// The ranked-rating context a settlement needs to compute the variable reputation
/// delta: the two seats' pre-match ratings — `rating_a` for the first canonical
/// outcome seat (the lower seat id, `agentA`), `rating_b` for the second — and the
/// owner-set K-factor. Supplied by the live rating ladder; the loopback driver has
/// no ladder, so it passes `None` and the settlement defers to the contract's fixed
/// delta (byte-identical).
#[derive(Clone, Copy)]
struct RankedContext {
    rating_a: i32,
    rating_b: i32,
    k: i32,
}

/// The ranked context a MULTI-SEAT (FFA / 3+) settlement needs: every seat's pre-match
/// rating in canonical ascending-seat order — `ratings[i]` is `result.outcomes[i].seat`'s
/// rating, the same positional pairing [`ranked_field_delta`] requires — and the
/// owner-set K-factor. Supplied by the live rating ladder; an unrated agent reads as
/// [`DEFAULT_RATING`] (so a loopback field, whose agents are all unseen, settles at the
/// default), and the owner-set multi-seat K is the live driver's to pass.
#[derive(Clone)]
struct FieldContext {
    ratings: Vec<i32>,
    k: i32,
}

/// Drive one finished match through the settler: classify it, then submit the
/// matching resolution carrying the canonical `replay.digest()`. The digest is
/// taken straight from [`ReplayRecord::digest`] (not re-derived from the result's
/// hex), so the on-chain commitment is byte-identical to the recorded replay. The
/// winner identity is the winning seat's roster `controller` — the harness
/// stand-in for the on-chain agent address. Returns the chosen [`Settlement`] for
/// the caller to report. A cancel is NOT produced here: a finished match always
/// has a result; `cancel` is the pre-play abort path.
///
/// When `ranked` is supplied, the settlement carries the variable Elo reputation
/// delta [`ranked_delta`] derives from the two ratings and the outcome — the
/// winner's signed gain for a decisive result, `agentA`'s signed change for a draw
/// (negative when `agentA` was favoured). The delta is settlement metadata: it does
/// NOT touch `digest`, so the committed identity is identical with or without it.
/// `ranked == None` carries `None` (defer to the contract's fixed delta).
fn settle_match(
    settler: &impl Settle,
    result: &MatchResult,
    replay: &ReplayRecord,
    ranked: Option<RankedContext>,
) -> Result<Settlement, SettleError> {
    // settle/settleDraw are strictly 1v1; a non-pair result has no settle/settleDraw
    // form, so refuse it here (a 3+ FFA settles through settle_field_match instead)
    // rather than emit a Win/Draw the contract structurally cannot accept.
    if result.outcomes.len() != 2 {
        return Err(SettleError::NotRankedPair);
    }
    let digest = replay.digest();
    let outcome = settlement(result);
    // The zero-sum per-seat Elo delta (keyed to the canonical seat order: `.a` to the
    // first outcome seat, `.b == -.a` to the second), when a ranked context is given.
    // The 2-seat guard above means ranked_delta always yields Some here.
    let delta = ranked.map(|r| {
        ranked_delta(result, r.rating_a, r.rating_b, r.k).expect("a 2-seat result has a ranked delta")
    });
    match outcome {
        Settlement::Win { seat } => {
            let winner = replay
                .seats
                .iter()
                .find(|s| s.seat == seat)
                .map(|s| s.controller.as_str())
                .expect("the winning seat is in the roster");
            // The winner's signed reputation: `.a` if it is the first outcome seat,
            // else `.b` — always the positive side of the zero-sum split for a win.
            let reputation = delta.map(|d| if seat == result.outcomes[0].seat { d.a } else { d.b });
            settler.settle(result.match_id, winner, reputation, digest)?;
        }
        // A draw carries `agentA`'s (the first outcome seat's) signed change; the
        // contract applies its negation to `agentB`. Even ratings ⇒ 0; otherwise the
        // favoured seat moves down.
        Settlement::Draw => settler.settle_draw(result.match_id, delta.map(|d| d.a), digest)?,
    }
    Ok(outcome)
}

/// Drive a finished MULTI-SEAT (FFA / 3+) match through the settler — the sibling of
/// [`settle_match`] for a result the 1v1 `settle`/`settleDraw` cannot express. Sources
/// each seat's pre-match rating from `field` in canonical order, computes the zero-sum
/// per-seat vector [`ranked_field_delta`], pairs each delta with its seat's roster
/// `controller` (the on-chain address stand-in), and submits the whole field through
/// [`Settle::settle_field`] carrying the canonical `replay.digest()`. Returns the seat
/// count settled.
///
/// The per-seat deltas are settlement metadata: they do NOT touch `digest`, so the
/// committed identity is byte-identical with or without them — the same property the 1v1
/// reputation delta has.
///
/// Refuses anything the 1v1 path owns: fewer than 3 seats is [`SettleError::NotRankedField`]
/// (a pair settles through [`settle_match`] in the single-delta shape, never as a
/// 2-vector), and a `field` whose ratings do not align 1:1 with the seats is
/// [`SettleError::RatingsMismatch`] — refused before any emit rather than mis-paired.
fn settle_field_match(
    settler: &impl Settle,
    result: &MatchResult,
    replay: &ReplayRecord,
    field: FieldContext,
) -> Result<usize, SettleError> {
    let n = result.outcomes.len();
    if n < 3 {
        return Err(SettleError::NotRankedField);
    }
    if field.ratings.len() != n {
        return Err(SettleError::RatingsMismatch);
    }
    let digest = replay.digest();
    // n >= 3 and ratings aligned 1:1 ⇒ Some; the two guards above make this total, the
    // same way the 2-seat guard makes `ranked_delta` total in `settle_match`.
    let deltas = ranked_field_delta(result, &field.ratings, field.k)
        .expect("a >=2-seat result with aligned ratings has a field delta");
    let entries = deltas
        .into_iter()
        .map(|SeatDelta { seat, delta }| {
            // Map each canonical seat to its roster controller — the same seat→identity
            // lookup the 1v1 winner path does, per seat. The result's outcome seats are a
            // subset of the roster, so the seat is always present.
            let agent = replay
                .seats
                .iter()
                .find(|s| s.seat == seat)
                .map(|s| s.controller.clone())
                .expect("a field seat is in the roster");
            FieldEntry { agent, delta }
        })
        .collect();
    settler.settle_field(result.match_id, entries, digest)?;
    Ok(n)
}

/// The K-factor the `--settle-dev-mock` loopback uses for a multi-seat field settle. The
/// loopback settles only an in-memory [`MockSettler`] (never a chain), so this sets the
/// magnitude of the demonstrated deltas, not any production economic knob — the live
/// driver passes the owner-set K. 32 matches the value the ranked unit tests use.
const DEV_MOCK_K: i32 = 32;

fn main() {
    let args = parse_args();
    let n = args.seats;
    let roster: Vec<SeatInfo> = (0..n)
        .map(|i| SeatInfo { seat: i, team: u16::from(i), controller: format!("agent-{i}") })
        .collect();
    let config = MatchConfig {
        tick_hz: 30,
        max_ticks: args.max_ticks,
        bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
        seats: n,
    };
    let mut m = Match::new(args.match_id, config, Rules::default(), roster, Vec::new(), args.seed);

    // The off-chain settlement seam: a finished match (or a pre-play abort) maps to
    // a MatchSettlement resolution through this settler. Mock-only and opt-in here;
    // the live Base submitter is operator-gated.
    let settler = args.settle_dev_mock.then(MockSettler::default);

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for seat in 0..n {
        emit(&mut out, seat, &GatewayMsg::Challenge { nonce: nonce_for(m.match_id(), seat) });
    }
    out.flush().expect("flush challenges");

    // Reply to each Join the moment it arrives — a seat's Welcome+Start must not
    // wait on another seat's Join, or a client that connects sequentially and
    // blocks on its own Welcome would deadlock against a harness waiting for the
    // next Join.
    for _ in 0..n {
        let line = next_line(&mut lines);
        let (seat, msg) = read_agent(&line);
        let AgentMsg::Join { protocol_version, agent_id, signature_hex } = msg else {
            panic!("expected a join during the handshake");
        };
        if check_version(protocol_version).is_err() {
            emit(
                &mut out,
                seat,
                &GatewayMsg::Reject {
                    reason: format!(
                        "protocol version mismatch: ours={PROTOCOL_VERSION}, theirs={protocol_version}"
                    ),
                },
            );
            out.flush().expect("flush reject");
            if let Some(s) = &settler {
                // An opened match that can never be played voids as a cancel —
                // refund, no result committed — exactly MatchSettlement.cancelMatch.
                eprintln!(
                    "[settle-dev-mock] {} cancel (handshake version mismatch): {:?}",
                    m.match_id(),
                    s.cancel(m.match_id())
                );
            }
            std::process::exit(1);
        }
        // The agent signed join_digest over THIS seat's challenge nonce; recover it
        // and refuse a presented-but-invalid ranked proof, mirroring the version arm.
        // An empty signature is an unranked seat and admits unchanged.
        let nonce = nonce_for(m.match_id(), seat);
        if let Err(e) = admit_join(&agent_id, nonce.as_bytes(), &signature_hex) {
            emit(
                &mut out,
                seat,
                &GatewayMsg::Reject { reason: format!("join signature rejected: {e:?}") },
            );
            out.flush().expect("flush reject");
            if let Some(s) = &settler {
                // A presented-but-invalid ranked proof voids the opened match like a
                // version mismatch — refund, no result committed — exactly cancelMatch.
                eprintln!(
                    "[settle-dev-mock] {} cancel (join signature rejected): {:?}",
                    m.match_id(),
                    s.cancel(m.match_id())
                );
            }
            std::process::exit(1);
        }
        emit(
            &mut out,
            seat,
            &GatewayMsg::Welcome { protocol_version: PROTOCOL_VERSION, match_id: m.match_id(), seat },
        );
        emit(
            &mut out,
            seat,
            &GatewayMsg::Start {
                match_id: m.match_id(),
                config: m.config(),
                blockers: m.blockers().to_vec(),
                pickup_points: m.pickup_spawns().iter().map(|p| p.position).collect(),
            },
        );
        out.flush().expect("flush welcome+start");
    }

    while m.phase() == MatchPhase::Live {
        for seat in 0..n {
            emit(&mut out, seat, &GatewayMsg::Observe(m.observe(seat)));
        }
        out.flush().expect("flush observations");

        let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
        for _ in 0..n {
            let line = next_line(&mut lines);
            let (seat, msg) = read_agent(&line);
            match msg {
                // ingest is the server-authoritative gate; a rejected action (wrong
                // tick/seat, downed, version) simply forfeits the tick.
                AgentMsg::Act(action) => {
                    if let Ok(intent) = m.ingest(seat, &action) {
                        intents.insert(seat, intent);
                    }
                }
                AgentMsg::Leave { .. } => {}
                AgentMsg::Join { .. } => panic!("unexpected join during the match"),
            }
        }
        m.step(&intents);
    }

    let result = m.result().expect("an ended match has a result").clone();
    for seat in 0..n {
        emit(&mut out, seat, &GatewayMsg::End(result.clone()));
    }
    out.flush().expect("flush results");

    if let Some(s) = &settler {
        let match_id = m.match_id();
        let replay = m.into_replay();
        // Loopback agents are unrated, so each reads as DEFAULT_RATING — exactly what the
        // live ladder returns for an unseen agent. A 1v1 then defers to the contract's
        // fixed delta (None, byte-identical to pre-ladder); a 3+ field, which has no
        // fixed-delta form, settles its zero-sum placement vector. The live driver passes
        // real ladder ratings and the owner-set K in place of these.
        let seats = result.outcomes.len();
        let report = if seats > 2 {
            let field = FieldContext { ratings: vec![DEFAULT_RATING; seats], k: DEV_MOCK_K };
            settle_field_match(s, &result, &replay, field).map(|n| format!("field of {n} seats"))
        } else {
            settle_match(s, &result, &replay, None).map(|o| format!("{o:?}"))
        };
        match report {
            Ok(desc) => eprintln!("[settle-dev-mock] {match_id} settled as {desc}: {:?}", s.resolution(match_id)),
            Err(e) => eprintln!("[settle-dev-mock] {match_id} settle failed: {e:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_proto::{address_from_verifying_key, join_digest, SeatOutcome};
    use k256::ecdsa::{RecoveryId, Signature, SigningKey};

    const MID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn id() -> Uuid {
        Uuid::parse_str(MID).unwrap()
    }

    fn roster(n: u8) -> Vec<SeatInfo> {
        (0..n).map(|i| SeatInfo { seat: i, team: u16::from(i), controller: format!("agent-{i}") }).collect()
    }

    fn replay_for(seats: Vec<SeatInfo>) -> ReplayRecord {
        ReplayRecord {
            protocol_version: PROTOCOL_VERSION,
            match_id: id(),
            seed: 0,
            seats,
            blockers: Vec::new(),
            pickups: Vec::new(),
            rules_commit: Vec::new(),
            config: MatchConfig::default(),
            ticks: Vec::new(),
        }
    }

    fn outcome(seat: SeatId, placement: u16, score: i32, alive: bool) -> SeatOutcome {
        SeatOutcome { seat, team: u16::from(seat), placement, score, alive_at_end: alive }
    }

    fn result_for(outcomes: Vec<SeatOutcome>) -> MatchResult {
        MatchResult {
            protocol_version: PROTOCOL_VERSION,
            match_id: id(),
            final_tick: 1,
            outcomes,
            replay_hash: "00".repeat(32),
        }
    }

    #[test]
    fn win_settles_once_with_the_winner_identity_and_core_digest() {
        // Seat 1 — NOT the first outcome — wins, so this also pins that the driver
        // resolves the placement-1 seat's controller rather than `outcomes[0]`.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 2, 1, false), outcome(1, 1, 5, true)]);
        let settler = MockSettler::default();

        let chosen = settle_match(&settler, &result, &replay, None).expect("settles");
        assert_eq!(chosen, Settlement::Win { seat: 1 });
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Win { winner: "agent-1".into(), reputation: None, replay_digest: replay.digest() }),
            "no ranked context ⇒ reputation None (defer to the contract's fixed delta), winner identity + core digest",
        );
    }

    #[test]
    fn retry_is_a_no_op_and_never_double_settles() {
        // FM1: a crash/retry after the match ends must not settle twice. The second
        // submit hits the per-matchId fence (AlreadyResolved) and leaves the
        // recorded resolution untouched.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]);
        let settler = MockSettler::default();

        settle_match(&settler, &result, &replay, None).expect("first settles");
        let first = settler.resolution(id());
        assert!(matches!(
            settle_match(&settler, &result, &replay, None),
            Err(SettleError::AlreadyResolved)
        ));
        assert_eq!(settler.resolution(id()), first, "the retry changes nothing");
    }

    #[test]
    fn a_tie_settles_as_a_draw_not_a_win() {
        // FM3: a draw must take settleDraw, never settle(winner). Both seats share
        // placement 1, so a win-only mapping would wrongly record a Win.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 4, true), outcome(1, 1, 4, true)]);
        let settler = MockSettler::default();

        let chosen = settle_match(&settler, &result, &replay, None).expect("settles");
        assert_eq!(chosen, Settlement::Draw);
        assert_eq!(settler.resolution(id()), Some(Resolution::Draw { reputation: None, replay_digest: replay.digest() }));
    }

    #[test]
    fn cancel_records_a_cancel_and_fences_a_later_settle() {
        // FM3 cancel mapping + FM1 fence across kinds: a cancelled match is
        // Cancelled (no winner, no committed digest) and can never then be settled.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]);
        let settler = MockSettler::default();

        settler.cancel(id()).expect("cancels");
        assert_eq!(settler.resolution(id()), Some(Resolution::Cancelled));
        assert!(matches!(settler.cancel(id()), Err(SettleError::AlreadyResolved)), "retry cancel is a no-op");
        assert!(
            matches!(settle_match(&settler, &result, &replay, None), Err(SettleError::AlreadyResolved)),
            "a cancelled match can never be settled",
        );
        assert_eq!(settler.resolution(id()), Some(Resolution::Cancelled), "still cancelled");
    }

    #[test]
    fn a_non_pair_match_is_not_settleable() {
        // settle_match is the 1v1 seam: MatchSettlement's settle/settleDraw take exactly
        // two agents, so a 3-seat FFA (and a single/empty result) is refused here rather
        // than emitted as an unsettleable Win/Draw — settle_field_match is what settles a
        // 3+ field. Nothing is recorded.
        let replay = replay_for(roster(3));
        let result =
            result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 3, true), outcome(2, 3, 1, false)]);
        let settler = MockSettler::default();

        assert!(matches!(
            settle_match(&settler, &result, &replay, None),
            Err(SettleError::NotRankedPair)
        ));
        assert_eq!(settler.resolution(id()), None, "a non-pair match records nothing");
    }

    #[test]
    fn committed_digest_equals_the_core_replay_digest() {
        // FM2: the digest committed toward settlement is byte-identical to
        // arena-core's canonical ReplayRecord.digest() of the played match — and to
        // the hex in the published MatchResult — so the on-chain commitment verifies
        // against the recorded replay. Driven by a really-simulated match, not a
        // fixture.
        let config = MatchConfig {
            tick_hz: 30,
            max_ticks: 2,
            bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
            seats: 2,
        };
        let mut m = Match::new(id(), config, Rules::default(), roster(2), Vec::new(), 0);
        while m.phase() == MatchPhase::Live {
            m.step(&BTreeMap::new());
        }
        let result = m.result().expect("ended").clone();
        let replay = m.into_replay();
        let settler = MockSettler::default();

        settle_match(&settler, &result, &replay, None).expect("settles");
        let committed = match settler.resolution(id()).expect("resolved") {
            Resolution::Win { replay_digest, .. }
            | Resolution::Draw { replay_digest, .. }
            | Resolution::Field { replay_digest, .. } => replay_digest,
            Resolution::Cancelled => panic!("a played match is never a cancel"),
        };
        assert_eq!(committed, replay.digest(), "commits the exact core digest");
        assert_eq!(hex::encode(committed), result.replay_hash, "matches the published result hash");
    }

    fn ranked(rating_a: i32, rating_b: i32, k: i32) -> RankedContext {
        RankedContext { rating_a, rating_b, k }
    }

    #[test]
    fn a_ranked_win_carries_the_winners_exact_zero_sum_core_delta() {
        // FM2: with a ranked context the settle carries EXACTLY the core ranked_delta's
        // winner side; the loser's −d is the contract-applied negation the core
        // guarantees (b == −a). Seat 1 (NOT the first outcome) wins from an even match,
        // so this also pins the winner→delta mapping picks `.b`, not `.a`.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 2, 1, false), outcome(1, 1, 5, true)]);
        let k = 32;
        let core = ranked_delta(&result, DEFAULT_RATING, DEFAULT_RATING, k).unwrap();
        assert_eq!(core.a, -core.b, "the core delta is zero-sum");
        assert!(core.b > 0, "seat 1 is the winner, so its side (.b) is the positive gain");

        let settler = MockSettler::default();
        settle_match(&settler, &result, &replay, Some(ranked(DEFAULT_RATING, DEFAULT_RATING, k))).unwrap();
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Win { winner: "agent-1".into(), reputation: Some(core.b), replay_digest: replay.digest() }),
            "the settle carries the winning seat's exact core delta",
        );
    }

    #[test]
    fn a_favoured_win_carries_less_reputation_than_an_upset_win() {
        // The variable delta tracks the rating gap: the SAME win (seat 0) earns the
        // winner LESS when favoured than when the underdog. Pinned against the core,
        // and the carried value matches the favoured computation verbatim.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]); // seat 0 (agentA) wins
        let k = 32;
        let favoured = ranked_delta(&result, 1900, 1500, k).unwrap().a; // agentA favoured
        let upset = ranked_delta(&result, 1300, 1500, k).unwrap().a; // agentA underdog
        assert!(favoured > 0 && upset > 0, "a win always gains");
        assert!(favoured < upset, "the favourite earns less for the same win ({favoured} < {upset})");

        let settler = MockSettler::default();
        settle_match(&settler, &result, &replay, Some(ranked(1900, 1500, k))).unwrap();
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Win { winner: "agent-0".into(), reputation: Some(favoured), replay_digest: replay.digest() }),
        );
    }

    #[test]
    fn a_draw_carries_agent_a_signed_core_delta() {
        // FM4 (draw): a draw between UNEQUAL ratings moves the favoured agentA (seat 0)
        // DOWN — settle_draw carries agentA's negative core delta (the contract negates
        // it onto agentB). Even ratings ⇒ a zero draw delta.
        let replay = replay_for(roster(2));
        let tie = result_for(vec![outcome(0, 1, 4, true), outcome(1, 1, 4, true)]);
        let k = 32;
        let core = ranked_delta(&tie, 1800, 1500, k).unwrap();
        assert!(core.a < 0, "a draw moves the favoured agentA down");

        let favoured = MockSettler::default();
        settle_match(&favoured, &tie, &replay, Some(ranked(1800, 1500, k))).unwrap();
        assert_eq!(
            favoured.resolution(id()),
            Some(Resolution::Draw { reputation: Some(core.a), replay_digest: replay.digest() }),
        );

        let even = MockSettler::default();
        settle_match(&even, &tie, &replay, Some(ranked(DEFAULT_RATING, DEFAULT_RATING, k))).unwrap();
        assert_eq!(
            even.resolution(id()),
            Some(Resolution::Draw { reputation: Some(0), replay_digest: replay.digest() }),
            "an even draw carries a zero delta",
        );
    }

    #[test]
    fn the_reputation_delta_never_perturbs_the_committed_digest() {
        // FM3: the delta is settlement metadata, not a digest input — the committed
        // digest is identical with a fixed (None) or a variable (Some) reputation, and
        // both equal the canonical core ReplayRecord.digest().
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]);
        let dig = |s: &MockSettler| match s.resolution(id()).expect("resolved") {
            Resolution::Win { replay_digest, .. }
            | Resolution::Draw { replay_digest, .. }
            | Resolution::Field { replay_digest, .. } => replay_digest,
            Resolution::Cancelled => unreachable!("a settled win is never a cancel"),
        };
        let fixed = MockSettler::default();
        settle_match(&fixed, &result, &replay, None).unwrap();
        let variable = MockSettler::default();
        settle_match(&variable, &result, &replay, Some(ranked(1700, 1400, 32))).unwrap();
        assert_eq!(dig(&fixed), dig(&variable), "the reputation delta does not change the committed digest");
        assert_eq!(dig(&variable), replay.digest(), "still the canonical core digest");
    }

    fn field(ratings: Vec<i32>, k: i32) -> FieldContext {
        FieldContext { ratings, k }
    }

    fn three_seat_result() -> MatchResult {
        // Strict 1/2/3 finish so every pairwise game is decisive (no ties to flatten the
        // per-seat deltas).
        result_for(vec![outcome(0, 1, 9, true), outcome(1, 2, 5, true), outcome(2, 3, 1, false)])
    }

    #[test]
    fn a_field_settle_maps_each_canonical_seat_to_its_controller_and_core_delta() {
        // FM1: a 3-seat result emits the zero-sum per-seat vector in canonical
        // ascending-seat order, each delta paired to ITS seat's controller. Distinct
        // ratings + a strict placement make the three deltas distinct, so a seat→agent
        // swap or a dropped seat is observable; the carried deltas equal arena-core's
        // ranked_field_delta verbatim.
        let replay = replay_for(roster(3));
        let result = three_seat_result();
        let k = 32;
        let ratings = vec![1500, 1400, 1600];
        let core = ranked_field_delta(&result, &ratings, k).expect("3-seat field has deltas");
        assert_eq!(core.iter().map(|d| i64::from(d.delta)).sum::<i64>(), 0, "the field is zero-sum");
        let ds: Vec<i32> = core.iter().map(|d| d.delta).collect();
        assert!(ds[0] != ds[1] && ds[1] != ds[2] && ds[0] != ds[2], "distinct deltas make a swap observable: {ds:?}");

        let settler = MockSettler::default();
        let n = settle_field_match(&settler, &result, &replay, field(ratings, k)).expect("settles");
        assert_eq!(n, 3, "all three seats settled");

        let expected: Vec<FieldEntry> = core
            .iter()
            .map(|d| FieldEntry { agent: format!("agent-{}", d.seat), delta: d.delta })
            .collect();
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Field { entries: expected, replay_digest: replay.digest() }),
            "each canonical seat maps to its controller + its exact core delta",
        );
    }

    #[test]
    fn a_field_settle_keys_the_controller_by_seat_id_not_roster_position() {
        // FM1 (position-vs-seat-id): the controller is keyed by SEAT ID, not by the
        // delta's position in the roster Vec — a roster stored out of seat order would
        // make a positional lookup credit the wrong agent. The identity-roster test above
        // can't see this (there seat == position == name), so build a roster whose Vec
        // order (seats 2, 0, 1) is decoupled from seat id and pin that each seat's delta
        // still lands on the controller whose seat matches.
        let seats = vec![
            SeatInfo { seat: 2, team: 2, controller: "carol".into() },
            SeatInfo { seat: 0, team: 0, controller: "alice".into() },
            SeatInfo { seat: 1, team: 1, controller: "bob".into() },
        ];
        let replay = replay_for(seats);
        let result = three_seat_result(); // outcomes sorted ascending: seats 0, 1, 2
        let k = 32;
        let ratings = vec![1500, 1400, 1600];
        let core = ranked_field_delta(&result, &ratings, k).unwrap();

        let settler = MockSettler::default();
        settle_field_match(&settler, &result, &replay, field(ratings, k)).expect("settles");

        let expected = vec![
            FieldEntry { agent: "alice".into(), delta: core[0].delta }, // seat 0
            FieldEntry { agent: "bob".into(), delta: core[1].delta },   // seat 1
            FieldEntry { agent: "carol".into(), delta: core[2].delta }, // seat 2
        ];
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Field { entries: expected, replay_digest: replay.digest() }),
            "each seat's delta lands on the controller whose seat matches, not roster position",
        );
    }

    #[test]
    fn the_field_seam_refuses_a_pair_so_n2_keeps_the_single_delta_shape() {
        // FM2: a 2-seat result must never be emitted as a 2-vector. The field seam refuses
        // a pair (NotRankedField), so the ONLY n=2 settle path is settle_match's single
        // winner/agentA delta — the live 1v1 path is unchanged.
        let replay = replay_for(roster(2));
        let result = result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 1, false)]);

        let field_settler = MockSettler::default();
        assert!(matches!(
            settle_field_match(&field_settler, &result, &replay, field(vec![1500, 1500], 32)),
            Err(SettleError::NotRankedField),
        ));
        assert_eq!(field_settler.resolution(id()), None, "a pair records no field resolution");

        let pair_settler = MockSettler::default();
        let k = 32;
        let core = ranked_delta(&result, 1500, 1500, k).unwrap();
        settle_match(&pair_settler, &result, &replay, Some(ranked(1500, 1500, k))).unwrap();
        assert_eq!(
            pair_settler.resolution(id()),
            Some(Resolution::Win { winner: "agent-0".into(), reputation: Some(core.a), replay_digest: replay.digest() }),
            "n=2 settles as the single winner delta, never a 2-vector",
        );
    }

    #[test]
    fn the_field_deltas_never_perturb_the_committed_digest() {
        // FM3: the per-seat deltas are settlement metadata — the committed digest is
        // identical across two settles of the SAME result with DIFFERENT ratings (hence
        // different deltas), and equals the canonical core ReplayRecord.digest().
        let replay = replay_for(roster(3));
        let result = three_seat_result();
        let ent = |s: &MockSettler| match s.resolution(id()).expect("resolved") {
            Resolution::Field { entries, replay_digest } => (entries, replay_digest),
            _ => unreachable!("a field settle records a Field"),
        };
        let a = MockSettler::default();
        settle_field_match(&a, &result, &replay, field(vec![1500, 1500, 1500], 32)).unwrap();
        let b = MockSettler::default();
        settle_field_match(&b, &result, &replay, field(vec![1900, 1300, 1700], 64)).unwrap();
        let (ea, da) = ent(&a);
        let (eb, db) = ent(&b);
        assert_ne!(ea, eb, "the two rating sets really produce different deltas");
        assert_eq!(da, db, "the field deltas do not change the committed digest");
        assert_eq!(da, replay.digest(), "still the canonical core digest");
    }

    #[test]
    fn a_field_settle_is_fenced_against_a_replay() {
        // The shared per-matchId fence: a second field settle of the same matchId is
        // AlreadyResolved and the first recorded vector is untouched — the off-chain
        // mirror of the on-chain Status fence (one settlement per matchId).
        let replay = replay_for(roster(3));
        let result = three_seat_result();
        let settler = MockSettler::default();
        settle_field_match(&settler, &result, &replay, field(vec![1500, 1500, 1500], 32)).expect("first settles");
        let first = settler.resolution(id());
        assert!(matches!(
            settle_field_match(&settler, &result, &replay, field(vec![1900, 1300, 1700], 64)),
            Err(SettleError::AlreadyResolved),
        ));
        assert_eq!(settler.resolution(id()), first, "the replay changes nothing");
    }

    #[test]
    fn a_field_settle_refuses_a_misaligned_or_subfield_result() {
        // RatingsMismatch: ratings must align 1:1 with the seats, else the positional
        // seat→rating pairing is wrong — refused before any emit. NotRankedField: fewer
        // than 3 seats is the 1v1 path's job, so a single/empty result is refused here.
        let replay3 = replay_for(roster(3));
        let result3 = three_seat_result();
        let settler = MockSettler::default();
        assert!(matches!(
            settle_field_match(&settler, &result3, &replay3, field(vec![1500, 1500], 32)),
            Err(SettleError::RatingsMismatch),
        ));
        assert_eq!(settler.resolution(id()), None, "a misaligned vector records nothing");

        let single = result_for(vec![outcome(0, 1, 1, true)]);
        assert!(matches!(
            settle_field_match(&settler, &single, &replay_for(roster(1)), field(vec![1500], 32)),
            Err(SettleError::NotRankedField),
        ));
        let empty = result_for(vec![]);
        assert!(matches!(
            settle_field_match(&settler, &empty, &replay_for(roster(0)), field(vec![], 32)),
            Err(SettleError::NotRankedField),
        ));
        assert_eq!(settler.resolution(id()), None, "nothing recorded across the refusals");
    }

    fn join_key() -> SigningKey {
        let bytes =
            hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318").unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    fn other_join_key() -> SigningKey {
        let bytes =
            hex::decode("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    /// Sign join_digest exactly as the agent SDK does — `[r||s||v]` hex, low-S, raw
    /// recovery id — so these tests exercise admit_join over a real agent proof.
    fn sign_join_proof(sk: &SigningKey, agent_id: &str, nonce: &[u8]) -> String {
        let digest = join_digest(PROTOCOL_VERSION, agent_id, nonce);
        let (sig, recid): (Signature, RecoveryId) = sk.sign_prehash_recoverable(&digest).unwrap();
        let mut raw = sig.to_bytes().to_vec();
        raw.push(recid.to_byte());
        hex::encode(raw)
    }

    #[test]
    fn admit_join_admits_a_valid_ranked_signature() {
        // The agent signs join_digest over its seat's challenge nonce with its session
        // key; admit_join recovers the signer and accepts the identity it claims.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let nonce = nonce_for(id(), 0);
        let sig = sign_join_proof(&sk, &addr, nonce.as_bytes());
        assert_eq!(admit_join(&addr, nonce.as_bytes(), &sig), Ok(()));
    }

    #[test]
    fn admit_join_admits_an_empty_signature_as_unranked() {
        // The baseline's default: no signature is an unranked seat, admitted with no
        // proof — the loopback is not ranked-only, so unranked play is untouched.
        let nonce = nonce_for(id(), 0);
        assert_eq!(admit_join("0xanyone", nonce.as_bytes(), ""), Ok(()));
    }

    #[test]
    fn admit_join_rejects_a_forged_claim_to_another_identity() {
        // A seat signs with its OWN key but claims a different agent_id (here the other
        // key's address): the recovered signer is not the claim, so the seat is refused
        // — a forger cannot present an identity whose key it does not hold.
        let sk = join_key();
        let nonce = nonce_for(id(), 0);
        let claimed = address_from_verifying_key(other_join_key().verifying_key());
        let sig = sign_join_proof(&sk, &claimed, nonce.as_bytes());
        assert_eq!(admit_join(&claimed, nonce.as_bytes(), &sig), Err(JoinVerifyError::AddressMismatch));
    }

    #[test]
    fn admit_join_rejects_a_signature_replayed_under_a_different_nonce() {
        // A Join captured on one seat's connection (nonce A) is worthless on another
        // (nonce B): the nonce is folded into the digest, so the signature recovers a
        // different address against B and is refused — cross-connection replay closed.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let sig = sign_join_proof(&sk, &addr, nonce_for(id(), 0).as_bytes());
        let other_nonce = nonce_for(id(), 1);
        assert_eq!(
            admit_join(&addr, other_nonce.as_bytes(), &sig),
            Err(JoinVerifyError::AddressMismatch)
        );
    }

    #[test]
    fn admit_join_rejects_a_malformed_signature() {
        // A PRESENTED but non-hex / wrong-length signature is a bad encoding, not waved
        // through: a ranked claim with a junk proof is refused, never silently seated.
        let nonce = nonce_for(id(), 0);
        assert_eq!(
            admit_join("0xclaim", nonce.as_bytes(), "not-hex"),
            Err(JoinVerifyError::BadSignatureEncoding)
        );
        assert_eq!(
            admit_join("0xclaim", nonce.as_bytes(), "00"),
            Err(JoinVerifyError::BadSignatureEncoding)
        );
    }
}
