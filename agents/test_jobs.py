"""Tests for the render-job emission seam (common/jobs.py).

These pin the producer side of the agents -> coordinator contract: render_jobs
only emits for a validated region, and every RenderJobSpec is coordinator-valid
(wire-format kind, validate_job_spec bounds, proto integer widths). Run from the
agents/ dir:
    .venv/bin/python -m pytest test_jobs.py -v
"""

import pytest
from pydantic import ValidationError

from common.jobs import (
    DEFAULT_DEADLINE_SECS,
    DEFAULT_MAX_PAYOUT_WEI,
    MAX_DEADLINE_SECS,
    MAX_INPUTS_BYTES,
    MAX_PAYOUT_WEI,
    RenderJobSpec,
    render_jobs,
)
from common.types import JobKind, LayerSpec, RegionCoord, ValidatorVerdict, WorldBrief


def _brief(x=42, y=-17, layer=0) -> WorldBrief:
    return WorldBrief(biome="scorched_grassland", region=RegionCoord(x=x, y=y, layer=layer))


def _layers(region_id="r+0042_-0017_l0") -> list[LayerSpec]:
    return [
        LayerSpec(specialist="terrain", region_id=region_id, path="terrain/a.usda", summary="ridge"),
        LayerSpec(specialist="biome", region_id=region_id, path="biome/a.usda", summary="grass"),
    ]


# --- render_jobs: the accepted-gate (FM2) ---

def test_render_jobs_empty_when_rejected():
    # A region the validator refused must never become a render job.
    jobs = render_jobs(_brief(), _layers(), ValidatorVerdict(accepted=False, issues=["bad"]))
    assert jobs == []


def test_render_jobs_emits_one_diffusion_tile_for_accepted_region():
    jobs = render_jobs(_brief(), _layers(), ValidatorVerdict(accepted=True))
    assert len(jobs) == 1
    job = jobs[0]
    assert job.kind is JobKind.DIFFUSION_TILE
    assert (job.region.x, job.region.y, job.region.layer) == (42, -17, 0)


def test_render_jobs_inputs_manifest_reflects_layers():
    jobs = render_jobs(_brief(), _layers(), ValidatorVerdict(accepted=True))
    inputs = jobs[0].inputs
    assert inputs["region_id"] == "r+0042_-0017_l0"
    assert inputs["world"] == "world.usda"
    assert inputs["layers"] == [
        {"specialist": "terrain", "path": "terrain/a.usda"},
        {"specialist": "biome", "path": "biome/a.usda"},
    ]


def test_render_jobs_defaults_are_coordinator_valid():
    # The policy defaults must themselves sit inside validate_job_spec's bounds,
    # else every emitted job would 422.
    assert 0 < DEFAULT_DEADLINE_SECS <= MAX_DEADLINE_SECS
    assert 0 <= int(DEFAULT_MAX_PAYOUT_WEI) <= MAX_PAYOUT_WEI
    jobs = render_jobs(_brief(), _layers(), ValidatorVerdict(accepted=True))
    assert jobs[0].deadline_secs == DEFAULT_DEADLINE_SECS
    assert jobs[0].max_payout_wei == DEFAULT_MAX_PAYOUT_WEI


def test_render_jobs_honors_overrides():
    jobs = render_jobs(
        _brief(), _layers(), ValidatorVerdict(accepted=True),
        deadline_secs=120, max_payout_wei="5000",
    )
    assert jobs[0].deadline_secs == 120
    assert jobs[0].max_payout_wei == "5000"


# --- to_request: the wire shape (FM1) ---

def test_to_request_matches_coordinator_body_shape():
    job = render_jobs(_brief(1, 2, 1), _layers("r+0001_+0002_l1"), ValidatorVerdict(accepted=True))[0]
    body = job.to_request()
    # kind is the snake_case STRING the coordinator deserializes, not the IntEnum int.
    assert body["kind"] == "diffusion_tile"
    assert isinstance(body["kind"], str)
    assert body["region"] == {"x": 1, "y": 2, "layer": 1}
    assert body["deadline_secs"] == DEFAULT_DEADLINE_SECS
    assert body["max_payout_wei"] == DEFAULT_MAX_PAYOUT_WEI
    assert body["inputs"]["region_id"] == "r+0001_+0002_l1"
    # The body is exactly the CreateJobRequest fields, nothing more.
    assert set(body) == {"kind", "region", "deadline_secs", "max_payout_wei", "inputs"}


# --- RenderJobSpec bounds mirror validate_job_spec (FM3) ---

def _spec(**over):
    base = dict(
        kind=JobKind.DIFFUSION_TILE,
        region=RegionCoord(x=0, y=0, layer=0),
        deadline_secs=DEFAULT_DEADLINE_SECS,
        max_payout_wei="1000",
        inputs={},
    )
    base.update(over)
    return RenderJobSpec(**base)


def test_spec_rejects_zero_and_overlong_deadline():
    with pytest.raises(ValidationError):
        _spec(deadline_secs=0)
    with pytest.raises(ValidationError):
        _spec(deadline_secs=MAX_DEADLINE_SECS + 1)
    # the inclusive boundary is accepted
    assert _spec(deadline_secs=MAX_DEADLINE_SECS).deadline_secs == MAX_DEADLINE_SECS


def test_spec_rejects_malformed_or_overlarge_payout():
    with pytest.raises(ValidationError):
        _spec(max_payout_wei="not-a-number")
    with pytest.raises(ValidationError):
        _spec(max_payout_wei="-1")
    with pytest.raises(ValidationError):
        _spec(max_payout_wei=str(MAX_PAYOUT_WEI + 1))
    # inclusive boundary accepted
    assert _spec(max_payout_wei=str(MAX_PAYOUT_WEI)).max_payout_wei == str(MAX_PAYOUT_WEI)


def test_spec_canonicalizes_payout():
    # A lenient input is re-emitted as a plain decimal the coordinator's u128 parse
    # accepts (no leading zeros, no underscores).
    assert _spec(max_payout_wei="007").max_payout_wei == "7"
    assert _spec(max_payout_wei="1_000").max_payout_wei == "1000"


def test_spec_rejects_oversized_inputs():
    # render_jobs never produces a 16 KiB manifest, but the model bound must hold for
    # a hand-built spec so the producer can't ship an inputs blob the coordinator 422s.
    big = {"blob": "x" * (MAX_INPUTS_BYTES + 1)}
    with pytest.raises(ValidationError):
        _spec(inputs=big)


# --- proto integer widths (FM4) ---

def test_spec_rejects_out_of_range_region():
    with pytest.raises(ValidationError):
        _spec(region=RegionCoord(x=0, y=0, layer=256))  # u8 overflow
    with pytest.raises(ValidationError):
        _spec(region=RegionCoord(x=2**31, y=0, layer=0))  # i32 overflow
    # boundaries accepted
    assert _spec(region=RegionCoord(x=2**31 - 1, y=-(2**31), layer=255)).region.layer == 255
