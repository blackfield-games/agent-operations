"""Optimization — LOD chains, instancing budgets, draw-call collapse, Nanite
fallbacks. Reads the triangle metric from every prior layer and RESOLVES the world
to the triangle budget instead of merely reporting it over: when the authored
geometry exceeds the budget it LOD-collapses the heaviest instanced (non-floor)
layers, heaviest-first, by a fixed deterministic policy — halving a layer's
triangle contribution per LOD step down to a 1/8 floor — re-checking the total
after each step until the world fits or every shed-able layer is at its floor. A
world still over budget once the shed-able geometry is exhausted is reported
over_budget (the terminal verdict the validator already treats as unrepairable).

The base terrain heightfield is the walkable surface and is never decimated, so
resolving can't punch holes in the world; a shed layer floors at 1/8, never zero,
so the world is never emptied to hit the number. The decision is recorded as
authoritative LOD directives in this layer (LIVRPS-stronger than the layers it
caps), so the emitted over_budget verdict is consistent with a real, parseable
reduction — never a number changed in isolation.

Deterministic: no model call, no randomness, no wall-clock. The same prior metrics
always yield the same LOD assignment and a byte-identical layer.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path
from common.types import WorldBrief, LayerSpec
from common.compose import STRENGTH_ORDER
from common.usd import usd_str

logger = logging.getLogger(__name__)

# Default triangle budget when no target override is configured. The real budget is
# platform- and LOD-tier-dependent (a phone and a workstation can't carry the same
# geometry), so an operator overrides it per target via BLACKFIELD_TRIANGLE_BUDGET
# (see _resolve_budget); this constant is the conservative default and stays the
# value the module emits when nothing is set, so the unconfigured pipeline is
# byte-identical to before the override existed.
TRIANGLE_BUDGET = 1_500_000

# Deepest LOD a shed-able layer may collapse to: scale 0.5**3 = 1/8 of its authored
# triangles. The floor that stops resolving from stripping a layer to nothing — a
# fully-collapsed scatter still renders at distance — so hitting the budget can
# never empty the world (the over-shed-below-usability guard).
MAX_LOD = 3

# Hard backstop on resolve passes. The loop's real bound is
# len(shed-able layers) * MAX_LOD — each pass drops exactly one layer exactly one
# LOD and no layer is dropped past MAX_LOD — so this only trips if that invariant is
# ever broken, turning a would-be runaway loop into a terminal over-budget verdict
# rather than a hang.
MAX_RESOLVE_PASSES = 256

# Specialists whose geometry is load-bearing and must never be decimated: the base
# terrain heightfield is the walkable surface, and collapsing it would punch holes
# in the world. Every other triangle contributor is instanced scatter (biome
# vegetation/debris, prop placements, npc characters) the optimizer may thin. A layer
# in this set still counts against the budget but is never reduced — the do-not-shed
# floor. Default set; an operator widens it (e.g. once structures become load-bearing
# geometry) via BLACKFIELD_GEOMETRY_FLOOR (see _resolve_floor). terrain is always
# floored regardless of the override.
GEOMETRY_FLOOR = {"terrain"}


def _resolve_budget() -> int:
    """The effective triangle budget: BLACKFIELD_TRIANGLE_BUDGET when set to a valid
    positive integer, else the module default [`TRIANGLE_BUDGET`].

    A malformed override (non-numeric, zero, negative) is REJECTED back to the default
    with a loud warning rather than raising — a typo'd target must not crash every
    region's optimization, and the default is a real conservative budget (not a 0 that
    sheds everything or an inf that sheds nothing). Reading the module global as the
    fallback keeps the existing ``monkeypatch.setattr(optimization, 'TRIANGLE_BUDGET')``
    tests authoritative when no env is set."""
    raw = os.environ.get("BLACKFIELD_TRIANGLE_BUDGET")
    if raw is None or raw.strip() == "":
        return TRIANGLE_BUDGET
    try:
        value = int(raw.strip())
    except ValueError:
        logger.warning(
            "BLACKFIELD_TRIANGLE_BUDGET=%r is not an integer; using default %d",
            raw, TRIANGLE_BUDGET,
        )
        return TRIANGLE_BUDGET
    if value <= 0:
        logger.warning(
            "BLACKFIELD_TRIANGLE_BUDGET=%d must be positive; using default %d",
            value, TRIANGLE_BUDGET,
        )
        return TRIANGLE_BUDGET
    return value


def _resolve_floor() -> set[str]:
    """The effective do-not-shed floor set: BLACKFIELD_GEOMETRY_FLOOR (a comma list of
    specialist names) when set, else the module default [`GEOMETRY_FLOOR`].

    `terrain` is ALWAYS in the floor (the walkable base is never decimable) even if the
    override omits it. Names not in the known specialist set ([`STRENGTH_ORDER`]) are
    dropped with a warning — a typo can't silently move a real layer in or out of the
    floor, and an unknown name can't quietly become a phantom floor entry."""
    raw = os.environ.get("BLACKFIELD_GEOMETRY_FLOOR")
    if raw is None or raw.strip() == "":
        return GEOMETRY_FLOOR
    names = {part.strip() for part in raw.split(",") if part.strip()}
    unknown = names - set(STRENGTH_ORDER)
    if unknown:
        logger.warning(
            "BLACKFIELD_GEOMETRY_FLOOR names unknown specialists %s; ignoring them",
            sorted(unknown),
        )
        names -= unknown
    return names | GEOMETRY_FLOOR


def _lod_scale(level: int) -> float:
    # Division by a power of two is exact in float (no rounding), so the scaled
    # triangle counts — and every budget comparison built on them — are reproducible
    # bit-for-bit across runs.
    return 1.0 / float(1 << level)


def _resolve(
    prior: list[LayerSpec],
    budget: int | None = None,
    floor: set[str] | None = None,
) -> tuple[float, float, int, list[tuple[str, str, float, int]]]:
    """Resolve the prior layers' geometry toward TRIANGLE_BUDGET.

    Returns ``(authored_total, effective_total, passes, directives)``: the pre-shed
    and post-shed triangle totals, the number of resolve passes run, and one
    ``(specialist, layer_path, authored_triangles, lod_level)`` per layer actually
    shed (lod_level > 0) in a deterministic order. ``effective_total`` is
    ``<= TRIANGLE_BUDGET`` iff the world resolved; otherwise the shed-able geometry
    was exhausted (every layer at MAX_LOD) and the world is genuinely over budget.

    The policy is fixed and deterministic — the heaviest CURRENT (post-LOD)
    contributor first, ties broken by the stable heaviest-authored-then-path order —
    so the same metrics always shed the same layers in the same order. Each pass
    halves one layer's contribution, a strict decrease, so the loop converges; it
    stops at under-budget (resolved) or when every shed-able layer sits at MAX_LOD
    (terminal). Floor-set layers are summed but never enter the candidate set.
    """
    if budget is None:
        budget = TRIANGLE_BUDGET
    if floor is None:
        floor = GEOMETRY_FLOOR
    floor_total = sum(
        float(layer.metrics.get("triangles", 0.0))
        for layer in prior
        if layer.specialist in floor
    )
    sheddable = sorted(
        (
            (layer.specialist, layer.path, float(layer.metrics.get("triangles", 0.0)))
            for layer in prior
            if layer.specialist not in floor
            and float(layer.metrics.get("triangles", 0.0)) > 0.0
        ),
        key=lambda s: (-s[2], s[1]),  # heaviest authored first, then path — a stable order
    )
    lods = [0] * len(sheddable)

    def effective() -> float:
        return floor_total + sum(
            authored * _lod_scale(lod)
            for (_, _, authored), lod in zip(sheddable, lods)
        )

    passes = 0
    while effective() > budget and passes < MAX_RESOLVE_PASSES:
        candidates = [i for i in range(len(sheddable)) if lods[i] < MAX_LOD]
        if not candidates:
            break  # every shed-able layer is at the floor LOD — nothing left to reduce
        # Heaviest current contributor; -i breaks ties toward the earlier (already
        # heaviest-authored) layer, so the choice is fully determined by the inputs.
        target = max(candidates, key=lambda i: (sheddable[i][2] * _lod_scale(lods[i]), -i))
        lods[target] += 1
        passes += 1

    authored_total = floor_total + sum(authored for _, _, authored in sheddable)
    directives = [
        (sheddable[i][0], sheddable[i][1], sheddable[i][2], lods[i])
        for i in range(len(sheddable))
        if lods[i] > 0
    ]
    return authored_total, effective(), passes, directives


def _directives_block(directives: list[tuple[str, str, float, int]]) -> str:
    """The LodDirectives prim recording each shed layer, or "" when nothing was shed.

    One child Scope per shed layer, indexed (the layer path carries `+`/`-`/`/` and
    can't be a prim name) and carrying the reduction in full: parsing the children
    back reconstructs the budget — observedTriangles equals authoredTriangles minus
    the summed (authored - effective) over these prims — so the over_budget verdict
    is checkable against the emitted layer, not taken on faith.
    """
    if not directives:
        return ""
    children = "\n".join(
        f'''        def Scope "Lod_{idx}"
        {{
            custom string specialist = {usd_str(specialist)}
            custom string layer = {usd_str(path)}
            custom int lodLevel = {lod}
            custom double lodScale = {_lod_scale(lod)}
            custom double authoredTriangles = {authored}
            custom double effectiveTriangles = {authored * _lod_scale(lod)}
        }}'''
        for idx, (specialist, path, authored, lod) in enumerate(directives)
    )
    return f'''

    def Scope "LodDirectives"
    {{
{children}
    }}'''


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    budget = _resolve_budget()
    floor = _resolve_floor()
    authored_total, effective_total, passes, directives = _resolve(prior, budget, floor)
    over_budget = effective_total > budget

    rel = f"optimization/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Optimization"
)

def Scope "Optimization"
{{
    custom int triangleBudget = {budget}
    custom double authoredTriangles = {authored_total}
    custom double observedTriangles = {effective_total}
    custom bool forcedLodCollapse = {str(bool(directives)).lower()}
    custom bool overBudget = {str(over_budget).lower()}
    custom int resolvePasses = {passes}{_directives_block(directives)}
}}
"""
    )

    shed = f", shed {len(directives)} layer(s) in {passes} pass(es)" if directives else ""
    return LayerSpec(
        specialist="optimization",
        region_id=brief.region.region_id,
        path=rel,
        summary=f"budget {effective_total:.0f}/{budget}{shed}",
        metrics={"over_budget": float(over_budget)},
    )
