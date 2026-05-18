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
