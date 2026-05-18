"""Terrain — heightfield + erosion + slope masks. Backed by Houdini MCP server.

For the wedge demo this emits a placeholder USD layer; real impl calls Houdini
via MCP for heightfield generation, exports as USD volumes / mesh prims.
"""

from __future__ import annotations

from pathlib import Path
from common.types import WorldBrief, LayerSpec


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"terrain/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Terrain"
)

def Mesh "Terrain" (
    kind = "component"
)
{{
    custom string biome = "{brief.biome}"
    custom int gridResolution = 512
    custom string heightfieldSource = "placeholder"
}}
"""
    )
    return LayerSpec(
        specialist="terrain",
        region_id=brief.region.region_id,
        path=rel,
        summary="placeholder heightfield mesh",
        metrics={"triangles": 262144.0},
    )
