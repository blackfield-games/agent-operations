"""End-to-end smoke test: runs the supervisor graph against a synthetic brief
and asserts the validator accepts. No LLM calls, no Temporal, no Ray — just the
in-process LangGraph pipeline + USD layer emission.

Run from the agents/ dir:
    uv run pytest test_pipeline.py -v
"""

import pytest

from common.types import WorldBrief, RegionCoord
from common.compose import compose_world
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
