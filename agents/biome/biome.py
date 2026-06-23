"""Biome — scatter vegetation, debris, weather profiles for the region's
material identity. SpeedTree + UE PCG via MCP for the real impl.
"""

from __future__ import annotations

import hashlib
import re
from pathlib import Path
from common.types import WorldBrief, LayerSpec
from common.usd import usd_str

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

# Vegetation-density ceiling biome clamps the BASE density to when the director's
# intent:must_not forbids overgrowth (the frontier "no dense vegetation" rule the
# director names as a must_not). A region the director marks dense_vegetation scatters
# no denser than this at the base — so even a jungle/forest brief in a scorched
# frontier reads sparse, the director's constraint overriding the raw biome keyword.
# Set BELOW the vegetated densities (grassland 2000 … jungle 4500) so the cap actually
# bites, and at/above the sparse post-conflict ones (scorched 300, desert 500, ruins
# 800) so a biome already under it is unchanged — the cap is a CEILING (min), never an
# increase. An operator-reviewable content/balance knob like BIOME_DENSITY; an
# art-director confirms it before it drives a real PCG budget (see the
# agents-biome-geometry-metrics escalation in BLOCKERS).
VEGETATION_CAP_DENSITY = 800

# The intent:must_not tokens biome reads as "cap vegetation density." A set so the
# director can grow the vocabulary (a future "overgrowth") without touching the cap
# logic; a must_not token NOT in here (interior_volumes, civilians — other specialists'
# constraints) is simply not a cap token, never an error. The director seeds at least
# one of these (test_director_seeds_a_biome_cap_token pins the contract) so the flow
# actually fires.
MUST_NOT_VEGETATION_CAP: frozenset[str] = frozenset({"dense_vegetation"})


def _must_not_from_director(prior: list[LayerSpec]) -> list[str]:
    """The constraints the director marked ``intent:must_not`` for this region, parsed
    off the director layer as bare tokens (the [`prop._must_have_from_director`] idiom).
    Empty when the director contributed no layer (it failed/timed out — biome then
    scatters at the raw biome density) or its layer predates the field. DEGRADES to
    empty rather than raising on a missing/unreadable layer: a raise would make the
    supervisor isolate biome as an empty sublayer, dropping the region's whole scatter
    over a director hiccup. biome reads these only to decide whether to CAP — an
    unrecognized token (a constraint owned by another specialist) is simply not a cap
    token (see [`_caps_vegetation`])."""
    director = next((layer for layer in prior if layer.specialist == "director"), None)
    if director is None:
        return []
    path = Path("layers") / director.path
    if not path.is_file():
        return []
    match = re.search(r'intent:must_not\s*=\s*"([^"]*)"', path.read_text())
    if not match or not match.group(1):
        return []
    return [token for token in match.group(1).split(",") if token]


def _caps_vegetation(must_not: list[str]) -> bool:
    """Whether the director's must_not forbids dense vegetation — true when any token is
    in [`MUST_NOT_VEGETATION_CAP`]. The other must_not tokens (interior_volumes,
    civilians) are other specialists' constraints and don't gate biome's density."""
    return any(token in MUST_NOT_VEGETATION_CAP for token in must_not)


def _scatter_count(biome: str, region_id: str, cap: int | None = None) -> int:
    """How many scatter instances biome places in this region — deterministic, so a
    pipeline re-run (and a validation-revision re-run of this stage) reproduces the
    layer byte-for-byte.

    Base density is the DOMINANT vegetation keyword in the biome string (max over the
    matched keywords, DEFAULT_DENSITY if none match), modulated by a per-region
    density jitter in [1.00, 2.00). The jitter is a STABLE hash of the region id
    (hashlib, never the per-process-salted builtin ``hash()``), so two regions of the
    same biome differ — giving the optimizer a budget that some regions cross and
    others do not — while the same region is identical on every run.

    ``cap``, when set, clamps the BASE density to it (a CEILING via ``min`` — never an
    increase) BEFORE the jitter: the director's ``intent:must_not`` overriding the biome
    keyword so a capped region scatters no denser than ``cap`` at the base, while the
    per-region jitter still varies it within that ceiling (so the optimizer keeps its
    over/under-budget spread even under the cap).
    """
    tokens = [t for t in re.split(r"[^a-z0-9]+", biome.lower()) if t]
    base = max((BIOME_DENSITY[t] for t in tokens if t in BIOME_DENSITY), default=DEFAULT_DENSITY)
    if cap is not None:
        base = min(base, cap)
    jitter_milli = 1000 + int.from_bytes(hashlib.sha256(region_id.encode("utf-8")).digest()[:4], "big") % 1000
    return base * jitter_milli // 1000


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"biome/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    capped = _caps_vegetation(_must_not_from_director(prior))
    count = _scatter_count(brief.biome, brief.region.region_id, VEGETATION_CAP_DENSITY if capped else None)
    # Emit the cap marker ONLY when it fires, so a region with no director must_not is
    # byte-identical to the pre-cap scatter (the back-compat floor the existing tests
    # pin). The capped count — not the raw density — is what's metered, so the
    # optimizer's budget sees the real, reduced geometry.
    cap_line = "\n    custom bool vegetationCapped = true" if capped else ""
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Biome"
)

def PointInstancer "Scatter"
{{
    custom string biome = {usd_str(brief.biome)}
    custom int instanceCount = {count}
    custom string scatterRule = "post_conflict_sparse"{cap_line}
}}
"""
    )
    summary = f"{count} scatter instances"
    if capped:
        summary += " (vegetation capped)"
    return LayerSpec(
        specialist="biome",
        region_id=brief.region.region_id,
        path=rel,
        summary=summary,
        metrics={"triangles": float(count * TRIS_PER_INSTANCE)},
    )
