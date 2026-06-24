"""Director — sets the design intent for a region. Weakest layer (base intent).

Writes a sparse USD layer that establishes:
  - region bounds + grid coords
  - faction control / story beats present in this region
  - "must-haves": comms tower, downed convoy, civilian extraction point
  - "must-nots": vegetation density caps (`dense_vegetation` — biome clamps its
    scatter density when present), no enclosed interiors (frontier rule)

Other specialists override; validator has final say.
"""

from __future__ import annotations

import hashlib
from pathlib import Path
from common.types import WorldBrief, LayerSpec
from common.usd import usd_str

LAYERS_DIR = Path("layers/director")

# The combat factions the director can seed into a region — the "faction control"
# half of its intent. npc draws its per-region archetype from the roster seeded here,
# so every name MUST be one npc can budget: a subset of npc.CHARACTER_TRIS, pinned by
# test_factions_roster_is_npc_budgetable. Civilians are excluded — the frontier
# `intent:must_not` already bars them, so seeding "civilian" would contradict the
# region's own intent. An art-director confirms the real per-biome faction makeup
# before it drives real placement (operator-reviewable, like the npc/prop geometry
# tables — see the agents-npc-geometry-metrics escalation in BLOCKERS).
FACTION_ARCHETYPES: tuple[str, ...] = ("drone", "raider", "scavenger", "sentinel")

# Factions present per region. >1 so npc has a real choice within the roster (a
# single-faction roster collapses to the old hardcoded pick); < the full vocabulary
# so the director's intent actually NARROWS npc's choice region to region.
ROSTER_SIZE = 2


def _faction_roster(region_id: str) -> list[str]:
    """The factions present in this region — a deterministic subset of
    [`FACTION_ARCHETYPES`]. Each archetype is ranked by a STABLE per-(region,
    archetype) hash (hashlib, never the per-process-salted builtin ``hash()``) and the
    top [`ROSTER_SIZE`] are taken, so different regions field different rosters
    (raiders + scavengers here, sentinels + drones there) while the same region is
    byte-identical on every run. Returned sorted for a canonical layer; npc selects
    within it."""
    ranked = sorted(
        FACTION_ARCHETYPES,
        key=lambda a: hashlib.sha256(f"factions:{region_id}:{a}".encode("utf-8")).digest(),
    )
    return sorted(ranked[:ROSTER_SIZE])


# The mood "beats" the director can seed into a region — the "story beats" half of its
# intent. lighting reads the beats line and turns recognized tokens into atmospheric fog
# (agents-lighting-honors-director-beats), so a per-region subset gives each region its
# own haze instead of the fixed line every region shared before. EVERY phrase is anchored
# on a DISTINCT token lighting models (a key of `lighting.BEAT_FOG_DENSITY`), so any
# non-empty subset drives fog — the director<->lighting contract (>=1 recognized beat,
# pinned by test_director_beats_carry_a_recognized_lighting_beat) holds STRUCTURALLY for
# every region while WHICH beats (and so how much fog) varies. Bare lowercase prose, no
# quotes/commas/newlines, so the free-form line round-trips through lighting's
# `intent:beats "([^"]*)"` capture and tokenizes cleanly. An art-director confirms the
# real per-region mood palette before it drives a render (operator-reviewable, like
# FACTION_ARCHETYPES and the npc/prop geometry tables).
BEAT_VOCAB: tuple[str, ...] = (
    "scorched earth",
    "drifting ash",
    "long abandoned",
    "choking dust",
    "recent conflict",
    "smoke on the wind",
    "gathering storm",
    "rolling fog",
)

# Beats present per region. >1 so a region reads as a layered mood and its fog accumulates
# across several beats (the additive haze lighting sums); < the vocabulary so the mood
# actually narrows region to region.
BEATS_SIZE = 3


def _region_beats(region_id: str) -> list[str]:
    """The mood beats present in this region — a deterministic subset of [`BEAT_VOCAB`],
    ranked by a STABLE per-(region, beat) hash (hashlib, never the per-process-salted
    builtin ``hash()``) and the top [`BEATS_SIZE`] taken, so different regions field
    different moods while the same region is byte-identical on every run. Returned sorted
    for a canonical layer; lighting reads the joined line back as additive fog. Mirrors
    [`_faction_roster`] — the same idiom that already varies intent:factions."""
    ranked = sorted(
        BEAT_VOCAB,
        key=lambda b: hashlib.sha256(f"beats:{region_id}:{b}".encode("utf-8")).digest(),
    )
    return sorted(ranked[:BEATS_SIZE])


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"director/{brief.region.region_id}.usda"
    full = LAYERS_DIR.parent / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    roster = _faction_roster(brief.region.region_id)
    beats = _region_beats(brief.region.region_id)
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
    custom string intent:beats = {usd_str(". ".join(beats) + ".")}
    custom string intent:must_have = "comms_tower,convoy_wreck"
    custom string intent:must_not = "dense_vegetation,interior_volumes,civilians"
    custom string intent:factions = {usd_str(",".join(roster))}
}}
"""
    )
    return LayerSpec(
        specialist="director",
        region_id=brief.region.region_id,
        path=rel,
        summary=f"intent for {brief.region.region_id} in {brief.biome}; factions {'+'.join(roster)}",
    )
