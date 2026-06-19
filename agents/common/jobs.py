"""Render-job emission — the agents -> coordinator seam.

A validated, composed region becomes the render jobs the mesh coordinator's
``POST /jobs`` accepts (see mesh/coordinator/src/main.rs ``CreateJobRequest`` and
``validate::validate_job_spec``). The bounds below MIRROR that validator so the
producer never emits a spec the coordinator would reject with 422; keep them in
lockstep with mesh/coordinator/src/validate.rs and mesh/proto/src/lib.rs.

A validated region emits one whole-tile ``DIFFUSION_TILE`` job plus a kind-specific
compute job for each renderable specialist that authored a layer (see
``SPECIALIST_RENDER_KIND``), so a heterogeneous earner fleet picks up
terrain/foliage/npc/optimization work and the coordinator's per-kind dispatch is
exercised. The specialist->kind taxonomy is operator-confirmable (see BLOCKERS).
The live HTTP POST transport is ``runtime/poster.py`` (agents-render-job-poster).
"""

from __future__ import annotations

import json
from typing import Any

from pydantic import BaseModel, Field, field_validator, model_validator

from common.types import JobKind, LayerSpec, RegionCoord, ValidatorVerdict, WorldBrief

# validate::validate_job_spec bounds (mesh/coordinator/src/validate.rs).
MAX_DEADLINE_SECS = 86_400  # 24h
MAX_PAYOUT_WEI = 10**30
MAX_INPUTS_BYTES = 16 * 1024
# proto::RegionCoord field widths (mesh/proto/src/lib.rs): x,y are i32; layer is u8.
_I32_MIN, _I32_MAX = -(2**31), 2**31 - 1
_U8_MAX = 255

# Policy defaults, caller-overridable. 1 $BLCKFLD = 1e18 wei.
DEFAULT_DEADLINE_SECS = 3600
DEFAULT_MAX_PAYOUT_WEI = str(10**18)

# The renderable authoring specialists, each mapped to the render kind that
# processes its layer. A clean bijection of the four renderable specialists onto
# the four non-diffusion JobKinds: terrain/optimization are exact name matches;
# biome authors vegetation (FOLIAGE); npc authors non-player characters (NPC_TICK).
# director/lighting/prop/validator are intentionally ABSENT — they author no
# kind-specific render workload (validator emits a verdict, director is
# camera/composition, lighting bakes into the diffusion, prop has no distinct
# kind), so they feed the composed tile rendered by DIFFUSION_TILE. The mapping is
# operator-confirmable before it drives a real renderer (see BLOCKERS).
SPECIALIST_RENDER_KIND: dict[str, JobKind] = {
    "terrain": JobKind.TERRAIN,
    "biome": JobKind.FOLIAGE,
    "npc": JobKind.NPC_TICK,
    "optimization": JobKind.OPTIMIZATION,
}


class RenderJobSpec(BaseModel):
    """One render job in the exact shape the coordinator's ``POST /jobs`` accepts.

    Bounds mirror ``validate_job_spec`` so a constructed spec is always coordinator-
    valid; ``to_request`` renders the wire body (kind as the snake_case string)."""

    kind: JobKind
    region: RegionCoord
    deadline_secs: int = Field(gt=0, le=MAX_DEADLINE_SECS)
    max_payout_wei: str
    inputs: dict[str, Any] = Field(default_factory=dict)

    @field_validator("max_payout_wei")
    @classmethod
    def _payout_parses_and_bounded(cls, v: str) -> str:
        try:
            wei = int(v)
        except (TypeError, ValueError):
            raise ValueError(
                f"max_payout_wei must be a decimal integer string, got {v!r}"
            ) from None
        if not 0 <= wei <= MAX_PAYOUT_WEI:
            raise ValueError(f"max_payout_wei out of range [0, {MAX_PAYOUT_WEI}]: {wei}")
        # Re-emit canonical decimal so a lenient input ("007", "1_000") can't ship a
        # form the coordinator's u128 parse would reject.
        return str(wei)

    @model_validator(mode="after")
    def _region_and_inputs_within_proto_widths(self) -> RenderJobSpec:
        r = self.region
        if not (_I32_MIN <= r.x <= _I32_MAX and _I32_MIN <= r.y <= _I32_MAX):
            raise ValueError(f"region x/y must fit proto i32: ({r.x}, {r.y})")
        if not 0 <= r.layer <= _U8_MAX:
            raise ValueError(f"region layer must fit proto u8 [0, {_U8_MAX}]: {r.layer}")
        # Mirror serde_json::to_vec(inputs).len(): compact, UTF-8 bytes (not the
        # \uXXXX-escaped ASCII json.dumps emits by default), so the byte count
        # matches what the coordinator measures.
        size = len(json.dumps(self.inputs, separators=(",", ":"), ensure_ascii=False).encode())
        if size > MAX_INPUTS_BYTES:
            raise ValueError(
                f"inputs serialize to {size} bytes, over MAX_INPUTS_BYTES={MAX_INPUTS_BYTES}"
            )
        return self

    def to_request(self) -> dict[str, Any]:
        """The JSON body for ``POST /jobs`` (CreateJobRequest): kind as the
        snake_case wire string, region as ``{x, y, layer}``, payout as a decimal
        string."""
        return {
            "kind": self.kind.wire_name,
            "region": {"x": self.region.x, "y": self.region.y, "layer": self.region.layer},
            "deadline_secs": self.deadline_secs,
            "max_payout_wei": self.max_payout_wei,
            "inputs": self.inputs,
        }


def render_jobs(
    brief: WorldBrief,
    layers: list[LayerSpec],
    verdict: ValidatorVerdict,
    *,
    deadline_secs: int = DEFAULT_DEADLINE_SECS,
    max_payout_wei: str = DEFAULT_MAX_PAYOUT_WEI,
) -> list[RenderJobSpec]:
    """Render jobs for a validated region, or ``[]`` if the validator rejected it.

    A rejected or unvalidated world must never become a render job (earners would
    burn compute on work the validator refused), so the accepted-gate is first.

    Emits the whole-tile ``DIFFUSION_TILE`` job (region id, composed-world filename,
    full per-specialist layer manifest) FIRST, then one kind-specific compute job
    per layer whose specialist is in ``SPECIALIST_RENDER_KIND`` — driven by the
    actual ``layers`` so a partial/failed run emits only what was authored. Per-kind
    jobs follow in ``JobKind`` enum order for stable output and each focuses on its
    single layer (the ``layer`` key, vs the diffusion job's ``layers`` manifest).
    """
    if not verdict.accepted:
        return []
    region = brief.region

    def _job(kind: JobKind, inputs: dict[str, Any]) -> RenderJobSpec:
        return RenderJobSpec(
            kind=kind,
            region=region,
            deadline_secs=deadline_secs,
            max_payout_wei=max_payout_wei,
            inputs=inputs,
        )

    jobs = [
        _job(
            JobKind.DIFFUSION_TILE,
            {
                "region_id": region.region_id,
                "world": "world.usda",
                "layers": [{"specialist": layer.specialist, "path": layer.path} for layer in layers],
            },
        )
    ]
    renderable = [
        (SPECIALIST_RENDER_KIND[layer.specialist], layer)
        for layer in layers
        if layer.specialist in SPECIALIST_RENDER_KIND
    ]
    renderable.sort(key=lambda pair: pair[0].value)
    jobs.extend(
        _job(
            kind,
            {
                "region_id": region.region_id,
                "world": "world.usda",
                "layer": {"specialist": layer.specialist, "path": layer.path},
            },
        )
        for kind, layer in renderable
    )
    return jobs
