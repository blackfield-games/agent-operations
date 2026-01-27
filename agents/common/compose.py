"""USD layer composition.

Single-stage rule from research-openusd-pipeline.md: one stage cannot be written
from multiple threads. Each specialist writes its own layer file in its own
process; this module composes them into a root world.usda via sublayers.

LIVRPS strength ordering — Validator strongest (last word on any prim), Director
weakest (the base "intent" layer). Order in SubLayerPaths: strongest first.
"""

from __future__ import annotations

from pathlib import Path
from .types import LayerSpec

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


def compose_world(layers: list[LayerSpec], out_dir: Path) -> Path:
    """Write a world.usda root layer that sublayers each specialist's file in
    strength order. Does NOT open a UsdStage — that's the engine's job.
    """
    by_specialist: dict[str, list[LayerSpec]] = {}
    for layer in layers:
        by_specialist.setdefault(layer.specialist, []).append(layer)

    ordered: list[str] = []
    for specialist in STRENGTH_ORDER:
        for layer in by_specialist.get(specialist, []):
            ordered.append(layer.path)

    out_dir.mkdir(parents=True, exist_ok=True)
    root = out_dir / "world.usda"
    sublayers = ",\n        ".join(f'@./{p}@' for p in ordered)
    root.write_text(
        f"""#usda 1.0
(
    defaultPrim = "World"
    upAxis = "Z"
    metersPerUnit = 1.0
    subLayers = [
        {sublayers}
    ]
)

def Xform "World" (
    kind = "group"
)
{{
}}
"""
    )
    return root
