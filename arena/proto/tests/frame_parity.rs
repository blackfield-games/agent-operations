//! Cross-implementer Gateway wire-FRAME parity conformance.
//!
//! Pins `arena_proto::frame_parity_vectors()` against the committed golden next to
//! this file. The SAME golden is loaded by the Python agent SDK
//! (`agents/test_arena_client.py`), so the Rust reference frames and the Python
//! `decode_gateway` / frame encoders are held to ONE source of truth — a wire-shape
//! drift on either side breaks loud. The sibling of `clamp_parity.rs` for the OTHER
//! hand-duplicated cross-impl surface: the canonical wire frames. Regenerate after an
//! intentional wire change with
//! `cargo test -p arena-proto --test frame_parity regenerate_frame_parity_golden -- --ignored`.

use std::collections::BTreeSet;

use arena_proto::{
    frame_parity_vectors, AgentMsg, FrameDirection, FrameParityVectors, GatewayMsg, PROTOCOL_VERSION,
};

#[test]
fn frame_parity_matches_the_committed_golden() {
    // Drift / silent staleness: the set is regenerated from the CURRENT types every
    // run and diffed against the committed golden, so a wire-frame change (a renamed
    // tag, a re-flattened variant, a new field) forces an intentional golden update —
    // the same trap the contracts ABI-drift gate and the clamp-parity golden guard,
    // now for the Rust⇄Python frame contract.
    let current = serde_json::to_string_pretty(&frame_parity_vectors()).unwrap() + "\n";
    let golden = include_str!("frame_parity.json");
    assert_eq!(
        current, golden,
        "frame parity golden drifted from the committed file. A wire-frame change IS a \
         cross-implementer convention change: regenerate it (cargo test -p arena-proto \
         --test frame_parity regenerate_frame_parity_golden -- --ignored) and update the \
         Python SDK to match — never let it drift silently."
    );
}

#[test]
fn frame_parity_is_byte_identical_across_runs() {
    // Float / platform / ordering leak: the generator is pure integer + UUID + fixed
    // strings, so two generations are bit-equal, the serialized form is byte-stable
    // (serde_json objects sort their keys), and it round-trips through serde unchanged
    // — the canonical-set guarantee the Python consumer relies on.
    let a = frame_parity_vectors();
    let b = frame_parity_vectors();
    assert_eq!(a, b, "the frame-parity generator is not deterministic");
    let ja = serde_json::to_string_pretty(&a).unwrap();
    let jb = serde_json::to_string_pretty(&b).unwrap();
    assert_eq!(ja, jb, "the serialized set is not byte-stable across runs");
    let back: FrameParityVectors = serde_json::from_str(&ja).unwrap();
    assert_eq!(back, a, "the persisted form does not round-trip");
}

#[test]
fn frame_parity_pins_the_wire_conventions() {
    let v = frame_parity_vectors();
    assert_eq!(v.domain, "blackfield/arena/frame-parity/v1");
    assert_eq!(v.protocol_version, PROTOCOL_VERSION);
    assert_eq!(v.match_id, "550e8400-e29b-41d4-a716-446655440000");
    let find = |label: &str| {
        v.frames
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("missing frame {label}"))
    };

    // Every server→agent frame decodes into a GatewayMsg, every agent→server frame
    // into an AgentMsg — the `type` tag and (for the newtype variants) the flattening
    // are pinned by a genuine decode, not a string match. A renamed tag or a variant
    // that nests instead of flattens fails here.
    for c in &v.frames {
        match c.direction {
            FrameDirection::ServerToAgent => {
                serde_json::from_value::<GatewayMsg>(c.frame.clone()).unwrap_or_else(|e| {
                    panic!("server frame {} did not decode as GatewayMsg: {e}", c.label)
                });
            }
            FrameDirection::AgentToServer => {
                serde_json::from_value::<AgentMsg>(c.frame.clone()).unwrap_or_else(|e| {
                    panic!("agent frame {} did not decode as AgentMsg: {e}", c.label)
                });
            }
        }
    }

    // Internal tagging + newtype flattening AT THE WIRE LEVEL: the observe/act/end
    // frames carry their `type` tag AND the inner struct's fields flattened next to it
    // (not nested under an "observation"/"action"/"match_result" key). This is the
    // exact shape decode_gateway in proto.py reconstructs.
    let observe = &find("observe").frame;
    assert_eq!(observe["type"], "observe");
    assert!(observe.get("own").is_some(), "observe must flatten the Observation (own at top level)");
    assert!(observe.get("observation").is_none(), "observe must NOT nest the Observation");
    let act = &find("act").frame;
    assert_eq!(act["type"], "act");
    assert!(act.get("intent").is_some(), "act must flatten the Action (intent at top level)");
    assert!(act.get("action").is_none(), "act must NOT nest the Action");
    let end = &find("end").frame;
    assert_eq!(end["type"], "end");
    assert!(end.get("outcomes").is_some(), "end must flatten the MatchResult (outcomes at top level)");

    // exact_reencode frames re-encode byte-for-byte: decode then to_value reproduces
    // the frame — the assert_round wire-shape pin, driven by the golden.
    for c in v.frames.iter().filter(|c| c.exact_reencode) {
        let round = match c.direction {
            FrameDirection::ServerToAgent => {
                serde_json::to_value(serde_json::from_value::<GatewayMsg>(c.frame.clone()).unwrap())
                    .unwrap()
            }
            FrameDirection::AgentToServer => {
                serde_json::to_value(serde_json::from_value::<AgentMsg>(c.frame.clone()).unwrap())
                    .unwrap()
            }
        };
        assert_eq!(round, c.frame, "frame {} did not re-encode byte-for-byte", c.label);
    }

    // Back-compat: the omit frames decode with the serde(default) filled. A future
    // required-field change (dropping #[serde(default)]) would fail to decode these
    // exact frames here — the live mixed-fleet guard the populated frames can't give.
    match serde_json::from_value::<GatewayMsg>(find("start_legacy_omits_optional_fields").frame.clone())
        .unwrap()
    {
        GatewayMsg::Start { blockers, pickup_points, .. } => {
            assert!(
                blockers.is_empty() && pickup_points.is_empty(),
                "an omitted blockers/pickup_points must default to empty"
            );
        }
        other => panic!("expected Start, got {other:?}"),
    }
    match serde_json::from_value::<GatewayMsg>(find("start_blocker_omits_height").frame.clone()).unwrap()
    {
        GatewayMsg::Start { blockers, .. } => {
            assert_eq!(blockers[0].height, 0, "an omitted blocker height defaults to 0 (infinitely tall)");
        }
        other => panic!("expected Start, got {other:?}"),
    }

    // Every match_id-carrying frame uses the one fixed parity id (byte-stability: no
    // per-frame UUID can sneak in and flap the golden).
    for c in &v.frames {
        if let Some(mid) = c.frame.get("match_id") {
            assert_eq!(mid, &v.match_id, "frame {} carries a foreign match_id", c.label);
        }
    }

    // One representative of EVERY variant is present (per-variant completeness). The
    // tag of each golden frame is computed by the EXHAUSTIVE gateway_tag/agent_tag
    // match below — adding a GatewayMsg/AgentMsg variant is a COMPILE error there
    // until it is tagged, which lands you next to the note to add a representative
    // frame here. The emitted set is built by DECODING each golden frame to its typed
    // variant and tagging it, so it can never claim a tag the golden does not carry.
    let mut emitted_server = BTreeSet::new();
    let mut emitted_agent = BTreeSet::new();
    for c in &v.frames {
        match c.direction {
            FrameDirection::ServerToAgent => {
                let m: GatewayMsg = serde_json::from_value(c.frame.clone()).unwrap();
                emitted_server.insert(gateway_tag(&m));
            }
            FrameDirection::AgentToServer => {
                let m: AgentMsg = serde_json::from_value(c.frame.clone()).unwrap();
                emitted_agent.insert(agent_tag(&m));
            }
        }
    }
    assert_eq!(
        emitted_server,
        BTreeSet::from(["challenge", "welcome", "reject", "start", "observe", "end"]),
        "a server→agent GatewayMsg variant has no golden frame"
    );
    assert_eq!(
        emitted_agent,
        BTreeSet::from(["join", "act", "leave"]),
        "an agent→server AgentMsg variant has no golden frame"
    );
}

/// The wire tag for each server→agent variant, via an EXHAUSTIVE match (no `_` arm):
/// adding a [`GatewayMsg`] variant fails to compile HERE until it is given a tag —
/// the compile-time half of the per-variant completeness guarantee. When you add the
/// arm you MUST also add a representative frame to `frame_parity_vectors()` and list
/// its tag in the completeness assert above (a variant present in the type but not the
/// golden is the silent gap this guard closes).
fn gateway_tag(m: &GatewayMsg) -> &'static str {
    match m {
        GatewayMsg::Challenge { .. } => "challenge",
        GatewayMsg::Welcome { .. } => "welcome",
        GatewayMsg::Reject { .. } => "reject",
        GatewayMsg::Start { .. } => "start",
        GatewayMsg::Observe(_) => "observe",
        GatewayMsg::End(_) => "end",
    }
}

/// The agent→server twin of [`gateway_tag`] — same exhaustive-match guard.
fn agent_tag(m: &AgentMsg) -> &'static str {
    match m {
        AgentMsg::Join { .. } => "join",
        AgentMsg::Act(_) => "act",
        AgentMsg::Leave { .. } => "leave",
    }
}

#[test]
#[ignore = "writes the golden fixture; run explicitly to regenerate"]
fn regenerate_frame_parity_golden() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/frame_parity.json");
    let json = serde_json::to_string_pretty(&frame_parity_vectors()).unwrap() + "\n";
    std::fs::write(path, json).unwrap();
    eprintln!("wrote {path}");
}
