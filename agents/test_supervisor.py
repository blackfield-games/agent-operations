"""Supervisor node-wrapper behavior: a raising specialist is isolated (logged +
empty sublayer) instead of crashing the graph; success and None pass through
unchanged. Tests patch supervisor.SPECIALISTS, never the Dev C specialist stubs.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_supervisor.py -v
"""

import asyncio

import runtime.supervisor as sup
from common.types import LayerSpec, RegionCoord, ValidatorVerdict, WorldBrief
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
        "rounds": sup.MAX_ROUNDS,
    }
    assert sup._after_validator(state) == END


# ---- end-to-end route-back loop (integrated build_graph run) ----


async def test_route_back_loop_reruns_validator_then_accepts(tmp_path, monkeypatch):
    # The tests above exercise merge_layers, _after_validator, and the validator
    # node in ISOLATION. This drives the WHOLE compiled graph: a rejected first
    # verdict must traverse the conditional edge back to the offending specialist,
    # replay every downstream node, and run the validator a second time. The
    # specialist stubs write USD layers under cwd, so isolate to tmp_path.
    monkeypatch.chdir(tmp_path)

    calls = {"n": 0}

    async def reject_then_accept(brief, layers):
        calls["n"] += 1
        if calls["n"] == 1:
            # Attributable to biome → _failing_specialist routes back to "biome".
            return ValidatorVerdict(accepted=False, issues=["missing specialist layers: ['biome']"])
        return ValidatorVerdict(accepted=True)

    # Patch ONLY the validator; the 7 specialists stay the real Dev C stubs.
    monkeypatch.setattr(sup.validator, "run", reject_then_accept)

    brief = WorldBrief(biome="scorched_grassland", region=RegionCoord(x=42, y=-17))
    result = await sup.build_graph().ainvoke({"brief": brief, "layers": [], "rounds": 0})

    assert result["verdict"].accepted is True
    assert result["rounds"] == 2  # reject → route-back → accept: validator ran twice
    assert calls["n"] == 2
    # The route-back re-ran biome→…→optimization; merge_layers de-duped the replays,
    # so the layer set stays one-per-specialist (a plain operator.add would give 12).
    assert len(result["layers"]) == 7
    assert sorted(layer.specialist for layer in result["layers"]) == sorted(sup.SPECIALISTS)


async def test_persistently_failing_validator_terminates_at_round_cap(tmp_path, monkeypatch):
    # FM3: a stage that never passes must not loop forever. The predicate test
    # above hand-sets rounds=MAX_ROUNDS; this drives the WHOLE compiled graph with
    # a validator that ALWAYS rejects (attributable, so every route-back targets a
    # real specialist and the run keeps cycling). The rounds cap must terminate it
    # at MAX_ROUNDS with the last non-accepted verdict — not recurse forever, and
    # not trip LangGraph's own GraphRecursionError first. Stubs write under cwd.
    monkeypatch.chdir(tmp_path)

    calls = {"n": 0}

    async def always_reject(brief, layers):
        calls["n"] += 1
        return ValidatorVerdict(accepted=False, issues=["missing specialist layers: ['biome']"])

    monkeypatch.setattr(sup.validator, "run", always_reject)

    brief = WorldBrief(biome="scorched_grassland", region=RegionCoord(x=42, y=-17))
    result = await sup.build_graph().ainvoke({"brief": brief, "layers": [], "rounds": 0})

    # Ended cleanly at the cap (no exception, no hang), with the rejection surfaced.
    assert result["verdict"].accepted is False
    assert result["rounds"] == sup.MAX_ROUNDS
    assert calls["n"] == sup.MAX_ROUNDS, "validator runs exactly MAX_ROUNDS times, then the guard ENDs"


# ---- rounds normalization (robust to a missing/None/negative counter) ----


def test_rounds_normalizes_missing_none_and_negative():
    # _rounds is the single normalizer behind all four round reads (3 increments
    # + the cap check). It must keep None/missing/negative from crashing them
    # while leaving a real count untouched: no reset of a live counter (FM1), no
    # clamp of a legitimately-high value (FM3).
    assert sup._rounds({}) == 0  # missing key
    assert sup._rounds({"rounds": None}) == 0  # present-but-None: the crash case
    assert sup._rounds({"rounds": 0}) == 0  # legitimate start, not "needs reset"
    assert sup._rounds({"rounds": 2}) == 2  # mid-run count passes through, not reset
    assert sup._rounds({"rounds": sup.MAX_ROUNDS}) == sup.MAX_ROUNDS  # at the cap, unchanged
    assert sup._rounds({"rounds": -1}) == 0  # negative floored


async def test_validator_node_increments_from_none_rounds(monkeypatch):
    # _validator_node runs BEFORE _after_validator, so its `rounds + 1` is the
    # FIRST site a None rounds reaches — guarding only the cap check would still
    # crash here. Normalized, None becomes 0 and the round advances to 1.
    async def accept(brief, layers):
        return ValidatorVerdict(accepted=True)

    monkeypatch.setattr(sup.validator, "run", accept)
    brief = WorldBrief(biome="scorched_grassland", region=RegionCoord(x=42, y=-17))
    out = await sup._validator_node({"brief": brief, "layers": [], "rounds": None})
    assert out["rounds"] == 1


def test_after_validator_handles_none_rounds():
    # The cap check `None >= MAX_ROUNDS` is a TypeError; normalized to 0 it sits
    # below the cap, so a fixable rejection still routes back to the culprit
    # instead of crashing the conditional edge.
    state = {
        "verdict": ValidatorVerdict(accepted=False, issues=["missing specialist layers: ['biome']"]),
        "rounds": None,
    }
    assert sup._after_validator(state) == "biome"


async def test_round_cap_holds_with_none_initial_rounds(tmp_path, monkeypatch):
    # End-to-end: an initial state carrying rounds=None (a partial restore) must
    # still terminate at the cap — not crash on the first validator pass
    # (None + 1) and not recurse forever. Mirrors the rounds=0 cap test above.
    monkeypatch.chdir(tmp_path)

    calls = {"n": 0}

    async def always_reject(brief, layers):
        calls["n"] += 1
        return ValidatorVerdict(accepted=False, issues=["missing specialist layers: ['biome']"])

    monkeypatch.setattr(sup.validator, "run", always_reject)

    brief = WorldBrief(biome="scorched_grassland", region=RegionCoord(x=42, y=-17))
    result = await sup.build_graph().ainvoke({"brief": brief, "layers": [], "rounds": None})

    assert result["verdict"].accepted is False
    assert result["rounds"] == sup.MAX_ROUNDS
    assert calls["n"] == sup.MAX_ROUNDS
