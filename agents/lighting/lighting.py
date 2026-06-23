"""Lighting — sky dome, sun direction, time-of-day, fog density. Cinematic
calibration drives the scorched-modern feel. Overcast + inferno-orange rim is
the locked palette per DECISIONS.md.

The director seeds a region's mood in ``intent:beats``; lighting reads it and turns
recognized beats into atmospheric fog — the one director intent no specialist consumed
(must_have->prop, must_not->biome, factions->npc). Only the fog responds: the locked
overcast+inferno-rim COLOR palette is never touched.
"""

from __future__ import annotations

import re
from pathlib import Path
from common.types import WorldBrief, LayerSpec

# Fog density each recognized story-beat keyword the director names in intent:beats
# contributes to the region's haze. Lighting's docstring lists "fog density" as a
# responsibility but emitted none until the director's beats drove it: a scorched,
# abandoned, recent-conflict frontier reads hazier than a clean one. Contributions
# ACCUMULATE (ash + smoke + dust physically add — unlike biome's max-dominant scatter
# density) and clamp to FOG_DENSITY_MAX. Keyed by a bare beat token scanned out of the
# free-form beats line (the dominant-keyword tokenization biome uses on its biome
# string). An operator-reviewable mood/balance knob like biome's BIOME_DENSITY; an
# art-director confirms the real values before they drive a render.
BEAT_FOG_DENSITY: dict[str, float] = {
    "scorched": 0.15,
    "ash": 0.20,
    "abandoned": 0.10,
    "dust": 0.15,
    "conflict": 0.25,
    "smoke": 0.30,
    "storm": 0.20,
    "fog": 0.30,
}

# Physical ceiling on accumulated fog — a fully obscuring haze. Several heavy beats can
# sum past it; the clamp keeps the emitted density physical. The locked color palette is
# never modulated by beats, so the DECISIONS.md lock holds while the atmosphere reads
# the director's intent.
FOG_DENSITY_MAX = 0.6


def _beats_from_director(prior: list[LayerSpec]) -> str:
    """The director's free-form ``intent:beats`` mood line for this region, read off the
    director layer (the [`biome._must_not_from_director`] idiom). Empty when the director
    contributed no layer (it failed/timed out — lighting then emits the base palette with
    no fog) or its layer predates the field. DEGRADES to "" rather than raising on a
    missing/unreadable layer: a raise would make the supervisor isolate lighting as an
    empty sublayer, dropping the region's whole lighting rig over a director hiccup. The
    line is READ to derive fog; it is never echoed into lighting's output, so a hostile
    free-form value cannot inject a prim (only vocab-controlled tokens reach the layer)."""
    director = next((layer for layer in prior if layer.specialist == "director"), None)
    if director is None:
        return ""
    path = Path("layers") / director.path
    if not path.is_file():
        return ""
    match = re.search(r'intent:beats\s*=\s*"([^"]*)"', path.read_text())
    if not match:
        return ""
    return match.group(1)


def _recognized_beats(beats: str) -> list[str]:
    """The beat tokens lighting models, scanned out of the director's free-form beats
    line. Tokenized like biome's biome string (split on non-alphanumerics, lowercased) so
    the prose mood line ("scorched. abandoned. recent conflict.") yields bare tokens;
    returned in [`BEAT_FOG_DENSITY`] table order and DEDUPED, so a repeated word ("smoke
    smoke") and word order never change the result. An unrecognized word (a beat lighting
    doesn't model) is simply absent, never an error."""
    present = {t for t in re.split(r"[^a-z0-9]+", beats.lower()) if t}
    return [token for token in BEAT_FOG_DENSITY if token in present]


def _fog_density(recognized: list[str]) -> float:
    """Accumulated atmospheric fog for the recognized beats — the sum of each token's
    contribution, clamped to [`FOG_DENSITY_MAX`]. Summed in the fixed table order
    [`_recognized_beats`] returns, so the float result is byte-identical across runs
    (a pipeline re-run and a validation-revision re-run reproduce the layer). 0.0 when no
    recognized beat is present — the base palette stays fog-less."""
    return min(sum(BEAT_FOG_DENSITY[token] for token in recognized), FOG_DENSITY_MAX)


async def run(brief: WorldBrief, prior: list[LayerSpec]) -> LayerSpec | None:
    rel = f"lighting/{brief.region.region_id}.usda"
    full = Path("layers") / rel
    full.parent.mkdir(parents=True, exist_ok=True)
    recognized = _recognized_beats(_beats_from_director(prior))
    density = _fog_density(recognized)
    # Emit the Atmosphere volume ONLY when a recognized beat drives fog, so a region with
    # no director (or no recognized beat) is byte-identical to the pre-beats palette (the
    # back-compat floor the pipeline/compose tests pin). drivenBy lists the recognized
    # vocabulary tokens — never the raw free-form beats — so the layer carries no
    # injectable director text. The Sky/Sun palette below is untouched per DECISIONS.md.
    fog_block = (
        f"""
    def Volume "Atmosphere"
    {{
        float inputs:density = {density:.2f}
        custom string drivenBy = "{",".join(recognized)}"
        custom string mood = "haze"
    }}"""
        if recognized
        else ""
    )
    full.write_text(
        f"""#usda 1.0
(
    defaultPrim = "Lighting"
)

def Xform "Lighting"
{{
    def DomeLight "Sky"
    {{
        float inputs:intensity = 0.4
        color3f inputs:color = (0.55, 0.58, 0.62)
        custom string mood = "overcast"
    }}
    def DistantLight "Sun"
    {{
        float inputs:intensity = 1.2
        color3f inputs:color = (1.0, 0.62, 0.32)
        float inputs:angle = 0.53
        custom string mood = "inferno_rim"
    }}{fog_block}
}}
"""
    )
    summary = "overcast sky + inferno-orange rim"
    if recognized:
        summary += f" + fog {density:.2f}"
    return LayerSpec(
        specialist="lighting",
        region_id=brief.region.region_id,
        path=rel,
        summary=summary,
    )
