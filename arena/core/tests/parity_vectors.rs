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
    PerceptionVerdict, WeaponMode, DASH_DISTANCE, RATING_DIFF_CAP, RATING_SCALE,
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
    assert_eq!(v.domain, "blackfield/arena/parity-vectors/v17");
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

    // Pawn-body occupancy (v11): under a positive pawn_radius a move whose swept path
    // would enter another alive pawn's body is refused — pawns are obstacles in the
    // shared slide path. OFF (radius 0) they overlap (the byte-identical default); ON,
    // the mover hard-stops one step short (never overlapping), a fast step cannot tunnel
    // a body any more than a thin wall, a step not toward the body is free (directional),
    // and a set radius with the body off-path never freezes the mover. A twin that lets
    // pawns overlap, that point-tests the destination (and so tunnels), or that
    // blanket-freezes a pawn near a body fails one of these.
    let pc = |label: &str| v.pawn_collisions.iter().find(|c| c.label == label).unwrap();
    let into = pc("into_pawn_blocked");
    assert!(into.blocked && into.end == into.start, "a step into a pawn body is refused — the mover holds");
    assert!(into.pawn_radius > 0, "the into-body case has occupancy ON");
    // The blocked step's destination sits EXACTLY at the contact distance, so the
    // inclusive boundary (contact counts as a collision) is the load-bearing pin.
    let into_dest = Vec2 { x: into.start.x + into.max_speed, y: into.start.y };
    let into_gap2 = (into_dest.x as i64 - into.obstacle.x as i64).pow(2)
        + (into_dest.y as i64 - into.obstacle.y as i64).pow(2);
    assert_eq!(into_gap2, (into.pawn_radius as i64).pow(2), "the refused step ends exactly at the contact radius (inclusive)");
    let short = pc("short_of_pawn_allowed");
    assert!(!short.blocked && short.end != short.start, "a step that stops short of contact proceeds");
    assert!(!within(short.end, short.obstacle, short.pawn_radius), "the allowed step ends OUTSIDE the body — no overlap");
    // into/short share everything but the start: the contact boundary is exactly one step.
    assert_eq!((into.obstacle, into.pawn_radius, into.move_dir), (short.obstacle, short.pawn_radius, short.move_dir),
        "into and short differ only by the start — the boundary is one step wide");
    let ortho = pc("clear_orthogonal_allowed");
    assert!(!ortho.blocked && ortho.end != ortho.start, "a step NOT toward the body is free (occupancy is directional)");
    let off = pc("occupancy_off_overlaps");
    assert_eq!(off.pawn_radius, 0, "the overlap case has occupancy OFF (the default)");
    assert!(!off.blocked && off.end == off.obstacle, "with occupancy off the mover ends ON the body's cell (overlap)");
    let fast = pc("fast_no_tunnel_through_pawn");
    assert!(fast.blocked && fast.end == fast.start, "a fast step is stopped by a body it would sweep through");
    // The would-be destination overshoots the body (NOT within the radius), so an
    // endpoint test reports a clean miss and tunnels — the swept test is load-bearing.
    let fast_dest = Vec2 { x: fast.start.x + fast.max_speed, y: fast.start.y };
    assert!(!within(fast_dest, fast.obstacle, fast.pawn_radius), "the would-be destination overshoots the body (a point test tunnels)");
    let far = pc("radius_set_far_obstacle_free");
    assert!(!far.blocked && far.end != far.start && far.pawn_radius > 0, "a set radius with the body off-path never freezes the mover");

    // Fall damage (domain v12): a hard landing hurts, a normal jump does not, the
    // threshold (not the impact alone) is the gate, and a lethal landing downs the pawn.
    let fd = |label: &str| v.fall_damage.iter().find(|c| c.label == label).unwrap();
    let safe = fd("self_jump_under_threshold_safe");
    let hurts = fd("boosted_landing_over_threshold_hurts");
    // The NON-DEGENERATE pin: safe and hurts share gravity, threshold, AND fall_damage —
    // only the launch (fall height) differs, so the threshold alone decides the outcome.
    assert_eq!(
        (safe.gravity, safe.fall_damage_threshold, safe.fall_damage),
        (hurts.gravity, hurts.fall_damage_threshold, hurts.fall_damage),
        "safe and hurts differ only by launch — the threshold alone decides"
    );
    assert!(safe.fall_damage > 0, "fall damage is ON for the safe case (spared by the threshold, not by being off)");
    assert!(
        safe.impact_speed <= safe.fall_damage_threshold && safe.damage == 0 && safe.landed_alive,
        "a fixed-impulse self-jump lands at/below the threshold and is unharmed"
    );
    assert!(
        hurts.impact_speed > hurts.fall_damage_threshold && hurts.impact_speed > safe.impact_speed,
        "the boosted launch lands harder than the self-jump, above the threshold"
    );
    assert_eq!(hurts.damage, hurts.fall_damage, "a hard landing deals exactly fall_damage");
    assert!(hurts.landed_alive, "a non-lethal hard landing hurts but does not down the pawn");
    // Off by default: the hardest landing harms nothing when fall_damage is 0.
    let off = fd("fall_damage_off_no_harm");
    assert_eq!(off.fall_damage, 0, "the off case has the feature disabled");
    assert!(
        off.impact_speed > 0 && off.damage == 0 && off.landed_alive,
        "with fall_damage 0 even a hard landing deals nothing (byte-identity)"
    );
    // The threshold is the gate, not the impact: the SAME soft self-jump that was safe at
    // threshold 3000 takes damage once the threshold drops to 0.
    let thr0 = fd("threshold_zero_soft_landing_hurts");
    assert_eq!(thr0.launch_z_vel, safe.launch_z_vel, "the threshold-0 case reuses the safe case's soft self-jump");
    assert_eq!(thr0.fall_damage_threshold, 0, "its threshold is 0");
    assert!(
        thr0.damage == thr0.fall_damage && thr0.impact_speed > 0,
        "the once-safe landing now takes damage — the threshold is what spared it"
    );
    // Lethal: a landing dealing more than the remaining HP downs the pawn, clamped through
    // the shared sink (overkill is not negative health).
    let lethal = fd("lethal_landing_downs_pawn");
    assert!(!lethal.landed_alive, "a lethal landing downs the pawn");
    assert!(
        lethal.fall_damage > lethal.start_health && lethal.damage == lethal.start_health,
        "overkill is clamped to the pawn's HP through the shared sink"
    );

    // Fall-kill attribution (domain v13): a knockback-into-a-lethal-fall credits the
    // LAUNCHER's score like a weapon down (the effective fall damage), with the same
    // self/friendly exclusion — a team launch downs the victim all the same but credits no
    // one. The enemy and friendly cases share every tuning and differ ONLY by team, so the
    // credit divergence is the team rule, not the setup.
    let fk = |label: &str| v.fall_kills.iter().find(|c| c.label == label).unwrap();
    let enemy_fk = fk("enemy_launch_credits_launcher");
    let friendly_fk = fk("friendly_launch_credits_no_one");
    // Both lethal falls down the victim and both launches lifted it — only the CREDIT
    // differs by team (a friendly hit launches and kills just the same).
    assert!(!enemy_fk.victim_alive && !friendly_fk.victim_alive, "a lethal knockback-fall downs the victim regardless of team");
    assert!(enemy_fk.victim_launch_z_vel > 0 && friendly_fk.victim_launch_z_vel > 0, "the knockback lifted the victim in both cases");
    assert!(enemy_fk.impact_speed > enemy_fk.fall_damage_threshold, "the boosted fall lands above the threshold (a hard, lethal landing)");
    assert!(enemy_fk.fall_damage_dealt > 0, "the lethal fall removed real HP");
    // The enemy launch credits the launcher EXACTLY the fall damage, on top of its hit.
    assert_eq!(enemy_fk.credited_seat, Some(0), "an enemy launch credits the launcher");
    assert_eq!(
        enemy_fk.launcher_score_after_fall - enemy_fk.launcher_score_before_fall,
        enemy_fk.fall_damage_dealt as i32,
        "the lethal fall adds exactly its damage to the launcher's score"
    );
    // The friendly launch downs the victim identically but credits NO ONE — and the friendly
    // hit itself scored nothing, so the launcher's score never moved at all.
    assert_eq!(friendly_fk.credited_seat, None, "a same-team launch credits no one");
    assert_eq!(friendly_fk.launcher_score_after_fall, friendly_fk.launcher_score_before_fall, "the friendly fall adds no score");
    assert_eq!(friendly_fk.launcher_score_before_fall, 0, "the friendly launching hit scored nothing either");
    // Non-degenerate: enemy and friendly differ ONLY by team — same gravity, knockback, fall
    // damage, and threshold — so the credit divergence is the team rule alone.
    assert_eq!(
        (enemy_fk.gravity, enemy_fk.knockback_velocity, enemy_fk.fall_damage, enemy_fk.fall_damage_threshold),
        (friendly_fk.gravity, friendly_fk.knockback_velocity, friendly_fk.fall_damage, friendly_fk.fall_damage_threshold),
        "enemy and friendly share all tuning"
    );
    assert!(!enemy_fk.friendly && friendly_fk.friendly, "the two cases differ by the team flag");

    // z-coupled occupancy (domain v14): under a positive pawn_height a body refusal also
    // requires the two pawns' feet within the band (|dz| <= pawn_height), so a pawn that
    // jumped high enough vaults the body. Every case shares ONE planar geometry (the discs
    // always overlap), so each verdict isolates the z band — not the XY collision.
    let zo = |label: &str| v.z_occupancy.iter().find(|c| c.label == label).unwrap();
    let within = zo("within_band_blocked");
    let high = zo("high_jump_clears");
    // The block/clear pair shares start, obstacle, radius, max_speed, AND pawn_height —
    // only mover_z differs, and that alone flips the verdict: z, nothing else, decides.
    assert_eq!(
        (within.start, within.obstacle, within.pawn_radius, within.pawn_height, within.max_speed),
        (high.start, high.obstacle, high.pawn_radius, high.pawn_height, high.max_speed),
        "the block/clear pair isolates z (identical XY geometry + band)"
    );
    assert_ne!(within.mover_z, high.mover_z, "the pair differs only in the mover's elevation");
    assert!(within.blocked && within.end == within.start, "a pawn within the band is held by the body");
    assert!(!high.blocked && high.end != high.start, "a pawn jumped above the band vaults and moves");
    // The band edge is INCLUSIVE: |dz| == pawn_height blocks, |dz| == pawn_height + 1 clears.
    let edge = zo("band_edge_inclusive_blocks");
    let past = zo("just_past_band_clears");
    assert_eq!((edge.mover_z - edge.obstacle_z).abs(), edge.pawn_height, "the edge case sits EXACTLY at the band");
    assert_eq!((past.mover_z - past.obstacle_z).abs(), past.pawn_height + 1, "the past case sits one unit beyond the band");
    assert!(edge.blocked, "|dz| == pawn_height is INCLUSIVE — still blocked");
    assert!(!past.blocked, "one unit past the band clears");
    // Symmetry: swapping which pawn is elevated (mover-high vs obstacle-high) preserves the
    // verdict, since |dz| is symmetric — A blocks B iff B blocks A.
    let swap_block = zo("swap_obstacle_high_blocks");
    let swap_clear = zo("swap_obstacle_high_clears");
    assert_eq!(
        (swap_block.mover_z - swap_block.obstacle_z).abs(),
        (within.mover_z - within.obstacle_z).abs(),
        "the swap mirrors the block case's |dz|"
    );
    assert_eq!(
        (swap_clear.mover_z - swap_clear.obstacle_z).abs(),
        (high.mover_z - high.obstacle_z).abs(),
        "the swap mirrors the clear case's |dz|"
    );
    assert_eq!(swap_block.blocked, within.blocked, "role-swap preserves the BLOCK verdict");
    assert_eq!(swap_clear.blocked, high.blocked, "role-swap preserves the CLEAR verdict");
    // The swapped cases elevate the OTHER pawn (mover grounded, obstacle high) — proving the
    // predicate reads both seats' z, not just the mover's.
    assert!(swap_block.mover_z == 0 && swap_block.obstacle_z > 0, "the block-swap elevates the obstacle, not the mover");
    assert!(swap_clear.mover_z == 0 && swap_clear.obstacle_z > 0, "the clear-swap elevates the obstacle, not the mover");

    // Wall-slide (v15): the flag flips a refused diagonal from a dead-stop to a slide along
    // the unblocked axis; an inside corner holds even on; and when BOTH axis retries are
    // clear the fixed X-before-Y order decides. A twin that dead-stops under the flag,
    // slides the wrong axis, lets a corner through, or resolves Y-first fails one of these.
    let ws = |label: &str| v.wall_slides.iter().find(|c| c.label == label).unwrap();
    let off = ws("diagonal_into_wall_dead_stops");
    let on = ws("diagonal_into_wall_slides_along_y");
    // The load-bearing pair: same start, intent, AND wall — only wall_slide differs, so the
    // flag alone flips the outcome (off holds at the origin, on slides north along the wall).
    assert_eq!((off.start, off.move_dir), (on.start, on.move_dir), "the off/on pair shares start + intent");
    assert_eq!(off.blockers, on.blockers, "the off/on pair shares the wall geometry — only wall_slide differs");
    assert!(!off.wall_slide && off.blocked && off.end == off.start, "flag off: a diagonal into the wall dead-stops");
    assert!(on.wall_slide && !on.blocked && on.end != on.start, "flag on: the refused diagonal slides instead of holding");
    assert!(on.end.x == off.start.x && on.end.y != off.start.y, "the slide runs along Y — the wall refused the X component");
    // An inside corner (both axis retries refused) holds even with the flag on — the slide
    // is not an unconditional "move somewhere".
    let corner = ws("inside_corner_still_holds");
    assert!(corner.wall_slide && corner.blocked && corner.end == corner.start, "an inside corner holds even with wall_slide on");
    // X-before-Y: a nub squarely on the diagonal refuses the full step while neither axis-
    // aligned path touches it, so the resolution ORDER alone picks the outcome — X, not Y.
    let xfirst = ws("both_retries_clear_x_first");
    assert!(xfirst.wall_slide && !xfirst.blocked, "the X-first case slides");
    assert!(
        xfirst.end.x != xfirst.start.x && xfirst.end.y == xfirst.start.y,
        "both retries clear -> X-first wins (slides on X, not Y)"
    );

    // Dash (v16): a ready ability press bursts the pawn an extra DASH_DISTANCE along move_dir
    // AFTER the walk and arms the cooldown; the SAME press on cooldown is a plain walk (no
    // burst); a zero-direction press does nothing and keeps the dash ready; and a dash into a
    // wall fires (arming the cooldown) yet holds at the post-walk position — the burst routes
    // through slide(), so it cannot tunnel. A twin that dashes the wrong distance, ignores the
    // cooldown gate, bursts a directionless press, or tunnels the burst fails one of these.
    let ds = |label: &str| v.dashes.iter().find(|c| c.label == label).unwrap();
    let ready = ds("ready_dash_bursts_walk_plus_distance");
    let oncd = ds("dash_on_cooldown_is_a_plain_walk");
    // The load-bearing pair: same start, intent, AND (empty) blockers — only the pawn's own
    // cooldown differs (ready 0 vs mid-cooldown), so the cooldown gate alone decides burst-vs-walk.
    assert_eq!((ready.start, ready.move_dir), (oncd.start, oncd.move_dir), "the ready/on-cooldown pair shares start + intent");
    assert_eq!(ready.blockers, oncd.blockers, "the ready/on-cooldown pair shares the geometry — only the cooldown differs");
    // Ready: the burst travels exactly the walk (max_speed) PLUS DASH_DISTANCE and fires, arming
    // the cooldown up from ready 0. A plain walk would move only max_speed — the burst is unmistakable.
    assert!(ready.dashed, "a ready ability press dashes");
    assert_eq!(ready.end.x, ready.start.x + ready.max_speed + DASH_DISTANCE, "the ready dash bursts walk + DASH_DISTANCE along move_dir");
    assert_eq!(ready.end.y, ready.start.y, "a due-east dash moves only x");
    assert!(ready.dash_cooldown_after > ready.dash_cooldown_before, "the fired dash armed its cooldown (up from ready 0)");
    // On cooldown: only the walk applies (no burst), and the cooldown is NOT re-armed — it just
    // ticked down by one. A twin that always bursts, or that re-arms a refused dash, fails here.
    assert!(!oncd.dashed, "a dash on cooldown does not fire");
    assert_eq!(oncd.end.x, oncd.start.x + oncd.max_speed, "the on-cooldown press is a plain walk — no burst");
    assert!(oncd.end.x != ready.end.x, "the cooldown gate alone separates the burst from the walk");
    assert_eq!(oncd.dash_cooldown_after, oncd.dash_cooldown_before - 1, "a cooldown-refused dash only ticks the clock down, never re-arms it");
    // Zero direction: a press with no direction is a no-op that does NOT spend the cooldown — the
    // pawn holds and the dash stays ready (after == before).
    let zero = ds("zero_direction_press_does_nothing");
    assert!(!zero.dashed && zero.end == zero.start, "a zero-direction press dashes nothing and moves nothing");
    assert_eq!(zero.dash_cooldown_after, zero.dash_cooldown_before, "a directionless press leaves the cooldown unspent (still ready)");
    // No tunnel: a wall across the burst path refuses it, so the dash FIRES (arming the cooldown
    // exactly like the clean ready dash — consume-on-trigger) yet the pawn holds at its post-walk
    // position, short of the wall — it does NOT punch through to the ready burst endpoint.
    let wall = ds("dash_into_wall_holds_no_tunnel");
    assert!(wall.dashed, "the dash into a wall still fires (consume-on-trigger)");
    assert_eq!(wall.dash_cooldown_after, ready.dash_cooldown_after, "a wall-refused dash arms the cooldown exactly like a clean fire");
    assert_eq!(wall.end.x, wall.start.x + wall.max_speed, "the burst is refused — only the walk applied, the pawn holds short of the wall");
    assert!(wall.end.x != ready.end.x, "the wall-refused dash did NOT tunnel to the ready burst endpoint");
    assert!(!wall.blockers.is_empty(), "the no-tunnel case carries its wall");
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
