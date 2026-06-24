"""Terrain — heightfield + erosion + slope masks. Backed by Houdini MCP server.

For the wedge demo this emits a placeholder USD layer; real impl calls Houdini
via MCP for heightfield generation, exports as USD volumes / mesh prims.
"""

from __future__ import annotations

import hashlib
from pathlib import Path
from common.types import WorldBrief, LayerSpec
from common.usd import usd_str

# Per-side vertex count of the heightfield grid before the per-region jitter. An
# operator/art-director content-budget knob like biome's density table, not a measured
# value (see the agents-* geometry-metrics escalation in BLOCKERS).
BASE_GRID_RESOLUTION = 512


def _grid_resolution(region_id: str) -> int:
    """The heightfield's per-side vertex count for this region — deterministic, so a
    pipeline re-run (and a validation-revision re-run) reproduces the layer byte-for-byte.

    BASE_GRID_RESOLUTION scaled by a stable per-region jitter in [1.00, 1.50). The jitter
    is a STABLE hash of the region id (hashlib, never the per-process-salted builtin
    ``hash()``), so two regions get different resolutions — and thus different triangle
    loads, giving the optimizer a terrain contribution that varies region to region —
    while the same region is identical on every run. The band is deliberately TIGHTER than
    the scatter layers' [1.00, 2.00): terrain is the optimizer's un-sheddable floor, so its
    resolution is bounded to keep the heightfield's own triangle count under the
    TRIANGLE_BUDGET (a floor that alone exceeded budget would make a region terminal by
    construction). Within this band terrain still varies enough that its contribution can
    tip a region's TOTAL over budget — the swing a constant floor could never provide.
    """
    jitter_milli = 1000 + int.from_bytes(hashlib.sha256(region_id.encode("utf-8")).digest()[:4], "big") % 500
    return BASE_GRID_RESOLUTION * jitter_milli // 1000


def _heightfield_triangles(grid_resolution: int) -> int:
    """Triangles a heightfield triangulates to over its grid: each of the (N-1)^2 quad
    cells splits into two, so ~2*N^2. COUPLED to the declared resolution, so the
    gridResolution the layer writes and the triangle count it meters can never drift —
    the bug a hardcoded 262144 (regardless of the declared 512) baked in."""
    return 2 * (grid_resolution - 1) ** 2


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"terrain/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    grid = _grid_resolution(brief.region.region_id)
    triangles = _heightfield_triangles(grid)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Terrain"
)

def Mesh "Terrain" (
    kind = "component"
)
{{
    custom string biome = {usd_str(brief.biome)}
    custom int gridResolution = {grid}
    custom string heightfieldSource = "placeholder"
}}
"""
    )
    return LayerSpec(
        specialist="terrain",
        region_id=brief.region.region_id,
        path=rel,
        summary=f"{grid}x{grid} heightfield mesh",
        metrics={"triangles": float(triangles)},
    )
