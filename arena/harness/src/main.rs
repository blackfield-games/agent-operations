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

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use arena_core::{Match, Rules};
use arena_proto::{
    check_version, ActionIntent, AgentMsg, GatewayMsg, MatchConfig, MatchPhase, SeatId, SeatInfo,
    Vec2, POSITION_SCALE, PROTOCOL_VERSION,
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
}

fn parse_args() -> Args {
    let mut match_id = DEFAULT_MATCH_ID.to_string();
    let mut seed: u64 = 0;
    let mut seats: u8 = 2;
    let mut max_ticks: u64 = 3600;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--match-id" => match_id = it.next().expect("--match-id needs a value"),
            "--seed" => seed = it.next().expect("--seed needs a value").parse().expect("seed is a u64"),
            "--seats" => seats = it.next().expect("--seats needs a value").parse().expect("seats is a u8"),
            "--max-ticks" => {
                max_ticks = it.next().expect("--max-ticks needs a value").parse().expect("max-ticks is a u64")
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    Args {
        match_id: Uuid::parse_str(&match_id).expect("--match-id is a valid UUID"),
        seed,
        seats,
        max_ticks,
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
    let mut m = Match::new(args.match_id, config, Rules::default(), roster, args.seed);

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
}
