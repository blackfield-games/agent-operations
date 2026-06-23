//! Cross-implementer move-clamp parity conformance.
//!
//! Pins `arena_proto::clamp_parity_vectors()` against the committed golden next to
//! this file. The SAME golden is loaded by the Python agent SDK
//! (`agents/test_arena_client.py`), so the Rust reference clamp and the Python
//! `ActionIntent.clamped()` are held to ONE source of truth — a drift on either
//! side breaks loud. Regenerate after an intentional clamp change with
//! `cargo test -p arena-proto --test clamp_parity regenerate_clamp_parity_golden -- --ignored`.

use arena_proto::{clamp_parity_vectors, ClampParityVectors, Vec2, MOVE_INTENT_SCALE};

#[test]
fn clamp_parity_matches_the_committed_golden() {
    // Drift / silent staleness: the set is regenerated from the CURRENT clamp every
    // run and diffed against the committed golden, so a move-clamp change forces an
    // intentional golden update — the same trap the contracts ABI-drift gate and the
    // arena-core parity golden guard, now for the Rust⇄Python clamp contract.
    let current = serde_json::to_string_pretty(&clamp_parity_vectors()).unwrap() + "\n";
    let golden = include_str!("clamp_parity.json");
    assert_eq!(
        current, golden,
        "clamp parity vectors drifted from the committed golden. A move-clamp change IS a \
         cross-implementer convention change: regenerate it (cargo test -p arena-proto \
         --test clamp_parity regenerate_clamp_parity_golden -- --ignored) and update the \
         Python SDK to match — never let it drift silently."
    );
}

#[test]
fn clamp_parity_is_byte_identical_across_runs() {
    // Float / platform leak: the generator is pure integer + ordered, so two
    // generations are bit-equal, the serialized form is byte-stable (no float, no map
    // ordering), and it round-trips through serde unchanged — the canonical-set
    // guarantee the Python consumer relies on.
    let a = clamp_parity_vectors();
    let b = clamp_parity_vectors();
    assert_eq!(a, b, "the clamp-parity generator is not deterministic");
    let ja = serde_json::to_string_pretty(&a).unwrap();
    let jb = serde_json::to_string_pretty(&b).unwrap();
    assert_eq!(ja, jb, "the serialized set is not byte-stable across runs");
    let back: ClampParityVectors = serde_json::from_str(&ja).unwrap();
    assert_eq!(back, a, "the persisted form does not round-trip");
}

#[test]
fn clamp_parity_pins_the_discriminating_conventions() {
    let v = clamp_parity_vectors();
    assert_eq!(v.domain, "blackfield/arena/clamp-parity/v2");
    assert_eq!(v.move_intent_scale, MOVE_INTENT_SCALE);
    let case = |label: &str| {
        v.cases
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("missing case {label}"))
    };
    // Sub-cap and at-cap requests pass through untouched (magnitude ≤ scale).
    for label in [
        "zero_is_inert",
        "short_passthrough",
        "at_cap_345_passthrough",
        "at_cap_axis_passthrough",
    ] {
        let c = case(label);
        assert_eq!(c.input, c.output, "{label} must pass through unchanged");
    }
    // Overlong → exact known clamp, asserted by VALUE (not via clamped()), so the pin
    // is a genuine value contract a second implementer must hit, not a tautology
    // against the impl under test.
    assert_eq!(case("overlong_345_scaled").output, Vec2 { x: 600, y: 800 });
    assert_eq!(case("just_over_axis_scaled").output, Vec2 { x: 1000, y: 0 });
    assert_eq!(case("overlong_diagonal_scaled").output, Vec2 { x: 707, y: 707 });
    assert_eq!(case("single_axis_negative_scaled").output, Vec2 { x: -1000, y: 0 });
    // Truncate toward zero, NOT floor: floor division would yield (-708, 707).
    assert_eq!(case("negative_axis_trunc_toward_zero").output, Vec2 { x: -707, y: 707 });
    // The {i32::MIN, i32::MIN} corner stays clamped — the u64-widening guard against
    // the i64 square-sum overflow that would otherwise let it through at full speed.
    assert_eq!(case("i32_min_overflow_corner").output, Vec2 { x: -707, y: -707 });
    // No output ever exceeds the cap — the clamp can only scale DOWN, never up.
    let max_sq = (MOVE_INTENT_SCALE as i64).pow(2);
    for c in &v.cases {
        let mag_sq = (c.output.x as i64).pow(2) + (c.output.y as i64).pow(2);
        assert!(
            mag_sq <= max_sq,
            "case {} output exceeds the cap: mag^2={mag_sq} > {max_sq}",
            c.label
        );
    }
}

#[test]
fn clamp_parity_fuzz_sweep_widens_the_executable_cross_check() {
    // The fixed-seed fuzz cases are what turn the by-hand equivalence review into a
    // committed gate: pin that they are present, span the overflow-prone i32 range,
    // and actually exercise the scale-down path (not passthrough), so a future
    // Rust⇄Python clamp divergence on an unnamed input reddens this golden. The 12
    // named discriminators MUST survive alongside them (append, never replace).
    let v = clamp_parity_vectors();
    for named in ["zero_is_inert", "negative_axis_trunc_toward_zero", "i32_min_overflow_corner"] {
        assert!(v.cases.iter().any(|c| c.label == named), "named case {named} dropped");
    }
    let fuzz: Vec<_> = v.cases.iter().filter(|c| c.label.starts_with("fuzz_")).collect();
    assert_eq!(fuzz.len(), 64, "expected 64 fuzz cases");
    assert_eq!(v.cases.len(), 76, "expected 12 named + 64 fuzz = 76 total");
    // Full-range i32 inputs are all overlong, so every fuzz case must scale DOWN to
    // the cap — proof the sweep hits the clamp path the cross-check is meant to cover.
    let max = MOVE_INTENT_SCALE as i64;
    for c in &fuzz {
        assert_ne!(c.input, c.output, "fuzz case {} unexpectedly passed through", c.label);
        let mag_sq = (c.output.x as i64).pow(2) + (c.output.y as i64).pow(2);
        assert!(mag_sq <= max * max, "fuzz case {} exceeds the cap", c.label);
    }
    // At least one draw lands near i32::MIN/MAX (|component| > 2^30), exercising the
    // u64-widened square-sum the overflow corners guard — full-range coverage, not
    // just small magnitudes.
    let reach = fuzz
        .iter()
        .map(|c| c.input.x.unsigned_abs().max(c.input.y.unsigned_abs()))
        .max()
        .unwrap();
    assert!(reach > 1 << 30, "fuzz sweep never reached the near-overflow range: {reach}");
}

#[test]
#[ignore = "writes the golden fixture; run explicitly to regenerate"]
fn regenerate_clamp_parity_golden() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/clamp_parity.json");
    let json = serde_json::to_string_pretty(&clamp_parity_vectors()).unwrap() + "\n";
    std::fs::write(path, json).unwrap();
    eprintln!("wrote {path}");
}
