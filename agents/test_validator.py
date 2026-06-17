"""Validator well-formedness gate: every emitted .usda layer is opened and
checked (exists, non-empty, #usda cookie, declares a defaultPrim) before the
run is accepted, and each issue names its specialist so the supervisor routes
the fix back to the offending node.

The gate venv ships without usd-core, so these exercise the structural path; the
pxr strict parse is an additional production-only guard that degrades to None
when pxr is absent (FM4).

Run from the agents/ dir:
    .venv/bin/python -m pytest test_validator.py -v
"""

from pathlib import Path

from common.types import LayerSpec, RegionCoord, WorldBrief
from runtime.supervisor import _failing_specialist
from validator import validator

SPECIALISTS = ["director", "terrain", "biome", "prop", "lighting", "npc", "optimization"]
REGION = "r+0042_-0017_l0"

VALID_BODY = """#usda 1.0
(
    defaultPrim = "X"
)

def Xform "X" {}
"""


def _brief() -> WorldBrief:
    return WorldBrief(biome="scorched_grassland", region=RegionCoord(x=42, y=-17))


def _layer(root: Path, specialist: str, body: str | None = VALID_BODY) -> LayerSpec:
    """Write a layer file for `specialist` under `root` (skipped when body is
    None, to model a LayerSpec pointing at a missing file) and return its spec."""
    rel = f"{specialist}/{REGION}.usda"
    if body is not None:
        full = root / rel
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(body)
    return LayerSpec(specialist=specialist, region_id=REGION, path=rel, summary="t")


def _full_set(root: Path) -> list[LayerSpec]:
    return [_layer(root, s) for s in SPECIALISTS]


# ---- _layer_wellformedness (pure, file-level) ----


def test_wellformed_layer_passes(tmp_path):
    layer = _layer(tmp_path, "terrain")
    assert validator._layer_wellformedness(tmp_path / layer.path) is None


def test_missing_file_is_flagged(tmp_path):
    layer = _layer(tmp_path, "terrain", body=None)
    reason = validator._layer_wellformedness(tmp_path / layer.path)
    assert reason is not None and "missing" in reason


def test_empty_file_is_flagged(tmp_path):
    layer = _layer(tmp_path, "terrain", body="   \n")
    reason = validator._layer_wellformedness(tmp_path / layer.path)
    assert reason is not None and "empty" in reason


def test_missing_usda_header_is_flagged(tmp_path):
    layer = _layer(tmp_path, "terrain", body='def Xform "X" {}\n')
    reason = validator._layer_wellformedness(tmp_path / layer.path)
    assert reason is not None and validator.USDA_MAGIC in reason


def test_header_not_at_byte_zero_is_flagged(tmp_path):
    # USD rejects any leading whitespace before the cookie; so does the engine.
    layer = _layer(tmp_path, "terrain", body="\n#usda 1.0\n")
    reason = validator._layer_wellformedness(tmp_path / layer.path)
    assert reason is not None and validator.USDA_MAGIC in reason


def test_missing_defaultprim_is_flagged(tmp_path):
    body = '#usda 1.0\n(\n    upAxis = "Z"\n)\n\ndef Xform "X" {}\n'
    layer = _layer(tmp_path, "terrain", body=body)
    reason = validator._layer_wellformedness(tmp_path / layer.path)
    assert reason is not None and "defaultPrim" in reason


def test_pxr_absent_degrades_to_none(tmp_path):
    # The gate venv has no usd-core: the strict parse must degrade silently, never
    # raise ImportError. A structurally valid layer stays accepted (FM4). If
    # usd-core is later installed, OpenAsAnonymous parses this valid body and still
    # returns None — so the assertion holds either way.
    layer = _layer(tmp_path, "terrain")
    assert validator._pxr_parse_issue(tmp_path / layer.path) is None


# ---- run() integration + specialist route-back ----


async def test_run_accepts_a_fully_wellformed_world(tmp_path):
    verdict = await validator.run(_brief(), _full_set(tmp_path), layers_root=tmp_path)
    assert verdict.accepted, verdict.issues
    assert verdict.issues == []


async def test_run_rejects_and_routes_back_to_the_malformed_specialist(tmp_path):
    layers = _full_set(tmp_path)
    # Corrupt terrain's emitted layer in place: valid spec, broken file.
    (tmp_path / f"terrain/{REGION}.usda").write_text('def Mesh "T" {}\n')

    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)

    assert not verdict.accepted
    assert any("terrain" in issue for issue in verdict.issues)
    # The issue text must drive a route-back to terrain, not a director fallback.
    assert _failing_specialist(verdict.issues) == "terrain"


async def test_run_routes_back_to_the_earliest_malformed_specialist(tmp_path):
    layers = _full_set(tmp_path)
    # Two broken layers: prop (downstream) and terrain (upstream). Repairing the
    # upstream cause first means routing back to terrain.
    (tmp_path / f"prop/{REGION}.usda").write_text("garbage\n")
    (tmp_path / f"terrain/{REGION}.usda").write_text("garbage\n")

    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)

    assert not verdict.accepted
    assert _failing_specialist(verdict.issues) == "terrain"


async def test_run_flags_a_dangling_layer_file(tmp_path):
    # FM2: a LayerSpec whose file was never written → the run is rejected and the
    # offending specialist is named, not silently composed into a dangling sublayer.
    layers = [_layer(tmp_path, s, body=None if s == "biome" else VALID_BODY) for s in SPECIALISTS]

    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)

    assert not verdict.accepted
    assert any("biome" in issue and "missing" in issue for issue in verdict.issues)
    assert _failing_specialist(verdict.issues) == "biome"
