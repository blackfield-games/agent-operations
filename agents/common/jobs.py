"""Render-job emission — the agents -> coordinator seam.

A validated, composed region becomes the render jobs the mesh coordinator's
``POST /jobs`` accepts (see mesh/coordinator/src/main.rs ``CreateJobRequest`` and
``validate::validate_job_spec``). The bounds below MIRROR that validator so the
producer never emits a spec the coordinator would reject with 422; keep them in
lockstep with mesh/coordinator/src/validate.rs and mesh/proto/src/lib.rs.

MVP: one ``DIFFUSION_TILE`` job per validated region (the canonical "render this
tile" kind). Per-kind fan-out across renderable specialist layers is deferred
(agents-render-job-emission-per-kind); the live HTTP POST transport is
agents-render-job-poster.
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
    MVP emits one ``DIFFUSION_TILE`` job whose ``inputs`` carry the region id, the
    composed-world filename, and a per-specialist layer manifest.
    """
    if not verdict.accepted:
        return []
    region = brief.region
    inputs: dict[str, Any] = {
        "region_id": region.region_id,
        "world": "world.usda",
        "layers": [{"specialist": layer.specialist, "path": layer.path} for layer in layers],
    }
    return [
        RenderJobSpec(
            kind=JobKind.DIFFUSION_TILE,
            region=region,
            deadline_secs=deadline_secs,
            max_payout_wei=max_payout_wei,
            inputs=inputs,
        )
    ]
