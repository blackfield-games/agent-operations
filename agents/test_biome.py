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
from common.types import LayerSpec, RegionCoord, WorldBrief
from director import director
from optimization import optimization
from terrain import terrain
from validator import validator


def _instance_count_from_usd(text: str) -> int:
    m = re.search(r"instanceCount\s*=\s*(\d+)", text)
    assert m, "emitted biome USD has no instanceCount"
    return int(m.group(1))


def _authored_from_usd(text: str) -> float:
    m = re.search(r"authoredTriangles\s*=\s*([\d.]+)", text)
    assert m, "optimization USD has no authoredTriangles"
    return float(m.group(1))


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


@pytest.mark.asyncio
async def test_director_must_not_caps_dense_biome(tmp_path, monkeypatch):
    # The director->biome must_not flow: a dense biome (jungle 4500) under a director that
    # forbids dense vegetation scatters at the cap, not the raw keyword density — the
    # instance count and metric drop, and the layer records vegetationCapped. director
    # intent now CONSTRAINS biome, not just the biome string.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="jungle", region=RegionCoord(x=5, y=9))
    dl = await director.run(brief, [])
    assert biome._caps_vegetation(biome._must_not_from_director([dl]))  # director seeds the cap token

    capped = await biome.run(brief, [dl])
    text = (tmp_path / "layers" / capped.path).read_text()
    assert "custom bool vegetationCapped = true" in text
    count = _instance_count_from_usd(text)
    # the metric matches the capped count actually written into the USD (no phantom geometry)
    assert capped.metrics["triangles"] == float(count * biome.TRIS_PER_INSTANCE)
    # and it is strictly below the uncapped scatter for the same region (the cap bit)
    uncapped = await biome.run(brief, [])
    assert capped.metrics["triangles"] < uncapped.metrics["triangles"]


@pytest.mark.asyncio
async def test_cap_is_a_ceiling_sparse_biome_geometry_unchanged(tmp_path, monkeypatch):
    # FM1 (cap direction): the cap is a CEILING (min) — a biome already below it
    # (scorched 300 < VEGETATION_CAP_DENSITY 800) keeps its exact instance count and
    # metric even under a capping director; the cap can only reduce density, never raise
    # it. The marker still fires (the constraint WAS honored) but the geometry is identical.
    monkeypatch.chdir(tmp_path)
    assert biome.BIOME_DENSITY["scorched"] < biome.VEGETATION_CAP_DENSITY
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    dl = await director.run(brief, [])
    assert biome._caps_vegetation(biome._must_not_from_director([dl]))
    capped = await biome.run(brief, [dl])
    uncapped = await biome.run(brief, [])  # no director -> no cap
    assert capped.metrics == uncapped.metrics  # equal metric <=> equal instance count


def test_cap_is_a_pure_ceiling_across_every_biome():
    # FM1 (cap never raises): for every biome keyword, the capped base count is <= the
    # uncapped count for the same region — a dense biome is reduced, a sparse one is
    # untouched, NONE is increased. The min() ceiling proven over the whole density table.
    rid = RegionCoord(x=5, y=9).region_id
    for b in [*biome.BIOME_DENSITY, "no_such_biome_keyword"]:
        uncapped = biome._scatter_count(b, rid)
        capped = biome._scatter_count(b, rid, biome.VEGETATION_CAP_DENSITY)
        assert capped <= uncapped
        # a biome at/under the cap is byte-identical; one above it is strictly reduced
        base = biome.BIOME_DENSITY.get(b, biome.DEFAULT_DENSITY)
        assert (capped == uncapped) == (base <= biome.VEGETATION_CAP_DENSITY)


@pytest.mark.asyncio
async def test_cap_lowers_optimizer_authored_by_the_shed(tmp_path, monkeypatch):
    # FM2 (budget honesty): the ONLY difference between capped and uncapped biome (for a
    # fixed region the terrain is identical) is the shed vegetation, so the optimizer's
    # authoredTriangles must DROP by EXACTLY the capped layer's triangle reduction — the
    # cap is counted toward the budget like any geometry, never hidden.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="jungle", region=RegionCoord(x=5, y=9))
    terr = await terrain.run(brief, [])
    dl = await director.run(brief, [])
    assert biome._caps_vegetation(biome._must_not_from_director([dl]))
    opt_path = tmp_path / "layers" / f"optimization/{brief.region.region_id}.usda"

    bio_uncapped = await biome.run(brief, [terr])  # no director -> no cap
    await optimization.run(brief, [terr, bio_uncapped])
    uncapped_authored = _authored_from_usd(opt_path.read_text())

    bio_capped = await biome.run(brief, [terr, dl])  # director present -> capped
    await optimization.run(brief, [terr, bio_capped])
    capped_authored = _authored_from_usd(opt_path.read_text())

    shed = bio_uncapped.metrics["triangles"] - bio_capped.metrics["triangles"]
    assert shed > 0
    assert uncapped_authored - capped_authored == shed


def test_unrecognized_must_not_tokens_dont_cap():
    # FM3 (vocabulary): must_not tokens biome doesn't own (interior_volumes, civilians —
    # other specialists' constraints) are NOT cap tokens; only a recognized vegetation
    # token gates the density, and an empty must_not never caps.
    assert biome._caps_vegetation(["interior_volumes", "civilians"]) is False
    assert biome._caps_vegetation(["dense_vegetation"]) is True
    assert biome._caps_vegetation(["interior_volumes", "dense_vegetation"]) is True
    assert biome._caps_vegetation([]) is False


@pytest.mark.asyncio
async def test_recognized_cap_token_is_in_the_director_must_not(tmp_path, monkeypatch):
    # The director<->biome contract: the director's emitted intent:must_not actually
    # carries a token biome reads as a cap, so the director->biome flow can never silently
    # stop firing if the director's vocabulary drifts. The sibling of the npc roster and
    # prop must-have vocabulary pins.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=2, y=4))
    dl = await director.run(brief, [])
    tokens = biome._must_not_from_director([dl])
    assert set(tokens) & biome.MUST_NOT_VEGETATION_CAP  # at least one recognized cap token


@pytest.mark.asyncio
async def test_director_absent_falls_back_byte_identical(tmp_path, monkeypatch):
    # FM3 (back-compat): with no director layer in prior, biome emits exactly the
    # density-only scatter it did before the cap flow existed — byte-identical whether
    # prior is empty or carries non-director layers, and carrying no vegetationCapped line.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="jungle", region=RegionCoord(x=3, y=7))
    terr = await terrain.run(brief, [])
    a = await biome.run(brief, [])
    usd_a = (tmp_path / "layers" / a.path).read_text()
    b = await biome.run(brief, [terr])  # prior present, but no director
    usd_b = (tmp_path / "layers" / b.path).read_text()
    assert usd_a == usd_b
    assert a.metrics == b.metrics
    assert "vegetationCapped" not in usd_a


@pytest.mark.asyncio
async def test_must_not_less_director_degrades_to_fallback(tmp_path, monkeypatch):
    # A director layer carrying no intent:must_not (a pre-intent record) parses to [] ->
    # the density-only scatter, byte-identical to the no-director render, not a raise that
    # would drop the biome layer. The sibling of prop's must_have-less degrade.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="jungle", region=RegionCoord(x=3, y=7))
    rel = f"director/{brief.region.region_id}.usda"
    dpath = tmp_path / "layers" / rel
    dpath.parent.mkdir(parents=True, exist_ok=True)
    dpath.write_text('#usda 1.0\ndef Scope "Director"\n{\n}\n')
    stub = LayerSpec(specialist="director", region_id=brief.region.region_id, path=rel, summary="no intent")
    assert biome._must_not_from_director([stub]) == []
    layer = await biome.run(brief, [stub])
    usd = (tmp_path / "layers" / layer.path).read_text()
    assert "vegetationCapped" not in usd
    plain = await biome.run(brief, [])  # overwrites the same path with the no-director render
    assert usd == (tmp_path / "layers" / plain.path).read_text()


def test_missing_director_file_degrades_to_empty(tmp_path, monkeypatch):
    # A director LayerSpec whose file is absent must degrade to [] (-> density-only
    # scatter), never raise — a raise would isolate biome as an empty sublayer in the
    # supervisor, dropping the region's whole scatter over a director hiccup.
    monkeypatch.chdir(tmp_path)
    stub = LayerSpec(
        specialist="director", region_id="r+0000_+0000_l0", path="director/missing.usda", summary="gone"
    )
    assert biome._must_not_from_director([stub]) == []


@pytest.mark.asyncio
async def test_capped_emission_is_deterministic(tmp_path, monkeypatch):
    # Determinism with a director present: the same brief + director yields byte-identical
    # capped USD and metric across runs, so a pipeline re-run (and a validation-revision
    # re-run) reproduces the cap too — not just the density-only scatter.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="jungle", region=RegionCoord(x=5, y=9))
    dl = await director.run(brief, [])
    a = await biome.run(brief, [dl])
    usd_a = (tmp_path / "layers" / a.path).read_text()
    b = await biome.run(brief, [dl])
    usd_b = (tmp_path / "layers" / b.path).read_text()
    assert usd_a == usd_b
    assert a.metrics == b.metrics
    assert "custom bool vegetationCapped = true" in usd_a  # the director path actually capped
