"""Unit tests for the State.layers reducer and USD composition.

A validator route-back re-enters an upstream specialist and the static edge chain
replays every downstream specialist before the validator runs again. The
merge_layers reducer de-dupes by (specialist, region_id) so those replays don't
append duplicate layers (which would make compose_world emit duplicate sublayers).

Run from the agents/ dir:
    .venv/bin/python -m pytest test_compose.py -v
"""

from common.compose import compose_world, STRENGTH_ORDER
from common.types import LayerSpec
from runtime.supervisor import merge_layers


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
