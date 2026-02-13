"""LangGraph supervisor — routes a WorldBrief through the 8 specialists in order:

    director → terrain → biome → prop → lighting → npc → optimization → validator

Each specialist returns a LayerSpec (or None if it has nothing to contribute).
Validator runs last and produces a verdict; on rejection the supervisor can
re-route to whichever specialist owns the failure.

This file is the in-process plan. For production, each node is wrapped as a
Temporal activity (see runtime/workflow.py) so individual steps survive crashes.
"""

from __future__ import annotations
from typing import TypedDict, Annotated
from langgraph.graph import StateGraph, END
from operator import add

from common.types import WorldBrief, LayerSpec, ValidatorVerdict
from director import director
from terrain import terrain
from biome import biome
from prop import prop
from lighting import lighting
from npc import npc
from optimization import optimization
from validator import validator


class State(TypedDict, total=False):
    brief: WorldBrief
    layers: Annotated[list[LayerSpec], add]
    verdict: ValidatorVerdict
    route_back: str  # specialist name if validator wants a re-run
    rounds: int


SPECIALISTS = {
    "director": director.run,
    "terrain": terrain.run,
    "biome": biome.run,
    "prop": prop.run,
    "lighting": lighting.run,
    "npc": npc.run,
    "optimization": optimization.run,
}


def _make_specialist_node(name: str):
    async def node(state: State) -> dict:
        fn = SPECIALISTS[name]
        layer = await fn(state["brief"], state.get("layers", []))
        return {"layers": [layer] if layer else []}
    return node


# Pipeline order = the static edge chain director → … → optimization. Re-entering
# a node replays every downstream specialist before the validator runs again, so
# routing back to the *earliest* failing node repairs upstream causes first.
PIPELINE_ORDER = list(SPECIALISTS)


def _failing_specialist(issues: list[str]) -> str:
    """Map validator issues to the earliest pipeline node that should re-run.

    The validator phrases issues with the offending specialist's name, e.g.
    "missing specialist layers: ['biome', 'prop']" or "triangle budget exceeded
    — re-run optimization with stricter LODs". We scan for those names and route
    back to whichever appears earliest in the pipeline. Unattributable failures
    (e.g. a pure style rejection naming no specialist) fall back to director,
    which owns the world brief / style intent.
    """
    candidates = {
        name
        for issue in issues
        for name in PIPELINE_ORDER
        if name in issue.lower()
    }
    if not candidates:
        return "director"
    return min(candidates, key=PIPELINE_ORDER.index)


async def _validator_node(state: State) -> dict:
    verdict = await validator.run(state["brief"], state.get("layers", []))
    return {"verdict": verdict, "rounds": state.get("rounds", 0) + 1}


def _after_validator(state: State) -> str:
    verdict = state.get("verdict")
    if not verdict:
        return END
    if verdict.accepted:
        return END
    if state.get("rounds", 0) >= 3:  # recursion guard from research-agent-runtime.md
        return END
    return _failing_specialist(verdict.issues)


def build_graph():
    g = StateGraph(State)
    g.add_node("director", _make_specialist_node("director"))
    g.add_node("terrain", _make_specialist_node("terrain"))
    g.add_node("biome", _make_specialist_node("biome"))
    g.add_node("prop", _make_specialist_node("prop"))
    g.add_node("lighting", _make_specialist_node("lighting"))
    g.add_node("npc", _make_specialist_node("npc"))
    g.add_node("optimization", _make_specialist_node("optimization"))
    g.add_node("validator", _validator_node)

    g.set_entry_point("director")
    g.add_edge("director", "terrain")
    g.add_edge("terrain", "biome")
    g.add_edge("biome", "prop")
    g.add_edge("prop", "lighting")
    g.add_edge("lighting", "npc")
    g.add_edge("npc", "optimization")
    g.add_edge("optimization", "validator")
    g.add_conditional_edges("validator", _after_validator)

    return g.compile()
