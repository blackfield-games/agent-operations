"""Command-line driver for the in-process agent runtime.

Builds a WorldBrief for a single region, runs the LangGraph supervisor through
all specialists + validator, composes the emitted USD layers into a root
world.usda, and prints a concise report. Process exit code is 0 when the
validator accepts the region and non-zero on rejection, so it is CI-usable.

Run from the agents/ dir so package imports resolve like the tests do:
    .venv/bin/python -m runtime.cli --x 42 --y -17 --biome scorched_grassland
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import anyio

from common.compose import compose_world, layer_summary
from common.types import LayerSpec, RegionCoord, ValidatorVerdict, WorldBrief
from runtime.supervisor import build_graph


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="runtime.cli",
        description="Author OpenUSD layers for one BLACKFIELD region.",
    )
    p.add_argument("--x", type=int, required=True, help="region grid X")
    p.add_argument("--y", type=int, required=True, help="region grid Y")
    p.add_argument("--layer", type=int, default=0, help="0=ground 1=canopy 2=sky")
    p.add_argument(
        "--biome", default="scorched_grassland", help="biome key for the region"
    )
    p.add_argument(
        "--aesthetic", default=None, help="override the default WorldBrief aesthetic"
    )
    p.add_argument("--out", default="out", help="output dir for world.usda")
    p.add_argument(
        "--json",
        action="store_true",
        help="emit the region report as JSON instead of human-readable text",
    )
    return p.parse_args(argv)


def _build_brief(args: argparse.Namespace) -> WorldBrief:
    region = RegionCoord(x=args.x, y=args.y, layer=args.layer)
    fields = {"biome": args.biome, "region": region}
    if args.aesthetic is not None:
        fields["aesthetic"] = args.aesthetic
    return WorldBrief(**fields)


def _render_report(
    region_id: str,
    verdict: ValidatorVerdict,
    layers: list[LayerSpec],
    world_path: Path,
) -> str:
    """Concise human-readable summary of one runtime invocation."""
    status = "ACCEPTED" if verdict.accepted else "REJECTED"
    lines = [
        f"region:    {region_id}",
        f"validator: {status}",
    ]
    if verdict.issues:
        lines.append("issues:")
        lines.extend(f"  - {issue}" for issue in verdict.issues)
    lines.append(f"layers:    {len(layers)} emitted")
    for layer in layers:
        lines.append(f"  [{layer.specialist:>12}] {layer.summary}")
    lines.append(f"world:     {world_path}")
    return "\n".join(lines)


def _render_report_json(
    region_id: str,
    verdict: ValidatorVerdict,
    layers: list[LayerSpec],
    world_path: Path,
) -> str:
    """Machine-readable JSON form of the runtime report (see `_render_report` for
    the human form). Stable keys for HUD/CI consumers; `layer_counts` is the
    per-specialist breakdown from `layer_summary`."""
    report = {
        "region_id": region_id,
        "accepted": verdict.accepted,
        "issues": list(verdict.issues),
        "layers": [
            {"specialist": layer.specialist, "summary": layer.summary} for layer in layers
        ],
        "layer_counts": layer_summary(layers),
        "world": str(world_path),
    }
    return json.dumps(report, indent=2)


async def _run_graph(brief: WorldBrief) -> dict:
    graph = build_graph()
    return await graph.ainvoke({"brief": brief, "layers": [], "rounds": 0})


def main(argv: list[str] | None = None) -> int:
    """Drive one region through the runtime. Returns a process exit code."""
    args = _parse_args(argv)
    brief = _build_brief(args)

    result = anyio.run(_run_graph, brief)

    verdict: ValidatorVerdict = result["verdict"]
    layers: list[LayerSpec] = result["layers"]

    world_path = compose_world(layers, Path(args.out))
    if args.json:
        print(_render_report_json(brief.region.region_id, verdict, layers, world_path))
    else:
        print(_render_report(brief.region.region_id, verdict, layers, world_path))

    return 0 if verdict.accepted else 1


if __name__ == "__main__":
    import sys

    sys.exit(main())
