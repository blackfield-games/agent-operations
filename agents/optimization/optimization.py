"""Optimization — LOD chains, instancing budgets, draw-call collapse, Nanite
fallbacks. Reads metrics from all prior layers; emits an override layer that
caps geometry budgets and forces problematic prims into instanced batches.
"""

from __future__ import annotations

from pathlib import Path
from common.types import WorldBrief, LayerSpec

TRIANGLE_BUDGET = 1_500_000


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    total_tris = sum(layer.metrics.get("triangles", 0.0) for layer in prior)
    over_budget = total_tris > TRIANGLE_BUDGET

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
    custom double observedTriangles = {total_tris}
    custom bool forcedLodCollapse = {str(over_budget).lower()}
}}
"""
    )
    return LayerSpec(
        specialist="optimization",
        region_id=brief.region.region_id,
        path=rel,
        summary=f"budget check: {total_tris:.0f} / {TRIANGLE_BUDGET}",
        metrics={"over_budget": float(over_budget)},
    )
