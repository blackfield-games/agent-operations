"""Biome — scatter vegetation, debris, weather profiles for the region's
material identity. SpeedTree + UE PCG via MCP for the real impl.
"""

from __future__ import annotations

from pathlib import Path
from common.types import WorldBrief, LayerSpec


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"biome/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Biome"
)

def PointInstancer "Scatter"
{{
    custom string biome = "{brief.biome}"
    custom int instanceCount = 0
    custom string scatterRule = "post_conflict_sparse"
}}
"""
    )
    return LayerSpec(
        specialist="biome",
        region_id=brief.region.region_id,
        path=rel,
        summary="empty scatter — populated by Houdini PCG pass",
    )
