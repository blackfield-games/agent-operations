"""Unit tests for the State.layers reducer and USD composition.

A validator route-back re-enters an upstream specialist and the static edge chain
replays every downstream specialist before the validator runs again. The
merge_layers reducer de-dupes by (specialist, region_id) so those replays don't
append duplicate layers (which would make compose_world emit duplicate sublayers).

Run from the agents/ dir:
    .venv/bin/python -m pytest test_compose.py -v
"""

import pytest

from common.compose import compose_world, layer_summary, STRENGTH_ORDER
from common.types import LayerSpec
from runtime.supervisor import SPECIALISTS, merge_layers


def test_rerun_layer_replaces_stale_one():
    # biome already ran (v1), then re-ran after a validator route-back (v2).
    existing = [
        LayerSpec(specialist="terrain", region_id="r+0042_-0017_l0", path="terrain/a.usda", summary="v1", metrics={}),
        LayerSpec(specialist="biome", region_id="r+0042_-0017_l0", path="biome/a.usda", summary="v1", metrics={}),
    ]
    new = [
        LayerSpec(specialist="biome", region_id="r+0042_-0017_l0", path="biome/a.usda", summary="v2-rerun", metrics={}),
    ]

    merged = merge_layers(existing, new)

    # The re-run collapses onto the original biome layer → still 2 layers, not 3.
    assert len(merged) == 2
    # Latest wins: the fresh re-run summary replaces the stale one.
    biome_layer = next(layer for layer in merged if layer.specialist == "biome")
    assert biome_layer.summary == "v2-rerun"
    # No two entries share (specialist, region_id).
    keys = [(layer.specialist, layer.region_id) for layer in merged]
    assert len(keys) == len(set(keys))


def test_same_specialist_different_region_is_not_merged():
    # biome covering two distinct regions: both must survive.
    existing = [
        LayerSpec(specialist="biome", region_id="r+0042_-0017_l0", path="biome/r1.usda", summary="r1", metrics={}),
    ]
    new = [
        LayerSpec(specialist="biome", region_id="r+0042_-0018_l0", path="biome/r2.usda", summary="r2", metrics={}),
    ]

    merged = merge_layers(existing, new)

    assert len(merged) == 2
    keys = {(layer.specialist, layer.region_id) for layer in merged}
    assert keys == {("biome", "r+0042_-0017_l0"), ("biome", "r+0042_-0018_l0")}


def test_compose_world_emits_one_sublayer_per_layer(tmp_path):
    # End-to-end: a de-duped merge feeds compose_world with no duplicate paths.
    existing = [
        LayerSpec(specialist="terrain", region_id="r+0042_-0017_l0", path="terrain/a.usda", summary="v1", metrics={}),
        LayerSpec(specialist="biome", region_id="r+0042_-0017_l0", path="biome/a.usda", summary="v1", metrics={}),
    ]
    new = [
        LayerSpec(specialist="biome", region_id="r+0042_-0017_l0", path="biome/a.usda", summary="v2-rerun", metrics={}),
    ]
    merged = merge_layers(existing, new)

    root = compose_world(merged, tmp_path)
    text = root.read_text()

    assert text.count("@./terrain/a.usda@") == 1
    assert text.count("@./biome/a.usda@") == 1


def test_strength_order_is_exactly_locked():
    assert STRENGTH_ORDER == [
        "validator",
        "optimization",
        "lighting",
        "prop",
        "npc",
        "biome",
        "terrain",
        "director",
    ]
    assert STRENGTH_ORDER[0] == "validator", "validator must be strongest (last word)"
    assert STRENGTH_ORDER[-1] == "director", "director must be weakest (base intent)"
    assert len(STRENGTH_ORDER) == len(set(STRENGTH_ORDER))


def test_every_routed_specialist_has_a_strength_order_slot():
    """Every specialist the supervisor routes emits a composed layer, so it MUST
    have a STRENGTH_ORDER slot: without one, compose_world rejects its layer (the
    unknown-specialist guard) and the world can't be composed — or, on a run where
    that specialist happens to emit nothing, ships silently missing it. This lifts
    compose_world's per-layer runtime guard to a set invariant caught in CI when
    SPECIALISTS and STRENGTH_ORDER drift, not on a production run.
    """
    unranked = set(SPECIALISTS) - set(STRENGTH_ORDER)
    assert unranked == set(), (
        f"specialist(s) {sorted(unranked)} are routed by supervisor.SPECIALISTS "
        f"but have no slot in common.compose.STRENGTH_ORDER — add them to "
        f"STRENGTH_ORDER (do NOT remove them from SPECIALISTS to silence this)"
    )


def test_strength_order_extras_are_exactly_the_validator():
    """STRENGTH_ORDER is the routed specialists PLUS exactly ``validator``. The
    validator is the strongest slot (its "last word" on any prim), but its node
    emits a verdict, not a LayerSpec, so it is intentionally absent from
    SPECIALISTS. Pinning the exact difference catches a stale STRENGTH_ORDER entry
    that is neither a routed specialist nor the validator (a rank for a removed
    specialist), which the subset check above cannot see.
    """
    extras = set(STRENGTH_ORDER) - set(SPECIALISTS)
    assert extras == {"validator"}, (
        f"STRENGTH_ORDER ranks non-specialist entr(ies) {sorted(extras)}; only "
        f"'validator' (router-only, emits no composed layer) may rank without "
        f"being in supervisor.SPECIALISTS"
    )


def test_coverage_invariant_discriminates_an_unranked_specialist():
    """The subset invariant is not vacuous: a specialist added to the routing set
    without a STRENGTH_ORDER slot is reported as unranked. Without this, a future
    refactor that weakened the check to always-pass would go unnoticed.
    """
    simulated = {*SPECIALISTS, "weather"}
    assert simulated - set(STRENGTH_ORDER) == {"weather"}
    # The real sets remain clean — the failing case above is purely simulated.
    assert set(SPECIALISTS) - set(STRENGTH_ORDER) == set()


def test_compose_orders_sublayers_strongest_first_regardless_of_input_order(tmp_path):
    # Build one LayerSpec per specialist, then pass them in REVERSED (weakest-first) order.
    # compose_world must fix the order itself — the caller cannot be relied upon.
    region = "r+0042_-0017_l0"
    layers = [
        LayerSpec(specialist=s, region_id=region, path=f"{s}/{s}.usda", summary=s, metrics={})
        for s in reversed(STRENGTH_ORDER)
    ]
    root = compose_world(layers, tmp_path)
    text = root.read_text()

    positions = [text.index(f"@./{s}/{s}.usda@") for s in STRENGTH_ORDER]
    assert positions == sorted(positions)
    assert all(positions[i] < positions[i + 1] for i in range(len(positions) - 1))


def test_stronger_specialist_wins_on_conflict(tmp_path):
    """Earlier-listed subLayers are stronger in USD, so a stronger specialist's
    opinion overrides a weaker one's on the same prim. For each (strong, weak)
    pair, compose_world must list the stronger specialist's layer first regardless
    of the order the layers are passed in.
    """
    pairs = [
        ("validator", "director"),
        ("optimization", "npc"),
        ("lighting", "terrain"),
        ("prop", "biome"),
    ]
    region = "r+0042_-0017_l0"
    for strong, weak in pairs:
        out_dir = tmp_path / f"{strong}_{weak}"
        layers = [
            LayerSpec(specialist=weak, region_id=region, path=f"{weak}/x.usda", summary=weak, metrics={}),
            LayerSpec(specialist=strong, region_id=region, path=f"{strong}/x.usda", summary=strong, metrics={}),
        ]
        root = compose_world(layers, out_dir)
        text = root.read_text()
        assert text.index(f"@./{strong}/x.usda@") < text.index(f"@./{weak}/x.usda@")


def test_unknown_specialist_raises_value_error(tmp_path):
    """A layer with an unrecognised specialist must raise ValueError immediately,
    not be silently dropped from the composed world."""
    layers = [
        LayerSpec(specialist="terrain", region_id="r+0000_+0000_l0", path="terrain/a.usda", summary="ok", metrics={}),
        LayerSpec(specialist="biome", region_id="r+0000_+0000_l0", path="biome/a.usda", summary="ok", metrics={}),
    ]
    # Construct a LayerSpec with an invalid specialist by bypassing Pydantic validation.
    bad = layers[0].model_copy(update={"specialist": "wizard"})
    with pytest.raises(ValueError, match="wizard"):
        compose_world([*layers, bad], tmp_path)


def test_unknown_specialist_does_not_create_output_file(tmp_path):
    """Fail-fast: the world.usda file must NOT be created when the input is invalid."""
    bad = LayerSpec(specialist="terrain", region_id="r+0000_+0000_l0", path="terrain/a.usda", summary="ok", metrics={})
    bad = bad.model_copy(update={"specialist": "gremlin"})
    with pytest.raises(ValueError):
        compose_world([bad], tmp_path)
    assert not (tmp_path / "world.usda").exists()


def test_layer_summary_counts_per_specialist():
    layers = [
        LayerSpec(specialist="terrain", region_id="r+0042_-0017_l0", path="terrain/a.usda", summary="a", metrics={}),
        LayerSpec(specialist="biome", region_id="r+0042_-0017_l0", path="biome/a.usda", summary="b", metrics={}),
        LayerSpec(specialist="biome", region_id="r+0042_-0018_l0", path="biome/b.usda", summary="c", metrics={}),
    ]
    # Two biome layers (distinct regions), one terrain.
    assert layer_summary(layers) == {"biome": 2, "terrain": 1}


def test_layer_summary_orders_by_strength_strongest_first():
    region = "r+0042_-0017_l0"
    # One layer per specialist, passed weakest-first; the summary must come back
    # strongest-first (STRENGTH_ORDER), regardless of input order.
    layers = [
        LayerSpec(specialist=s, region_id=region, path=f"{s}/{s}.usda", summary=s, metrics={})
        for s in reversed(STRENGTH_ORDER)
    ]
    assert list(layer_summary(layers).keys()) == STRENGTH_ORDER
    assert all(count == 1 for count in layer_summary(layers).values())


def test_layer_summary_empty_is_empty_dict():
    assert layer_summary([]) == {}


def test_layer_summary_appends_unknown_specialists_sorted_after_known():
    """Unlike compose_world (which REJECTS unknown specialists), layer_summary TOLERATES
    them — a count is harmless. Known specialists come first in STRENGTH_ORDER, then any
    unknown specialist is appended in sorted() order, never interleaved and never by input
    order. Forge the unknowns via the Pydantic bypass (specialist is a Literal of the 8).
    """
    region = "r+0042_-0017_l0"
    wizard1 = LayerSpec(specialist="terrain", region_id=region, path="x/w1.usda", summary="w1", metrics={}).model_copy(update={"specialist": "wizard"})
    wizard2 = LayerSpec(specialist="biome", region_id=region, path="x/w2.usda", summary="w2", metrics={}).model_copy(update={"specialist": "wizard"})
    alchemist = LayerSpec(specialist="terrain", region_id=region, path="x/a.usda", summary="a", metrics={}).model_copy(update={"specialist": "alchemist"})
    # First-occurrence input order is wizard, terrain, biome, alchemist — deliberately NOT
    # the expected output order, so the assertion proves the ordering logic, not insertion order.
    layers = [
        wizard1,
        LayerSpec(specialist="terrain", region_id=region, path="terrain/t.usda", summary="t", metrics={}),
        LayerSpec(specialist="biome", region_id=region, path="biome/b1.usda", summary="b1", metrics={}),
        alchemist,
        LayerSpec(specialist="biome", region_id="r+0042_-0018_l0", path="biome/b2.usda", summary="b2", metrics={}),
        wizard2,
    ]
    result = layer_summary(layers)
    # Known first in STRENGTH_ORDER (biome before terrain), then unknowns sorted (alchemist before wizard).
    assert list(result.keys()) == ["biome", "terrain", "alchemist", "wizard"]
    assert result == {"biome": 2, "terrain": 1, "alchemist": 1, "wizard": 2}


def test_compose_world_happy_path_all_known_specialists(tmp_path):
    """Regression guard: all valid specialists compose without error and produce world.usda."""
    region = "r+0042_-0017_l0"
    layers = [
        LayerSpec(specialist=s, region_id=region, path=f"{s}/{s}.usda", summary=s, metrics={})
        for s in STRENGTH_ORDER
    ]
    root = compose_world(layers, tmp_path)
    assert root.exists()
    assert root.name == "world.usda"


def test_compose_rejects_empty_layer_path_naming_specialist(tmp_path):
    """An empty path would emit `@./@` and reference nothing; reject it, fail-fast
    (no file written), and name the offending specialist."""
    layers = [
        LayerSpec(specialist="terrain", region_id="r+0000_+0000_l0", path="", summary="x", metrics={}),
    ]
    with pytest.raises(ValueError, match="terrain"):
        compose_world(layers, tmp_path)
    assert not (tmp_path / "world.usda").exists()


def test_compose_rejects_at_delimiter_in_path_naming_specialist(tmp_path):
    """An `@` closes the USDA asset reference early and injects arbitrary syntax —
    the actual world.usda corruption the guard exists to stop (FM2)."""
    layers = [
        LayerSpec(specialist="biome", region_id="r+0000_+0000_l0", path="biome/a@evil.usda", summary="x", metrics={}),
    ]
    with pytest.raises(ValueError, match="biome"):
        compose_world(layers, tmp_path)
    assert not (tmp_path / "world.usda").exists()


def test_compose_rejects_newline_in_path_naming_specialist(tmp_path):
    """A newline breaks out of the `subLayers = [ ]` block (FM2)."""
    layers = [
        LayerSpec(specialist="prop", region_id="r+0000_+0000_l0", path="prop/a\n].usda", summary="x", metrics={}),
    ]
    with pytest.raises(ValueError, match="prop"):
        compose_world(layers, tmp_path)
    assert not (tmp_path / "world.usda").exists()


@pytest.mark.parametrize(
    "bad_path",
    [
        "biome/a\rb.usda",  # carriage return
        "biome/a\tb.usda",  # tab
        "biome/ a.usda",  # embedded space
        "   ",  # whitespace-only
        "@biome/a.usda",  # leading @ delimiter
        "biome/a.usda@",  # trailing @ delimiter
    ],
)
def test_compose_rejects_other_usda_breaking_paths(tmp_path, bad_path):
    """Belt-and-suspenders for the allowlist: every non-allowed breaker (CR, tab,
    space, whitespace-only, leading/trailing @) is rejected fail-fast, not just
    the mid-string @ and newline cases above. A future blocklist/`\\s` rewrite that
    regressed any of these would be caught here."""
    layers = [
        LayerSpec(specialist="biome", region_id="r+0000_+0000_l0", path=bad_path, summary="x", metrics={}),
    ]
    with pytest.raises(ValueError, match="biome"):
        compose_world(layers, tmp_path)
    assert not (tmp_path / "world.usda").exists()


def test_compose_accepts_legitimate_relative_paths(tmp_path):
    """FM1 discriminator: the allowlist must NOT reject the real convention —
    nested subdirs, the `+`/`-`/`_` region-id chars, and dots all compose
    unchanged and in strength order. A too-aggressive guard would raise here."""
    region = "r+0042_-0017_l0"
    layers = [
        LayerSpec(specialist="terrain", region_id=region, path="terrain/sub/r+0042_-0017_l0.usda", summary="t", metrics={}),
        LayerSpec(specialist="biome", region_id=region, path="biome/a.b.usda", summary="b", metrics={}),
    ]
    root = compose_world(layers, tmp_path)
    text = root.read_text()
    assert "@./terrain/sub/r+0042_-0017_l0.usda@" in text
    assert "@./biome/a.b.usda@" in text
    # Stronger specialist (biome) is listed before the weaker (terrain).
    assert text.index("@./biome/a.b.usda@") < text.index("@./terrain/sub/r+0042_-0017_l0.usda@")


def test_compose_empty_set_warns_and_writes_parseable_empty_world(tmp_path, caplog):
    """An all-None / empty specialist set must DEGRADE (warn), not raise (FM3),
    and write a still-parseable, explicitly-empty world.usda."""
    import logging

    with caplog.at_level(logging.WARNING):
        root = compose_world([], tmp_path)

    assert any("no layers to compose" in r.message for r in caplog.records), \
        f"expected an empty-set warning, got {[r.message for r in caplog.records]}"
    assert root.exists(), "an empty set must still produce world.usda, not crash"
    text = root.read_text()
    assert text.startswith("#usda 1.0")
    assert 'defaultPrim = "World"' in text
    assert "subLayers = []" in text, f"empty set must write an explicitly-empty array: {text!r}"
