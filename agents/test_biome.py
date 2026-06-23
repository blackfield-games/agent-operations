"""Biome scatter-geometry emission: the count is deterministic and region-variable,
the triangles metric matches the instanceCount actually written into the USD (no
phantom geometry), and the geometry flows into the optimizer so a dense region can
exceed the triangle budget — the contributor terrain alone never provided.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_biome.py -v
"""

import re

import pytest

from biome import biome
from common.types import RegionCoord, WorldBrief
from optimization import optimization
from terrain import terrain
from validator import validator


def _instance_count_from_usd(text: str) -> int:
    m = re.search(r"instanceCount\s*=\s*(\d+)", text)
    assert m, "emitted biome USD has no instanceCount"
    return int(m.group(1))


@pytest.mark.asyncio
async def test_triangles_metric_matches_declared_instance_count(tmp_path, monkeypatch):
    # FM1 (phantom geometry): the metric must equal the instanceCount actually
    # written into the PointInstancer, never an independent magic number.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    layer = await biome.run(brief, [])
    count = _instance_count_from_usd((tmp_path / "layers" / layer.path).read_text())
    assert count > 0  # a real, non-empty scatter
    assert layer.metrics["triangles"] == float(count * biome.TRIS_PER_INSTANCE)


@pytest.mark.asyncio
async def test_emission_is_deterministic(tmp_path, monkeypatch):
    # FM2 (determinism): the same brief yields byte-identical USD + identical metric
    # across runs, so a pipeline re-run and a validation-revision re-run reproduce it.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    a = await biome.run(brief, [])
    usd_a = (tmp_path / "layers" / a.path).read_text()
    b = await biome.run(brief, [])
    usd_b = (tmp_path / "layers" / b.path).read_text()
    assert usd_a == usd_b
    assert a.metrics == b.metrics


def test_count_is_stable_for_a_region_no_hash_salt():
    # determinism of the pure count fn — hashlib, not the per-process-salted builtin
    # hash(), so it survives a fresh interpreter.
    rid = RegionCoord(x=3, y=7).region_id
    assert biome._scatter_count("forest", rid) == biome._scatter_count("forest", rid)


def test_distinct_regions_yield_distinct_geometry():
    # FM3 (variability): a constant count would leave the optimizer a flat
    # contributor and keep over_budget unreachable. Distinct regions differ.
    counts = {biome._scatter_count("forest", RegionCoord(x=x, y=0).region_id) for x in range(8)}
    assert len(counts) > 1


@pytest.mark.asyncio
async def test_dense_biome_is_lod_resolved_by_the_optimizer(tmp_path, monkeypatch):
    # FM4 (optimizer integration / the unblock): biome triangles flow into
    # optimization.run's sum, so a dense biome region's RAW geometry exceeds the
    # budget — the thing terrain alone never could. The optimizer (now budget-
    # resolving) LOD-sheds that scatter back under budget rather than fast-failing,
    # which is exactly the contributor the LOD-resolve task needed to do real work.
    monkeypatch.chdir(tmp_path)
    terrain_tris = (await terrain.run(WorldBrief(biome="forest", region=RegionCoord(x=0, y=0)), [])).metrics[
        "triangles"
    ]
    over = [
        RegionCoord(x=x, y=0)
        for x in range(40)
        if biome._scatter_count("forest", RegionCoord(x=x, y=0).region_id) * biome.TRIS_PER_INSTANCE
        + terrain_tris
        > optimization.TRIANGLE_BUDGET
    ]
    assert over, "no forest region exceeds the budget — the geometry model regressed"
    brief = WorldBrief(biome="forest", region=over[0])
    terr = await terrain.run(brief, [])
    bio = await biome.run(brief, [terr])
    opt = await optimization.run(brief, [terr, bio])
    assert opt.metrics["over_budget"] == 0.0  # resolved by shedding, not a terminal fail
    opt_usd = (tmp_path / "layers" / opt.path).read_text()
    assert "forcedLodCollapse = true" in opt_usd  # a real shed happened
    assert bio.path in opt_usd  # the biome scatter is the layer that was LOD-collapsed


@pytest.mark.asyncio
async def test_sparse_biome_needs_no_shedding(tmp_path, monkeypatch):
    # The complement to the dense case: a sparse post-conflict biome fits under budget
    # on its own, so the optimizer resolves it WITHOUT shedding (no forced collapse) —
    # the shed signal stays meaningful (driven by geometry, not always-on).
    monkeypatch.chdir(tmp_path)
    for x in range(20):
        brief = WorldBrief(biome="scorched", region=RegionCoord(x=x, y=0))
        terr = await terrain.run(brief, [])
        bio = await biome.run(brief, [terr])
        opt = await optimization.run(brief, [terr, bio])
        assert opt.metrics["over_budget"] == 0.0
        assert "forcedLodCollapse = false" in (tmp_path / "layers" / opt.path).read_text()


@pytest.mark.asyncio
async def test_hostile_biome_string_injects_no_prim(tmp_path, monkeypatch):
    # brief.biome is CLI-supplied free-form text interpolated into the layer. A
    # value carrying a USD quote/newline must not close the string literal and
    # forge a prim — the validator's structural scan (the prod gate ships without
    # usd-core) would otherwise read the injected prim and misattribute it to biome
    # or, via a fabricated path collision, to an innocent specialist.
    monkeypatch.chdir(tmp_path)
    hostile = 'forest"\n}\ndef Mesh "Injected"\n{\n  custom int n = 1'
    brief = WorldBrief(biome=hostile, region=RegionCoord(x=1, y=2))
    layer = await biome.run(brief, [])
    text = (tmp_path / "layers" / layer.path).read_text()
    prims = {path for _, _, path in validator._prim_specs(text)}
    assert prims == {"/Scatter"}  # only the intended PointInstancer, no /Injected
    # the no-phantom-geometry invariant still holds for a hostile input
    count = _instance_count_from_usd(text)
    assert layer.metrics["triangles"] == float(count * biome.TRIS_PER_INSTANCE)
