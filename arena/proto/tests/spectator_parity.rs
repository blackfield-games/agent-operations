//! Cross-implementer SPECTATOR wire-FRAME parity conformance.
//!
//! Pins `arena_proto::spectator_parity_vectors()` against the committed golden next to
//! this file. The SAME golden is loaded by the Python agent SDK
//! (`agents/test_arena_client.py`), so the Rust reference frames and the Python
//! `decode_spectator` are held to ONE source of truth — a wire-shape drift on either
//! side breaks loud. The spectator sibling of `frame_parity.rs` (the per-seat play
//! protocol); the spectator protocol is one-directional (server→spectator), so every
//! frame decodes as a `SpectatorMsg` with no direction split. Regenerate after an
//! intentional wire change with
//! `cargo test -p arena-proto --test spectator_parity regenerate_spectator_parity_golden -- --ignored`.

use std::collections::BTreeSet;

use arena_proto::{spectator_parity_vectors, SpectatorMsg, SpectatorParityVectors, PROTOCOL_VERSION};

#[test]
fn spectator_parity_matches_the_committed_golden() {
    // Drift / silent staleness: the set is regenerated from the CURRENT types every run
    // and diffed against the committed golden, so a spectator wire-frame change (a renamed
    // tag, a re-flattened variant, a new field) forces an intentional golden update — the
    // frame_parity discipline, now for the Rust⇄Python spectator contract.
    let current = serde_json::to_string_pretty(&spectator_parity_vectors()).unwrap() + "\n";
    let golden = include_str!("spectator_parity.json");
    assert_eq!(
        current, golden,
        "spectator parity golden drifted from the committed file. A spectator wire-frame \
         change IS a cross-implementer convention change: regenerate it (cargo test -p \
         arena-proto --test spectator_parity regenerate_spectator_parity_golden -- --ignored) \
         and update the Python SDK to match — never let it drift silently."
    );
}

#[test]
fn spectator_parity_is_byte_identical_across_runs() {
    // Float / platform / ordering leak: the generator is pure integer + UUID + fixed
    // strings, so two generations are bit-equal, the serialized form is byte-stable, and it
    // round-trips through serde unchanged — the canonical-set guarantee the Python consumer
    // relies on.
    let a = spectator_parity_vectors();
    let b = spectator_parity_vectors();
    assert_eq!(a, b, "the spectator-parity generator is not deterministic");
    let ja = serde_json::to_string_pretty(&a).unwrap();
    let jb = serde_json::to_string_pretty(&b).unwrap();
    assert_eq!(ja, jb, "the serialized set is not byte-stable across runs");
    let back: SpectatorParityVectors = serde_json::from_str(&ja).unwrap();
    assert_eq!(back, a, "the persisted form does not round-trip");
}

#[test]
fn spectator_parity_pins_the_wire_conventions() {
    let v = spectator_parity_vectors();
    assert_eq!(v.domain, "blackfield/arena/spectator-parity/v1");
    assert_eq!(v.protocol_version, PROTOCOL_VERSION);
    assert_eq!(v.match_id, "550e8400-e29b-41d4-a716-446655440000");
    let find = |label: &str| {
        v.frames.iter().find(|c| c.label == label).unwrap_or_else(|| panic!("missing frame {label}"))
    };

    // Every frame decodes into a SpectatorMsg — the `type` tag and (for the newtype
    // variants) the flattening are pinned by a genuine decode, not a string match. A
    // renamed tag or a variant that nests instead of flattens fails here.
    for c in &v.frames {
        serde_json::from_value::<SpectatorMsg>(c.frame.clone())
            .unwrap_or_else(|e| panic!("frame {} did not decode as SpectatorMsg: {e}", c.label));
    }

    // Internal tagging + newtype flattening AT THE WIRE LEVEL: the frame/end cases carry
    // their `type` tag AND the inner struct's fields flattened next to it (not nested under
    // a "broadcast"/"match_result" key). This is the exact shape decode_spectator in
    // proto.py reconstructs.
    let frame = &find("frame_populated").frame;
    assert_eq!(frame["type"], "frame");
    assert!(frame.get("entities").is_some(), "frame must flatten the Broadcast (entities at top level)");
    assert!(frame.get("broadcast").is_none(), "frame must NOT nest the Broadcast");
    let end = &find("end").frame;
    assert_eq!(end["type"], "end");
    assert!(end.get("outcomes").is_some(), "end must flatten the MatchResult (outcomes at top level)");
    assert!(end.get("match_result").is_none(), "end must NOT nest the MatchResult");

    // BroadcastEntity is the caster-camera shape, distinct from VisibleEntity: it carries
    // health/max_health/score/alive and NO in_line_of_sight — and never a private HUD field
    // (ammo/cooldown). The golden's populated frame pins that exact key set on the wire.
    let entity = &frame["entities"][0];
    for k in ["entity_id", "kind", "team", "position", "z", "facing", "health", "max_health", "score", "alive"] {
        assert!(entity.get(k).is_some(), "a BroadcastEntity must carry {k}");
    }
    for k in ["in_line_of_sight", "ammo", "cooldown"] {
        assert!(entity.get(k).is_none(), "a BroadcastEntity must NOT carry {k} (private/perception state)");
    }

    // exact_reencode frames re-encode byte-for-byte: decode then to_value reproduces the
    // frame — the wire-shape pin, driven by the golden.
    for c in v.frames.iter().filter(|c| c.exact_reencode) {
        let round =
            serde_json::to_value(serde_json::from_value::<SpectatorMsg>(c.frame.clone()).unwrap()).unwrap();
        assert_eq!(round, c.frame, "frame {} did not re-encode byte-for-byte", c.label);
    }

    // Back-compat: the omit frame decodes with the serde(default) filled. A future required
    // `starting_remaining` (dropping #[serde(default)]) would fail to decode this exact frame
    // — the live mixed-fleet guard the populated frame can't give.
    match serde_json::from_value::<SpectatorMsg>(find("frame_legacy_omits_starting_remaining").frame.clone())
        .unwrap()
    {
        SpectatorMsg::Frame(b) => {
            assert_eq!(b.starting_remaining, 0, "an omitted starting_remaining must default to 0");
        }
        other => panic!("expected Frame, got {other:?}"),
    }

    // Every match_id-carrying frame uses the one fixed parity id (byte-stability: no
    // per-frame UUID can sneak in and flap the golden).
    for c in &v.frames {
        if let Some(mid) = c.frame.get("match_id") {
            assert_eq!(mid, &v.match_id, "frame {} carries a foreign match_id", c.label);
        }
    }

    // One representative of EVERY variant is present (per-variant completeness). The tag of
    // each golden frame is computed by the EXHAUSTIVE spectator_tag match below — adding a
    // SpectatorMsg variant is a COMPILE error there until it is tagged, which lands you next
    // to the note to add a representative frame. The emitted set is built by DECODING each
    // golden frame to its typed variant and tagging it, so it can never claim a tag the
    // golden does not carry.
    let mut emitted = BTreeSet::new();
    for c in &v.frames {
        let m: SpectatorMsg = serde_json::from_value(c.frame.clone()).unwrap();
        emitted.insert(spectator_tag(&m));
    }
    assert_eq!(
        emitted,
        BTreeSet::from(["frame", "end"]),
        "a SpectatorMsg variant has no golden frame"
    );
}

/// The wire tag for each [`SpectatorMsg`] variant, via an EXHAUSTIVE match (no `_` arm):
/// adding a variant fails to compile HERE until it is given a tag — the compile-time half
/// of the per-variant completeness guarantee. When you add the arm you MUST also add a
/// representative frame to `spectator_parity_vectors()` and list its tag in the
/// completeness assert above.
fn spectator_tag(m: &SpectatorMsg) -> &'static str {
    match m {
        SpectatorMsg::Frame(_) => "frame",
        SpectatorMsg::End(_) => "end",
    }
}

#[test]
#[ignore = "writes the golden fixture; run explicitly to regenerate"]
fn regenerate_spectator_parity_golden() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/spectator_parity.json");
    let json = serde_json::to_string_pretty(&spectator_parity_vectors()).unwrap() + "\n";
    std::fs::write(path, json).unwrap();
    eprintln!("wrote {path}");
}
