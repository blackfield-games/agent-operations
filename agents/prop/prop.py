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


def _must_have_from_director(prior: list[LayerSpec]) -> list[str]:
    """The hero assets the director marked ``intent:must_have`` for this region, parsed
    off the director layer as bare intent tokens (the [`npc._roster_from_director`]
    idiom). Empty when the director contributed no layer (it failed/timed out — prop
    then places only the hash fill) or its layer predates must_have. DEGRADES to empty
    rather than raising on a missing/unreadable layer: a raise would make the supervisor
    isolate prop as an empty sublayer, dropping the region's fill AND its hero assets
    over a director hiccup. The tokens are bare identifiers (no escaped quote the simple
    capture would miss); prop still never trusts a token it can't place — the caller
    bridges every one through [`_required_assets`]."""
    director = next((layer for layer in prior if layer.specialist == "director"), None)
    if director is None:
        return []
    path = Path("layers") / director.path
    if not path.is_file():
        return []
    match = re.search(r'intent:must_have\s*=\s*"([^"]*)"', path.read_text())
    if not match or not match.group(1):
        return []
    return [token for token in match.group(1).split(",") if token]


def _required_assets(tokens: list[str]) -> list[str]:
    """The library assets prop must guarantee for `tokens`, bridged through
    [`MUST_HAVE_ASSET`] in order. A token with NO mapping is SKIPPED with a warning —
    not raised, and never silently 0-budgeted: an unknown director token is vocabulary
    drift prop should survive (placing every asset it CAN, logging the gap) rather than
    isolating its whole layer as empty (the FM3 degrade-don't-raise posture). A MAPPED
    asset is always budgetable — [`MUST_HAVE_ASSET`] values are a pinned subset of
    [`ASSET_TRIS`] — so the caller's [`_asset_tris`] never fires on a must-have; that
    loud guard is reserved for prop's own table going inconsistent, not director input.
    Order is preserved (no dedup), mirroring [`_must_have_from_director`]; duplicates
    become distinct, honestly-counted placements."""
    assets: list[str] = []
    for token in tokens:
        asset = MUST_HAVE_ASSET.get(token)
        if asset is None:
            logger.warning(
                "prop: director intent:must_have token %r has no MUST_HAVE_ASSET "
                "mapping; skipping (region will not get this hero asset)",
                token,
            )
            continue
        assets.append(asset)
    return assets


def _required_block(assets: list[str]) -> str:
    """The guaranteed hero placements as root ``Xform`` prims, or ``""`` when there are
    none — so a region with no director must_have emits a layer byte-identical to the
    hash-only fill (the back-compat floor the existing tests pin). Prims are INDEX-named
    (``Required_0`` …): an asset name is not guaranteed a unique/valid prim identifier,
    and indexing keeps the layer well-formed even if two tokens resolve to the same
    asset. Each carries the asset BOTH as a ``references`` arc (real geometry the
    optimizer budgets) and a parseable ``requiredAsset`` attribute, and sits beside the
    fill ``PointInstancer`` as a sibling root prim (compose sublayers by path, so extra
    roots compose cleanly via LIVRPS)."""
    if not assets:
        return ""
    prims = "\n\n".join(
        f'''def Xform "Required_{idx}" (
    references = @../../assets/library/props/{asset}.usd@
)
{{
    custom string requiredAsset = {usd_str(asset)}
    double3 xformOp:translate = (0, 0, 0)
    uniform token[] xformOpOrder = ["xformOp:translate"]
}}'''
        for idx, asset in enumerate(assets)
    )
    return "\n" + prims + "\n"


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"prop/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    count = _placement_count(brief.region.region_id)
    fill_asset = _select_asset(brief.region.region_id)
    required = _required_assets(_must_have_from_director(prior))
    # Every placed asset is metered: the hash-selected scatter (count instances) plus
    # one of each director-required hero. Summing them into the single triangles metric
    # keeps the optimizer's budget honest — a must-have placed in the USD but unmetered
    # would hide geometry the validator's budget sum must see.
    tris = count * _asset_tris(fill_asset) + sum(_asset_tris(asset) for asset in required)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Props"
)

def PointInstancer "Props"
{{
    custom string propAsset = {usd_str(fill_asset)}
    custom int placementCount = {count}

    def Xform "Prototype" (
        references = @../../assets/library/props/{fill_asset}.usd@
    )
    {{
        double3 xformOp:translate = (0, 0, 0)
        uniform token[] xformOpOrder = ["xformOp:translate"]
    }}
}}
{_required_block(required)}"""
    )
    summary = f"{count} × {fill_asset}"
    if required:
        summary += f" + must-have {', '.join(required)}"
    return LayerSpec(
        specialist="prop",
        region_id=brief.region.region_id,
        path=rel,
        summary=summary,
        metrics={"triangles": float(tris)},
    )
