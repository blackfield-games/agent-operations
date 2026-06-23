"""Prop placement-geometry emission: the count is deterministic and region-variable,
the triangles metric matches the placementCount actually written into the USD (no
phantom geometry), an unknown asset fails loudly instead of contributing 0, and the
geometry flows into the optimizer's budget as a shed-able non-floor layer.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_prop.py -v
"""

import logging
import re

import pytest

from common.types import LayerSpec, RegionCoord, WorldBrief
from biome import biome
from director import director
from optimization import optimization
from prop import prop
from terrain import terrain


def _placement_count_from_usd(text: str) -> int:
    m = re.search(r"placementCount\s*=\s*(\d+)", text)
    assert m, "emitted prop USD has no placementCount"
    return int(m.group(1))


def _asset_from_usd(text: str) -> str:
    m = re.search(r'propAsset\s*=\s*"([^"]+)"', text)
    assert m, "emitted prop USD has no propAsset"
    return m.group(1)


def _authored_from_usd(text: str) -> float:
    m = re.search(r"authoredTriangles\s*=\s*([\d.]+)", text)
    assert m, "optimization USD has no authoredTriangles"
    return float(m.group(1))


def _required_assets_from_usd(text: str) -> list[str]:
    return re.findall(r'requiredAsset\s*=\s*"([^"]+)"', text)


@pytest.mark.asyncio
async def test_triangles_metric_matches_declared_placement_count(tmp_path, monkeypatch):
    # FM1 (phantom geometry): the metric must equal the placementCount times the table
    # value for the asset ACTUALLY written into the USD — never an independent magic
    # number, and never a pick that diverges between the propAsset field and the math.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    layer = await prop.run(brief, [])
    text = (tmp_path / "layers" / layer.path).read_text()
    count = _placement_count_from_usd(text)
    asset = _asset_from_usd(text)
    assert count > 0  # a real, non-empty placement
    assert layer.metrics["triangles"] == float(count * prop.ASSET_TRIS[asset])


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
    # every shipped asset has a positive budget.
    assert all(prop._asset_tris(a) > 0 for a in prop.ASSET_TRIS)


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
    # force the heaviest asset so the scenario exceeds budget regardless of the
    # per-region asset pick — the test isolates shed-ability, not the selection hash.
    monkeypatch.setattr(prop, "_select_asset", lambda rid: "comms_tower_01")
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


def test_asset_selection_is_deterministic_and_region_variable():
    # FM2: the per-region asset pick is a stable function of the region id (two calls
    # agree) and DIFFERS across regions — a constant pick would defeat region-variable
    # composition. Distinct regions select more than one distinct asset.
    rid = RegionCoord(x=3, y=7).region_id
    assert prop._select_asset(rid) == prop._select_asset(rid)
    assets = {prop._select_asset(RegionCoord(x=x, y=0).region_id) for x in range(20)}
    assert len(assets) > 1


def test_selected_asset_is_always_a_table_key():
    # FM3: the pick is drawn from ASSET_TRIS keys, so it can NEVER be an unknown asset
    # that would slip past _asset_tris as a 0-tri phantom layer — every region's pick is
    # budget-known, and the count and asset are independently salted (a region's pick is
    # not just a function of its count).
    for x in range(20):
        for y in range(-5, 5):
            assert prop._select_asset(RegionCoord(x=x, y=y).region_id) in prop.ASSET_TRIS


@pytest.mark.asyncio
async def test_director_must_have_assets_are_placed_and_metered(tmp_path, monkeypatch):
    # The director->prop intent flow: every asset the director marks intent:must_have is
    # GUARANTEED a placement in prop's layer (a Required prim that references it) AND its
    # triangles are counted — the metric equals the hash fill PLUS the full must-have sum,
    # never the fill alone. director intent now shapes prop, not just the region hash.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    dl = await director.run(brief, [])
    required = prop._required_assets(prop._must_have_from_director([dl]))
    assert required  # the director seeds a non-empty must_have

    layer = await prop.run(brief, [dl])
    text = (tmp_path / "layers" / layer.path).read_text()
    for asset in required:
        assert f"references = @../../assets/library/props/{asset}.usd@" in text
    assert _required_assets_from_usd(text) == required  # placed, in order, all of them

    count = _placement_count_from_usd(text)
    fill = _asset_from_usd(text)
    expected = count * prop.ASSET_TRIS[fill] + sum(prop.ASSET_TRIS[a] for a in required)
    assert layer.metrics["triangles"] == float(expected)


@pytest.mark.asyncio
async def test_must_have_raises_optimizer_authored_by_exactly_its_budget(tmp_path, monkeypatch):
    # FM2 (budget accounting): the ONLY difference between prop-with-director and
    # prop-without is the guaranteed must-have assets (the fill is a function of region id
    # alone), so the optimizer's authoredTriangles must rise by EXACTLY their summed
    # budget — the heroes are counted toward the budget like any geometry, never hidden.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    terr = await terrain.run(brief, [])
    dl = await director.run(brief, [])
    required = prop._required_assets(prop._must_have_from_director([dl]))
    assert required
    opt_path = tmp_path / "layers" / f"optimization/{brief.region.region_id}.usda"

    prop_without = await prop.run(brief, [terr])
    await optimization.run(brief, [terr, prop_without])
    without = _authored_from_usd(opt_path.read_text())

    prop_with = await prop.run(brief, [terr, dl])
    await optimization.run(brief, [terr, prop_with])
    with_must = _authored_from_usd(opt_path.read_text())

    assert with_must - without == float(sum(prop.ASSET_TRIS[a] for a in required))


def test_must_have_asset_values_are_a_subset_of_asset_tris():
    # FM1 (mapping integrity): every MUST_HAVE_ASSET target is a budgeted library asset,
    # so a guaranteed placement is ALWAYS metered — _asset_tris can never fire on a
    # must-have, and a future token can't introduce an un-budgetable hero.
    assert set(prop.MUST_HAVE_ASSET.values()) <= set(prop.ASSET_TRIS)
    assert all(prop._asset_tris(a) > 0 for a in prop.MUST_HAVE_ASSET.values())


@pytest.mark.asyncio
async def test_director_must_have_tokens_are_all_prop_mappable(tmp_path, monkeypatch):
    # The director<->prop contract: every token the director can seed as intent:must_have
    # is one prop can place. Pins the vocabulary at the source — a future director
    # must_have token without a MUST_HAVE_ASSET entry fails HERE, loudly, not silently as
    # a skipped hero at emission. The sibling of test_faction_roster_is_npc_budgetable.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=2, y=4))
    dl = await director.run(brief, [])
    tokens = prop._must_have_from_director([dl])
    assert tokens
    assert all(t in prop.MUST_HAVE_ASSET for t in tokens)
    assert prop._required_assets(tokens) == [prop.MUST_HAVE_ASSET[t] for t in tokens]  # none skipped


@pytest.mark.asyncio
async def test_director_absent_falls_back_byte_identical(tmp_path, monkeypatch):
    # FM3 (back-compat): with no director layer in prior, prop emits exactly the hash-only
    # fill it did before the must-have flow existed — byte-identical whether prior is
    # empty or carries non-director layers, and carrying no Required prim.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    terr = await terrain.run(brief, [])
    a = await prop.run(brief, [])
    usd_a = (tmp_path / "layers" / a.path).read_text()
    b = await prop.run(brief, [terr])  # prior present, but no director
    usd_b = (tmp_path / "layers" / b.path).read_text()
    assert usd_a == usd_b
    assert a.metrics == b.metrics
    assert "Required_" not in usd_a


@pytest.mark.asyncio
async def test_must_have_less_director_degrades_to_fallback(tmp_path, monkeypatch):
    # A director layer carrying no intent:must_have (a pre-intent record) parses to [] →
    # the hash-only fill, byte-identical to the no-director render, not a raise that would
    # drop the prop layer. The sibling of npc's rosterless-director degrade.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    rel = f"director/{brief.region.region_id}.usda"
    dpath = tmp_path / "layers" / rel
    dpath.parent.mkdir(parents=True, exist_ok=True)
    dpath.write_text('#usda 1.0\ndef Scope "Director"\n{\n}\n')
    stub = LayerSpec(specialist="director", region_id=brief.region.region_id, path=rel, summary="no intent")
    assert prop._must_have_from_director([stub]) == []
    layer = await prop.run(brief, [stub])
    usd = (tmp_path / "layers" / layer.path).read_text()
    assert "Required_" not in usd
    plain = await prop.run(brief, [])  # overwrites the same path with the no-director render
    assert usd == (tmp_path / "layers" / plain.path).read_text()


def test_missing_director_file_degrades_to_empty(tmp_path, monkeypatch):
    # A director LayerSpec whose file is absent must degrade to [] (→ hash-only fill),
    # never raise — a raise would isolate prop as an empty sublayer in the supervisor.
    monkeypatch.chdir(tmp_path)
    stub = LayerSpec(
        specialist="director", region_id="r+0000_+0000_l0", path="director/missing.usda", summary="gone"
    )
    assert prop._must_have_from_director([stub]) == []


def test_unmapped_must_have_token_is_skipped_not_raised(caplog):
    # FM1 (mapping integrity): an intent:must_have token with no MUST_HAVE_ASSET entry is
    # SKIPPED with a warning — not raised (which would drop prop's whole layer) and not
    # silently 0-budgeted. The mapped tokens around it are still placed, in order.
    with caplog.at_level(logging.WARNING):
        assets = prop._required_assets(["comms_tower", "no_such_token_99", "convoy_wreck"])
    assert assets == ["comms_tower_01", "convoy_wreck_01"]  # unknown dropped, known kept, ordered
    assert "no_such_token_99" in caplog.text


@pytest.mark.asyncio
async def test_must_have_emission_is_deterministic(tmp_path, monkeypatch):
    # Determinism with a director present: the same brief + director yields byte-identical
    # USD and metric across runs, so a pipeline re-run (and a validation-revision re-run)
    # reproduces the heroes too — not just the hash fill.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched", region=RegionCoord(x=5, y=9))
    dl = await director.run(brief, [])
    a = await prop.run(brief, [dl])
    usd_a = (tmp_path / "layers" / a.path).read_text()
    b = await prop.run(brief, [dl])
    usd_b = (tmp_path / "layers" / b.path).read_text()
    assert usd_a == usd_b
    assert a.metrics == b.metrics
    assert "Required_0" in usd_a  # the director path actually emitted heroes
