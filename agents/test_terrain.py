"""Terrain heightfield emission: gridResolution is region-varied (a stable hashlib
jitter, not a constant), the triangle metric tracks that declared resolution (~2*N^2,
never the old hardcoded 262144), the emission is deterministic across runs, and the
metric satisfies the validator's terrain:{'triangles'} presence/finiteness contract.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_terrain.py -v
"""

import os
import re
import subprocess
import sys

import pytest

from common.types import RegionCoord, WorldBrief
from optimization import optimization
from terrain import terrain
from validator import validator


def _grid_resolution_from_usd(text: str) -> int:
    m = re.search(r"gridResolution\s*=\s*(\d+)", text)
    assert m, "emitted terrain USD has no gridResolution"
    return int(m.group(1))


@pytest.mark.asyncio
async def test_triangles_track_declared_resolution(tmp_path, monkeypatch):
    # FM2 (resolution coupling): the metric is derived from the gridResolution actually
    # written into the mesh (2 tris per (N-1)^2 heightfield quad cells, ~2*N^2), never the
    # old hardcoded 262144 that a changed resolution left untouched.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    layer = await terrain.run(brief, [])
    grid = _grid_resolution_from_usd((tmp_path / "layers" / layer.path).read_text())
    assert layer.metrics["triangles"] == float(2 * (grid - 1) ** 2)
    assert layer.metrics["triangles"] == float(terrain._heightfield_triangles(grid))
    assert layer.metrics["triangles"] != 262144.0  # no longer the resolution-decoupled constant


@pytest.mark.asyncio
async def test_emission_is_deterministic(tmp_path, monkeypatch):
    # FM4 (determinism): the same brief yields byte-identical USD + identical metric across
    # runs, so a pipeline re-run (and a validation-revision re-run) reproduces it. Guards
    # the derivation against an accidental per-process-salted builtin hash().
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    a = await terrain.run(brief, [])
    usd_a = (tmp_path / "layers" / a.path).read_text()
    b = await terrain.run(brief, [])
    usd_b = (tmp_path / "layers" / b.path).read_text()
    assert usd_a == usd_b
    assert a.metrics == b.metrics


def test_resolution_is_stable_across_processes():
    # FM4 (the real guard): a pipeline re-run is a FRESH process, so byte-for-byte
    # reproducibility needs a digest stable across interpreters — precisely what the
    # per-process-salted builtin hash() is NOT (it reseeds every process). Recompute
    # _grid_resolution in subprocesses under different PYTHONHASHSEEDs; a regression to
    # builtin hash() would diverge here, which the same-process emission check above (one
    # interpreter) structurally cannot catch.
    rid = RegionCoord(x=3, y=7).region_id
    agents_dir = os.path.dirname(os.path.abspath(__file__))
    snippet = (
        "import sys; sys.path.insert(0, '.'); "
        "from terrain import terrain; print(terrain._grid_resolution(sys.argv[1]))"
    )
    outs = set()
    for seed in ("0", "1", "42"):
        proc = subprocess.run(
            [sys.executable, "-c", snippet, rid],
            capture_output=True,
            text=True,
            cwd=agents_dir,
            env={**os.environ, "PYTHONHASHSEED": seed},
        )
        assert proc.returncode == 0, proc.stderr
        outs.add(proc.stdout.strip())
    # one value across every seed, and it equals the in-process result -> hashlib, not hash()
    assert outs == {str(terrain._grid_resolution(rid))}


def test_distinct_regions_vary():
    # FM1 (variability): a constant gridResolution would leave the optimizer a flat terrain
    # contributor everywhere — terrain could never be the layer that tips a region over
    # budget. Distinct regions differ in BOTH resolution and the triangle count it drives.
    grids = {terrain._grid_resolution(RegionCoord(x=x, y=0).region_id) for x in range(16)}
    tris = {terrain._heightfield_triangles(g) for g in grids}
    assert len(grids) > 1
    assert len(tris) > 1


def test_floor_stays_under_budget_but_varies():
    # The floor invariant: terrain is the optimizer's un-sheddable floor, so its OWN
    # triangle count must stay under TRIANGLE_BUDGET for every region — a floor that alone
    # exceeded budget would make a region terminal by construction — while still varying so
    # its contribution can tip a region's TOTAL over budget alongside the scatter layers.
    counts = {
        terrain._heightfield_triangles(terrain._grid_resolution(RegionCoord(x=x, y=y).region_id))
        for x in range(20)
        for y in range(20)
    }
    assert len(counts) > 1  # not a flat contributor
    assert all(c < optimization.TRIANGLE_BUDGET for c in counts)


@pytest.mark.asyncio
async def test_satisfies_validator_triangle_contract(tmp_path, monkeypatch):
    # FM3 + the validator contract: the real terrain.run emission (the path no test covered
    # before) produces a layer whose 'triangles' metric is present and finite — exactly what
    # the validator's per-role geometry gate requires (a missing/NaN/inf terrain metric
    # would fail the world). Exercised through the real validator helper, not a hand-rolled
    # check, so it tracks the contract if it tightens.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=1, y=2))
    layer = await terrain.run(brief, [])
    assert validator._metrics_issues(layer) == []  # satisfies terrain:{'triangles'}
    assert layer.metrics["triangles"] > 0  # a real, non-empty heightfield
