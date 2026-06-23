"""NPC character-geometry emission: the spawn count is deterministic and
region-variable, the triangles metric matches the spawnCount actually written into
the USD (no phantom geometry), an unknown archetype fails loudly instead of
contributing 0, and the geometry flows into the optimizer's budget as a shed-able
non-floor layer.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_npc.py -v
"""

import re

import pytest

from common.types import RegionCoord, WorldBrief
from biome import biome
from npc import npc
from optimization import optimization
from terrain import terrain


def _spawn_count_from_usd(text: str) -> int:
    m = re.search(r"spawnCount\s*=\s*(\d+)", text)
    assert m, "emitted npc USD has no spawnCount"
    return int(m.group(1))


def _authored_from_usd(text: str) -> float:
    m = re.search(r"authoredTriangles\s*=\s*([\d.]+)", text)
    assert m, "optimization USD has no authoredTriangles"
    return float(m.group(1))


@pytest.mark.asyncio
async def test_triangles_metric_matches_declared_spawn_count(tmp_path, monkeypatch):
    # FM1 (phantom geometry): the metric must equal the spawnCount actually written
    # into the Spawns prim times the PER-ARCHETYPE table value — never an independent
    # magic number, and scaled by which archetype spawns (the table), not a flat constant.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    layer = await npc.run(brief, [])
    count = _spawn_count_from_usd((tmp_path / "layers" / layer.path).read_text())
    assert count > 0  # a real, non-empty roster
    assert layer.metrics["triangles"] == float(count * npc.CHARACTER_TRIS[npc.NPC_ARCHETYPE])


@pytest.mark.asyncio
async def test_emission_is_deterministic(tmp_path, monkeypatch):
    # FM2 (determinism): the same brief yields byte-identical USD + identical metric
    # across runs, so a pipeline re-run and a validation-revision re-run reproduce it.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    a = await npc.run(brief, [])
    usd_a = (tmp_path / "layers" / a.path).read_text()
    b = await npc.run(brief, [])
    usd_b = (tmp_path / "layers" / b.path).read_text()
    assert usd_a == usd_b
    assert a.metrics == b.metrics


def test_count_is_stable_for_a_region_no_hash_salt():
    # determinism of the pure count fn — hashlib, not the per-process-salted builtin
    # hash(), so it survives a fresh interpreter.
    rid = RegionCoord(x=3, y=7).region_id
    assert npc._spawn_count(rid) == npc._spawn_count(rid)


def test_distinct_regions_yield_distinct_geometry():
    # FM2 (variability): a constant count would leave the optimizer a flat contributor.
    # Distinct regions differ.
    counts = {npc._spawn_count(RegionCoord(x=x, y=0).region_id) for x in range(8)}
    assert len(counts) > 1


def test_unknown_archetype_raises_not_silent_zero():
    # FM3 (archetype-table integrity): an archetype absent from CHARACTER_TRIS must fail
    # loudly, not default to 0 triangles — a silent 0 would hide its geometry from the budget.
    with pytest.raises(KeyError):
        npc._character_tris("no_such_archetype_99")
    # every shipped archetype has a positive budget, and the default archetype is in the table.
    assert all(npc._character_tris(a) > 0 for a in npc.CHARACTER_TRIS)
    assert npc._character_tris(npc.NPC_ARCHETYPE) == npc.CHARACTER_TRIS[npc.NPC_ARCHETYPE]


@pytest.mark.asyncio
async def test_npc_triangles_flow_into_the_optimizer_budget(tmp_path, monkeypatch):
    # FM4 (optimizer integration): npc triangles enter optimization.run's authored sum.
    # Adding the npc layer raises the optimizer's authoredTriangles by EXACTLY npc's
    # triangles — it is counted toward the budget, never silently dropped.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    terr = await terrain.run(brief, [])
    bio = await biome.run(brief, [terr])
    npcl = await npc.run(brief, [terr, bio])
    opt_path = tmp_path / "layers" / f"optimization/{brief.region.region_id}.usda"

    await optimization.run(brief, [terr, bio])
    without = _authored_from_usd(opt_path.read_text())
    await optimization.run(brief, [terr, bio, npcl])
    with_npc = _authored_from_usd(opt_path.read_text())

    assert with_npc - without == npcl.metrics["triangles"]


@pytest.mark.asyncio
async def test_npc_is_a_sheddable_non_floor_layer(tmp_path, monkeypatch):
    # FM4 (shed-able non-floor): npc is NOT in GEOMETRY_FLOOR, so the optimizer can
    # LOD-collapse it when the world is over budget. Lower the budget to isolate npc as
    # the sole shed-able layer above the terrain floor and prove it gets collapsed.
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(optimization, "TRIANGLE_BUDGET", 350_000)
    assert "npc" not in optimization.GEOMETRY_FLOOR
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    terr = await terrain.run(brief, [])
    npcl = await npc.run(brief, [terr])
    assert terr.metrics["triangles"] + npcl.metrics["triangles"] > optimization.TRIANGLE_BUDGET
    opt = await optimization.run(brief, [terr, npcl])
    usd = (tmp_path / "layers" / f"optimization/{brief.region.region_id}.usda").read_text()
    assert "forcedLodCollapse = true" in usd  # a real shed happened
    assert npcl.path in usd  # and npc is the layer that was LOD-collapsed
    assert opt.metrics["over_budget"] == 0.0  # resolved by shedding npc, not terminal
