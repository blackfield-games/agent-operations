from __future__ import annotations

from enum import IntEnum
from typing import Literal
from pydantic import BaseModel, Field


class JobKind(IntEnum):
    TERRAIN = 0
    FOLIAGE = 1
    NPC_TICK = 2
    DIFFUSION_TILE = 3
    OPTIMIZATION = 4

    @property
    def wire_name(self) -> str:
        """The coordinator's on-the-wire spelling of this kind. The mesh proto
        serializes JobKind with ``#[serde(rename_all = "snake_case")]``, so
        ``POST /jobs`` expects "terrain"/"foliage"/"npc_tick"/"diffusion_tile"/
        "optimization" — strings, not this IntEnum's integer form. The variant
        names are SCREAMING_SNAKE, so ``.lower()`` is exactly that snake_case;
        pinned per-variant in test_types.py so a rename on either side breaks."""
        return self.name.lower()


class RegionCoord(BaseModel):
    x: int
    y: int
    layer: int = 0  # 0 = ground, 1 = canopy, 2 = sky/weather

    @property
    def region_id(self) -> str:
        return f"r{self.x:+05d}_{self.y:+05d}_l{self.layer}"


class WorldBrief(BaseModel):
    """Shared context handed to every specialist. Cacheable via prompt cache."""
    aesthetic: str = "scorched-modern, post-conflict, cinematic"
    palette: list[str] = ["steel", "concrete", "inferno-orange", "olive", "brass"]
    biome: str
    region: RegionCoord
    style_anchors: list[str] = Field(default_factory=list)  # paths to moodboard images


class LayerSpec(BaseModel):
    """A USD layer produced by a specialist. Strength order is fixed in compose.py."""
    specialist: Literal[
        "director", "terrain", "biome", "prop", "lighting", "npc", "optimization", "validator"
    ]
    region_id: str
    path: str  # relative path under layers/, e.g. "terrain/r+0042_-0017_l0.usda"
    summary: str  # ≤200 chars, what this layer changes
    metrics: dict[str, float] = Field(default_factory=dict)  # tri count, lights, etc.


class ValidatorVerdict(BaseModel):
    accepted: bool
    issues: list[str] = Field(default_factory=list)
    # Specialists the validator blames for `issues`, attributed structurally at the
    # point each issue is raised (not parsed back out of the text). The supervisor
    # prefers this over scanning `issues` for names, so a prim path segment that
    # merely looks like a specialist name can't misroute route-back. Empty/absent on
    # an older or synthesized verdict, where the supervisor falls back to the scan.
    failing_specialists: list[str] = Field(default_factory=list)
    # A rejection that re-running a specialist cannot repair — e.g. the world is over
    # the triangle budget after optimization, the terminal LOD authority, whose output
    # is deterministic, so a route-back recomputes the same result and loops to
    # MAX_ROUNDS. The supervisor ends the revision loop instead of routing back when a
    # terminal verdict leaves no FIXABLE specialist to re-run. Distinct from
    # `failing_specialists` (the re-runnable culprits): a verdict can be both terminal
    # AND name a fixable specialist, in which case the fixable issue is repaired first.
    terminal: bool = False
    fixes_applied: list[str] = Field(default_factory=list)
    layer_kept: bool = True


class RenderReceipt(BaseModel):
    """Posted to EAS on Base after validator accepts a layer."""
    earner: str  # 0x address
    job_id: str  # bytes32 hex
    render_seconds: int
    job_kind: JobKind
    output_hash: str  # sha256 of layer file
    region_id: str
