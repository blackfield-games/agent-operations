"""NPC — places autonomous characters (scavenger squads, wildlife, sentinels).
Driven by NVIDIA ACE for cognition, behavior-tree bridge for combat. Live NPC
state streams via LiveLink/UDP (per research-openusd-pipeline.md), not USD — but
the spawned characters' MESHES are real geometry the optimizer must budget, so this
layer declares a deterministic per-region spawn count and the triangles it implies
alongside the faction archetype + cognition handle.
"""

from __future__ import annotations

import hashlib
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

# The archetype npc spawns. One faction for the deterministic placeholder; the real
# impl picks per-brief from the faction roster the director seeds.
NPC_ARCHETYPE = "scavenger"

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


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"npc/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    count = _spawn_count(brief.region.region_id)
    tris = count * _character_tris(NPC_ARCHETYPE)
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "NPCs"
)

def Xform "NPCs"
{{
    def Xform "Spawns"
    {{
        custom string archetype = {usd_str(NPC_ARCHETYPE)}
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
        summary=f"{count} × {NPC_ARCHETYPE}, ACE cognition",
        metrics={"triangles": float(tris)},
    )
