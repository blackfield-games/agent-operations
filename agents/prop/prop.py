"""Prop — concrete artifacts (comms tower, convoy wreck, ammo crate clusters).
Pulled from a curated asset library; novel props go through asset agent (text-to-3D
draft → validate → retopo → UV → texture → LOD → import).
"""

from __future__ import annotations

from pathlib import Path
from common.types import WorldBrief, LayerSpec


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"prop/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Props"
)

def Xform "Props" (
    kind = "group"
)
{{
    def Xform "CommsTower" (
        references = @../../assets/library/props/comms_tower_01.usd@
    )
    {{
        double3 xformOp:translate = (0, 0, 0)
        uniform token[] xformOpOrder = ["xformOp:translate"]
    }}
}}
"""
    )
    return LayerSpec(
        specialist="prop",
        region_id=brief.region.region_id,
        path=rel,
        summary="comms tower reference placeholder",
    )
