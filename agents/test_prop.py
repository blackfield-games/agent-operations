"""Prop placement-geometry emission: the count is deterministic and region-variable,
the triangles metric matches the placementCount actually written into the USD (no
phantom geometry), an unknown asset fails loudly instead of contributing 0, and the
geometry flows into the optimizer's budget as a shed-able non-floor layer.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_prop.py -v
"""

import re

import pytest

from common.types import RegionCoord, WorldBrief
from biome import biome
from optimization import optimization
from prop import prop
from terrain import terrain


def _placement_count_from_usd(text: str) -> int:
    m = re.search(r"placementCount\s*=\s*(\d+)", text)
    assert m, "emitted prop USD has no placementCount"
    return int(m.group(1))


def _authored_from_usd(text: str) -> float:
    m = re.search(r"authoredTriangles\s*=\s*([\d.]+)", text)
    assert m, "optimization USD has no authoredTriangles"
    return float(m.group(1))


@pytest.mark.asyncio
async def test_triangles_metric_matches_declared_placement_count(tmp_path, monkeypatch):
    # FM1 (phantom geometry): the metric must equal the placementCount actually written
    # into the PointInstancer times the PER-ASSET table value — never an independent
    # magic number, and scaled by which asset is placed (the table), not a flat constant.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    layer = await prop.run(brief, [])
    count = _placement_count_from_usd((tmp_path / "layers" / layer.path).read_text())
    assert count > 0  # a real, non-empty placement
    assert layer.metrics["triangles"] == float(count * prop.ASSET_TRIS[prop.PROP_ASSET])


@pytest.mark.asyncio
async def test_emission_is_deterministic(tmp_path, monkeypatch):
    # FM2 (determinism): the same brief yields byte-identical USD + identical metric
    # across runs, so a pipeline re-run and a validation-revision re-run reproduce it.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    a = await prop.run(brief, [])
    usd_a = (tmp_path / "layers" / a.path).read_text()
    b = await prop.run(brief, [])
    usd_b = (tmp_path / "layers" / b.path).read_text()
    assert usd_a == usd_b
    assert a.metrics == b.metrics


def test_count_is_stable_for_a_region_no_hash_salt():
    # determinism of the pure count fn — hashlib, not the per-process-salted builtin
    # hash(), so it survives a fresh interpreter.
    rid = RegionCoord(x=3, y=7).region_id
    assert prop._placement_count(rid) == prop._placement_count(rid)


def test_distinct_regions_yield_distinct_geometry():
    # FM2 (variability): a constant count would leave the optimizer a flat contributor.
    # Distinct regions differ.
    counts = {prop._placement_count(RegionCoord(x=x, y=0).region_id) for x in range(8)}
    assert len(counts) > 1


def test_unknown_asset_raises_not_silent_zero():
    # FM3 (asset-table integrity): an asset absent from ASSET_TRIS must fail loudly, not
    # default to 0 triangles — a silent 0 would hide its geometry from the budget.
    with pytest.raises(KeyError):
        prop._asset_tris("no_such_asset_99")
    # every shipped asset has a positive budget, and the default asset is in the table.
    assert all(prop._asset_tris(a) > 0 for a in prop.ASSET_TRIS)
    assert prop._asset_tris(prop.PROP_ASSET) == prop.ASSET_TRIS[prop.PROP_ASSET]


@pytest.mark.asyncio
async def test_prop_triangles_flow_into_the_optimizer_budget(tmp_path, monkeypatch):
    # FM4 (optimizer integration): prop triangles enter optimization.run's authored sum.
    # Adding the prop layer raises the optimizer's authoredTriangles by EXACTLY prop's
    # triangles — it is counted toward the budget, never silently dropped.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    terr = await terrain.run(brief, [])
    bio = await biome.run(brief, [terr])
    prp = await prop.run(brief, [terr, bio])
    opt_path = tmp_path / "layers" / f"optimization/{brief.region.region_id}.usda"

    await optimization.run(brief, [terr, bio])
    without = _authored_from_usd(opt_path.read_text())
    await optimization.run(brief, [terr, bio, prp])
    with_prop = _authored_from_usd(opt_path.read_text())

    assert with_prop - without == prp.metrics["triangles"]


@pytest.mark.asyncio
async def test_prop_is_a_sheddable_non_floor_layer(tmp_path, monkeypatch):
    # FM4 (shed-able non-floor): prop is NOT in GEOMETRY_FLOOR, so the optimizer can
    # LOD-collapse it when the world is over budget. Behind a heavy biome prop is never
    # realistically the bottleneck, so lower the budget to isolate prop as the sole
    # shed-able layer above the terrain floor and prove it gets collapsed.
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(optimization, "TRIANGLE_BUDGET", 500_000)
    assert "prop" not in optimization.GEOMETRY_FLOOR
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    terr = await terrain.run(brief, [])
    prp = await prop.run(brief, [terr])
    assert terr.metrics["triangles"] + prp.metrics["triangles"] > optimization.TRIANGLE_BUDGET
    opt = await optimization.run(brief, [terr, prp])
    usd = (tmp_path / "layers" / f"optimization/{brief.region.region_id}.usda").read_text()
    assert "forcedLodCollapse = true" in usd  # a real shed happened
    assert prp.path in usd  # and prop is the layer that was LOD-collapsed
    assert opt.metrics["over_budget"] == 0.0  # resolved by shedding prop, not terminal
