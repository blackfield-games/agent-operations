"""Lighting — sky dome, sun direction, time-of-day, fog density. Cinematic
calibration drives the scorched-modern feel. Overcast + inferno-orange rim is
the locked palette per DECISIONS.md.
"""

from __future__ import annotations

from pathlib import Path
from common.types import WorldBrief, LayerSpec


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"lighting/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    full.write_text(
        """#usda 1.0
(
    defaultPrim = "Lighting"
)

def Xform "Lighting"
{
    def DomeLight "Sky"
    {
        float inputs:intensity = 0.4
        color3f inputs:color = (0.55, 0.58, 0.62)
        custom string mood = "overcast"
    }
    def DistantLight "Sun"
    {
        float inputs:intensity = 1.2
        color3f inputs:color = (1.0, 0.62, 0.32)
        float inputs:angle = 0.53
        custom string mood = "inferno_rim"
    }
}
"""
    )
    return LayerSpec(
        specialist="lighting",
        region_id=brief.region.region_id,
        path=rel,
        summary="overcast sky + inferno-orange rim",
    )
