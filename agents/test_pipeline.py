"""End-to-end smoke test: runs the supervisor graph against a synthetic brief
and asserts the validator accepts. No LLM calls, no Temporal, no Ray — just the
in-process LangGraph pipeline + USD layer emission.

Run from the agents/ dir:
    uv run pytest test_pipeline.py -v
"""

import re

import pytest

from common.types import WorldBrief, RegionCoord
from common.compose import compose_world
from biome import biome
from director import director
from prop import prop
from runtime.supervisor import build_graph


@pytest.mark.asyncio
async def test_smoke(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched_grassland", region=RegionCoord(x=42, y=-17))

    graph = build_graph()
    result = await graph.ainvoke({"brief": brief, "layers": [], "rounds": 0})

    assert result["verdict"].accepted, f"validator rejected: {result['verdict'].issues}"
    assert len(result["layers"]) == 7  # all specialists fired

    root = compose_world(result["layers"], tmp_path / "out")
    text = root.read_text()
    assert "validator" not in text  # validator doesn't write its own layer
    assert all(s in text for s in ["director", "terrain", "biome", "prop", "lighting", "npc"])


@pytest.mark.asyncio
async def test_npc_archetype_comes_from_the_director_roster_through_the_graph(tmp_path, monkeypatch):
    # The director→npc intent hand-off survives the REAL supervisor edges (not a
    # hand-built prior): the npc layer the graph emits carries an archetype the director
    # actually seeded for the region.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched_grassland", region=RegionCoord(x=42, y=-17))

    result = await build_graph().ainvoke({"brief": brief, "layers": [], "rounds": 0})

    npc_layer = next(layer for layer in result["layers"] if layer.specialist == "npc")
    text = (tmp_path / "layers" / npc_layer.path).read_text()
    archetype = re.search(r'archetype\s*=\s*"([^"]*)"', text).group(1)
    assert archetype in director._faction_roster(brief.region.region_id)


@pytest.mark.asyncio
async def test_prop_places_director_must_have_through_the_graph(tmp_path, monkeypatch):
    # The director→prop intent hand-off survives the REAL supervisor edges (not a
    # hand-built prior): the prop layer the graph emits carries a Required placement for
    # every asset the director marked intent:must_have for the region, in order.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="scorched_grassland", region=RegionCoord(x=42, y=-17))

    result = await build_graph().ainvoke({"brief": brief, "layers": [], "rounds": 0})

    director_layer = next(layer for layer in result["layers"] if layer.specialist == "director")
    required = prop._required_assets(prop._must_have_from_director([director_layer]))
    assert required  # the director seeds a non-empty must_have

    prop_layer = next(layer for layer in result["layers"] if layer.specialist == "prop")
    text = (tmp_path / "layers" / prop_layer.path).read_text()
    placed = re.findall(r'requiredAsset\s*=\s*"([^"]+)"', text)
    assert placed == required


@pytest.mark.asyncio
async def test_biome_vegetation_capped_through_the_graph(tmp_path, monkeypatch):
    # The director→biome must_not hand-off survives the REAL supervisor edges (not a
    # hand-built prior): the biome layer the graph emits records vegetationCapped because
    # the director seeded the cap token (dense_vegetation) for the region — the constraint
    # hand-off proven through the real director→biome edge, mirroring the npc + prop proofs.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="jungle", region=RegionCoord(x=11, y=-4))

    result = await build_graph().ainvoke({"brief": brief, "layers": [], "rounds": 0})

    director_layer = next(layer for layer in result["layers"] if layer.specialist == "director")
    assert biome._caps_vegetation(biome._must_not_from_director([director_layer]))  # director seeded the cap

    biome_layer = next(layer for layer in result["layers"] if layer.specialist == "biome")
    text = (tmp_path / "layers" / biome_layer.path).read_text()
    assert "custom bool vegetationCapped = true" in text


@pytest.mark.asyncio
async def test_validator_intent_gate_passes_for_the_real_pipeline_world(tmp_path, monkeypatch):
    # The closing of declare → honor → VERIFY, end to end: the validator's director-intent
    # gate (run as the graph's final node) ACCEPTS the world the real pipeline emits. The
    # director seeds a mapped must_have AND a recognized must_not cap token, prop/biome
    # honor them, and the gate finds nothing unmet — so a future false-reject regression
    # (the worst failure mode) breaks here, not silently in production. Asserting the gate
    # had real intent to check (non-empty mapped must_have + an active cap) distinguishes a
    # genuine pass from a director that accidentally stopped seeding intent.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="jungle", region=RegionCoord(x=11, y=-4))

    result = await build_graph().ainvoke({"brief": brief, "layers": [], "rounds": 0})

    director_layer = next(layer for layer in result["layers"] if layer.specialist == "director")
    assert prop._required_assets(prop._must_have_from_director([director_layer]))
    assert biome._caps_vegetation(biome._must_not_from_director([director_layer]))

    assert result["verdict"].accepted, result["verdict"].issues
