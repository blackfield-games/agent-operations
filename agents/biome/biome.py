"""Biome — scatter vegetation, debris, weather profiles for the region's
material identity. SpeedTree + UE PCG via MCP for the real impl.
"""

from __future__ import annotations

import hashlib
import re
from pathlib import Path
from common.types import WorldBrief, LayerSpec

# Triangles in one scatter instance's low-poly source mesh (a foliage card cluster
# or debris chunk, pre-LOD). A content-budget knob, not a measured value — the
# operator/art-director confirms it before it drives a real PCG/render budget (see
# the agents-biome-geometry-metrics escalation in BLOCKERS).
TRIS_PER_INSTANCE = 240

# Base scatter instances a biome places per region, keyed by a vegetation keyword
# in the free-form biome string. The DOMINANT matched keyword sets the density
# (max, not sum — a "scorched_grassland" scatters like a grassland; "scorched" is
# the palette, not extra geometry). Sparse post-conflict keywords scatter little;
# the vegetated ones scatter densely enough that, at the upper end of the per-region
# jitter, a region's geometry exceeds the optimizer's TRIANGLE_BUDGET and must be
# LOD-shed — which is the whole reason geometry is routed through optimization. A
# biome string with no matched keyword falls to DEFAULT_DENSITY.
BIOME_DENSITY: dict[str, int] = {
    "jungle": 4500,
    "forest": 4000,
    "overgrown": 4200,
    "wetland": 3000,
    "grassland": 2000,
    "savanna": 1800,
    "tundra": 900,
    "desert": 500,
    "ruins": 800,
    "urban": 600,
    "industrial": 450,
    "scorched": 300,
}
DEFAULT_DENSITY = 1200


def _scatter_count(biome: str, region_id: str) -> int:
    """How many scatter instances biome places in this region — deterministic, so a
    pipeline re-run (and a validation-revision re-run of this stage) reproduces the
    layer byte-for-byte.

    Base density is the DOMINANT vegetation keyword in the biome string (max over the
    matched keywords, DEFAULT_DENSITY if none match), modulated by a per-region
    density jitter in [1.00, 2.00). The jitter is a STABLE hash of the region id
    (hashlib, never the per-process-salted builtin ``hash()``), so two regions of the
    same biome differ — giving the optimizer a budget that some regions cross and
    others do not — while the same region is identical on every run.
    """
    tokens = [t for t in re.split(r"[^a-z0-9]+", biome.lower()) if t]
    base = max((BIOME_DENSITY[t] for t in tokens if t in BIOME_DENSITY), default=DEFAULT_DENSITY)
    jitter_milli = 1000 + int.from_bytes(hashlib.sha256(region_id.encode("utf-8")).digest()[:4], "big") % 1000
    return base * jitter_milli // 1000


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"biome/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    count = _scatter_count(brief.biome, brief.region.region_id)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Biome"
)

def PointInstancer "Scatter"
{{
    custom string biome = "{brief.biome}"
    custom int instanceCount = {count}
    custom string scatterRule = "post_conflict_sparse"
}}
"""
    )
    return LayerSpec(
        specialist="biome",
        region_id=brief.region.region_id,
        path=rel,
        summary=f"{count} scatter instances",
        metrics={"triangles": float(count * TRIS_PER_INSTANCE)},
    )
