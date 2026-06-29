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

from common.types import LayerSpec, RegionCoord, WorldBrief
from biome import biome
from director import director
from npc import npc
from optimization import optimization
from terrain import terrain


def _spawn_count_from_usd(text: str) -> int:
    m = re.search(r"spawnCount\s*=\s*(\d+)", text)
    assert m, "emitted npc USD has no spawnCount"
    return int(m.group(1))


def _archetype_from_usd(text: str) -> str:
    m = re.search(r'archetype\s*=\s*"([^"]*)"', text)
    assert m, "emitted npc USD has no archetype"
    return m.group(1)


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
    assert "npc" not in optimization.GEOMETRY_FLOOR
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    terr = await terrain.run(brief, [])
    npcl = await npc.run(brief, [terr])
    # Budget just above the (region-varied) terrain floor but below floor+npc, so the
    # un-sheddable terrain fits while npc must LOD-collapse to resolve. Relative to the
    # region's ACTUAL terrain — no longer a fixed 262144 — since a constant budget below
    # the now-larger floor would make terrain alone terminal and mask npc's shed-ability.
    monkeypatch.setattr(
        optimization, "TRIANGLE_BUDGET", int(terr.metrics["triangles"] + npcl.metrics["triangles"] // 2)
    )
    assert terr.metrics["triangles"] + npcl.metrics["triangles"] > optimization.TRIANGLE_BUDGET
    opt = await optimization.run(brief, [terr, npcl])
    usd = (tmp_path / "layers" / f"optimization/{brief.region.region_id}.usda").read_text()
    assert "forcedLodCollapse = true" in usd  # a real shed happened
    assert npcl.path in usd  # and npc is the layer that was LOD-collapsed
    assert opt.metrics["over_budget"] == 0.0  # resolved by shedding npc, not terminal


@pytest.mark.asyncio
async def test_archetype_is_drawn_from_the_director_roster(tmp_path, monkeypatch):
    # The director->npc intent flow: npc's archetype is one the director seeded for the
    # region, not the hardcoded default — director intent now shapes npc placement, and
    # the triangle metric scales by the SELECTED archetype's budget.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    dl = await director.run(brief, [])
    roster = director._faction_roster(brief.region.region_id)
    layer = await npc.run(brief, [dl])
    text = (tmp_path / "layers" / layer.path).read_text()
    archetype = _archetype_from_usd(text)
    assert archetype in roster
    count = _spawn_count_from_usd(text)
    assert layer.metrics["triangles"] == float(count * npc.CHARACTER_TRIS[archetype])


def test_archetype_selection_is_deterministic_no_hash_salt():
    # hashlib, not the per-process-salted builtin hash(): the same region + roster picks
    # the same archetype across a fresh interpreter, so a re-run reproduces the layer.
    roster = ["raider", "scavenger"]
    rid = RegionCoord(x=5, y=9).region_id
    assert npc._select_archetype(rid, roster) == npc._select_archetype(rid, roster)


def test_selection_is_invariant_to_roster_order():
    # npc sorts the roster before hashing, so the director writing it in any order
    # yields the same pick — the canonical-order contract both sides rely on.
    rid = RegionCoord(x=5, y=9).region_id
    assert npc._select_archetype(rid, ["sentinel", "drone"]) == npc._select_archetype(rid, ["drone", "sentinel"])


def test_distinct_regions_can_field_distinct_archetypes():
    # The pick varies region to region within a roster, so the director's intent has a
    # visible effect rather than collapsing to one faction everywhere.
    roster = list(director.FACTION_ARCHETYPES)
    picks = {npc._select_archetype(RegionCoord(x=x, y=0).region_id, roster) for x in range(16)}
    assert len(picks) > 1


def test_empty_roster_falls_back_to_default():
    # No director roster → the budgeted fallback, never an empty/unknown pick.
    assert npc._select_archetype(RegionCoord(x=1, y=2).region_id, []) == npc.NPC_ARCHETYPE


def test_forbidden_archetypes_bridges_the_plural_must_not_token():
    # The director's must_not token is the PLURAL "civilians"; the npc archetype the
    # SINGULAR "civilian" — MUST_NOT_ARCHETYPE maps them so the ban actually bites.
    assert npc._forbidden_archetypes(["civilians"]) == frozenset({"civilian"})
    assert "civilian" in npc.CHARACTER_TRIS  # the barred token names a real, budgeted archetype


def test_forbidden_archetypes_skips_tokens_it_does_not_own():
    # must_not also carries other specialists' constraints — dense_vegetation is biome's
    # cap, interior_volumes names no archetype. npc bans ONLY archetypes it owns, so a
    # non-archetype must_not never narrows its pick (no false roster shrink).
    assert npc._forbidden_archetypes(["dense_vegetation", "interior_volumes"]) == frozenset()
    assert npc._forbidden_archetypes(["dense_vegetation", "civilians"]) == frozenset({"civilian"})


def test_select_excludes_a_forbidden_roster_member():
    # A desynced roster that NAMES a barred archetype: _select_archetype must never return
    # it, picking only from the allowed remainder across every region.
    roster = ["civilian", "raider"]
    forbidden = frozenset({"civilian"})
    for x in range(-6, 7):
        rid = RegionCoord(x=x, y=0).region_id
        assert npc._select_archetype(rid, roster, forbidden) == "raider"


def test_select_falls_back_when_every_roster_member_is_forbidden():
    # Roster collapses to nothing under the ban → the budgeted fallback, never an empty or
    # barred pick (NPC_ARCHETYPE is "scavenger", itself never forbidden).
    rid = RegionCoord(x=1, y=2).region_id
    assert npc._select_archetype(rid, ["civilian"], frozenset({"civilian"})) == npc.NPC_ARCHETYPE
    assert npc.NPC_ARCHETYPE not in npc._forbidden_archetypes(["civilians"])


def test_select_is_byte_identical_when_nothing_is_forbidden():
    # FM1 (no-op on the normal path): FACTION_ARCHETYPES has no civilian, so a real roster
    # filters to itself and the pick is unchanged — the forbidden arg cannot perturb the
    # region hash when it excludes nothing, keeping the emitted layer byte-identical.
    roster = list(director.FACTION_ARCHETYPES)
    for x in range(-6, 7):
        rid = RegionCoord(x=x, y=0).region_id
        base = npc._select_archetype(rid, roster)
        assert npc._select_archetype(rid, roster, frozenset()) == base
        assert npc._select_archetype(rid, roster, frozenset({"civilian"})) == base


@pytest.mark.asyncio
async def test_run_honors_must_not_and_never_fields_a_civilian(tmp_path, monkeypatch):
    # End-to-end honor leg: a director layer that (wrongly) rosters civilian AND forbids it
    # via intent:must_not — npc must drop the barred archetype and emit an allowed one, so a
    # validator route-back converges instead of re-picking the same civilian forever.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    rel = f"director/{brief.region.region_id}.usda"
    layer_path = tmp_path / "layers" / rel
    layer_path.parent.mkdir(parents=True, exist_ok=True)
    layer_path.write_text(
        '#usda 1.0\ndef Scope "Director"\n{\n'
        '    custom string intent:factions = "civilian,raider"\n'
        '    custom string intent:must_not = "dense_vegetation,interior_volumes,civilians"\n'
        "}\n"
    )
    stub = LayerSpec(specialist="director", region_id=brief.region.region_id, path=rel, summary="rosters civilian")
    layer = await npc.run(brief, [stub])
    assert _archetype_from_usd((tmp_path / "layers" / layer.path).read_text()) == "raider"


@pytest.mark.asyncio
async def test_no_director_layer_falls_back_to_default(tmp_path, monkeypatch):
    # prior without a director layer (a pre-director pipeline order, or a failed
    # director isolated as an empty sublayer) → the fallback archetype.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    terr = await terrain.run(brief, [])
    layer = await npc.run(brief, [terr])
    assert _archetype_from_usd((tmp_path / "layers" / layer.path).read_text()) == npc.NPC_ARCHETYPE


@pytest.mark.asyncio
async def test_rosterless_director_degrades_to_fallback(tmp_path, monkeypatch):
    # A director layer carrying no intent:factions (a pre-factions record) parses to an
    # empty roster → fallback, not a raise that would drop the npc layer.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    rel = f"director/{brief.region.region_id}.usda"
    layer_path = tmp_path / "layers" / rel
    layer_path.parent.mkdir(parents=True, exist_ok=True)
    layer_path.write_text('#usda 1.0\ndef Scope "Director"\n{\n}\n')
    stub = LayerSpec(specialist="director", region_id=brief.region.region_id, path=rel, summary="no factions")
    assert npc._roster_from_director([stub]) == []
    layer = await npc.run(brief, [stub])
    assert _archetype_from_usd((tmp_path / "layers" / layer.path).read_text()) == npc.NPC_ARCHETYPE


def test_missing_director_file_degrades_to_empty(tmp_path, monkeypatch):
    # A director LayerSpec whose file is absent must degrade to [] (→ fallback), never
    # raise — a raise would isolate npc as an empty sublayer in the supervisor.
    monkeypatch.chdir(tmp_path)
    stub = LayerSpec(
        specialist="director", region_id="r+0000_+0000_l0", path="director/missing.usda", summary="gone"
    )
    assert npc._roster_from_director([stub]) == []


def test_faction_roster_is_npc_budgetable():
    # The director↔npc contract: every faction the director can seed is one npc can
    # budget, so _select_archetype's pick never trips _character_tris and drops a layer.
    # Pins the vocabulary at the source — a future faction added to the director without
    # a CHARACTER_TRIS budget fails here, not silently at emission.
    assert set(director.FACTION_ARCHETYPES) <= set(npc.CHARACTER_TRIS)
    for x in range(-6, 7):
        rid = RegionCoord(x=x, y=0).region_id
        archetype = npc._select_archetype(rid, director._faction_roster(rid))
        assert npc._character_tris(archetype) > 0
