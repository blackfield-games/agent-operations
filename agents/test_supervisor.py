"""Supervisor node-wrapper behavior: a raising specialist is isolated (logged +
empty sublayer) instead of crashing the graph; success and None pass through
unchanged. Tests patch supervisor.SPECIALISTS, never the Dev C specialist stubs.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_supervisor.py -v
"""

import runtime.supervisor as sup
from common.types import LayerSpec


async def test_failing_specialist_is_isolated_not_fatal(monkeypatch):
    async def boom(brief, layers):
        raise RuntimeError("specialist exploded")

    monkeypatch.setitem(sup.SPECIALISTS, "terrain", boom)
    node = sup._make_specialist_node("terrain")

    # The raise is contained: the node yields an empty sublayer rather than
    # propagating, so the graph continues to the next specialist / validator.
    out = await node({"brief": object(), "layers": []})
    assert out == {"layers": []}


async def test_specialist_node_passes_through_a_returned_layer(monkeypatch):
    layer = LayerSpec(
        specialist="terrain", region_id="r+0000_+0000_l0",
        path="terrain/x.usda", summary="ok", metrics={},
    )

    async def ok(brief, layers):
        return layer

    monkeypatch.setitem(sup.SPECIALISTS, "terrain", ok)
    node = sup._make_specialist_node("terrain")
    out = await node({"brief": object(), "layers": []})
    assert out == {"layers": [layer]}


async def test_specialist_node_empty_when_specialist_returns_none(monkeypatch):
    async def none(brief, layers):
        return None

    monkeypatch.setitem(sup.SPECIALISTS, "terrain", none)
    node = sup._make_specialist_node("terrain")
    out = await node({"brief": object(), "layers": []})
    assert out == {"layers": []}
