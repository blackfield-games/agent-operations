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
    SPECIALIST_RENDER_KIND,
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


def test_render_jobs_first_job_is_the_whole_tile_diffusion():
    # The whole-tile DIFFUSION_TILE is always emitted first, ahead of any per-kind
    # fan-out, carrying the accepted region's coordinates.
    jobs = render_jobs(_brief(), _layers(), ValidatorVerdict(accepted=True))
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


# --- per-kind render-job fan-out (agents-render-job-emission-per-kind) ---

_ALL_SPECIALISTS = ("director", "terrain", "biome", "prop", "lighting", "npc", "optimization", "validator")


def _layer(specialist, region_id="r+0042_-0017_l0") -> LayerSpec:
    return LayerSpec(specialist=specialist, region_id=region_id, path=f"{specialist}/a.usda", summary=specialist)


def test_specialist_render_kind_is_the_renderable_bijection():
    # Exactly the four renderable specialists -> the four non-diffusion kinds.
    # director/lighting/prop/validator are intentionally absent (no render kind),
    # and the whole-tile DIFFUSION_TILE is never a per-specialist target.
    assert SPECIALIST_RENDER_KIND == {
        "terrain": JobKind.TERRAIN,
        "biome": JobKind.FOLIAGE,
        "npc": JobKind.NPC_TICK,
        "optimization": JobKind.OPTIMIZATION,
    }
    assert JobKind.DIFFUSION_TILE not in SPECIALIST_RENDER_KIND.values()
    assert len(set(SPECIALIST_RENDER_KIND.values())) == 4


def test_render_jobs_fans_out_one_job_per_renderable_specialist():
    # A full eight-specialist run emits the whole-tile diffusion job plus one job
    # for each of the four renderable specialists, in JobKind enum order.
    layers = [_layer(s) for s in _ALL_SPECIALISTS]
    jobs = render_jobs(_brief(), layers, ValidatorVerdict(accepted=True))
    assert [job.kind for job in jobs] == [
        JobKind.DIFFUSION_TILE, JobKind.TERRAIN, JobKind.FOLIAGE, JobKind.NPC_TICK, JobKind.OPTIMIZATION,
    ]


def test_render_jobs_per_kind_only_for_present_layers():
    # FM1: only terrain + biome authored layers -> diffusion + TERRAIN + FOLIAGE;
    # no NPC_TICK / OPTIMIZATION job for a specialist that produced nothing.
    jobs = render_jobs(_brief(), _layers(), ValidatorVerdict(accepted=True))
    assert [job.kind for job in jobs] == [JobKind.DIFFUSION_TILE, JobKind.TERRAIN, JobKind.FOLIAGE]


def test_render_jobs_unmapped_specialist_emits_no_kind_job():
    # FM2: director/lighting/prop/validator map to no render kind, so a run of only
    # those emits the whole-tile diffusion job and nothing else.
    layers = [_layer(s) for s in ("director", "lighting", "prop", "validator")]
    jobs = render_jobs(_brief(), layers, ValidatorVerdict(accepted=True))
    assert [job.kind for job in jobs] == [JobKind.DIFFUSION_TILE]


def test_render_jobs_order_is_stable_regardless_of_layer_order():
    # FM3: emission order is JobKind enum order, not layer input order — a shuffled
    # layer list yields the identical job order.
    shuffled = [_layer(s) for s in ("optimization", "npc", "terrain", "biome")]
    jobs = render_jobs(_brief(), shuffled, ValidatorVerdict(accepted=True))
    assert [job.kind for job in jobs] == [
        JobKind.DIFFUSION_TILE, JobKind.TERRAIN, JobKind.FOLIAGE, JobKind.NPC_TICK, JobKind.OPTIMIZATION,
    ]


def test_render_jobs_per_kind_inputs_focus_their_single_layer():
    # A per-kind job carries the singular `layer` it renders; the diffusion job keeps
    # the plural `layers` manifest — the two input shapes are distinct.
    jobs = render_jobs(_brief(), _layers(), ValidatorVerdict(accepted=True))
    terrain = next(job for job in jobs if job.kind is JobKind.TERRAIN)
    assert terrain.inputs["region_id"] == "r+0042_-0017_l0"
    assert terrain.inputs["world"] == "world.usda"
    assert terrain.inputs["layer"] == {"specialist": "terrain", "path": "terrain/a.usda"}
    assert "layers" not in terrain.inputs
    diffusion = jobs[0]
    assert "layers" in diffusion.inputs and "layer" not in diffusion.inputs


def test_render_jobs_rejected_emits_no_jobs_at_all():
    # FM1 of the base slice still holds for the whole fan-out: a rejected region
    # yields neither a diffusion job nor any per-kind job.
    layers = [_layer(s) for s in _ALL_SPECIALISTS]
    assert render_jobs(_brief(), layers, ValidatorVerdict(accepted=False, issues=["x"])) == []


def test_render_jobs_every_emitted_spec_is_coordinator_valid():
    # FM4: every job (not just the diffusion tile) carries a valid wire kind and
    # validate_job_spec-bounded fields, so none is 422-rejectable.
    layers = [_layer(s) for s in _ALL_SPECIALISTS]
    jobs = render_jobs(_brief(), layers, ValidatorVerdict(accepted=True))
    for job in jobs:
        body = job.to_request()
        assert isinstance(body["kind"], str)
        assert body["kind"] in {"diffusion_tile", "terrain", "foliage", "npc_tick", "optimization"}
        assert 0 < body["deadline_secs"] <= MAX_DEADLINE_SECS
        assert 0 <= int(body["max_payout_wei"]) <= MAX_PAYOUT_WEI
        assert set(body) == {"kind", "region", "deadline_secs", "max_payout_wei", "inputs"}


def test_render_jobs_duplicate_specialist_layers_each_emit_a_job():
    # Defensive: should a specialist contribute two layers, each renders (one job per
    # mapped layer), preserving input order within the kind (stable sort).
    layers = [
        LayerSpec(specialist="terrain", region_id="r+0042_-0017_l0", path="terrain/a.usda", summary="a"),
        LayerSpec(specialist="terrain", region_id="r+0042_-0017_l0", path="terrain/b.usda", summary="b"),
    ]
    jobs = render_jobs(_brief(), layers, ValidatorVerdict(accepted=True))
    assert [job.kind for job in jobs] == [JobKind.DIFFUSION_TILE, JobKind.TERRAIN, JobKind.TERRAIN]
    assert [job.inputs["layer"]["path"] for job in jobs[1:]] == ["terrain/a.usda", "terrain/b.usda"]


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


def test_spec_measures_inputs_in_utf8_bytes_like_serde():
    # The coordinator measures inputs via serde_json::to_vec (compact UTF-8). A
    # 2-byte char counts as 2, not as the 6-byte \uXXXX escape json.dumps emits by
    # default — so a non-ASCII manifest well under the UTF-8 limit is accepted even
    # though the escaped form would exceed it. 4000 * "é" = ~8 KiB UTF-8 (< 16 KiB)
    # but ~24 KiB escaped (> 16 KiB): a regression to ensure_ascii=True rejects this.
    payload = {"blob": "é" * 4000}
    assert _spec(inputs=payload).inputs == payload  # accepted (no ValidationError)


# --- proto integer widths (FM4) ---

def test_spec_rejects_out_of_range_region():
    with pytest.raises(ValidationError):
        _spec(region=RegionCoord(x=0, y=0, layer=256))  # u8 overflow
    with pytest.raises(ValidationError):
        _spec(region=RegionCoord(x=2**31, y=0, layer=0))  # i32 overflow
    # boundaries accepted
    assert _spec(region=RegionCoord(x=2**31 - 1, y=-(2**31), layer=255)).region.layer == 255
