"""USD layer composition.

Single-stage rule from research-openusd-pipeline.md: one stage cannot be written
from multiple threads. Each specialist writes its own layer file in its own
process; this module composes them into a root world.usda via sublayers.

LIVRPS strength ordering — Validator strongest (last word on any prim), Director
weakest (the base "intent" layer). Order in SubLayerPaths: strongest first.
"""

from __future__ import annotations

import logging
import re
from pathlib import Path
from .types import LayerSpec

logger = logging.getLogger(__name__)

# A layer path is interpolated verbatim into a `@...@` USDA asset reference, so it
# must contain only characters that can't break out of that reference or the
# enclosing `subLayers = [ ]` block. This allowlist matches the real convention the
# specialists emit — a relative path under layers/, e.g.
# "terrain/r+0042_-0017_l0.usda": ASCII letters/digits and `. _ / + -` (the `+`/`-`
# come from the region_id, `/` from the specialist subdir, `.` from the extension).
# Anything else — empty, the `@` delimiter, a newline, whitespace, quotes — is
# rejected rather than escaped, so behavior is unambiguous.
_SAFE_LAYER_PATH = re.compile(r"[A-Za-z0-9._/+-]+")

# strongest → weakest
STRENGTH_ORDER = [
    "validator",
    "optimization",
    "lighting",
    "prop",
    "npc",
    "biome",
    "terrain",
    "director",
]


def layer_summary(layers: list[LayerSpec]) -> dict[str, int]:
    """Per-specialist layer counts, for observability in the runtime report and
    the CLI ``--json`` output. Content-free: it counts layers, never reads their
    data.

    Keys follow STRENGTH_ORDER (strongest first) so the breakdown reads top-down;
    a specialist with no emitted layer is omitted. Unlike `compose_world`, a count
    is harmless for an unknown specialist, so any such specialist is appended in
    sorted order after the known ones rather than rejected.
    """
    counts: dict[str, int] = {}
    for layer in layers:
        counts[layer.specialist] = counts.get(layer.specialist, 0) + 1
    ordered = {s: counts[s] for s in STRENGTH_ORDER if s in counts}
    for specialist in sorted(counts.keys() - set(STRENGTH_ORDER)):
        ordered[specialist] = counts[specialist]
    return ordered


def compose_world(layers: list[LayerSpec], out_dir: Path) -> Path:
    """Write a world.usda root layer that sublayers each specialist's file in
    strength order. Does NOT open a UsdStage — that's the engine's job.
    """
    # Guard against silent data loss: a specialist not in STRENGTH_ORDER would be
    # silently dropped by the iteration below, omitting its layer from the world
    # with no error or warning.  Fail fast before touching the filesystem.
    known = set(STRENGTH_ORDER)
    unknown = {layer.specialist for layer in layers} - known
    if unknown:
        raise ValueError(
            f"unknown specialist(s) {sorted(unknown)}; valid: {STRENGTH_ORDER}"
        )

    by_specialist: dict[str, list[LayerSpec]] = {}
    for layer in layers:
        by_specialist.setdefault(layer.specialist, []).append(layer)

    # Validate every path BEFORE touching the filesystem (fail-fast, like the
    # unknown-specialist guard): a path that breaks the `@...@` asset reference
    # would silently corrupt world.usda, which no downstream gate re-parses.
    ordered: list[str] = []
    for specialist in STRENGTH_ORDER:
        for layer in by_specialist.get(specialist, []):
            if not _SAFE_LAYER_PATH.fullmatch(layer.path):
                raise ValueError(
                    f"{specialist} emitted an invalid layer path {layer.path!r}: "
                    f"paths must be non-empty and match {_SAFE_LAYER_PATH.pattern} "
                    f"(no '@', newline, whitespace, or quotes that would break the "
                    f"USDA asset reference)"
                )
            ordered.append(layer.path)

    # An all-None / empty specialist set is not an error — the supervisor isolates
    # a failed specialist by contributing no layer — but it yields a world with no
    # content, so warn rather than silently writing an empty stage.
    if not ordered:
        logger.warning(
            "compose_world: no layers to compose; writing an empty world.usda "
            "(subLayers = []) — the run produced no specialist output"
        )

    out_dir.mkdir(parents=True, exist_ok=True)
    root = out_dir / "world.usda"
    items = ",\n        ".join(f'@./{p}@' for p in ordered)
    sublayers_block = f"[\n        {items}\n    ]" if ordered else "[]"
    root.write_text(
        f"""#usda 1.0
(
    defaultPrim = "World"
    upAxis = "Z"
    metersPerUnit = 1.0
    subLayers = {sublayers_block}
)

def Xform "World" (
    kind = "group"
)
{{
}}
"""
    )
    return root
