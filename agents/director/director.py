"""Director — sets the design intent for a region. Weakest layer (base intent).

Writes a sparse USD layer that establishes:
  - region bounds + grid coords
  - faction control / story beats present in this region
  - "must-haves": comms tower, downed convoy, civilian extraction point
  - "must-nots": vegetation density caps, no enclosed interiors (frontier rule)

Other specialists override; validator has final say.
"""

from __future__ import annotations

from pathlib import Path
from common.types import WorldBrief, LayerSpec

LAYERS_DIR = Path("layers/director")


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"director/{brief.region.region_id}.usda"
    full = LAYERS_DIR.parent / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Director"
    customLayerData = {{
        string aesthetic = "{brief.aesthetic}"
        string biome = "{brief.biome}"
        string region_id = "{brief.region.region_id}"
    }}
)

def Scope "Director"
{{
    custom string intent:beats = "scorched. abandoned. recent conflict."
    custom string intent:must_have = "comms_tower,convoy_wreck"
    custom string intent:must_not = "interior_volumes,civilians"
}}
"""
    )
    return LayerSpec(
        specialist="director",
        region_id=brief.region.region_id,
        path=rel,
        summary=f"intent for {brief.region.region_id} in {brief.biome}",
    )
