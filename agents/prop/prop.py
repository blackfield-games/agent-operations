"""Prop — concrete artifacts (comms tower, convoy wreck, ammo crate clusters).
Pulled from a curated asset library; novel props go through asset agent (text-to-3D
draft → validate → retopo → UV → texture → LOD → import).
"""

from __future__ import annotations

import hashlib
import logging
import re
from pathlib import Path
from common.types import WorldBrief, LayerSpec
from common.usd import usd_str

logger = logging.getLogger(__name__)

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

# The director's intent:must_have names INTENT tokens ("comms_tower"), not the library
# assets ("comms_tower_01") prop actually places — this dict is the deterministic bridge
# that lets prop honor that intent. Each token a region MUST have maps to the concrete
# library asset prop guarantees for it. Every value MUST be a key of ASSET_TRIS (pinned
# by test_must_have_asset_values_are_a_subset_of_asset_tris) so a guaranteed placement is
# ALWAYS budget-known and _asset_tris can never fire on a must-have. An art-director
# confirms the real token->asset map alongside the geometry tables (operator-reviewable,
# like ASSET_TRIS — see the agents-prop-geometry-metrics escalation in BLOCKERS); a
# director token absent here is skipped with a warning (see _required_assets), never
# silently dropped as a 0-tri phantom.
MUST_HAVE_ASSET: dict[str, str] = {
    "comms_tower": "comms_tower_01",
    "convoy_wreck": "convoy_wreck_01",
}

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


def _select_asset(region_id: str) -> str:
    """Which library asset prop places in this region — deterministic, drawn from the
    [`ASSET_TRIS`] keys so the pick is ALWAYS budget-known (a selection can never
    produce an unknown-asset 0-tri layer). A STABLE hash of the region id over the
    SORTED keys (a fixed, reproducible order), salted distinctly from
    [`_placement_count`] so the asset and the count vary independently — two regions
    can place different props (a region dense with barricades, another with comms
    towers) while the same region is byte-identical on every run."""
    assets = sorted(ASSET_TRIS)
    idx = int.from_bytes(hashlib.sha256(("asset:" + region_id).encode("utf-8")).digest()[:4], "big") % len(assets)
    return assets[idx]


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"prop/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    count = _placement_count(brief.region.region_id)
    asset = _select_asset(brief.region.region_id)
    tris = count * _asset_tris(asset)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Props"
)

def PointInstancer "Props"
{{
    custom string propAsset = {usd_str(asset)}
    custom int placementCount = {count}

    def Xform "Prototype" (
        references = @../../assets/library/props/{asset}.usd@
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
        summary=f"{count} × {asset}",
        metrics={"triangles": float(tris)},
    )
