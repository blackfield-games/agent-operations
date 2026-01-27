"""Validator — last gate, strongest layer. Combines:

  - NVIDIA Omni Asset Validator (schema, geometry, materials, performance)
  - Style classifier (embed render against moodboard anchors, cosine sim ≥ threshold)
  - Playability gates (no enclosed interiors in frontier, spawn reachability)

This stub runs the cheapest checks inline. Real impl shells out to
`omni.asset_validator` and a sidecar style-classifier service via MCP.

Returns a ValidatorVerdict; supervisor reads `.accepted` and decides whether
to commit the composed world or route back to a specialist.
"""

from __future__ import annotations

from common.types import WorldBrief, LayerSpec, ValidatorVerdict

STYLE_SIM_THRESHOLD = 0.72


async def run(brief: WorldBrief, layers: list[LayerSpec]) -> ValidatorVerdict:
    issues: list[str] = []

    expected = {"director", "terrain", "biome", "prop", "lighting", "npc", "optimization"}
    got = {layer.specialist for layer in layers}
    missing = expected - got
    if missing:
        issues.append(f"missing specialist layers: {sorted(missing)}")

    opt = next((layer for layer in layers if layer.specialist == "optimization"), None)
    if opt and opt.metrics.get("over_budget", 0.0) > 0:
        issues.append("triangle budget exceeded — re-run optimization with stricter LODs")

    # style check: TODO call sidecar with brief.style_anchors + rendered preview
    # for now, accept if no other issues
    accepted = len(issues) == 0

    return ValidatorVerdict(accepted=accepted, issues=issues)
