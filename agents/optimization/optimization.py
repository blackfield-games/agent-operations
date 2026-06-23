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

from pathlib import Path
from common.types import WorldBrief, LayerSpec
from common.usd import usd_str

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
# vegetation/debris) the optimizer may thin. A layer in this set still counts
# against the budget but is never reduced — the do-not-shed floor.
GEOMETRY_FLOOR = {"terrain"}


def _lod_scale(level: int) -> float:
    # Division by a power of two is exact in float (no rounding), so the scaled
    # triangle counts — and every budget comparison built on them — are reproducible
    # bit-for-bit across runs.
    return 1.0 / float(1 << level)


def _resolve(
    prior: list[LayerSpec],
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
    floor_total = sum(
        float(layer.metrics.get("triangles", 0.0))
        for layer in prior
        if layer.specialist in GEOMETRY_FLOOR
    )
    sheddable = sorted(
        (
            (layer.specialist, layer.path, float(layer.metrics.get("triangles", 0.0)))
            for layer in prior
            if layer.specialist not in GEOMETRY_FLOOR
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
    while effective() > TRIANGLE_BUDGET and passes < MAX_RESOLVE_PASSES:
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
    authored_total, effective_total, passes, directives = _resolve(prior)
    over_budget = effective_total > TRIANGLE_BUDGET

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
    custom int triangleBudget = {TRIANGLE_BUDGET}
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
        summary=f"budget {effective_total:.0f}/{TRIANGLE_BUDGET}{shed}",
        metrics={"over_budget": float(over_budget)},
    )
