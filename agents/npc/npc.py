"""NPC — places autonomous characters (scavenger squads, wildlife, sentinels).
Driven by NVIDIA ACE for cognition, behavior-tree bridge for combat. Live NPC
state streams via LiveLink/UDP (per research-openusd-pipeline.md), not USD — but
the spawned characters' MESHES are real geometry the optimizer must budget, so this
layer declares a deterministic per-region spawn count and the triangles it implies
alongside the faction archetype + cognition handle.
"""

from __future__ import annotations

import hashlib
import re
from pathlib import Path
from common.types import WorldBrief, LayerSpec
from common.usd import usd_str

# Triangles in one spawned character's source mesh (pre-LOD), keyed by archetype.
# A content-budget TABLE, not a single per-layer constant — a heavy sentinel mech
# dwarfs a drone, so the triangle count must scale with WHICH archetype spawns, not
# just how many. A rigged, skinned character also carries more geometry than a static
# prop. An art-director confirms the real per-archetype budgets before they drive a
# real render budget (see the agents-npc-geometry-metrics escalation in BLOCKERS).
# Every archetype npc spawns MUST appear here: an unknown archetype raises rather
# than silently contributing 0 triangles, which would hide its geometry from the
# optimizer's budget.
CHARACTER_TRIS: dict[str, int] = {
    "sentinel": 40000,
    "raider": 24000,
    "scavenger": 18000,
    "civilian": 12000,
    "drone": 6000,
}

# The FALLBACK archetype npc spawns when the director seeded no faction roster for the
# region — the director contributed no layer (it failed/timed out) or a record predates
# intent:factions. When a roster IS present, _select_archetype draws from it instead.
# Kept a budgeted archetype so npc always emits a non-empty, budget-known layer.
NPC_ARCHETYPE = "scavenger"

# Director intent:must_not tokens that forbid an npc archetype outright — the token→
# archetype bridge (the [`prop.MUST_HAVE_ASSET`] idiom). The frontier bars civilians via
# the director's intent:must_not, so npc must never field the "civilian" archetype there
# even when a desynced roster names it. The token is the PLURAL "civilians"; the archetype
# the SINGULAR "civilian" — the bridge maps them. A must_not token absent here is another
# specialist's constraint (dense_vegetation is biome's cap; interior_volumes is no
# archetype) and does not gate npc's pick (see [`_forbidden_archetypes`]).
MUST_NOT_ARCHETYPE: dict[str, str] = {"civilians": "civilian"}

# Base character spawns per region before the per-region jitter. Characters are
# discrete actors — far sparser than scatter vegetation — but a populated region of
# rigged scavengers (18k tris each) is still a real shed-able budget contributor, so
# a distant-NPC LOD/cull pass has something to cut. Low enough that a default region
# stays well under TRIANGLE_BUDGET alongside terrain + biome + prop.
BASE_SPAWNS = 6


def _character_tris(archetype: str) -> int:
    """Per-archetype triangle budget. Raises on an archetype absent from
    [`CHARACTER_TRIS`] rather than defaulting to 0 — a silent 0 would drop the
    characters' geometry from the optimizer's triangle budget (a phantom-clean
    under-count), so an unconfigured archetype must fail loudly at emission, not ship
    an invisible-to-the-budget layer."""
    if archetype not in CHARACTER_TRIS:
        raise KeyError(f"npc archetype {archetype!r} has no triangle budget in CHARACTER_TRIS")
    return CHARACTER_TRIS[archetype]


def _spawn_count(region_id: str) -> int:
    """How many characters npc spawns in this region — deterministic, so a pipeline
    re-run (and a validation-revision re-run of this stage) reproduces the layer
    byte-for-byte.

    A per-region density jitter in [1.00, 2.00) scales [`BASE_SPAWNS`]. The jitter is
    a STABLE hash of the region id (hashlib, never the per-process-salted builtin
    ``hash()``), so two regions differ — giving the optimizer a budget that some
    regions cross and others do not — while the same region is identical on every run.
    """
    jitter_milli = 1000 + int.from_bytes(hashlib.sha256(region_id.encode("utf-8")).digest()[:4], "big") % 1000
    return BASE_SPAWNS * jitter_milli // 1000


def _roster_from_director(prior: list[LayerSpec]) -> list[str]:
    """The faction roster the director seeded for this region, parsed from the director
    layer's ``intent:factions``. Empty when the director contributed no layer (it
    failed/timed out — npc then falls back to [`NPC_ARCHETYPE`]) or its layer predates
    factions. DEGRADES to empty rather than raising on a missing/unreadable layer: a
    raise would make the supervisor isolate npc as an empty sublayer, dropping the
    region's NPC geometry over a director hiccup. Faction names are bare identifiers,
    so the comma-list never carries an escaped quote the simple capture would miss; npc
    still never trusts a name it can't budget — the caller validates every pick through
    [`_character_tris`]."""
    director = next((layer for layer in prior if layer.specialist == "director"), None)
    if director is None:
        return []
    path = Path("layers") / director.path
    if not path.is_file():
        return []
    match = re.search(r'intent:factions\s*=\s*"([^"]*)"', path.read_text())
    if not match or not match.group(1):
        return []
    return [faction for faction in match.group(1).split(",") if faction]


def _must_not_from_director(prior: list[LayerSpec]) -> list[str]:
    """The constraints the director marked ``intent:must_not`` for this region, parsed off
    the director layer as bare tokens (the [`biome._must_not_from_director`] idiom). Empty
    when the director contributed no layer (it failed/timed out — npc then applies no ban)
    or its layer predates the field. DEGRADES to empty rather than raising on a
    missing/unreadable layer: a raise would make the supervisor isolate npc as an empty
    sublayer, dropping the region's NPC geometry over a director hiccup. npc reads these
    only to decide which archetypes to EXCLUDE — an unrecognized token (a constraint owned
    by another specialist) maps to no archetype (see [`_forbidden_archetypes`])."""
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


def _forbidden_archetypes(must_not: list[str]) -> frozenset[str]:
    """The archetypes the director's ``must_not`` forbids — each recognized token bridged
    through [`MUST_NOT_ARCHETYPE`]. An unmapped token (``dense_vegetation`` is biome's cap,
    ``interior_volumes`` names no archetype) contributes nothing, so npc bans ONLY
    archetypes it actually owns and a non-archetype must_not never narrows its pick. The
    [`biome._caps_vegetation`] recognizer twin, returning the set [`_select_archetype`]
    filters the roster against — an immutable frozenset, safe as the selector's default."""
    return frozenset(MUST_NOT_ARCHETYPE[token] for token in must_not if token in MUST_NOT_ARCHETYPE)


def _select_archetype(region_id: str, roster: list[str], forbidden: frozenset[str] = frozenset()) -> str:
    """Which archetype npc spawns in this region — drawn from the director's per-region
    faction `roster` by a STABLE hash of the region id over the SORTED roster (a fixed,
    reproducible order), salted distinctly from [`_spawn_count`] so the archetype and
    the count vary independently. Members in `forbidden` (the director's intent:must_not,
    via [`_forbidden_archetypes`]) are excluded BEFORE the pick, so a desynced roster naming
    a barred archetype converges to an allowed one rather than looping the validator route-
    back. Falls back to [`NPC_ARCHETYPE`] when the director seeded no roster OR every member
    is forbidden, so npc always emits a budgeted layer. The pick is validated by the caller
    through [`_character_tris`], which raises on an archetype npc can't budget.

    The exclusion preserves sorted order, so on the common path (nothing forbidden) the
    region hash indexes exactly as before and the emitted layer is byte-identical."""
    allowed = [archetype for archetype in sorted(roster) if archetype not in forbidden]
    if not allowed:
        return NPC_ARCHETYPE
    idx = int.from_bytes(hashlib.sha256(("archetype:" + region_id).encode("utf-8")).digest()[:4], "big") % len(allowed)
    return allowed[idx]


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"npc/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    count = _spawn_count(brief.region.region_id)
    forbidden = _forbidden_archetypes(_must_not_from_director(prior))
    archetype = _select_archetype(brief.region.region_id, _roster_from_director(prior), forbidden)
    tris = count * _character_tris(archetype)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "NPCs"
)

def Xform "NPCs"
{{
    def Xform "Spawns"
    {{
        custom string archetype = {usd_str(archetype)}
        custom int spawnCount = {count}
        custom string cognition = "ace://default"
    }}
}}
"""
    )
    return LayerSpec(
        specialist="npc",
        region_id=brief.region.region_id,
        path=rel,
        summary=f"{count} × {archetype}, ACE cognition",
        metrics={"triangles": float(tris)},
    )
