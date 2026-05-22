"""Supervisor node-wrapper behavior: a raising specialist is isolated (logged +
empty sublayer) instead of crashing the graph; success and None pass through
unchanged. Tests patch supervisor.SPECIALISTS, never the Dev C specialist stubs.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_supervisor.py -v
"""

import asyncio

import runtime.supervisor as sup
from common.types import LayerSpec, ValidatorVerdict
from langgraph.graph import END


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


async def test_validator_node_isolates_a_raising_validator(monkeypatch):
    async def boom(brief, layers):
        raise RuntimeError("validator exploded")
    monkeypatch.setattr(sup.validator, "run", boom)
    out = await sup._validator_node({"brief": object(), "layers": []})
    assert out["validator_errored"] is True
    assert out["verdict"].accepted is False
    assert out["rounds"] == 1


def test_after_validator_ends_on_validator_error():
    state = {"verdict": ValidatorVerdict(accepted=False, issues=["x"]),
             "validator_errored": True, "rounds": 1}
    assert sup._after_validator(state) == END


async def test_validator_node_passes_through_a_normal_verdict(monkeypatch):
    verdict = ValidatorVerdict(accepted=True)
    async def ok(brief, layers):
        return verdict
    monkeypatch.setattr(sup.validator, "run", ok)
    out = await sup._validator_node({"brief": object(), "layers": []})
    assert out["verdict"] is verdict
    assert out.get("validator_errored") is None
    assert out["rounds"] == 1


async def test_specialist_node_times_out_to_empty_sublayer(monkeypatch):
    async def hang(brief, layers):
        await asyncio.sleep(10)

    monkeypatch.setitem(sup.SPECIALISTS, "terrain", hang)
    monkeypatch.setattr(sup, "NODE_TIMEOUT_SECS", 0.01)
    node = sup._make_specialist_node("terrain")

    # A hung specialist hits the same isolation path as a raising one.
    out = await node({"brief": object(), "layers": []})
    assert out == {"layers": []}


async def test_validator_node_times_out_to_errored_verdict(monkeypatch):
    async def hang(brief, layers):
        await asyncio.sleep(10)

    monkeypatch.setattr(sup.validator, "run", hang)
    monkeypatch.setattr(sup, "NODE_TIMEOUT_SECS", 0.01)
    out = await sup._validator_node({"brief": object(), "layers": []})
    assert out["validator_errored"] is True
    assert out["verdict"].accepted is False
    assert out["verdict"].issues == ["validator timed out"]
    assert out["rounds"] == 1


# ---- merge_layers reducer (route-back de-dup) ----


def _layer(specialist, region_id, summary):
    return LayerSpec(
        specialist=specialist, region_id=region_id,
        path=f"{specialist}/{region_id}.usda", summary=summary, metrics={},
    )


def test_merge_layers_keeps_latest_per_specialist_region():
    # A route-back replays a downstream specialist, producing a second layer for
    # the same (specialist, region_id). The reducer keeps the fresh one so
    # compose_world never emits a duplicate sublayer.
    region = "r+0000_+0000_l0"
    old = _layer("terrain", region, "stale")
    new = _layer("terrain", region, "fresh")
    merged = sup.merge_layers([old], [new])
    assert len(merged) == 1
    assert merged[0].summary == "fresh"


def test_merge_layers_keeps_distinct_specialists_and_regions():
    a = _layer("terrain", "r+0000_+0000_l0", "t")
    b = _layer("biome", "r+0000_+0000_l0", "b")  # same region, other specialist
    c = _layer("terrain", "r+0001_+0000_l0", "t2")  # same specialist, other region
    merged = sup.merge_layers([a], [b, c])
    assert len(merged) == 3
    assert {(m.specialist, m.region_id) for m in merged} == {
        ("terrain", "r+0000_+0000_l0"),
        ("biome", "r+0000_+0000_l0"),
        ("terrain", "r+0001_+0000_l0"),
    }


def test_merge_layers_handles_empty_sides():
    layer = _layer("terrain", "r+0000_+0000_l0", "x")
    assert sup.merge_layers([], [layer]) == [layer]
    assert sup.merge_layers([layer], []) == [layer]


# ---- _after_validator routing (complements the validator_errored case above) ----


def test_after_validator_ends_on_accepted():
    state = {"verdict": ValidatorVerdict(accepted=True), "rounds": 1}
    assert sup._after_validator(state) == END


def test_after_validator_ends_when_no_verdict():
    assert sup._after_validator({}) == END


def test_after_validator_routes_back_to_failing_specialist():
    state = {
        "verdict": ValidatorVerdict(accepted=False, issues=["missing specialist layers: ['biome']"]),
        "rounds": 1,
    }
    assert sup._after_validator(state) == "biome"


def test_after_validator_ends_on_recursion_guard():
    # Even with an attributable, fixable issue, the round cap ends the loop so a
    # persistently-failing region can't recurse forever.
    state = {
        "verdict": ValidatorVerdict(accepted=False, issues=["missing specialist layers: ['biome']"]),
        "rounds": 3,
    }
    assert sup._after_validator(state) == END
