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

from pathlib import Path

from common.types import WorldBrief, LayerSpec, ValidatorVerdict

STYLE_SIM_THRESHOLD = 0.72

# USD text layers must begin with this cookie; pxr refuses to open a file without
# it and the engine silently skips a sublayer it cannot parse.
USDA_MAGIC = "#usda"


async def run(
    brief: WorldBrief,
    layers: list[LayerSpec],
    layers_root: Path = Path("layers"),
) -> ValidatorVerdict:
    issues: list[str] = []

    expected = {"director", "terrain", "biome", "prop", "lighting", "npc", "optimization"}
    got = {layer.specialist for layer in layers}
    missing = expected - got
    if missing:
        issues.append(f"missing specialist layers: {sorted(missing)}")

    opt = next((layer for layer in layers if layer.specialist == "optimization"), None)
    if opt and opt.metrics.get("over_budget", 0.0) > 0:
        issues.append("triangle budget exceeded — re-run optimization with stricter LODs")

    # Open every emitted layer and confirm the engine could too: a garbled header,
    # a missing defaultPrim, or a dangling file composes into a world.usda that
    # fails silently at load. Each issue leads with the specialist name so the
    # supervisor's _failing_specialist routes the fix back to the offending node.
    for layer in layers:
        reason = _layer_wellformedness(layers_root / layer.path)
        if reason:
            issues.append(f"{layer.specialist} layer {layer.path} {reason}")

    # style check: TODO call sidecar with brief.style_anchors + rendered preview
    # for now, accept if no other issues
    accepted = len(issues) == 0

    return ValidatorVerdict(accepted=accepted, issues=issues)


def _layer_wellformedness(path: Path) -> str | None:
    """Why `path` is not a well-formed USD layer, or None if it is.

    Deterministic — no LLM, no GPU, no UsdStage. The structural checks are
    authoritative on their own (the gate venv has no usd-core); when pxr *is*
    installed it adds a strict parse on top, so a syntactically broken body that
    still has a header and a defaultPrim is caught in production.
    """
    try:
        text = path.read_text()
    except FileNotFoundError:
        return "is missing — no file on disk"
    except OSError as e:
        return f"is unreadable: {e}"

    if not text.strip():
        return "is empty"
    if not text.startswith(USDA_MAGIC):
        return f"does not start with the {USDA_MAGIC} header"
    if "defaultPrim" not in text:
        return "declares no defaultPrim"
    return _pxr_parse_issue(path)


def _pxr_parse_issue(path: Path) -> str | None:
    """Strict parse via pxr when available, else None. Guarded import so the gate
    venv — which ships without usd-core — degrades to the structural checks above
    instead of raising ImportError through the whole agents suite (FM4)."""
    try:
        from pxr import Sdf
    except ImportError:
        return None
    try:
        layer = Sdf.Layer.OpenAsAnonymous(str(path))
    except Exception:
        # pxr surfaces a malformed body as a Tf runtime error, not a return value.
        return "is not parseable as a USD layer"
    if not layer:
        return "is not parseable as a USD layer"
    if not layer.defaultPrim:
        return "declares no defaultPrim"
    return None
