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
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use arena_core::{
    arena_map, named_arena, ranked_delta, ranked_field_delta, settlement, AimMode, Match, Rules,
    SeatDelta, Settlement, DEFAULT_RATING,
};
use arena_match::{
    JoinOutcome, JoinRequest, LadderSnapshot, MatchParams, Matchmaker, SignatureVerifier, SnapshotError,
};
use arena_proto::{
    check_version, verify_join_signature, ActionIntent, AgentMsg, GatewayMsg, JoinVerifyError,
    MatchConfig, MatchMode, MatchPhase, MatchResult, ReplayRecord, SeatId, SeatInfo, Vec2,
    POSITION_SCALE, PROTOCOL_VERSION,
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
    /// When set, form the match through the `arena-match` [`Matchmaker`] under this
    /// [`MatchMode`] instead of seating the roster directly — so the Human/Agent/Mixed
    /// gating and authenticated ranked admission are exercised end to end. `None` (no
    /// `--mode`) is the pre-this-flag direct-seating path, byte-identical.
    mode: Option<MatchMode>,
    /// Seats that join as humans in `--mode mixed` (comma-separated). Only Mixed needs
    /// the hint: the arena-01 `Join` carries no controller kind, so a token-less join
    /// is otherwise a casual agent, and a Mixed match requires at least one of each. In
    /// `human` mode every seat is a human (a signed join is the agent intruder Mixed
    /// would admit), and in `agent` mode every seat is a ranked agent, so the list is
    /// consulted for Mixed only. Empty by default.
    human_seats: Vec<SeatId>,
    /// Persist and restore the matchmaker's ranked rating ladder across runs. When set,
    /// the ladder is SEEDED from this file at startup (a missing or empty file starts
    /// fresh — byte-identical to today; a present-but-corrupt one aborts the run loudly
    /// rather than silently resetting standings) and the POST-settle ladder is written
    /// back atomically after the match. Only a `--mode` run moves a ladder, so the flag
    /// is consulted on that path; `None` keeps the in-memory-only behaviour.
    ladder_file: Option<PathBuf>,
    /// The builtin arena whose static geometry — vision blockers + world pickups — the
    /// match plays under, resolved through [`arena_map`]. Set by `--map <key>`; the
    /// default `""` is the empty arena (no occlusion, no items), byte-identical to the
    /// pre-this-flag harness. Applies to BOTH the direct and `--mode` paths, so a match
    /// reaches the named arena's cover + pickups (and an agent SDK receives them in
    /// [`GatewayMsg::Start`]) however the roster is formed.
    arena: &'static str,
    /// Perception-memory window in ticks (`Rules::perception_memory_ticks`): how long a
    /// seat remembers a lost entity's last-known position (surfaced as a `VisibleEntity`
    /// with `in_line_of_sight == false`). Set by `--perception-memory`; the default `0`
    /// disables memory, byte-identical to the pre-this-flag harness. Applies to BOTH the
    /// direct and `--mode` paths through [`rules_from`]: the matchmaker carries it on
    /// [`MatchParams::rules`], so a matchmade/ranked match forms under the same window a
    /// hand-seated one does.
    perception_memory: u16,
    /// Forward field-of-view cone as an octant spread (`Rules::fov_octant_spread`,
    /// `0..=4`): a seat perceives an in-range enemy only when its bearing is within this
    /// many octants of the seat's facing. Set by `--fov`; the default `4` is the full
    /// circle — omnidirectional, byte-identical to the pre-flag harness (and the replay
    /// digest). Applies to BOTH the direct and `--mode` paths through [`rules_from`]: the
    /// matchmaker carries it on [`MatchParams::rules`], so a matchmade/ranked match forms
    /// under the same cone a hand-seated one does.
    fov: u8,
    /// Fire-beam aim resolution (`Rules::aim_mode`): `octant` snaps the beam to the nearest of
    /// eight 45° octants, `fine` resolves it on the 64-way (5.625°) table so a sub-octant lead
    /// lands a shot the octant snap would miss. Set by `--aim-mode`; the default `octant` is
    /// byte-identical to the pre-flag harness (and the replay digest). Applies to BOTH the
    /// direct and `--mode` paths through [`rules_from`]: the matchmaker carries it on
    /// [`MatchParams::rules`], so a matchmade/ranked match forms under the same aim resolution
    /// a hand-seated one does.
    aim_mode: AimMode,
    /// Allow allied damage (`Rules::friendly_fire`): when set, a fire (beam, projectile, or
    /// melee swing) that crosses a same-team body damages it instead of passing through — the
    /// hit lands but never scores a kill for the shooter. A presence flag (`--friendly-fire`,
    /// no value, like `--settle-dev-mock`); the default `false` spares allies, byte-identical
    /// to the pre-flag harness (and the replay digest). Applies to BOTH the direct and `--mode`
    /// paths through [`rules_from`]: the matchmaker carries it on [`MatchParams::rules`], so a
    /// matchmade/ranked match forms under the same allied-damage rule a hand-seated one does.
    /// The effect surfaces only with teamed rosters — today's harness seats a free-for-all
    /// (every seat its own team), so the rule is dark until a teamed deployment configures it —
    /// but `friendly_fire` is a real `Rules` determinant folded into the digest.
    friendly_fire: bool,
}

/// Parse a `--mode` value into a [`MatchMode`]; the harness exposes the three
/// `arena-match` modes by their lowercase names.
fn parse_mode(value: &str) -> MatchMode {
    match value {
        "human" => MatchMode::Human,
        "agent" => MatchMode::Agent,
        "mixed" => MatchMode::Mixed,
        other => panic!("--mode is one of human|agent|mixed, got {other:?}"),
    }
}

/// Resolve a `--map` value to a builtin arena's canonical `'static` key, aborting on
/// an unknown one (mirroring [`parse_mode`]). The reject is loud and deliberate: an
/// unrecognised key would otherwise degrade through [`arena_map`] to the empty arena,
/// silently playing no-cover instead of the map the operator asked for.
fn parse_arena(value: &str) -> &'static str {
    named_arena(value).unwrap_or_else(|| panic!("--map names an unknown arena: {value:?}"))
}

/// Parse a `--fov` value to a forward-cone octant spread, rejecting anything outside the
/// sim's `0..=4` domain loudly (mirroring [`parse_mode`]/[`parse_arena`]). A spread `>4`
/// would saturate to the full circle in the sim — silently playing omnidirectional
/// perception instead of the cone the operator asked for — so the harness refuses it
/// rather than clamp.
fn parse_fov(value: &str) -> u8 {
    let spread: u8 = value.parse().expect("--fov is an octant spread (0..=4)");
    assert!(spread <= 4, "--fov is an octant spread in 0..=4 (4 = full circle), got {spread}");
    spread
}

/// Parse an `--aim-mode` value to a fire-beam resolution, rejecting an unknown name loudly
/// (mirroring [`parse_mode`]/[`parse_arena`]). `aim_mode` is a hit-resolution determinant —
/// it changes which shots connect — so a typo must abort, never silently default to `octant`
/// and mis-resolve combat.
fn parse_aim_mode(value: &str) -> AimMode {
    match value {
        "octant" => AimMode::Octant,
        "fine" => AimMode::Fine,
        other => panic!("--aim-mode is one of octant|fine, got {other:?}"),
    }
}

fn parse_args() -> Args {
    parse_args_from(std::env::args().skip(1))
}

/// The argv parse loop over the post-`argv[0]` tokens, taken as an iterator so it is
/// unit-testable with a synthetic stream (`parse_args` feeds it the real env args). A
/// presence flag (`--settle-dev-mock`, `--friendly-fire`) flips its bool WITHOUT consuming
/// the next token; a value flag pulls exactly one `it.next()`.
fn parse_args_from(args: impl Iterator<Item = String>) -> Args {
    let mut match_id = DEFAULT_MATCH_ID.to_string();
    let mut seed: u64 = 0;
    let mut seats: u8 = 2;
    let mut max_ticks: u64 = 3600;
    let mut settle_dev_mock = false;
    let mut mode: Option<MatchMode> = None;
    let mut human_seats: Vec<SeatId> = Vec::new();
    let mut ladder_file: Option<PathBuf> = None;
    let mut arena: &'static str = "";
    let mut perception_memory: u16 = 0;
    let mut fov: u8 = 4;
    let mut aim_mode = AimMode::Octant;
    let mut friendly_fire = false;
    let mut it = args;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--match-id" => match_id = it.next().expect("--match-id needs a value"),
            "--seed" => seed = it.next().expect("--seed needs a value").parse().expect("seed is a u64"),
            "--seats" => seats = it.next().expect("--seats needs a value").parse().expect("seats is a u8"),
            "--max-ticks" => {
                max_ticks = it.next().expect("--max-ticks needs a value").parse().expect("max-ticks is a u64")
            }
            "--settle-dev-mock" => settle_dev_mock = true,
            "--mode" => mode = Some(parse_mode(&it.next().expect("--mode needs a value"))),
            "--human-seats" => {
                let v = it.next().expect("--human-seats needs a value");
                human_seats = v
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse().expect("each --human-seats entry is a u8"))
                    .collect();
            }
            "--ladder-file" => ladder_file = Some(it.next().expect("--ladder-file needs a value").into()),
            "--map" => arena = parse_arena(&it.next().expect("--map needs a value")),
            "--perception-memory" => {
                perception_memory = it
                    .next()
                    .expect("--perception-memory needs a value")
                    .parse()
                    .expect("perception-memory is a u16 (ticks)")
            }
            "--fov" => fov = parse_fov(&it.next().expect("--fov needs a value")),
            "--aim-mode" => aim_mode = parse_aim_mode(&it.next().expect("--aim-mode needs a value")),
            "--friendly-fire" => friendly_fire = true,
            other => panic!("unknown argument: {other}"),
        }
    }
    Args {
        match_id: Uuid::parse_str(&match_id).expect("--match-id is a valid UUID"),
        seed,
        seats,
        max_ticks,
        settle_dev_mock,
        mode,
        human_seats,
        ladder_file,
        arena,
        perception_memory,
        fov,
        aim_mode,
        friendly_fire,
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
///
/// Returns the RECOVERED identity the seat proved possession of: `Some(agent_id)` for
/// an admitted ranked seat — `verify_join_signature` succeeds only when the recovered
/// signer's address equals the claim, so the claim IS the verified identity — and
/// `None` for an unranked seat (no key, nothing to recover). The caller seats a ranked
/// seat under this address so settlement credits the real identity, not the roster label.
fn admit_join(agent_id: &str, nonce: &[u8], signature_hex: &str) -> Result<Option<String>, JoinVerifyError> {
    if signature_hex.is_empty() {
        return Ok(None);
    }
    verify_join_signature(PROTOCOL_VERSION, agent_id, nonce, signature_hex)?;
    Ok(Some(agent_id.to_owned()))
}

/// Overlay the handshake-recovered ranked identities onto the seated roster so a match
/// settles to the address each ranked seat PROVED it controls, not the pre-built
/// `agent-{i}` stand-in. Each `(seat, address)` came from [`admit_join`] returning
/// `Some` for a verified signature; an unranked seat has no entry and keeps its roster
/// label. Only the `controller` LABEL changes — `seat` and `team` stay index-driven, so
/// seat order, team assignment, and reproducibility are untouched; the only effect is
/// that [`settle_match`]/[`settle_field_match`], which credit `SeatInfo.controller`,
/// resolve the verified identity.
fn seat_recovered_identities(seats: &mut [SeatInfo], recovered: &[(SeatId, String)]) {
    for (seat, address) in recovered {
        if let Some(s) = seats.iter_mut().find(|s| s.seat == *seat) {
            s.controller = address.clone();
        }
    }
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

/// The K-factor the loopback uses for ranked settlement — both the `--settle-dev-mock`
/// multi-seat field settle and the matchmaker rating ladder ([`settle_ranked_ladder`]).
/// The loopback moves only in-memory state (a [`MockSettler`] / the local ladder, never a
/// chain), so this sets the magnitude of the demonstrated deltas, not any production
/// economic knob — the live driver passes the owner-set K. 32 matches the value the
/// ranked unit tests use; sharing one constant keeps the dev-mock and ladder deltas from
/// silently diverging.
const DEV_MOCK_K: i32 = 32;

/// Pump a formed, live match to its end: each tick, observe every seat, read each
/// seat's Act (the server-authoritative `ingest` forfeits a rejected action), step,
/// then emit the terminal result to every seat. The single gameplay loop both the
/// direct and matchmade paths share — every rule still lives in `arena-core`; this is
/// transport only. Returns the canonical [`MatchResult`].
fn pump_to_end(
    m: &mut Match,
    n: u8,
    lines: &mut impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
) -> MatchResult {
    while m.phase() == MatchPhase::Live {
        for seat in 0..n {
            emit(out, seat, &GatewayMsg::Observe(m.observe(seat)));
        }
        out.flush().expect("flush observations");

        let mut intents: BTreeMap<SeatId, ActionIntent> = BTreeMap::new();
        for _ in 0..n {
            let line = next_line(lines);
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
        emit(out, seat, &GatewayMsg::End(result.clone()));
    }
    out.flush().expect("flush results");
    result
}

/// Settle a finished match through the optional mock settler. Overlays any
/// handshake-recovered ranked identities onto the roster first — `recovered` is the
/// direct path's verified `(seat, address)` pairs, and EMPTY for a matchmade match,
/// whose formed roster already carries each verified address as its seat controller.
/// A 1v1 (or degenerate) result settles through [`settle_match`] (deferring to the
/// contract's fixed reputation delta); a 3+ field through [`settle_field_match`].
fn settle_finished(
    settler: &Option<MockSettler>,
    result: &MatchResult,
    m: Match,
    recovered: &[(SeatId, String)],
) {
    let Some(s) = settler else { return };
    let match_id = m.match_id();
    let mut replay = m.into_replay();
    seat_recovered_identities(&mut replay.seats, recovered);
    // Loopback agents are unrated, so each reads as DEFAULT_RATING — exactly what the
    // live ladder returns for an unseen agent. A 1v1 then defers to the contract's
    // fixed delta (None, byte-identical to pre-ladder); a 3+ field, which has no
    // fixed-delta form, settles its zero-sum placement vector. The live driver passes
    // real ladder ratings and the owner-set K in place of these.
    let seats = result.outcomes.len();
    let report = if seats > 2 {
        let field = FieldContext { ratings: vec![DEFAULT_RATING; seats], k: DEV_MOCK_K };
        settle_field_match(s, result, &replay, field).map(|n| format!("field of {n} seats"))
    } else {
        settle_match(s, result, &replay, None).map(|o| format!("{o:?}"))
    };
    match report {
        Ok(desc) => eprintln!("[settle-dev-mock] {match_id} settled as {desc}: {:?}", s.resolution(match_id)),
        Err(e) => eprintln!("[settle-dev-mock] {match_id} settle failed: {e:?}"),
    }
}

/// Settle a matchmade match's terminal `result` into the matchmaker's rating ladder.
/// [`Matchmaker::build`] registered every Agent-mode match in its pending-ranked
/// registry at formation, so the result must move the ladder AND consume that
/// registration — else it leaks until the eviction cap reaps it. The arm is chosen by
/// outcome seat count, mirroring [`settle_match`] vs [`settle_field_match`]: a 1v1 via
/// [`Matchmaker::apply_ranked_result`], a 3+/team field via
/// [`Matchmaker::apply_ranked_field_result`] (pushing a 3+ result through the 1v1 arm
/// is a silent no-op that leaks the registration — FM1). A casual / human / Mixed match
/// was never registered, so the apply is a clean no-op (`None`); likewise a replayed
/// result whose registration the first apply already consumed, so a retry or duplicate
/// End never moves the ladder twice (FM2). The K is the shared loopback `DEV_MOCK_K`
/// (FM3). Each settled seat's post-match rating + signed delta is emitted as ONE
/// structured `[ladder]` JSON line — `{"match_id","seats":[{"seat","rating","delta"}]}`,
/// the rating resolved through the roster's seat→`controller` map — so the Python SDK
/// can parse a machine-readable frame (never the old human-formatted delta line) to
/// surface an A2A author's ladder standing. A no-op (casual / human / Mixed / replay)
/// emits nothing and is not an error; the emission has no wire effect on the match.
fn settle_ranked_ladder(mm: &Matchmaker<SignatureVerifier>, result: &MatchResult, seats: &[SeatInfo]) {
    // Fold both settle arms into one (seat, delta) list: the 1v1 delta lands `.a` on the
    // first outcome seat and `.b` on the second (canonical order); the field carries its
    // own seats. A no-op arm (never registered / already settled) bails before any emit.
    let moved: Vec<(SeatId, i32)> = if result.outcomes.len() == 2 {
        match mm.apply_ranked_result(result, DEV_MOCK_K) {
            Some(d) => vec![(result.outcomes[0].seat, d.a), (result.outcomes[1].seat, d.b)],
            None => return,
        }
    } else {
        match mm.apply_ranked_field_result(result, DEV_MOCK_K) {
            Some(deltas) => deltas.iter().map(|d| (d.seat, d.delta)).collect(),
            None => return,
        }
    };
    // Pair each settled seat to its controller's POST-settle ladder rating (the apply
    // above already wrote it), keyed by seat for the SDK. An out-of-roster seat can't
    // occur — the apply validated every outcome seat against the roster — so the lookup
    // is total; DEFAULT_RATING is an inert fallback that never fires here.
    let entries: Vec<serde_json::Value> = moved
        .into_iter()
        .map(|(seat, delta)| {
            let rating = seats
                .iter()
                .find(|s| s.seat == seat)
                .and_then(|s| mm.rating(&s.controller))
                .unwrap_or(DEFAULT_RATING);
            serde_json::json!({ "seat": seat, "rating": rating, "delta": delta })
        })
        .collect();
    let line = serde_json::json!({ "match_id": result.match_id.to_string(), "seats": entries });
    eprintln!("[ladder] {line}");
}

/// The match parameters a `--mode` run forms under — the direct path's config (30 Hz,
/// the same square bounds, free-for-all teams, the empty arena) mirrored so a matchmade
/// match plays like a hand-seated one. `seats_per_match == n` makes the match form
/// exactly when the last seat joins, consuming the whole queue, so its roster is in seat
/// (submission) order and the transport's envelope seat stays the match seat.
fn matchmaker_params(n: u8, max_ticks: u64, arena: &'static str) -> MatchParams {
    MatchParams { seats_per_match: n, max_ticks, arena, ..MatchParams::default() }
}

/// Why a `--ladder-file` could not be trusted. A MISSING or empty file is NOT an error
/// (it is the legal "start fresh" path — [`read_ladder_file`] returns `Ok(None)`); these
/// are the cases where a file EXISTS with content the harness refuses to misread, so it
/// aborts loudly rather than silently resetting accumulated standings to `DEFAULT_RATING`.
#[derive(Debug)]
enum LadderFileError {
    Read(io::Error),
    Parse(serde_json::Error),
    Restore(SnapshotError),
}

impl std::fmt::Display for LadderFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LadderFileError::Read(e) => write!(f, "unreadable ladder file: {e}"),
            LadderFileError::Parse(e) => write!(f, "malformed ladder file: {e}"),
            LadderFileError::Restore(e) => write!(f, "{e}"),
        }
    }
}

/// Read a persisted ladder snapshot from `path`. A MISSING or empty (all-whitespace)
/// file is the ONLY legal "start fresh" signal and returns `Ok(None)` — a fresh ladder
/// is byte-identical to a run with no `--ladder-file`. A present, non-empty file that is
/// not valid [`LadderSnapshot`] JSON is a loud `Err`, never a silent fresh start (which
/// would erase real standings); the schema-version check lives in
/// [`Matchmaker::from_snapshot`], reported here as [`LadderFileError::Restore`].
fn read_ladder_file(path: &Path) -> Result<Option<LadderSnapshot>, LadderFileError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(LadderFileError::Read(e)),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(&bytes).map(Some).map_err(LadderFileError::Parse)
}

/// The sibling temp path a ladder write stages to before its atomic rename: same
/// directory as `path` (so the rename stays on one filesystem and so is atomic) and
/// process-unique (so a concurrent run, or a leftover temp from a crashed one, can't
/// collide on it).
fn ladder_tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map_or_else(|| OsString::from("ladder"), |n| n.to_os_string());
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
}

/// Persist `snapshot` to `path` durably: serialize as JSON to a sibling temp file, then
/// atomic-rename it over `path`. A crash mid-write leaves the TEMP (not `path`) partial,
/// so the previous good snapshot is never truncated in place — an interrupted persist
/// loses the new write, never corrupts the old one.
fn write_ladder(path: &Path, snapshot: &LadderSnapshot) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(snapshot).expect("a LadderSnapshot always serializes");
    let tmp = ladder_tmp_path(path);
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

/// Refuse to start a run whose `--ladder-file` can't be trusted: a corrupt or
/// stale-schema ladder is reported and the process exits non-zero, NEVER silently reset
/// to `DEFAULT_RATING` (which would erase real standings under a transient read glitch
/// or a forgotten schema bump).
fn abort_ladder(path: &Path, err: &LadderFileError) -> ! {
    eprintln!("[ladder] refusing to start from {}: {err}", path.display());
    std::process::exit(1);
}

/// Construct the matchmaker for a `--mode` run, seeding its rating ladder from
/// `--ladder-file` when one is given and present so standings accumulate across runs. No
/// flag, or a missing / empty file, starts a fresh `DEFAULT_RATING` ladder — byte-identical
/// to the pre-persistence harness. A present but corrupt or wrong-schema file aborts the
/// run via [`abort_ladder`] rather than silently resetting standings.
fn build_matchmaker(args: &Args, n: u8) -> Matchmaker<SignatureVerifier> {
    // Carry the same Rules the direct path forms under (rules_from), so a matchmade match
    // plays under exactly the tuning a hand-seated one does — this is what threads
    // --perception-memory through the --mode/ranked path the matchmaker owns.
    let params = MatchParams { rules: rules_from(args), ..matchmaker_params(n, args.max_ticks, args.arena) };
    let Some(path) = &args.ladder_file else {
        return Matchmaker::new(SignatureVerifier, params);
    };
    match read_ladder_file(path) {
        Ok(None) => Matchmaker::new(SignatureVerifier, params),
        Ok(Some(snapshot)) => Matchmaker::from_snapshot(SignatureVerifier, params, snapshot)
            .unwrap_or_else(|e| abort_ladder(path, &LadderFileError::Restore(e))),
        Err(e) => abort_ladder(path, &e),
    }
}

/// Map a seat's Join (its claimed `agent_id` + `signature_hex`) to a matchmaker
/// [`JoinRequest`] for `mode`. The arena-01 Join carries no controller kind, so it is
/// inferred from the mode and whether a signature is present:
/// - `human`: a token-less seat is a human; a SIGNED join is an agent presenting a
///   ranked claim into a human-only match — built as a ranked agent so the matchmaker
///   refuses it `WrongKindForMode`.
/// - `agent`: every seat is an agent — ranked when signed, casual when token-less (and
///   a casual seat is refused `Unauthenticated`, since Agent mode is ranked-only).
/// - `mixed`: a seat listed in `human_seats` is a human; any other is an agent — ranked
///   when signed, casual cross-play when token-less.
fn join_request_for(
    mode: MatchMode,
    seat: SeatId,
    human_seats: &[SeatId],
    agent_id: &str,
    signature_hex: &str,
) -> JoinRequest {
    let is_human = match mode {
        MatchMode::Human => signature_hex.is_empty(),
        MatchMode::Agent => false,
        MatchMode::Mixed => human_seats.contains(&seat),
    };
    if is_human {
        JoinRequest::human(agent_id)
    } else if signature_hex.is_empty() {
        JoinRequest::casual_agent(agent_id)
    } else {
        JoinRequest::ranked_agent(agent_id, signature_hex)
    }
}

/// Emit a Reject for `seat` and terminate: a handshake refusal (version, wrong kind for
/// the mode, or an unauthenticated ranked claim) voids the opened match as a cancel
/// (refund, no result committed — exactly `MatchSettlement.cancelMatch`), then exits,
/// mirroring the direct path's reject arms. The match never forms.
fn reject_and_exit(
    out: &mut impl Write,
    settler: &Option<MockSettler>,
    match_id: Uuid,
    seat: SeatId,
    reason: String,
    cause: &str,
) -> ! {
    emit(out, seat, &GatewayMsg::Reject { reason });
    out.flush().expect("flush reject");
    if let Some(s) = settler {
        eprintln!("[settle-dev-mock] {match_id} cancel ({cause}): {:?}", s.cancel(match_id));
    }
    std::process::exit(1);
}

/// Form the match through the `arena-match` [`Matchmaker`] under `mode`, instead of
/// seating a fixed roster. Issues a per-seat challenge, then COLLECTS every Join before
/// replying: the matchmaker forms the match only on the last seat, so — unlike the
/// direct path — no Welcome can be sent until every seat is in (a driver must send all
/// Joins before blocking on its Welcome). Each seat is then routed through
/// [`Matchmaker::join`] in seat order; because a match that consumes the whole queue is
/// rostered in submission (FIFO) order, the formed seat i is transport seat i, so the
/// multiplexed envelope seat stays the match seat.
///
/// The nonce handed to the matchmaker is exactly the challenge issued to that seat — what
/// the agent signed over — NOT the formed match's id, which the matchmaker mints only
/// after admission. A version mismatch, a wrong-kind-for-mode join, or an unauthenticated
/// ranked claim emits a Reject (+ cancel settle) and exits; the match never forms.
/// Returns the [`Matchmaker`] alongside the formed [`Match`] (whose roster already
/// credits each verified ranked identity) after emitting Welcome+Start to every seat —
/// the matchmaker outlives the pump so the terminal result can settle into its ladder
/// (it registered an Agent match in `pending_ranked` at formation).
fn handshake_matchmade(
    args: &Args,
    mode: MatchMode,
    n: u8,
    settler: &Option<MockSettler>,
    lines: &mut impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
) -> (Matchmaker<SignatureVerifier>, Match) {
    let mm = build_matchmaker(args, n);

    for seat in 0..n {
        emit(out, seat, &GatewayMsg::Challenge { nonce: nonce_for(args.match_id, seat) });
    }
    out.flush().expect("flush challenges");

    // Collect every Join first — the matchmaker forms on the LAST seat, so no Welcome can
    // be issued until all are in. Version is the protocol gate checked here; kind +
    // identity are the matchmaker's, checked as each seat is routed below.
    let mut joins: BTreeMap<SeatId, (String, String)> = BTreeMap::new();
    for _ in 0..n {
        let line = next_line(lines);
        let (seat, msg) = read_agent(&line);
        let AgentMsg::Join { protocol_version, agent_id, signature_hex } = msg else {
            panic!("expected a join during the handshake");
        };
        if check_version(protocol_version).is_err() {
            reject_and_exit(
                out,
                settler,
                args.match_id,
                seat,
                format!("protocol version mismatch: ours={PROTOCOL_VERSION}, theirs={protocol_version}"),
                "handshake version mismatch",
            );
        }
        joins.insert(seat, (agent_id, signature_hex));
    }
    assert_eq!(joins.len(), usize::from(n), "expected exactly one join per seat during the handshake");

    // Route each seat through the matchmaker IN SEAT ORDER so the formed roster's seats
    // line up with the transport. The match consumes the whole queue, so it forms on the
    // last seat; earlier seats queue.
    let mut formed: Option<Match> = None;
    for seat in 0..n {
        let (agent_id, signature_hex) = &joins[&seat];
        let req = join_request_for(mode, seat, &args.human_seats, agent_id, signature_hex);
        match mm.join(mode, nonce_for(args.match_id, seat).as_bytes(), req) {
            Ok(JoinOutcome::Queued) => {}
            Ok(outcome) => formed = outcome.into_formed(),
            Err(e) => {
                reject_and_exit(out, settler, args.match_id, seat, format!("join rejected: {e}"), "join rejected")
            }
        }
    }
    let m = formed.expect("the last seat forms the match (a Mixed match needs at least one --human-seats)");

    for seat in 0..n {
        emit(
            out,
            seat,
            &GatewayMsg::Welcome { protocol_version: PROTOCOL_VERSION, match_id: m.match_id(), seat },
        );
        emit(
            out,
            seat,
            &GatewayMsg::Start {
                match_id: m.match_id(),
                config: m.config(),
                blockers: m.blockers().to_vec(),
                pickup_points: m.pickup_spawns().iter().map(|p| p.position).collect(),
            },
        );
    }
    out.flush().expect("flush welcome+start");
    (mm, m)
}

/// The combat [`Rules`] both seating paths form under, derived from the harness flags so
/// a matchmade (`--mode`) match and a hand-seated direct match play under the SAME tuning.
/// The matchmaker carries it via [`MatchParams::rules`] ([`build_matchmaker`]); the direct
/// path passes it straight to [`Match::new_with_pickups`] ([`build_direct_match`]). The
/// perception-memory window (`--perception-memory`), the FOV cone (`--fov`), the aim
/// resolution (`--aim-mode`), and allied damage (`--friendly-fire`) are dialable; every
/// other field stays at [`Rules::default`], and each knob defaults to its `Rules::default`
/// value, so a no-flag run is byte-identical to the pre-knob harness.
fn rules_from(args: &Args) -> Rules {
    Rules {
        perception_memory_ticks: args.perception_memory,
        fov_octant_spread: args.fov,
        aim_mode: args.aim_mode,
        friendly_fire: args.friendly_fire,
        ..Rules::default()
    }
}

/// Build the direct-path (no `--mode`) match: a fixed `agent-{i}` free-for-all roster
/// under the configured arena geometry. The matchmade path forms its own match through
/// the [`Matchmaker`] ([`build_matchmaker`]); this is the hand-seated twin.
///
/// Both paths resolve geometry through [`arena_map`], so `--map` reaches the direct
/// path too. The default empty arena (`args.arena == ""`) yields empty blockers +
/// pickups, which is exactly what [`Match::new`] produces (it is `new_with_pickups`
/// with no pickups) — so a no-flag run is byte-identical to the pre-map harness.
fn build_direct_match(args: &Args, n: u8) -> Match {
    let roster: Vec<SeatInfo> = (0..n)
        .map(|i| SeatInfo { seat: i, team: u16::from(i), controller: format!("agent-{i}") })
        .collect();
    let config = MatchConfig {
        tick_hz: 30,
        max_ticks: args.max_ticks,
        bounds: Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE },
        seats: n,
    };
    let map = arena_map(args.arena);
    let rules = rules_from(args);
    Match::new_with_pickups(
        args.match_id,
        config,
        rules,
        roster,
        map.blockers,
        map.pickups,
        args.seed,
    )
}

fn main() {
    let args = parse_args();
    let n = args.seats;

    // The off-chain settlement seam: a finished match (or a pre-play abort) maps to
    // a MatchSettlement resolution through this settler. Mock-only and opt-in here;
    // the live Base submitter is operator-gated.
    let settler = args.settle_dev_mock.then(MockSettler::default);

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // --mode routes formation through the arena-match Matchmaker (mode-gated,
    // authenticated). Its formed roster already credits each verified ranked identity,
    // so settlement overlays no recovered ids.
    if let Some(mode) = args.mode {
        let (mm, mut m) = handshake_matchmade(&args, mode, n, &settler, &mut lines, &mut out);
        let result = pump_to_end(&mut m, n, &mut lines, &mut out);
        // Settle the ladder while the roster is still alive (it maps seat→controller for
        // the rating readout); `settle_finished` then consumes the match.
        settle_ranked_ladder(&mm, &result, m.seats());
        settle_finished(&settler, &result, m, &[]);
        if let Some(path) = &args.ladder_file {
            // Persist the POST-settle ladder (the settle above moved it) so the next run
            // resumes these standings; atomic temp-then-rename keeps a crash mid-write
            // from corrupting the prior good snapshot.
            write_ladder(path, &mm.snapshot()).unwrap_or_else(|e| {
                eprintln!("[ladder] failed to persist to {}: {e}", path.display());
                std::process::exit(1);
            });
        }
        return;
    }

    // Direct-seating path (no --mode): seat a fixed agent-{i} roster, byte-identical to
    // the pre-matchmaker harness.
    let mut m = build_direct_match(&args, n);

    for seat in 0..n {
        emit(&mut out, seat, &GatewayMsg::Challenge { nonce: nonce_for(m.match_id(), seat) });
    }
    out.flush().expect("flush challenges");

    // Reply to each Join the moment it arrives — a seat's Welcome+Start must not
    // wait on another seat's Join, or a client that connects sequentially and
    // blocks on its own Welcome would deadlock against a harness waiting for the
    // next Join.
    let mut recovered: Vec<(SeatId, String)> = Vec::new();
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
        match admit_join(&agent_id, nonce.as_bytes(), &signature_hex) {
            Err(e) => {
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
            // A verified ranked seat is seated under the address it proved (the recovered
            // signer verify_join_signature accepted as the claim); unranked keeps its label.
            Ok(Some(address)) => recovered.push((seat, address)),
            Ok(None) => {}
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

    let result = pump_to_end(&mut m, n, &mut lines, &mut out);
    settle_finished(&settler, &result, m, &recovered);
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

    fn third_join_key() -> SigningKey {
        let bytes =
            hex::decode("7777777777777777777777777777777777777777777777777777777777777777").unwrap();
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
        // Admitted AND the recovered identity is the claimed address — the seat is later
        // seated under it, not the roster label.
        assert_eq!(admit_join(&addr, nonce.as_bytes(), &sig), Ok(Some(addr.clone())));
    }

    #[test]
    fn admit_join_admits_an_empty_signature_as_unranked() {
        // The baseline's default: no signature is an unranked seat, admitted with no
        // proof — the loopback is not ranked-only, so unranked play is untouched.
        let nonce = nonce_for(id(), 0);
        // Admitted with NO recovered identity — an unranked seat keeps its roster label.
        assert_eq!(admit_join("0xanyone", nonce.as_bytes(), ""), Ok(None));
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

    #[test]
    fn seat_recovered_identities_seats_ranked_addresses_and_keeps_unranked_labels() {
        // FM1 + FM4: a Mixed roster — seat 0 is verified ranked, seat 1 is unranked. The
        // ranked seat adopts the address it proved during the handshake; the unranked seat
        // (no entry in the recovered set) keeps its agent-1 roster label.
        let mut seats = roster(2);
        let addr = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23".to_string();
        seat_recovered_identities(&mut seats, &[(0, addr.clone())]);
        assert_eq!(seats[0].controller, addr, "the ranked seat adopts the recovered address");
        assert_eq!(seats[1].controller, "agent-1", "the unranked seat keeps its roster label");
    }

    #[test]
    fn seat_recovered_identities_changes_only_the_label_not_seat_or_team() {
        // FM3: the identity overlay touches the controller LABEL only — seat and team stay
        // index-driven, so seat order, team assignment, and the match's reproducibility are
        // untouched even when every seat is ranked.
        let mut seats = roster(2);
        seat_recovered_identities(&mut seats, &[(0, "0xaaa".into()), (1, "0xbbb".into())]);
        assert_eq!(
            (seats[0].seat, seats[0].team, seats[1].seat, seats[1].team),
            (0, 0, 1, 1),
            "seat and team are untouched by the identity overlay",
        );
    }

    #[test]
    fn seat_recovered_identities_with_no_ranked_seats_keeps_every_roster_label() {
        // FM1: an all-unranked match (empty recovered set) is byte-identical to before —
        // every seat keeps agent-{i}, so unranked play is never perturbed by the overlay.
        let mut seats = roster(3);
        let before = seats.clone();
        seat_recovered_identities(&mut seats, &[]);
        assert_eq!(seats, before, "no recovered identities ⇒ the roster is unchanged");
    }

    #[test]
    fn a_ranked_win_settles_the_recovered_address_not_the_roster_label() {
        // FM2 end to end: seat 1 wins a 1v1 after being seated under the address it proved
        // ranked, so settle_match must credit THAT address, not agent-1. Were the recovered
        // identity not overlaid onto the seat settle_match reads, the winner would settle as
        // the agent-1 roster label — so crediting the real address is the discriminating proof.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let mut replay = replay_for(roster(2));
        seat_recovered_identities(&mut replay.seats, &[(1, addr.clone())]);
        let result = result_for(vec![outcome(0, 2, 1, false), outcome(1, 1, 5, true)]);
        let settler = MockSettler::default();

        settle_match(&settler, &result, &replay, None).expect("settles");
        assert_eq!(
            settler.resolution(id()),
            Some(Resolution::Win { winner: addr, reputation: None, replay_digest: replay.digest() }),
            "the verified ranked identity is credited, not the agent-1 roster label",
        );
    }

    // ===== arena-match Matchmaker entry (--mode) =====

    use arena_match::JoinError;
    use arena_proto::ControllerKind;

    fn mode_args(seats: u8, mode: MatchMode, human_seats: Vec<SeatId>) -> Args {
        Args {
            match_id: id(),
            seed: 0,
            seats,
            max_ticks: 4,
            settle_dev_mock: false,
            mode: Some(mode),
            human_seats,
            ladder_file: None,
            arena: "",
            perception_memory: 0,
            fov: 4,
            aim_mode: AimMode::Octant,
            friendly_fire: false,
        }
    }

    /// A transport envelope carrying one seat's Join, exactly as the matchmade
    /// handshake reads it off the pipe.
    fn join_line(seat: SeatId, agent_id: &str, signature_hex: &str) -> String {
        let frame = serde_json::to_value(AgentMsg::Join {
            protocol_version: PROTOCOL_VERSION,
            agent_id: agent_id.to_string(),
            signature_hex: signature_hex.to_string(),
        })
        .unwrap();
        serde_json::json!({ "seat": seat, "frame": frame }).to_string()
    }

    fn mm2() -> Matchmaker<SignatureVerifier> {
        Matchmaker::new(SignatureVerifier, matchmaker_params(2, 4, ""))
    }

    #[test]
    fn parse_mode_maps_each_name() {
        assert_eq!(parse_mode("human"), MatchMode::Human);
        assert_eq!(parse_mode("agent"), MatchMode::Agent);
        assert_eq!(parse_mode("mixed"), MatchMode::Mixed);
    }

    #[test]
    fn matchmaker_params_mirror_the_direct_seating_config() {
        // A matchmade match must play like a hand-seated one: same tick rate, bounds,
        // free-for-all teams, empty arena — and seats_per_match == n so it forms exactly
        // when the last seat joins (consuming the whole queue, rostered in seat order).
        let p = matchmaker_params(3, 1234, "");
        assert_eq!(p.seats_per_match, 3);
        assert_eq!(p.max_ticks, 1234);
        assert_eq!(p.tick_hz, 30);
        assert_eq!(p.team_size, 1, "free-for-all, like the direct roster's team == seat");
        assert_eq!(p.bounds, Vec2 { x: 50 * POSITION_SCALE, y: 50 * POSITION_SCALE });
        assert_eq!(p.arena, "", "the empty arena, like the direct path's no-pickups match");
        assert_eq!(
            matchmaker_params(3, 1234, "reference").arena,
            "reference",
            "a named arena threads through to the matchmaker, so --map reaches the matchmade path"
        );
    }

    // ===== --map arena selection =====

    fn direct_args(seats: u8, arena: &'static str, perception_memory: u16) -> Args {
        Args {
            match_id: id(),
            seed: 0,
            seats,
            max_ticks: 4,
            settle_dev_mock: false,
            mode: None,
            human_seats: vec![],
            ladder_file: None,
            arena,
            perception_memory,
            fov: 4,
            aim_mode: AimMode::Octant,
            friendly_fire: false,
        }
    }

    /// The first [`GatewayMsg::Start`] decoded out of the harness's emitted stdout
    /// envelopes — what an agent actually receives, proving the geometry crosses the wire
    /// and isn't merely held on the in-memory `Match`.
    fn first_start(stdout: &str) -> GatewayMsg {
        stdout
            .lines()
            .find_map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).ok()?;
                let msg: GatewayMsg = serde_json::from_value(v.get("frame")?.clone()).ok()?;
                matches!(msg, GatewayMsg::Start { .. }).then_some(msg)
            })
            .expect("a Start frame is emitted")
    }

    #[test]
    fn parse_arena_resolves_a_known_key() {
        assert_eq!(parse_arena("reference"), "reference");
    }

    #[test]
    #[should_panic(expected = "unknown arena")]
    fn parse_arena_rejects_an_unknown_key() {
        // FM2: a typo must abort loudly, NOT degrade through arena_map to the empty arena
        // (which would silently play no-cover instead of the map the operator asked for).
        parse_arena("does-not-exist");
    }

    #[test]
    fn parse_fov_accepts_the_whole_domain() {
        // 0 (facing octant alone) through 4 (full circle) are the sim's valid spreads.
        assert_eq!((0..=4).map(|s| parse_fov(&s.to_string())).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    #[should_panic(expected = "0..=4")]
    fn parse_fov_rejects_an_out_of_range_spread() {
        // FM1: a spread >4 must abort loudly, NOT saturate to the full circle in the sim
        // (which would silently play omnidirectional instead of the cone asked for).
        parse_fov("7");
    }

    #[test]
    fn parse_aim_mode_maps_each_name() {
        assert_eq!(parse_aim_mode("octant"), AimMode::Octant);
        assert_eq!(parse_aim_mode("fine"), AimMode::Fine);
    }

    #[test]
    #[should_panic(expected = "octant|fine")]
    fn parse_aim_mode_rejects_an_unknown_name() {
        // FM2: an unrecognized aim name must abort loudly, NOT default to Octant — aim_mode
        // is a hit-resolution determinant, so a silent default would mis-resolve combat and
        // commit a replay that disagrees with what the operator asked for.
        parse_aim_mode("coarse");
    }

    #[test]
    fn direct_match_default_arena_is_empty() {
        // FM1: no --map (arena == "") yields empty geometry — exactly Match::new's
        // no-blockers/no-pickups match, so the no-flag run stays byte-identical.
        let m = build_direct_match(&direct_args(2, "", 0), 2);
        assert!(m.blockers().is_empty(), "the default arena has no cover");
        assert!(m.pickup_spawns().is_empty(), "the default arena has no items");
        assert_eq!(m.rules().perception_memory_ticks, 0, "no --perception-memory: memory off");
    }

    #[test]
    fn direct_match_named_arena_loads_cover_and_pickups() {
        // FM3/FM4: --map reference reaches the DIRECT path — the formed match carries the
        // reference arena's occluder + two health pickups.
        let m = build_direct_match(&direct_args(2, "reference", 0), 2);
        assert!(!m.blockers().is_empty(), "the reference arena has a vision occluder");
        assert_eq!(m.pickup_spawns().len(), 2, "the reference arena has two health pickups");
    }

    #[test]
    fn direct_match_threads_the_perception_memory_window_into_rules() {
        // FM1/FM3: --perception-memory reaches the sim's Rules (the seat memory.rs reads),
        // so the knob actually turns the core feature on; default 0 stays off. The memory
        // BEHAVIOR (a lost enemy surfaces in_line_of_sight=false) is arena-core's own test;
        // here we pin the wiring deterministically via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().perception_memory_ticks,
            0,
            "the default window is off (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&direct_args(2, "reference", 45), 2).rules().perception_memory_ticks,
            45,
            "--perception-memory 45 threads into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_the_perception_window_into_a_matchmade_match() {
        // The frontier this slice closes: --perception-memory now reaches the --mode path.
        // build_matchmaker carries the window via MatchParams.rules, so a match the
        // MATCHMAKER forms runs under it — not the Rules::default() the matchmaker hardcoded
        // before. Proven by forming a 2-seat match through the built matchmaker (Human seats
        // are token-less, so no signing) and reading rules() back, the accessor the
        // direct-path twin above uses — so matchmade and hand-seated agree on the window.
        let mm = build_matchmaker(&direct_args(2, "", 45), 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().perception_memory_ticks,
            45,
            "the matchmaker forms under the --perception-memory window (matchmade == hand-seated)"
        );

        // No flag still forms memory-off — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(off.rules().perception_memory_ticks, 0, "no --perception-memory: the matchmaker forms memory-off");
    }

    #[test]
    fn direct_match_threads_the_fov_cone_into_rules() {
        // FM2: --fov reaches the sim's Rules (the in_fov perception cone); default 4 = full
        // circle so a no-flag run is byte-identical (and its replay digest unchanged). The
        // cone BEHAVIOR (an out-of-cone enemy is not perceived) is arena-core's own test;
        // here we pin the wiring deterministically via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().fov_octant_spread,
            4,
            "no --fov is the full circle (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { fov: 1, ..direct_args(2, "reference", 0) }, 2).rules().fov_octant_spread,
            1,
            "--fov 1 threads the narrow cone into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_the_fov_cone_into_a_matchmade_match() {
        // FM3 (path skew): --fov must reach the --mode path too, not just the direct one.
        // build_matchmaker carries the cone via MatchParams.rules, so a MATCHMADE match
        // forms under it — proven by forming a 2-seat Human match through the built
        // matchmaker and reading rules() back, the accessor the direct twin uses (so
        // matchmade and hand-seated agree on the cone).
        let mm = build_matchmaker(&Args { fov: 1, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().fov_octant_spread,
            1,
            "the matchmaker forms under the --fov cone (matchmade == hand-seated)"
        );

        // No flag still forms full-circle — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(off.rules().fov_octant_spread, 4, "no --fov: the matchmaker forms omnidirectional");
    }

    #[test]
    fn direct_match_threads_the_aim_mode_into_rules() {
        // FM1 (default drift): no --aim-mode is Octant — the 8-way snap, byte-identical to the
        // pre-flag harness (and its replay digest). The aim BEHAVIOR (a sub-octant lead lands
        // under Fine) is arena-core's own test; here we pin the wiring via the rules() accessor.
        assert_eq!(
            build_direct_match(&direct_args(2, "", 0), 2).rules().aim_mode,
            AimMode::Octant,
            "no --aim-mode is Octant (byte-identical to the pre-flag harness)"
        );
        assert_eq!(
            build_direct_match(&Args { aim_mode: AimMode::Fine, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .aim_mode,
            AimMode::Fine,
            "--aim-mode fine threads Fine into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_the_aim_mode_into_a_matchmade_match() {
        // FM3 (path skew): --aim-mode must reach the --mode path too, not just the direct one.
        // build_matchmaker carries the mode via MatchParams.rules, so a MATCHMADE match forms
        // under it — proven by forming a 2-seat Human match and reading rules() back (the same
        // accessor the direct twin uses, so matchmade and hand-seated agree on the aim).
        let mm = build_matchmaker(&Args { aim_mode: AimMode::Fine, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert_eq!(
            formed.rules().aim_mode,
            AimMode::Fine,
            "the matchmaker forms under --aim-mode fine (matchmade == hand-seated)"
        );

        // No flag still forms Octant — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert_eq!(off.rules().aim_mode, AimMode::Octant, "no --aim-mode: the matchmaker forms Octant");
    }

    #[test]
    fn direct_match_threads_friendly_fire_into_rules() {
        // FM1 (default drift): no --friendly-fire spares allies (Rules::friendly_fire == false),
        // byte-identical to the pre-flag harness (and its replay digest). The allied-damage
        // BEHAVIOR is arena-core's own test; here we pin the wiring via the rules() accessor.
        assert!(
            !build_direct_match(&direct_args(2, "", 0), 2).rules().friendly_fire,
            "no --friendly-fire spares allies (byte-identical to the pre-flag harness)"
        );
        assert!(
            build_direct_match(&Args { friendly_fire: true, ..direct_args(2, "reference", 0) }, 2)
                .rules()
                .friendly_fire,
            "--friendly-fire threads allied damage into Rules (alongside an arena, independently)"
        );
    }

    #[test]
    fn build_matchmaker_threads_friendly_fire_into_a_matchmade_match() {
        // FM3 (path skew): --friendly-fire must reach the --mode path too, not just the direct
        // one. build_matchmaker carries it via MatchParams.rules, so a MATCHMADE match forms
        // under it — proven by forming a 2-seat Human match and reading rules() back (the same
        // accessor the direct twin uses, so matchmade and hand-seated agree on allied damage).
        let mm = build_matchmaker(&Args { friendly_fire: true, ..direct_args(2, "", 0) }, 2);
        mm.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let formed = mm
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("the second Human seat forms the match");
        assert!(
            formed.rules().friendly_fire,
            "the matchmaker forms under --friendly-fire (matchmade == hand-seated)"
        );

        // No flag still spares allies — byte-identical to the pre-knob matchmaker.
        let off = build_matchmaker(&direct_args(2, "", 0), 2);
        off.join(MatchMode::Human, b"", JoinRequest::human("a")).unwrap();
        let off = off
            .join(MatchMode::Human, b"", JoinRequest::human("b"))
            .unwrap()
            .into_formed()
            .expect("forms");
        assert!(!off.rules().friendly_fire, "no --friendly-fire: the matchmaker spares allies");
    }

    #[test]
    fn friendly_fire_flag_consumes_no_following_token() {
        // FM2 (flag-with-value confusion): --friendly-fire is a PRESENCE flag — it flips the bool
        // WITHOUT swallowing the next token. If the arm wrongly called it.next(), it would eat the
        // following --seats and that token's "3" would abort as an unknown argument. Pin that a
        // --friendly-fire IMMEDIATELY before --seats 3 parses BOTH: the flag on, seats == 3.
        let parsed = parse_args_from(["--friendly-fire", "--seats", "3"].into_iter().map(String::from));
        assert!(parsed.friendly_fire, "--friendly-fire flips the flag");
        assert_eq!(parsed.seats, 3, "--friendly-fire consumed no token, so --seats 3 still parsed");

        // Absent, the parse loop defaults it off (the parse-level twin of the threading FM1).
        let none = parse_args_from(["--seats", "2"].into_iter().map(String::from));
        assert!(!none.friendly_fire, "no --friendly-fire defaults off");
    }

    #[test]
    fn matchmade_named_arena_reaches_the_start_frame() {
        // FM4: --map reference reaches the MATCHMADE path too, and the geometry crosses the
        // wire — the Start frame an agent receives carries the cover + pickups, not just the
        // in-memory Match. Two agents sign their seat's challenge and form an Agent match.
        let sk0 = join_key();
        let sk1 = other_join_key();
        let addr0 = address_from_verifying_key(sk0.verifying_key());
        let addr1 = address_from_verifying_key(sk1.verifying_key());
        let sig0 = sign_join_proof(&sk0, &addr0, nonce_for(id(), 0).as_bytes());
        let sig1 = sign_join_proof(&sk1, &addr1, nonce_for(id(), 1).as_bytes());
        let input = format!("{}\n{}\n", join_line(0, &addr0, &sig0), join_line(1, &addr1, &sig1));
        let mut lines = io::BufReader::new(io::Cursor::new(input)).lines();
        let mut out: Vec<u8> = Vec::new();
        let mut args = mode_args(2, MatchMode::Agent, vec![]);
        args.arena = "reference";

        let (_mm, m) = handshake_matchmade(&args, MatchMode::Agent, 2, &None, &mut lines, &mut out);
        assert!(!m.blockers().is_empty(), "the formed match plays under the reference arena's cover");
        assert_eq!(m.pickup_spawns().len(), 2, "and its two health pickups");

        let stdout = String::from_utf8(out).unwrap();
        let GatewayMsg::Start { blockers, pickup_points, .. } = first_start(&stdout) else {
            unreachable!("first_start returns a Start variant")
        };
        assert!(!blockers.is_empty(), "the agent's Start frame carries the cover");
        assert_eq!(pickup_points.len(), 2, "the agent's Start frame carries the two pickup points");
    }

    #[test]
    fn join_request_for_infers_controller_kind_from_mode_and_signature() {
        // Human mode: a token-less seat is a human; a SIGNED join is an agent intruder.
        assert_eq!(join_request_for(MatchMode::Human, 0, &[], "h", "").kind, ControllerKind::Human);
        let intruder = join_request_for(MatchMode::Human, 0, &[], "a", "deadbeef");
        assert_eq!(intruder.kind, ControllerKind::Agent, "a signed join in human mode is the agent intruder");
        // Agent mode: every seat is an agent — ranked iff a token is present.
        let casual = join_request_for(MatchMode::Agent, 0, &[], "a", "");
        assert_eq!((casual.kind, casual.token), (ControllerKind::Agent, None));
        let ranked = join_request_for(MatchMode::Agent, 0, &[], "a", "ff");
        assert_eq!(ranked.kind, ControllerKind::Agent);
        assert_eq!(ranked.token.as_deref(), Some("ff"));
        // Mixed: a declared human seat is human; any other is an agent (casual if token-less).
        assert_eq!(join_request_for(MatchMode::Mixed, 0, &[0], "h", "").kind, ControllerKind::Human);
        let casual_mixed = join_request_for(MatchMode::Mixed, 1, &[0], "a", "");
        assert_eq!((casual_mixed.kind, casual_mixed.token), (ControllerKind::Agent, None));
    }

    #[test]
    fn agent_mode_forms_an_authenticated_match_and_settles_to_the_verified_addresses() {
        // FM1: the harness pumps the matchmaker's FORMED match — its own minted id, its
        // verified-address roster IN SEAT ORDER — not a self-built Match on the challenge
        // salt with agent-{i} labels. Two agents sign their seat's challenge;
        // handshake_matchmade routes both through the Matchmaker and returns the formed
        // match, which then pumps + settles.
        let sk0 = join_key();
        let sk1 = other_join_key();
        let addr0 = address_from_verifying_key(sk0.verifying_key());
        let addr1 = address_from_verifying_key(sk1.verifying_key());
        let sig0 = sign_join_proof(&sk0, &addr0, nonce_for(id(), 0).as_bytes());
        let sig1 = sign_join_proof(&sk1, &addr1, nonce_for(id(), 1).as_bytes());
        let input = format!("{}\n{}\n", join_line(0, &addr0, &sig0), join_line(1, &addr1, &sig1));
        let mut lines = io::BufReader::new(io::Cursor::new(input)).lines();
        let mut out: Vec<u8> = Vec::new();
        let args = mode_args(2, MatchMode::Agent, vec![]);

        let (_mm, mut m) = handshake_matchmade(&args, MatchMode::Agent, 2, &None, &mut lines, &mut out);
        let minted = m.match_id();
        assert_ne!(minted, id(), "the formed match carries the matchmaker's minted id, not the challenge salt");
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains(&minted.to_string()), "welcome/start announce the formed match id");

        while m.phase() == MatchPhase::Live {
            m.step(&BTreeMap::new());
        }
        let result = m.result().expect("ended").clone();
        let replay = m.into_replay();
        let controllers: Vec<&str> = replay.seats.iter().map(|s| s.controller.as_str()).collect();
        assert_eq!(
            controllers,
            vec![addr0.as_str(), addr1.as_str()],
            "the formed roster credits the verified addresses in seat order (seat i = the seat-i signer)",
        );

        let settler = MockSettler::default();
        settle_match(&settler, &result, &replay, None).expect("a 2-seat match settles");
        match settler.resolution(minted).expect("resolved") {
            Resolution::Win { winner, .. } => {
                assert!(winner == addr0 || winner == addr1, "the winner is a verified address: {winner}")
            }
            Resolution::Draw { .. } => {}
            other => panic!("a played 1v1 settles Win or Draw, got {other:?}"),
        }
    }

    #[test]
    fn the_ranked_join_is_verified_against_the_seat_challenge_not_the_formed_id() {
        // FM2: the matchmaker checks the signature against the per-connection CHALLENGE
        // nonce passed to join() — the harness must hand it exactly the nonce it issued,
        // never the id the matchmaker mints after admission. A signature over seat 0's
        // nonce, presented under seat 1's, recovers a different address and is refused.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let sig_over_seat0 = sign_join_proof(&sk, &addr, nonce_for(id(), 0).as_bytes());

        let mm = mm2();
        let matched = join_request_for(MatchMode::Agent, 0, &[], &addr, &sig_over_seat0);
        assert!(
            matches!(mm.join(MatchMode::Agent, nonce_for(id(), 0).as_bytes(), matched), Ok(JoinOutcome::Queued)),
            "the signature over this seat's challenge is admitted",
        );
        let mismatched = join_request_for(MatchMode::Agent, 1, &[], &addr, &sig_over_seat0);
        assert!(
            matches!(
                mm.join(MatchMode::Agent, nonce_for(id(), 1).as_bytes(), mismatched),
                Err(JoinError::Unauthenticated { .. })
            ),
            "the same signature under a different challenge nonce is refused",
        );
    }

    #[test]
    fn human_mode_refuses_a_signed_agent_join() {
        // FM3: a signed join in human-only mode is an agent presenting a ranked claim —
        // refused WrongKindForMode, never seated, so a human match stays human.
        let sk = join_key();
        let addr = address_from_verifying_key(sk.verifying_key());
        let sig = sign_join_proof(&sk, &addr, nonce_for(id(), 0).as_bytes());
        let req = join_request_for(MatchMode::Human, 0, &[], &addr, &sig);
        assert!(matches!(
            mm2().join(MatchMode::Human, nonce_for(id(), 0).as_bytes(), req),
            Err(JoinError::WrongKindForMode { kind: ControllerKind::Agent, mode: MatchMode::Human }),
        ));
    }

    #[test]
    fn agent_mode_refuses_a_token_less_join() {
        // FM3: Agent mode is ranked-only — a token-less agent is unauthenticated and
        // never reaches a ranked seat.
        let req = join_request_for(MatchMode::Agent, 0, &[], "0xnobody", "");
        assert!(matches!(
            mm2().join(MatchMode::Agent, nonce_for(id(), 0).as_bytes(), req),
            Err(JoinError::Unauthenticated { .. }),
        ));
    }

    #[test]
    fn mixed_mode_admits_a_token_less_agent_as_casual_and_forms_with_a_human() {
        // FM3: a token-less agent in Mixed is admitted as a casual cross-play seat (not
        // rejected), and a human + that casual agent forms a Mixed match.
        let mm = mm2();
        let human = join_request_for(MatchMode::Mixed, 0, &[0], "human-0", "");
        assert!(
            matches!(mm.join(MatchMode::Mixed, nonce_for(id(), 0).as_bytes(), human), Ok(JoinOutcome::Queued)),
            "the human seat queues",
        );
        let casual = join_request_for(MatchMode::Mixed, 1, &[0], "agent-1", "");
        let formed = mm.join(MatchMode::Mixed, nonce_for(id(), 1).as_bytes(), casual).expect("admitted casual");
        assert!(formed.into_formed().is_some(), "a human + a casual agent forms a Mixed cross-play match");
    }

    /// Form a ranked Agent match of `keys.len()` seats through a fresh matchmaker — each
    /// seat signs its challenge so the verifier admits it. Returns the matchmaker (its
    /// ladder + the pending_ranked registration live) and the formed match, so a test can
    /// settle the match's terminal result back into the ladder.
    fn formed_ranked_match(keys: &[SigningKey]) -> (Matchmaker<SignatureVerifier>, Match) {
        let mm = Matchmaker::new(SignatureVerifier, matchmaker_params(keys.len() as u8, 4, ""));
        let mut formed = None;
        for (seat, sk) in keys.iter().enumerate() {
            let seat = seat as SeatId;
            let addr = address_from_verifying_key(sk.verifying_key());
            let nonce = nonce_for(id(), seat);
            let req = join_request_for(MatchMode::Agent, seat, &[], &addr, &sign_join_proof(sk, &addr, nonce.as_bytes()));
            if let Some(m) = mm.join(MatchMode::Agent, nonce.as_bytes(), req).expect("admitted").into_formed() {
                formed = Some(m);
            }
        }
        (mm, formed.expect("the last seat forms the match"))
    }

    /// A synthetic terminal result for `match_id` with the given placement `outcomes`.
    /// The id is the matchmaker's MINTED id (not the fixed test id), so the ladder settle
    /// resolves the registration `build()` keyed under it.
    fn ranked_result(match_id: Uuid, outcomes: Vec<SeatOutcome>) -> MatchResult {
        MatchResult { protocol_version: PROTOCOL_VERSION, match_id, final_tick: 1, outcomes, replay_hash: "00".repeat(32) }
    }

    #[test]
    fn settle_ranked_ladder_moves_a_1v1_winner_up_and_loser_down_by_the_configured_k() {
        // A formed Agent 1v1 settled into the ladder: seat 0 wins, so its agent gains and
        // seat 1's loses by the EXACT zero-sum ranked_delta the core computes at DEV_MOCK_K
        // from the two DEFAULT_RATING pre-ratings, and the pending_ranked entry is consumed.
        // A bare-literal K would move a different magnitude; an unmoved ladder or a still
        // -pending entry would mean the result never settled.
        let keys = [join_key(), other_join_key()];
        let addr0 = address_from_verifying_key(keys[0].verifying_key());
        let addr1 = address_from_verifying_key(keys[1].verifying_key());
        let (mm, m) = formed_ranked_match(&keys);
        assert_eq!(mm.unsettled_ranked(), 1, "the formed Agent match registered one pending result");

        let result = ranked_result(m.match_id(), vec![outcome(0, 1, 10, true), outcome(1, 2, 0, false)]);
        let expected = ranked_delta(&result, DEFAULT_RATING, DEFAULT_RATING, DEV_MOCK_K).unwrap();
        settle_ranked_ladder(&mm, &result, m.seats());

        assert!(expected.a > 0 && expected.a == -expected.b, "a decisive win is a positive, zero-sum move");
        assert_eq!(mm.rating(&addr0), Some(DEFAULT_RATING + expected.a), "winner moves by +delta at the configured K");
        assert_eq!(mm.rating(&addr1), Some(DEFAULT_RATING + expected.b), "loser moves by -delta");
        assert_eq!(mm.unsettled_ranked(), 0, "the registration is consumed");
    }

    #[test]
    fn settle_ranked_ladder_is_idempotent_on_a_replayed_result() {
        // FM2: applying the same terminal result twice (a retry / duplicate End) must not
        // move ratings twice — the registration is consumed on the first settle, so the
        // second is a no-op.
        let keys = [join_key(), other_join_key()];
        let addr0 = address_from_verifying_key(keys[0].verifying_key());
        let (mm, m) = formed_ranked_match(&keys);
        let result = ranked_result(m.match_id(), vec![outcome(0, 1, 10, true), outcome(1, 2, 0, false)]);

        settle_ranked_ladder(&mm, &result, m.seats());
        let after_first = mm.rating(&addr0);
        assert_eq!(mm.unsettled_ranked(), 0);
        settle_ranked_ladder(&mm, &result, m.seats());
        assert_eq!(mm.rating(&addr0), after_first, "a replayed result does not move the ladder again");
        assert_eq!(mm.unsettled_ranked(), 0, "still consumed, not re-registered");
    }

    #[test]
    fn settle_ranked_ladder_settles_a_3_seat_field_through_the_field_arm() {
        // FM1: a 3-seat result MUST settle through apply_ranked_field_result, moving every
        // seat by its placement delta and consuming the registration. Routed (wrongly)
        // through the 1v1 arm it is a silent no-op — ladder unmoved, registration leaked —
        // so a moved 3-seat ladder proves the arm is chosen by outcome count.
        let keys = [join_key(), other_join_key(), third_join_key()];
        let addrs: Vec<String> = keys.iter().map(|k| address_from_verifying_key(k.verifying_key())).collect();
        let (mm, m) = formed_ranked_match(&keys);
        assert_eq!(mm.unsettled_ranked(), 1);

        let result = ranked_result(
            m.match_id(),
            vec![outcome(0, 1, 9, true), outcome(1, 2, 5, true), outcome(2, 3, 1, false)],
        );
        let expected = ranked_field_delta(&result, &[DEFAULT_RATING; 3], DEV_MOCK_K).unwrap();
        settle_ranked_ladder(&mm, &result, m.seats());

        assert!(expected[0].delta > 0 && expected[2].delta < 0, "1st gains, last loses");
        assert_eq!(expected.iter().map(|d| i64::from(d.delta)).sum::<i64>(), 0, "the field is zero-sum");
        for (i, d) in expected.iter().enumerate() {
            assert_eq!(mm.rating(&addrs[i]), Some(DEFAULT_RATING + d.delta), "seat {i} moves by its field delta");
        }
        assert_eq!(mm.unsettled_ranked(), 0, "the field registration is consumed");
    }

    /// A process-unique temp path per test (tests run in parallel threads), tagged so two
    /// ladder tests never share a file.
    fn temp_ladder_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("arena-ladder-test-{}-{tag}.json", std::process::id()))
    }

    #[test]
    fn a_ladder_file_persists_ratings_across_two_sequential_runs() {
        // The headline: run 1 moves the ladder and writes the file; run 2 SEEDS from it, so
        // the moved standings survive a fresh process instead of resetting to DEFAULT_RATING.
        let path = temp_ladder_path("persist");
        let _ = std::fs::remove_file(&path);

        // Run 1: form + settle a ranked 1v1, then persist the moved ladder.
        let keys = [join_key(), other_join_key()];
        let addr0 = address_from_verifying_key(keys[0].verifying_key());
        let addr1 = address_from_verifying_key(keys[1].verifying_key());
        let (mm1, m) = formed_ranked_match(&keys);
        let result = ranked_result(m.match_id(), vec![outcome(0, 1, 10, true), outcome(1, 2, 0, false)]);
        settle_ranked_ladder(&mm1, &result, m.seats());
        let moved0 = mm1.rating(&addr0).expect("seat 0 has a moved rating");
        let moved1 = mm1.rating(&addr1).expect("seat 1 has a moved rating");
        assert_ne!(moved0, DEFAULT_RATING, "the winner actually moved off the default");
        write_ladder(&path, &mm1.snapshot()).expect("persist the moved ladder");

        // Run 2: a fresh matchmaker built from the same --ladder-file resumes those ratings.
        let mut args = mode_args(2, MatchMode::Agent, vec![]);
        args.ladder_file = Some(path.clone());
        let mm2 = build_matchmaker(&args, 2);
        assert_eq!(mm2.rating(&addr0), Some(moved0), "run 2 resumes the winner's standing exactly");
        assert_eq!(mm2.rating(&addr1), Some(moved1), "run 2 resumes the loser's standing exactly");
        assert_eq!(mm2.unsettled_ranked(), 0, "a restore starts with no pending registrations");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_ladder_file_starts_a_fresh_ladder_identical_to_no_file() {
        // A --ladder-file that does not exist is the legal "start fresh" path: the built
        // matchmaker is byte-identical to one with no flag (an empty DEFAULT_RATING ladder),
        // so a first run against a not-yet-written file behaves exactly like today.
        let path = temp_ladder_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(read_ladder_file(&path).expect("a missing file is not an error").is_none(), "missing reads as start-fresh");

        let mut args = mode_args(2, MatchMode::Agent, vec![]);
        args.ladder_file = Some(path);
        let from_missing = build_matchmaker(&args, 2);
        let no_file = Matchmaker::new(SignatureVerifier, matchmaker_params(2, 4, ""));
        assert_eq!(from_missing.snapshot(), no_file.snapshot(), "a missing file yields the fresh in-memory ladder");
    }

    #[test]
    fn an_empty_ladder_file_starts_fresh_not_an_error() {
        // A 0-byte (or whitespace-only) file — e.g. a freshly `touch`ed path — is also the
        // start-fresh signal, distinct from a present snapshot, so it never errors.
        let path = temp_ladder_path("empty");
        std::fs::write(&path, b"   \n").expect("write an empty file");
        assert!(read_ladder_file(&path).expect("an empty file is not an error").is_none(), "empty reads as start-fresh");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_corrupt_or_stale_schema_ladder_file_is_a_loud_error_not_a_silent_reset() {
        // FM2: a present, non-empty file the harness can't trust must surface an Err (the run
        // aborts), NEVER a silent fresh ladder that would erase real standings.
        let path = temp_ladder_path("corrupt");

        // Non-JSON garbage: a hard parse error, not Ok(None).
        std::fs::write(&path, b"not a snapshot {{{").expect("write garbage");
        assert!(matches!(read_ladder_file(&path), Err(LadderFileError::Parse(_))), "garbage is a loud parse Err");

        // Valid JSON but a stale schema version: read parses it, but from_snapshot rejects it,
        // so build_matchmaker would abort rather than restore wrong ratings.
        let stale = LadderSnapshot {
            version: arena_match::LADDER_SNAPSHOT_VERSION + 1,
            ratings: BTreeMap::from([("0xabc".to_string(), 1800)]),
        };
        write_ladder(&path, &stale).expect("write a stale-schema snapshot");
        let parsed = read_ladder_file(&path).expect("valid JSON parses").expect("non-empty file");
        assert!(
            matches!(
                Matchmaker::from_snapshot(SignatureVerifier, matchmaker_params(2, 4, ""), parsed),
                Err(SnapshotError::Version { .. })
            ),
            "a stale-schema snapshot is rejected on restore, not silently loaded",
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_ladder_stages_through_a_temp_and_never_corrupts_the_prior_snapshot() {
        // FM3: the write stages to a sibling temp then atomic-renames, so an interrupted
        // persist (one that never reached the rename) leaves the PRIOR good snapshot intact.
        let path = temp_ladder_path("atomic");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(ladder_tmp_path(&path));

        let a = LadderSnapshot { version: arena_match::LADDER_SNAPSHOT_VERSION, ratings: BTreeMap::from([("0xa".to_string(), 1700)]) };
        write_ladder(&path, &a).expect("write A");
        assert!(!ladder_tmp_path(&path).exists(), "the staging temp is renamed away, never left behind");
        assert_eq!(read_ladder_file(&path).unwrap(), Some(a.clone()), "the live file reads back as A");
        assert_eq!(ladder_tmp_path(&path).parent(), path.parent(), "the temp is a same-directory sibling (atomic rename)");

        // A garbage half-write to the temp path (a persist interrupted before the rename) must
        // NOT touch the live file: it still reads as the prior good A.
        std::fs::write(ladder_tmp_path(&path), b"half written {").expect("stage garbage on the temp");
        assert_eq!(read_ladder_file(&path).unwrap(), Some(a), "an unfinished temp leaves the prior snapshot intact");

        // A completed overwrite swaps the live file to B atomically (consuming the temp).
        let b = LadderSnapshot { version: arena_match::LADDER_SNAPSHOT_VERSION, ratings: BTreeMap::from([("0xb".to_string(), 1500)]) };
        write_ladder(&path, &b).expect("write B");
        assert_eq!(read_ladder_file(&path).unwrap(), Some(b), "the live file is now B, never a half-write");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(ladder_tmp_path(&path));
    }
}
