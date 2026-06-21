//! Cross-implementation parity-vector conformance checks.
//!
//! These run against the PUBLIC `arena_core::parity_vectors()` surface — the same
//! surface the (operator-gated) UE5 dedicated-server twin consumes — and against
//! the committed golden fixture next to this file. A passing run proves the
//! reference core is self-consistent and PINNED: the integer combat/perception
//! conventions cannot drift without an intentional golden update. It does NOT
//! prove any second implementation agrees — there is no UE5 consumer yet.

use arena_core::{parity_vectors, ParityVectors, PerceptionVerdict, WeaponMode};
use arena_proto::Vec2;

const EAST: u16 = 0;
const WEST: u16 = 0x8000;

/// Independent squared-distance range test, recomputed here rather than reusing the
/// core's own `within`, so the swept-projectile endpoint assertions are a genuine
/// cross-check and not a tautology against the implementation under test.
fn within(a: Vec2, b: Vec2, range: i32) -> bool {
    let dx = b.x as i128 - a.x as i128;
    let dy = b.y as i128 - a.y as i128;
    let r = range as i128;
    dx * dx + dy * dy <= r * r
}

#[test]
fn parity_vectors_match_the_committed_golden() {
    // FM2 (fixture drift / silent staleness): the set is regenerated from the
    // CURRENT core every run and diffed against the committed golden, so a
    // combat-logic change forces an intentional golden update — the same trap the
    // contracts ABI-drift gate guards against, now for the UE5-twin wire contract.
    let current = serde_json::to_string_pretty(&parity_vectors()).unwrap() + "\n";
    let golden = include_str!("parity_vectors.json");
    assert_eq!(
        current, golden,
        "parity vectors drifted from the committed golden. A combat or perception \
         change IS a cross-implementer convention change: regenerate it \
         (cargo test regenerate_parity_vectors_golden -- --ignored) and update the \
         UE5 twin to match — never let it drift silently."
    );
}

#[test]
fn parity_vectors_are_byte_identical_across_runs() {
    // FM3 (float / platform leak): the generator is pure integer + ordered, so two
    // independent generations are bit-equal, the serialized form is byte-stable (no
    // float, no map ordering), and it round-trips through serde unchanged — the
    // canonical-set guarantee the twin relies on.
    let a = parity_vectors();
    let b = parity_vectors();
    assert_eq!(a, b, "the parity-vector generator is not deterministic");
    let ja = serde_json::to_string_pretty(&a).unwrap();
    let jb = serde_json::to_string_pretty(&b).unwrap();
    assert_eq!(ja, jb, "the serialized set is not byte-stable across runs");
    let back: ParityVectors = serde_json::from_str(&ja).unwrap();
    assert_eq!(back, a, "the persisted form does not round-trip");
}

#[test]
fn parity_vectors_pin_the_discriminating_conventions() {
    // FM1 (under-specified / non-discriminating vectors): assert each case pins a
    // LOAD-BEARING convention, mutation-checked, so a wrong twin convention fails at
    // least one vector — not a happy-path tautology.
    let v = parity_vectors();
    assert_eq!(v.domain, "blackfield/arena/parity-vectors/v1");
    assert_eq!(v.protocol_version, arena_proto::PROTOCOL_VERSION);

    // Spawns: both facing branches and a perturbed spawn line are present, so the
    // facing rule and the PRNG jitter are exercised, not a flat one-way roster.
    let four = v.spawns.iter().find(|c| c.label == "four_seats_jittered").unwrap();
    assert!(four.spawns.iter().any(|s| s.facing == EAST), "a left-of-centre seat faces east");
    assert!(four.spawns.iter().any(|s| s.facing == WEST), "a right-of-centre seat faces west");
    assert!(four.spawns.iter().any(|s| s.position.y != 0), "jitter perturbed the spawn line off y=0");

    // Perception: all four verdicts occur and the visible set is EXACTLY the
    // in-bound candidates, so range, cone, and line-of-sight are each load-bearing.
    let p = &v.perception[0];
    for want in [
        PerceptionVerdict::Visible,
        PerceptionVerdict::OutOfRange,
        PerceptionVerdict::OutOfCone,
        PerceptionVerdict::Occluded,
    ] {
        assert!(
            p.candidates.iter().any(|c| c.verdict == want),
            "perception case is missing a {want:?} edge — that filter would be vacuous"
        );
    }
    let expected: Vec<u32> = p
        .candidates
        .iter()
        .filter(|c| c.verdict == PerceptionVerdict::Visible)
        .map(|c| c.seat as u32)
        .collect();
    assert_eq!(p.visible, expected, "the visible set must be exactly the Visible-verdict candidates");

    // Hit boundary: the sub-octant target is MISSED under octant aim and LANDED under
    // fine aim — a twin that snaps the fine beam to the octant fails one of these.
    let dmg = |label: &str| v.hits.iter().find(|h| h.label == label).unwrap().damage;
    assert_eq!(dmg("sub_octant_octant_misses"), 0, "the octant beam misses the sub-octant target");
    assert!(dmg("sub_octant_fine_hits") > 0, "the finer beam lands the shot the octant missed");
    assert!(dmg("dead_on_octant") > 0 && dmg("dead_on_fine") > 0, "a dead-on shot hits in either mode");

    // Projectile sweep: a 20 m/tick shot hits a 5 m target on its first swept step
    // though BOTH endpoints miss — a per-tick point check tunnels and reports no hit,
    // so this case discriminates swept from point collision.
    let sweep = v.projectiles.iter().find(|c| c.label == "fast_sweep_no_tunnel").unwrap();
    assert_eq!(sweep.ticks_to_hit, Some(1), "the fast shot hits on its first swept step");
    assert!(sweep.damage > 0);
    assert!(!within(sweep.shooter_position, sweep.target_position, sweep.hit_radius), "the launch point alone misses");
    let after = Vec2 { x: sweep.shooter_position.x + sweep.projectile_speed, y: sweep.shooter_position.y };
    assert!(!within(after, sweep.target_position, sweep.hit_radius), "the post-step point alone misses");
    let miss = v.projectiles.iter().find(|c| c.label == "off_line_clean_miss").unwrap();
    assert_eq!(miss.ticks_to_hit, None, "an off-line shot never reaches the target");

    // Full matches: every committed record re-runs to its own committed result
    // (self-consistency); the digest commits the INPUTS, so the octant and fine
    // matches share a digest under the same action stream; and the rules bind the
    // OUTCOMES, so the projectile match diverges. Tampering either determinant must
    // break verification.
    for c in &v.matches {
        assert!(c.record.verify().is_ok(), "committed match {} does not self-verify", c.label);
    }
    let pick = |label: &str| &v.matches.iter().find(|c| c.label == label).unwrap().record;
    let (octant, fine, proj) = (pick("octant_hitscan"), pick("fine_hitscan"), pick("projectile"));
    assert_eq!(
        octant.result.replay_hash, fine.result.replay_hash,
        "same action stream -> same digest, regardless of aim mode (the digest commits inputs)"
    );
    assert_ne!(octant.result.replay_hash, proj.result.replay_hash, "the projectile stream differs -> a different digest");
    assert_ne!(octant.result.final_tick, proj.result.final_tick, "weapon mode is an outcome determinant");

    let mut flipped = proj.clone();
    flipped.rules.weapon_mode = WeaponMode::Hitscan;
    assert!(flipped.verify().is_err(), "flipping weapon_mode must break the committed result");
    let mut reseeded = octant.clone();
    reseeded.replay.seed ^= 1;
    assert!(reseeded.verify().is_err(), "the committed digest binds the seed");

    // Pickups: the pickup match carries a configured item layout that the digest
    // commits (v3), so its hash differs from the same-script no-pickup octant match —
    // and dropping the pickup from the committed record breaks verification.
    let pickup = pick("pickup_collected");
    assert!(!pickup.replay.pickups.is_empty(), "the pickup match records its item layout");
    assert_ne!(
        pickup.result.replay_hash, octant.result.replay_hash,
        "the pickup layout binds the digest — a pickup match differs from the bare one"
    );
    let mut dropped = pickup.clone();
    dropped.replay.pickups.clear();
    assert!(dropped.verify().is_err(), "dropping the committed pickup must break the hash");
}

/// Rewrite the committed golden from the current core. Ignored in CI; run it
/// deliberately (`cargo test regenerate_parity_vectors_golden -- --ignored`) when a
/// combat/perception change has intentionally moved the conformance contract, then
/// re-point the UE5 twin at the new vectors.
#[test]
#[ignore = "writes the golden fixture; run explicitly to regenerate"]
fn regenerate_parity_vectors_golden() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/parity_vectors.json");
    let json = serde_json::to_string_pretty(&parity_vectors()).unwrap();
    std::fs::write(path, json + "\n").unwrap();
}
