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

use arena_core::{settlement, Match, Rules, Settlement};
use arena_proto::{
    check_version, ActionIntent, AgentMsg, GatewayMsg, MatchConfig, MatchPhase, MatchResult,
    ReplayRecord, SeatId, SeatInfo, Vec2, POSITION_SCALE, PROTOCOL_VERSION,
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
    /// The match is not a 1v1 ranked pair. `MatchSettlement` settles exactly two
    /// agents (`agentA` vs `agentB`); a result with any other seat count has no
    /// on-chain `settle`/`settleDraw` form, so the driver refuses to emit an
    /// unsettleable resolution rather than commit one the contract can't accept.
    NotRankedPair,
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
trait Settle {
    fn settle(&self, match_id: Uuid, winner: &str, replay_digest: [u8; 32]) -> Result<(), SettleError>;
    fn settle_draw(&self, match_id: Uuid, replay_digest: [u8; 32]) -> Result<(), SettleError>;
    fn cancel(&self, match_id: Uuid) -> Result<(), SettleError>;
}

/// One recorded resolution, mirroring the terminal state the contract would hold.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolution {
    Win { winner: String, replay_digest: [u8; 32] },
    Draw { replay_digest: [u8; 32] },
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
    fn settle(&self, match_id: Uuid, winner: &str, replay_digest: [u8; 32]) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Win { winner: winner.to_string(), replay_digest })
    }

    fn settle_draw(&self, match_id: Uuid, replay_digest: [u8; 32]) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Draw { replay_digest })
    }

    fn cancel(&self, match_id: Uuid) -> Result<(), SettleError> {
        self.record(match_id, Resolution::Cancelled)
    }
}

/// Drive one finished match through the settler: classify it, then submit the
/// matching resolution carrying the canonical `replay.digest()`. The digest is
/// taken straight from [`ReplayRecord::digest`] (not re-derived from the result's
/// hex), so the on-chain commitment is byte-identical to the recorded replay. The
/// winner identity is the winning seat's roster `controller` — the harness
/// stand-in for the on-chain agent address. Returns the chosen [`Settlement`] for
/// the caller to report. A cancel is NOT produced here: a finished match always
/// has a result; `cancel` is the pre-play abort path.
fn settle_match(
    settler: &impl Settle,
    result: &MatchResult,
    replay: &ReplayRecord,
) -> Result<Settlement, SettleError> {
    // MatchSettlement is strictly 1v1; a non-pair match (FFA, a single seat, an
    // empty result) has no on-chain settle form, so refuse it here rather than emit
    // a Win/Draw the contract structurally cannot accept.
    if result.outcomes.len() != 2 {
        return Err(SettleError::NotRankedPair);
    }
    let digest = replay.digest();
    let outcome = settlement(result);
    match outcome {
        Settlement::Win { seat } => {
            let winner = replay
                .seats
                .iter()
                .find(|s| s.seat == seat)
                .map(|s| s.controller.as_str())
                .expect("the winning seat is in the roster");
            settler.settle(result.match_id, winner, digest)?;
        }
        Settlement::Draw => settler.settle_draw(result.match_id, digest)?,
    }
    Ok(outcome)
}

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
        let AgentMsg::Join { protocol_version, .. } = msg else {
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
        emit(
            &mut out,
            seat,
            &GatewayMsg::Welcome { protocol_version: PROTOCOL_VERSION, match_id: m.match_id(), seat },
        );
        emit(&mut out, seat, &GatewayMsg::Start { match_id: m.match_id(), config: m.config() });
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
        match settle_match(s, &result, &replay) {
            Ok(outcome) => {
                eprintln!("[settle-dev-mock] {match_id} settled as {outcome:?}: {:?}", s.resolution(match_id))
            }
            Err(e) => eprintln!("[settle-dev-mock] {match_id} settle failed: {e:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_proto::SeatOutcome;

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

        let chosen = settle_match(&settler, &result, &replay).expect("settles");
        assert_eq!(chosen, Settlement::Win { seat: 1 });
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Win { winner: "agent-1".into(), replay_digest: replay.digest() }),
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

        settle_match(&settler, &result, &replay).expect("first settles");
        let first = settler.resolution(id());
        assert!(matches!(
            settle_match(&settler, &result, &replay),
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

        let chosen = settle_match(&settler, &result, &replay).expect("settles");
        assert_eq!(chosen, Settlement::Draw);
        assert_eq!(settler.resolution(id()), Some(Resolution::Draw { replay_digest: replay.digest() }));
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
            matches!(settle_match(&settler, &result, &replay), Err(SettleError::AlreadyResolved)),
            "a cancelled match can never be settled",
        );
        assert_eq!(settler.resolution(id()), Some(Resolution::Cancelled), "still cancelled");
    }

    #[test]
    fn a_non_pair_match_is_not_settleable() {
        // MatchSettlement settles exactly two agents, so a 3-seat FFA (and likewise
        // a single/empty result) is refused rather than emitted as an unsettleable
        // Win/Draw — and nothing is recorded.
        let replay = replay_for(roster(3));
        let result =
            result_for(vec![outcome(0, 1, 5, true), outcome(1, 2, 3, true), outcome(2, 3, 1, false)]);
        let settler = MockSettler::default();

        assert!(matches!(
            settle_match(&settler, &result, &replay),
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

        settle_match(&settler, &result, &replay).expect("settles");
        let committed = match settler.resolution(id()).expect("resolved") {
            Resolution::Win { replay_digest, .. } | Resolution::Draw { replay_digest } => replay_digest,
            Resolution::Cancelled => panic!("a played match is never a cancel"),
        };
        assert_eq!(committed, replay.digest(), "commits the exact core digest");
        assert_eq!(hex::encode(committed), result.replay_hash, "matches the published result hash");
    }
}
