"""Unit tests for the State.layers reducer and USD composition.

A validator route-back re-enters an upstream specialist and the static edge chain
replays every downstream specialist before the validator runs again. The
merge_layers reducer de-dupes by (specialist, region_id) so those replays don't
append duplicate layers (which would make compose_world emit duplicate sublayers).

Run from the agents/ dir:
    .venv/bin/python -m pytest test_compose.py -v
"""

from common.compose import compose_world
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
