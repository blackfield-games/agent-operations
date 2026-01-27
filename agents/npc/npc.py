"""NPC — places autonomous characters. Driven by NVIDIA ACE for cognition,
behavior-tree bridge for combat. NPC layer is the side-channel: live NPC state
streams via LiveLink/UDP (per research-openusd-pipeline.md), not USD.

This layer encodes only spawn points + faction roster.
"""

from __future__ import annotations

from pathlib import Path
from common.types import WorldBrief, LayerSpec


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"npc/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    full.write_text(
        """#usda 1.0
(
    defaultPrim = "NPCs"
)

def Xform "NPCs"
{
    def Xform "Spawns"
    {
        custom string roster = "scavenger_squad_4"
        custom string cognition = "ace://default"
    }
}
"""
    )
    return LayerSpec(
        specialist="npc",
        region_id=brief.region.region_id,
        path=rel,
        summary="scavenger spawns, ACE cognition handle",
    )
