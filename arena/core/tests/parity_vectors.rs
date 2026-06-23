//! Cross-implementation parity-vector conformance checks.
//!
//! These run against the PUBLIC `arena_core::parity_vectors()` surface — the same
//! surface the (operator-gated) UE5 dedicated-server twin consumes — and against
//! the committed golden fixture next to this file. A passing run proves the
//! reference core is self-consistent and PINNED: the integer combat/perception
//! conventions cannot drift without an intentional golden update. It does NOT
//! prove any second implementation agrees — there is no UE5 consumer yet.

use arena_core::{
    expected_score_bp, parity_vectors, rating_delta, AimMode, MatchOutcome, ParityVectors,
    PerceptionVerdict, WeaponMode, RATING_DIFF_CAP, RATING_SCALE,
};
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
    assert_eq!(v.domain, "blackfield/arena/parity-vectors/v11");
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

    // Physical cover — movement: a step into a wall is refused (the pawn holds), one
    // alongside it is free, a fast step cannot tunnel a thin wall, and a pawn spawned
    // inside a wall can still leave it. A twin that walks through walls, point-tests
    // movement (and so tunnels), or traps a wall-spawned pawn fails one of these.
    let mv = |label: &str| v.moves.iter().find(|c| c.label == label).unwrap();
    let into = mv("into_wall_blocked");
    assert!(into.blocked && into.end == into.start, "a step into a wall is refused");
    let along = mv("alongside_wall_allowed");
    assert!(!along.blocked && along.end != along.start, "a step alongside the wall is free");
    let fast = mv("fast_step_no_tunnel");
    assert!(fast.blocked && fast.end == fast.start, "a fast step is stopped by a thin wall");
    // The unblocked destination is PAST the wall, so an endpoint/point test tunnels —
    // the swept test is load-bearing here.
    let tunnel_dest = Vec2 { x: fast.start.x + fast.max_speed, y: fast.start.y };
    assert!(tunnel_dest.x > fast.blockers[0].max.x, "the would-be destination overshoots the wall");
    let escapes = mv("spawn_in_wall_escapes");
    assert!(!escapes.blocked && escapes.end != escapes.start, "a pawn spawned in a wall can step out");

    // Hit boundary: the sub-octant target is MISSED under octant aim and LANDED under
    // fine aim — a twin that snaps the fine beam to the octant fails one of these.
    let dmg = |label: &str| v.hits.iter().find(|h| h.label == label).unwrap().damage;
    assert_eq!(dmg("sub_octant_octant_misses"), 0, "the octant beam misses the sub-octant target");
    assert!(dmg("sub_octant_fine_hits") > 0, "the finer beam lands the shot the octant missed");
    assert!(dmg("dead_on_octant") > 0 && dmg("dead_on_fine") > 0, "a dead-on shot hits in either mode");
    // Physical cover — hitscan: the SAME dead-on shot is blocked by a wall on the
    // sightline (the wall is load-bearing — without it dead_on_octant lands).
    let blocked_hit = v.hits.iter().find(|h| h.label == "blocked_by_wall").unwrap();
    assert_eq!(blocked_hit.damage, 0, "a wall on the sightline blocks the beam");
    assert!(!blocked_hit.blockers.is_empty(), "the blocked hit carries its occluder");

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
    // Physical cover — projectile: a wall on the path absorbs the shot (the target
    // behind it is never hit), and a target IN FRONT of a wall is still hit on the
    // same swept step — a wall-first twin would wrongly absorb that shot.
    let proj_blocked = v.projectiles.iter().find(|c| c.label == "blocked_by_wall").unwrap();
    assert_eq!(proj_blocked.ticks_to_hit, None, "a wall absorbs the projectile before the target");
    assert_eq!(proj_blocked.damage, 0);
    assert!(!proj_blocked.blockers.is_empty(), "the blocked projectile carries its occluder");
    let front = v.projectiles.iter().find(|c| c.label == "pawn_in_front_of_wall_is_hit").unwrap();
    assert_eq!(front.ticks_to_hit, Some(1), "a target in front of a wall is hit on the first swept step");
    assert!(front.damage > 0 && !front.blockers.is_empty(), "cover behind the target shields nothing");

    // z-coupled combat (v4): the vertical hit rule gates every weapon mode. With the
    // tolerance off a high target is hit (z ignored); with it on, a target above the
    // tolerance is cleared for hitscan, melee, AND the level projectile; and the boundary
    // is inclusive. A twin that ignores z under a set tolerance, that couples only some
    // modes, or that uses an exclusive boundary fails one of these.
    let vh = |label: &str| v.vertical_hits.iter().find(|c| c.label == label).unwrap();
    assert!(vh("hitscan_off_ignores_elevation").damage > 0, "tolerance off: an elevated target is still hit (planar)");
    assert!(vh("projectile_off_ignores_elevation").damage > 0, "tolerance off: a projectile ignores elevation too");
    assert_eq!(vh("hitscan_above_tolerance_cleared").damage, 0, "hitscan: a target above the tolerance is cleared");
    assert_eq!(vh("melee_above_tolerance_cleared").damage, 0, "melee shares the one vertical rule");
    assert_eq!(vh("projectile_above_tolerance_cleared").damage, 0, "the level projectile clears a high target");
    let boundary = vh("hitscan_at_tolerance_lands");
    assert_eq!(
        (boundary.shooter_z - boundary.target_z).abs(),
        boundary.vertical_hit_tolerance,
        "the boundary case sits exactly at the tolerance"
    );
    assert!(boundary.damage > 0, "a target exactly at the tolerance is hit — the bound is inclusive");
    // The cleared cases are gated by ELEVATION, not the planar setup: each is genuinely
    // above its (non-zero) tolerance, and the matching tolerance-off case at z=5000 lands,
    // so z alone produced the miss.
    for label in ["hitscan_above_tolerance_cleared", "melee_above_tolerance_cleared", "projectile_above_tolerance_cleared"] {
        let c = vh(label);
        assert!(
            c.vertical_hit_tolerance > 0 && (c.shooter_z - c.target_z).abs() > c.vertical_hit_tolerance,
            "{label} must be strictly above its tolerance for the clear to mean z-coupling"
        );
    }

    // z-aware occlusion (v5): a height-bounded wall blocks a ground look but is cleared
    // by a high-enough sightline; an infinitely-tall (height 0) wall blocks at any
    // elevation; a rising look still in the wall's band where it crosses is blocked. A
    // twin that ignores height fails the high-look case; one that lets the rising look
    // through fails the in-band case.
    let voc = |label: &str| v.vision_over_cover.iter().find(|c| c.label == label).unwrap();
    let ground = voc("ground_look_blocked");
    assert!(ground.occluded && ground.blocker.height > 0, "a ground look is blocked by the finite wall");
    let high = voc("high_look_clears_the_wall");
    assert!(!high.occluded, "a sightline over the top is not occluded by a height-bounded wall");
    // The high look is the SAME geometry as the infinite case except for the wall
    // height, so height alone decides — the discriminating pin.
    let infinite = voc("infinite_wall_blocks_high_look");
    assert!(infinite.occluded && infinite.blocker.height == 0, "an infinitely-tall wall blocks the high look");
    assert_eq!(
        (high.from, high.from_z, high.to, high.to_z),
        (infinite.from, infinite.from_z, infinite.to, infinite.to_z),
        "high vs infinite differ ONLY in the wall height"
    );
    assert!(voc("rising_look_still_in_band_blocked").occluded, "a look still below the top where it crosses is blocked");

    // z-aware traversal (v6): the physical twin of the sight rule — a height-bounded wall
    // stops a ground path but is cleared by a high-enough level path; an infinitely-tall
    // wall stops it at any elevation; grazing the top still blocks; and travel is
    // DIRECTIONAL (start exempt, destination not). A twin that ignores height fails the
    // high case; one that exempts the destination fails the end-inside case.
    let moc = |label: &str| v.movement_over_cover.iter().find(|c| c.label == label).unwrap();
    let mground = moc("ground_path_blocked");
    assert!(mground.blocked && mground.blocker.height > 0, "a ground path is blocked by the finite wall");
    let mhigh = moc("high_path_clears_the_wall");
    assert!(!mhigh.blocked, "a level path over the top is not blocked by a height-bounded wall");
    let minf = moc("infinite_wall_blocks_high_path");
    assert!(minf.blocked && minf.blocker.height == 0, "an infinitely-tall wall blocks the high path");
    assert_eq!(
        (mhigh.from, mhigh.from_z, mhigh.to, mhigh.to_z),
        (minf.from, minf.from_z, minf.to, minf.to_z),
        "high vs infinite differ ONLY in the wall height"
    );
    assert!(moc("grazing_top_blocked").blocked, "a path grazing the wall top is blocked (conservative boundary)");
    // The directional exemption — the movement/sight divergence (sight exempts both ends).
    assert!(!moc("start_inside_is_exempt").blocked, "a path starting inside the wall may leave it");
    assert!(moc("end_inside_is_blocked").blocked, "a path ending inside the wall is still blocked");
    // Sight and traversal AGREE on what a given elevation clears (the same Blocker.height
    // bounds both), so a twin cannot satisfy one rule while diverging on the other.
    assert_eq!(high.occluded, mhigh.blocked, "sight and traversal agree: the high wall is cleared by both");
    assert_eq!(infinite.occluded, minf.blocked, "sight and traversal agree: the infinite wall blocks both");

    // Full matches: every committed record re-runs to its own committed result
    // (self-consistency); the v5 digest commits the inputs, the rules, AND the config. The octant
    // and fine matches run the IDENTICAL action stream and differ ONLY in aim_mode, so
    // the tuning difference alone now separates their hashes — where before the rules
    // binding they shared one. Tampering any determinant must break verification.
    for c in &v.matches {
        assert!(c.record.verify().is_ok(), "committed match {} does not self-verify", c.label);
    }
    let pick = |label: &str| &v.matches.iter().find(|c| c.label == label).unwrap().record;
    let (octant, fine, proj) = (pick("octant_hitscan"), pick("fine_hitscan"), pick("projectile"));
    assert_eq!(octant.replay.ticks, fine.replay.ticks, "octant and fine run the identical action stream");
    assert_ne!(octant.rules.aim_mode, fine.rules.aim_mode, "octant and fine differ only by aim_mode");
    assert_eq!(octant.rules.aim_mode, AimMode::Octant);
    assert_ne!(
        octant.result.replay_hash, fine.result.replay_hash,
        "the rules now bind the digest -> aim_mode alone changes the hash (the gap this closed)"
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

    // Config (v5): every committed match carries the arena config, and the digest
    // folds its DETERMINANTS — so the UE5 twin must fold bounds + max_ticks too, or its
    // match hashes diverge. An arena-bound or tick-cap tamper on the stored replay moves
    // the digest; a stored config inconsistent with the record fails verification.
    let octant_rec = octant.clone();
    assert_eq!(octant_rec.replay.config, octant_rec.config, "the committed replay carries its match config");
    let base_hash = octant_rec.replay.digest();
    let mut bound_tampered = octant_rec.clone();
    bound_tampered.replay.config.bounds.x += 1;
    assert_ne!(base_hash, bound_tampered.replay.digest(), "an arena bound binds the match digest (v5)");
    let mut cap_tampered = octant_rec.clone();
    cap_tampered.replay.config.max_ticks += 1;
    assert_ne!(base_hash, cap_tampered.replay.digest(), "the tick cap binds the match digest (v5)");
    let mut inconsistent = octant_rec.clone();
    inconsistent.replay.config.bounds.y += 1;
    assert!(inconsistent.verify().is_err(), "a stored config inconsistent with the record must not verify");

    // Ranked rating (v7): the zero-sum Elo reputation delta. Every case is zero-sum;
    // an even win is half of K; the favourite gains LESS for a win than a coin flip and
    // the underdog gains MORE for the mirror upset on the SAME rating gap; a draw moves
    // the favourite down; and a gap past the cap saturates the win to nothing. A twin
    // with a float curve, a different rounding, or a non-zero-sum split fails one.
    let rd = |label: &str| v.rating_deltas.iter().find(|c| c.label == label).unwrap();
    for c in &v.rating_deltas {
        assert_eq!(c.delta.a + c.delta.b, 0, "rating case {} is not zero-sum", c.label);
        assert_eq!(c.delta.b, -c.delta.a);
        assert_eq!(
            c.expected_a_bp,
            expected_score_bp(c.rating_a - c.rating_b),
            "rating case {} expected score must match the curve",
            c.label
        );
    }
    let even_win = rd("even_win");
    assert_eq!(even_win.outcome, MatchOutcome::WinA);
    assert_eq!(even_win.delta.a, even_win.k / 2, "an even win is half of K");
    assert_eq!(even_win.expected_a_bp, RATING_SCALE / 2, "an even match is a coin flip");
    assert_eq!(rd("even_draw").delta.a, 0, "an even draw moves nobody");
    let fav = rd("favoured_a_wins");
    let ups = rd("upset_b_wins");
    assert_eq!(ups.outcome, MatchOutcome::WinB);
    assert!(fav.delta.a < even_win.delta.a, "the favourite gains less for a win than a coin flip");
    assert!(ups.delta.b > even_win.delta.a, "the underdog gains more than a coin flip for the upset");
    assert_eq!(fav.expected_a_bp, ups.expected_a_bp, "the favoured win and the mirror upset share one rating gap");
    assert!(rd("favoured_a_draws").delta.a < 0, "a draw moves the favourite down");
    let cap = rd("beyond_cap_favoured_win");
    assert_eq!(cap.delta.a, 0, "a gap past the cap saturates the win to nothing");
    assert_eq!(cap.expected_a_bp, expected_score_bp(RATING_DIFF_CAP), "the expected score is clamped at the cap");

    // Multi-seat ranked rating (v8): an FFA / 3+ field settled as a sum of pairwise
    // games. Every field is zero-sum with each per-seat delta mapped to its OWN seat in
    // canonical order; the n=2 field reduces to the 1v1 curve bit-for-bit; equal ratings
    // make placement alone drive a symmetric spread; a tie for a place is a draw (the
    // favourite moves DOWN toward the underdog, not a mutual win); and a gap past the cap
    // saturates. A twin that normalizes by field size, mis-maps a seat, mishandles the
    // tie, or drops the cap fails one of these.
    let fd = |label: &str| v.field_deltas.iter().find(|c| c.label == label).unwrap();
    for c in &v.field_deltas {
        assert_eq!(
            c.deltas.iter().map(|d| d.delta as i64).sum::<i64>(),
            0,
            "field {} mints or burns reputation — not zero-sum",
            c.label
        );
        assert_eq!(c.deltas.len(), c.seats.len(), "field {} delta count must match the seat count", c.label);
        for (d, s) in c.deltas.iter().zip(&c.seats) {
            assert_eq!(d.seat, s.seat, "field {} delta is misaligned from its seat (a swap credits the wrong agent)", c.label);
        }
    }
    // n=2 reduction: routed through the multi-seat path, a two-seat field equals the 1v1
    // ranked curve bit-for-bit — seat 0 placed first (WinA), the delta mapping matching
    // the agentA/agentB convention.
    let two = fd("two_seat_matches_ranked_delta");
    let one_v_one = rating_delta(two.seats[0].rating, two.seats[1].rating, MatchOutcome::WinA, two.k);
    assert_eq!(two.deltas[0].delta, one_v_one.a, "the two-seat field's seat 0 must equal the 1v1 delta.a");
    assert_eq!(two.deltas[1].delta, one_v_one.b, "the two-seat field's seat 1 must equal the 1v1 delta.b");
    // Equal ratings: placement alone drives a fixed symmetric spread that sums to 0 and
    // mirrors to its own negation (seat i and seat n-1-i are exact opposites). A field-size
    // normalization or a placement-mapping bug breaks the exact values.
    let spread: Vec<i32> = fd("all_equal_field").deltas.iter().map(|d| d.delta).collect();
    assert_eq!(spread, vec![48, 24, 0, -24, -48], "equal ratings give a placement-only symmetric spread");
    let mirrored: Vec<i32> = spread.iter().rev().map(|d| -d).collect();
    assert_eq!(spread, mirrored, "the equal-rating spread is symmetric about the median seat");
    // A skill spread with a clean 1/2/3 finish: the first-place seat gains and the last
    // loses. The winner need NOT hold the max swing — a prohibitive favourite gains little
    // for placing first — so this pins direction, not magnitude order.
    let skill = fd("three_way_skill_spread");
    assert!(skill.deltas[0].delta > 0, "the first-place seat gains");
    assert!(skill.deltas[2].delta < 0, "the last-place seat loses");
    // A tie for 2nd between the favoured seat 1 and the underdog seat 2: their pairwise
    // game is a DRAW, so the favourite ends BELOW the underdog (a mutual-win twin would
    // invert this) while the clean winner/last still bound the field.
    let tie = fd("four_way_with_tie");
    assert_eq!(tie.seats[1].placement, tie.seats[2].placement, "seats 1 and 2 share the placement");
    assert!(tie.deltas[1].delta < tie.deltas[2].delta, "the favourite that only tied the underdog ends below it");
    assert!(tie.deltas[0].delta > 0 && tie.deltas[3].delta < 0, "the winner gains and the last-place loses");
    // Saturation: the 3000-rated seat 0 is upset into 2nd, its expected score clamped at
    // the cap, so it sheds ~a full K while both expected wins over the 200-rated seat 2
    // round to nothing — the ±cap is load-bearing in the multi-seat path too.
    let sat = fd("saturated_gap_upset");
    assert!(sat.deltas[0].delta < 0, "the upset favourite loses despite its rating");
    assert!(sat.deltas[0].delta.abs() >= sat.k - 2, "the capped upset sheds nearly a full K");
    assert_eq!(sat.deltas[2].delta, 0, "the expected wins over the weakest, past the cap, move nothing");

    // Knockback (v9): a damaging hit pops a grounded survivor UPWARD by exactly
    // knockback_velocity — the variable-fall source — through the one shared damage sink
    // (so every weapon mode launches), and never recoils the shooter. Gated on gravity>0
    // AND knockback>0, so a 2D or knockback-off match is byte-identical. A twin that drops
    // the impulse, signs it downward, recoils the shooter, or ignores the gate fails one.
    let kb = |label: &str| v.knockback.iter().find(|c| c.label == label).unwrap();
    let launched = kb("hitscan_launches_grounded_target");
    assert!(launched.damage > 0 && launched.target_alive, "the launching hit damages a survivor");
    assert_eq!(
        launched.target_z_vel, launched.knockback_velocity,
        "a grounded survivor is popped up by exactly knockback_velocity"
    );
    assert!(launched.target_z_vel > 0, "the impulse is UPWARD — a dropped or downward one fails here");
    assert_eq!(launched.shooter_z_vel, 0, "the shooter never recoils (the impulse hits the target)");
    // The shared sink: melee launches identically, so the rule is mode-agnostic.
    let melee = kb("melee_shares_the_knockback_sink");
    assert_eq!(melee.target_z_vel, launched.target_z_vel, "every weapon mode funnels through one knockback sink");
    assert!(melee.damage > 0 && melee.target_alive);
    // The gate: knockback off and gravity off each suppress the impulse while the hit
    // still lands — so the launch is driven by the rule, not the planar setup.
    let kb_off = kb("knockback_off_no_launch");
    assert_eq!(kb_off.target_z_vel, 0, "knockback_velocity 0 (the default) leaves the target grounded");
    assert!(kb_off.damage > 0, "knockback-off still lands the hit — only the launch is suppressed");
    let grav_off = kb("gravity_off_no_launch");
    assert_eq!(grav_off.target_z_vel, 0, "gravity off suppresses the impulse even with knockback set");
    assert!(grav_off.damage > 0, "gravity-off still lands the planar hit");

    // Directional knockback (v10): a damaging hit ALSO shoves a surviving target one
    // knockback_horizontal step AWAY from the shooter, through the same shared sink (every
    // weapon mode shoves), gated on knockback_horizontal>0 ALONE — no gravity — and never
    // moves the shooter. The vertical cases above all run knockback_horizontal 0, so their
    // target_pos is the unshoved baseline. A twin that drops the shove, signs it toward the
    // shooter, shoves the shooter, or demands gravity fails one.
    assert_eq!(launched.target_pos.x, 1500, "the vertical-only cases leave the target at its start x (kh 0)");
    let shoved = kb("hitscan_shoves_grounded_target");
    assert!(shoved.damage > 0 && shoved.target_alive, "the shoving hit damages a survivor");
    assert_eq!(shoved.gravity, 0, "the planar shove needs NO gravity — it fires in a 2D match");
    assert!(
        shoved.target_pos.x > launched.target_pos.x,
        "the survivor is shoved EAST, away from the shooter at the origin — a dropped or toward-shooter shove fails here"
    );
    assert_eq!(
        shoved.target_pos.x,
        launched.target_pos.x + shoved.knockback_horizontal,
        "shoved by exactly knockback_horizontal along the bearing"
    );
    assert_eq!(shoved.target_pos.y, 0, "a due-east bearing moves only x");
    assert_eq!(shoved.shooter_pos, launched.shooter_pos, "the shooter never moves (the shove hits the target)");
    // The shared sink shoves in every weapon mode: a melee cleave and a projectile land the
    // identical displacement (the projectile along its travel direction).
    assert_eq!(kb("melee_shares_the_shove_sink").target_pos, shoved.target_pos, "a melee cleave shoves identically");
    assert_eq!(kb("projectile_shoves_along_travel").target_pos, shoved.target_pos, "a projectile shoves along its travel, the same step");
    // The gate complement: the vertical-only cases (knockback_horizontal 0) are NOT shoved,
    // so the displacement is rule-driven, not the planar setup.
    assert_eq!(grav_off.target_pos.x, launched.target_pos.x, "knockback_horizontal 0 leaves the target unshoved");
    assert_eq!(kb_off.target_pos, launched.target_pos, "the no-launch case is un-shoved too (kh 0)");
    // Both axes compose on ONE hit: a pop AND a shove from the same damage — a twin that
    // fires only one is caught.
    let both = kb("pop_and_shove_compose");
    assert_eq!(both.target_z_vel, both.knockback_velocity, "the compose case still pops up");
    assert_eq!(
        both.target_pos.x,
        launched.target_pos.x + both.knockback_horizontal,
        "AND shoves east — both knockback axes fire on one hit"
    );
    assert!(both.target_z_vel > 0 && both.target_pos.x > launched.target_pos.x, "neither axis is dropped when both are armed");
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
