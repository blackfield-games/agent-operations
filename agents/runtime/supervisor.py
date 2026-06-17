"""LangGraph supervisor — routes a WorldBrief through the 8 specialists in order:

    director → terrain → biome → prop → lighting → npc → optimization → validator

Each specialist returns a LayerSpec (or None if it has nothing to contribute).
Validator runs last and produces a verdict; on rejection the supervisor can
re-route to whichever specialist owns the failure.

This file is the in-process plan. For production, each node is wrapped as a
Temporal activity (see runtime/workflow.py) so individual steps survive crashes.
"""

from __future__ import annotations
import asyncio
import logging
import os
import re
from typing import TypedDict, Annotated

from langgraph.graph import StateGraph, END

from common.types import WorldBrief, LayerSpec, ValidatorVerdict
from director import director
from terrain import terrain
from biome import biome
from prop import prop
from lighting import lighting
from npc import npc
from optimization import optimization
from validator import validator

logger = logging.getLogger(__name__)

# A specialist or validator node that HANGS (not just one that raises) must not
# stall the whole region compose. Each node await is bounded by this timeout
# (seconds); on expiry the node is isolated exactly like a raising node. Tunable
# via env for slow/fast backends; tests monkeypatch it to a tiny value.
NODE_TIMEOUT_SECS = float(os.environ.get("AGENT_NODE_TIMEOUT_SECS", "120"))

# Hard cap on validator rounds. After this many validate → route-back →
# revalidate cycles, a still-rejected region ENDs with its last non-accepted
# verdict instead of recursing forever — a persistently-failing stage must not
# hang the pipeline. Recursion guard from research-agent-runtime.md.
MAX_ROUNDS = 3


def merge_layers(existing: list[LayerSpec], new: list[LayerSpec]) -> list[LayerSpec]:
    """Reducer for State.layers: concatenate, then keep only the latest layer
    per (specialist, region_id).

    A validator route-back re-enters an upstream node, and the static edge chain
    then replays every downstream specialist before the validator runs again (see
    the PIPELINE_ORDER note below). With a plain operator.add reducer those replays
    would append a SECOND LayerSpec for the same (specialist, region_id), and
    compose_world — which groups by specialist — would emit duplicate sublayers.
    De-duping here keeps the fresh re-run layer (latest wins) and drops the stale
    one. Insertion order is preserved for first-seen layers; compose_world re-sorts
    by STRENGTH_ORDER anyway, so the order of this list does not matter.
    """
    merged: dict[tuple[str, str], LayerSpec] = {}
    for layer in [*existing, *new]:
        merged[(layer.specialist, layer.region_id)] = layer  # latest wins
    return list(merged.values())


class State(TypedDict, total=False):
    brief: WorldBrief
    layers: Annotated[list[LayerSpec], merge_layers]
    verdict: ValidatorVerdict
    route_back: str  # specialist name if validator wants a re-run
    rounds: int
    validator_errored: bool  # set when the validator node itself raised


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
        try:
            layer = await asyncio.wait_for(
                fn(state["brief"], state.get("layers", [])),
                timeout=NODE_TIMEOUT_SECS,
            )
        except asyncio.TimeoutError:
            # A hung specialist is contained like a raising one: log and
            # contribute no sublayer so the validator can route back to it.
            logger.warning(
                "specialist %r timed out after %ss; isolating as an empty sublayer",
                name, NODE_TIMEOUT_SECS,
            )
            return {"layers": []}
        except Exception:
            # A specialist raising must not tank the whole region compose: log it
            # and contribute no sublayer. The validator then sees the missing
            # layer and can route back to this specialist on the next round.
            logger.exception("specialist %r failed; isolating as an empty sublayer", name)
            return {"layers": []}
        return {"layers": [layer] if layer else []}
    return node


# Pipeline order = the static edge chain director → … → optimization. Re-entering
# a node replays every downstream specialist before the validator runs again, so
# routing back to the *earliest* failing node repairs upstream causes first.
PIPELINE_ORDER = list(SPECIALISTS)


def _failing_specialist(issues: list[str]) -> str:
    r"""Map validator issues to the earliest pipeline node that should re-run.

    The validator phrases issues with the offending specialist's name, e.g.
    "missing specialist layers: ['biome', 'prop']" or "triangle budget exceeded
    — re-run optimization with stricter LODs". We scan for those names and route
    back to whichever appears earliest in the pipeline. Unattributable failures
    (e.g. a pure style rejection naming no specialist) fall back to director,
    which owns the world brief / style intent.

    Match on word boundaries, not substrings: the composition-conflict gate embeds
    prim paths in its issues (`</World/terrain_lod/Lamp>`), and a plain substring
    scan routes that conflict to `terrain` — an innocent upstream node — because
    "terrain" sits inside "terrain_lod". `_` is a regex word char, so `\bterrain\b`
    skips `terrain_lod`/`propeller`/`npcs` while still matching the explicitly named
    culprits and punctuation-delimited names (`['biome']`, `terrain/r…`). A prim path
    whose segment is *exactly* a specialist name (`/World/Terrain`) still matches —
    a narrow, accepted residual.
    """
    candidates = {
        name
        for issue in issues
        for name in PIPELINE_ORDER
        if re.search(rf"\b{re.escape(name)}\b", issue.lower())
    }
    if not candidates:
        return "director"
    return min(candidates, key=PIPELINE_ORDER.index)


async def _validator_node(state: State) -> dict:
    try:
        verdict = await asyncio.wait_for(
            validator.run(state["brief"], state.get("layers", [])),
            timeout=NODE_TIMEOUT_SECS,
        )
    except asyncio.TimeoutError:
        # A hung validator is contained like a raising one: end the compose with
        # a non-accepted verdict. Re-running can't fix an unresponsive validator,
        # so validator_errored makes _after_validator END (no route-back).
        logger.warning(
            "validator timed out after %ss; ending compose with a non-accepted verdict",
            NODE_TIMEOUT_SECS,
        )
        return {
            "verdict": ValidatorVerdict(accepted=False, issues=["validator timed out"]),
            "rounds": state.get("rounds", 0) + 1,
            "validator_errored": True,
        }
    except Exception:
        # A crashing validator (e.g. the Dev C validation service is
        # unavailable) must not propagate and tank the region compose —
        # mirror the specialist isolation. Record a non-accepted verdict and
        # flag the error so _after_validator ENDs gracefully: re-running the
        # pipeline can't fix an unavailable validator, so we don't route back.
        logger.exception("validator failed; ending compose with a non-accepted verdict")
        return {
            "verdict": ValidatorVerdict(accepted=False, issues=["validator errored"]),
            "rounds": state.get("rounds", 0) + 1,
            "validator_errored": True,
        }
    return {"verdict": verdict, "rounds": state.get("rounds", 0) + 1}


def _after_validator(state: State) -> str:
    verdict = state.get("verdict")
    if not verdict:
        return END
    if state.get("validator_errored"):  # validator itself raised — END, don't loop
        return END
    if verdict.accepted:
        return END
    if state.get("rounds", 0) >= MAX_ROUNDS:  # recursion guard, see MAX_ROUNDS
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
