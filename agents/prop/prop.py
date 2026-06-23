"""Prop — concrete artifacts (comms tower, convoy wreck, ammo crate clusters).
Pulled from a curated asset library; novel props go through asset agent (text-to-3D
draft → validate → retopo → UV → texture → LOD → import).
"""

from __future__ import annotations

import hashlib
from pathlib import Path
from common.types import WorldBrief, LayerSpec
from common.usd import usd_str

# Triangles in one placed prop's source mesh (pre-LOD), keyed by the library asset.
# A content-budget TABLE, not a single per-layer constant — different props carry
# wildly different geometry (a comms tower dwarfs an ammo crate), so the triangle
# count must scale with WHICH asset is placed, not just how many. An art-director
# confirms the real per-asset budgets before they drive a real render budget (see the
# agents-prop-geometry-metrics escalation in BLOCKERS). Every asset prop places MUST
# appear here: an unknown asset raises rather than silently contributing 0 triangles,
# which would hide its geometry from the optimizer's budget.
ASSET_TRIS: dict[str, int] = {
    "comms_tower_01": 12000,
    "fuel_depot_01": 9000,
    "convoy_wreck_01": 8000,
    "barricade_01": 1500,
    "ammo_crate_01": 600,
}

# The library asset prop instances across a region. One asset for the deterministic
# placeholder; the real impl picks per-brief from the asset agent's library.
PROP_ASSET = "comms_tower_01"

# Base prop placements per region before the per-region jitter. Props are discrete
# artifacts — far sparser than scatter vegetation — but a heavy hero asset
# (comms_tower_01 at 12k tris) still makes prop a real shed-able budget contributor,
# so a distant-prop LOD/cull pass has something to cut. Low enough that a default
# region stays well under TRIANGLE_BUDGET alongside terrain + biome.
BASE_PLACEMENTS = 20


def _asset_tris(asset: str) -> int:
    """Per-asset triangle budget. Raises on an asset absent from [`ASSET_TRIS`] rather
    than defaulting to 0 — a silent 0 would drop the prop's geometry from the
    optimizer's triangle budget (a phantom-clean under-count), so an unconfigured
    asset must fail loudly at emission, not ship an invisible-to-the-budget layer."""
    if asset not in ASSET_TRIS:
        raise KeyError(f"prop asset {asset!r} has no triangle budget in ASSET_TRIS")
    return ASSET_TRIS[asset]


def _placement_count(region_id: str) -> int:
    """How many props prop places in this region — deterministic, so a pipeline re-run
    (and a validation-revision re-run of this stage) reproduces the layer byte-for-byte.

    A per-region density jitter in [1.00, 2.00) scales [`BASE_PLACEMENTS`]. The jitter
    is a STABLE hash of the region id (hashlib, never the per-process-salted builtin
    ``hash()``), so two regions differ — giving the optimizer a budget that some
    regions cross and others do not — while the same region is identical on every run.
    """
    jitter_milli = 1000 + int.from_bytes(hashlib.sha256(region_id.encode("utf-8")).digest()[:4], "big") % 1000
    return BASE_PLACEMENTS * jitter_milli // 1000


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"prop/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    count = _placement_count(brief.region.region_id)
    tris = count * _asset_tris(PROP_ASSET)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Props"
)

def PointInstancer "Props"
{{
    custom string propAsset = {usd_str(PROP_ASSET)}
    custom int placementCount = {count}

    def Xform "Prototype" (
        references = @../../assets/library/props/{PROP_ASSET}.usd@
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
        summary=f"{count} × {PROP_ASSET}",
        metrics={"triangles": float(tris)},
    )
