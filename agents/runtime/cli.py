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
import logging
import os
from pathlib import Path

import anyio
import httpx

from common.compose import compose_world, layer_summary
from common.jobs import RenderJobSpec, render_jobs
from common.types import LayerSpec, RegionCoord, ValidatorVerdict, WorldBrief
from runtime.poster import PostResult, post_jobs
from runtime.supervisor import build_graph

logger = logging.getLogger(__name__)

INGEST_TOKEN_ENV = "COORDINATOR_INGEST_TOKEN"


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
    p.add_argument(
        "--post-to",
        default=None,
        metavar="URL",
        help=(
            "coordinator base URL to POST an accepted region's render jobs to "
            f"(bearer token from ${INGEST_TOKEN_ENV}); opt-in, off by default"
        ),
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
    rounds: int,
    jobs: list[RenderJobSpec],
    posted: list[PostResult] | None = None,
) -> str:
    """Concise human-readable summary of one runtime invocation."""
    status = "ACCEPTED" if verdict.accepted else "REJECTED"
    lines = [
        f"region:    {region_id}",
        f"validator: {status}",
        f"rounds:    {rounds}",
    ]
    if verdict.issues:
        lines.append("issues:")
        lines.extend(f"  - {issue}" for issue in verdict.issues)
    lines.append(f"layers:    {len(layers)} emitted")
    for layer in layers:
        lines.append(f"  [{layer.specialist:>12}] {layer.summary}")
    lines.append(f"world:     {world_path}")
    # Render jobs the coordinator would dispatch — empty when the region was rejected.
    lines.append(f"jobs:      {len(jobs)} render job(s)")
    for job in jobs:
        lines.append(
            f"  [{job.kind.wire_name}] {job.region.region_id} "
            f"deadline={job.deadline_secs}s payout={job.max_payout_wei}wei"
        )
    if posted is not None:
        accepted = sum(1 for r in posted if r.ok)
        lines.append(f"posted:    {accepted}/{len(posted)} accepted by coordinator")
        for r in posted:
            tag = f"id={r.job_id}" if r.ok else r.outcome.value
            lines.append(f"  [{r.kind}] {r.region_id} -> {tag}")
    return "\n".join(lines)


def _render_report_json(
    region_id: str,
    verdict: ValidatorVerdict,
    layers: list[LayerSpec],
    world_path: Path,
    rounds: int,
    jobs: list[RenderJobSpec],
    posted: list[PostResult] | None = None,
) -> str:
    """Machine-readable JSON form of the runtime report (see `_render_report` for
    the human form). Stable keys for HUD/CI consumers; `layer_counts` is the
    per-specialist breakdown from `layer_summary`; `rounds` is the number of
    validator passes (1 when accepted on the first round, more after route-backs);
    `render_jobs` is each emitted job in the exact `POST /jobs` body shape (empty
    when the region was rejected); `posted` is present only with `--post-to` and
    holds the per-job coordinator outcome (no token)."""
    report = {
        "region_id": region_id,
        "accepted": verdict.accepted,
        "issues": list(verdict.issues),
        "rounds": rounds,
        "layers": [
            {"specialist": layer.specialist, "summary": layer.summary} for layer in layers
        ],
        "layer_counts": layer_summary(layers),
        "world": str(world_path),
        "render_jobs": [job.to_request() for job in jobs],
    }
    if posted is not None:
        report["posted"] = [
            {
                "region_id": r.region_id,
                "kind": r.kind,
                "outcome": r.outcome.value,
                "status_code": r.status_code,
                "job_id": r.job_id,
            }
            for r in posted
        ]
    return json.dumps(report, indent=2)


async def _run_graph(brief: WorldBrief) -> dict:
    graph = build_graph()
    return await graph.ainvoke({"brief": brief, "layers": [], "rounds": 0})


def _post_jobs(jobs: list[RenderJobSpec], base_url: str) -> list[PostResult]:
    """POST an accepted region's render jobs to the coordinator. The bearer token
    comes from $COORDINATOR_INGEST_TOKEN and is never printed; a missing token
    posts unauthenticated (the coordinator 401s if it requires one)."""
    token = (os.environ.get(INGEST_TOKEN_ENV) or "").strip() or None
    if token is None:
        logger.warning(
            "--post-to set but $%s is empty; posting unauthenticated "
            "(the coordinator will 401 if it requires a token)",
            INGEST_TOKEN_ENV,
        )
    with httpx.Client() as client:
        return post_jobs(jobs, base_url=base_url, token=token, client=client)


def main(argv: list[str] | None = None) -> int:
    """Drive one region through the runtime. Returns a process exit code."""
    args = _parse_args(argv)
    brief = _build_brief(args)

    result = anyio.run(_run_graph, brief)

    verdict: ValidatorVerdict = result["verdict"]
    layers: list[LayerSpec] = result["layers"]
    rounds: int = result.get("rounds", 0)

    world_path = compose_world(layers, Path(args.out))
    jobs = render_jobs(brief, layers, verdict)

    # Posting is opt-in; a rejected region emits no jobs, so there is nothing to ship.
    posted = _post_jobs(jobs, args.post_to) if args.post_to and jobs else None

    region_id = brief.region.region_id
    if args.json:
        print(_render_report_json(region_id, verdict, layers, world_path, rounds, jobs, posted))
    else:
        print(_render_report(region_id, verdict, layers, world_path, rounds, jobs, posted))

    if not verdict.accepted:
        return 1
    if posted is not None and not all(r.ok for r in posted):
        return 2  # accepted + authored, but a job failed to reach the coordinator
    return 0


if __name__ == "__main__":
    import sys

    sys.exit(main())
