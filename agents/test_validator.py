"""Validator acceptance gates: every emitted .usda layer is opened and checked
(exists, non-empty, #usda cookie, declares a defaultPrim), and the layers are
then checked for cross-layer composition conflicts — two specialists defining the
same prim path with incompatible types, or an override dangling over a prim no one
defines. Each issue names its specialist so the supervisor routes the fix back to
the offending (pipeline-earliest) node.

The gate venv ships without usd-core, so these exercise the structural path; the
pxr strict parse / composed-stage resolution is an additional production-only
guard that degrades to the structural checks when pxr is absent (FM4).

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


# Metrics each role is contracted to emit (mirrors the real terrain/optimization
# stubs); other specialists emit none. _full_set applies these so a fully-formed
# world satisfies the validator's per-role metrics schema.
_ROLE_METRICS = {"terrain": {"triangles": 262144.0}, "optimization": {"over_budget": 0.0}}


def _layer(
    root: Path,
    specialist: str,
    body: str | None = VALID_BODY,
    metrics: dict | None = None,
) -> LayerSpec:
    """Write a layer file for `specialist` under `root` (skipped when body is
    None, to model a LayerSpec pointing at a missing file) and return its spec.
    `metrics` defaults to the role's contracted metrics so a layer satisfies the
    schema unless a test deliberately overrides it."""
    rel = f"{specialist}/{REGION}.usda"
    if body is not None:
        full = root / rel
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(body)
    if metrics is None:
        metrics = dict(_ROLE_METRICS.get(specialist, {}))
    return LayerSpec(specialist=specialist, region_id=REGION, path=rel, summary="t", metrics=metrics)


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


# ---- per-role metrics schema ----


def _with_override(root: Path, specialist: str, layer: LayerSpec) -> list[LayerSpec]:
    """A full well-formed set with one specialist's spec replaced by `layer`."""
    return [layer if s == specialist else _layer(root, s) for s in SPECIALISTS]


async def test_run_rejects_missing_required_metric_and_routes_to_optimization(tmp_path):
    # The optimizer emits NO over_budget — the budget gate would silently pass via
    # its .get(..., 0.0) default. The schema must catch the missing metric.
    opt = _layer(tmp_path, "optimization", metrics={})
    verdict = await validator.run(_brief(), _with_override(tmp_path, "optimization", opt), layers_root=tmp_path)
    assert not verdict.accepted
    assert any("optimization" in i and "over_budget" in i and "missing" in i for i in verdict.issues)
    assert _failing_specialist(verdict.issues) == "optimization"


async def test_run_rejects_misspelled_metric_key_and_routes_to_terrain(tmp_path):
    # terrain emits `tri` instead of `triangles`; the optimizer's sum silently reads
    # 0 for it. The schema catches the missing `triangles` and routes back to terrain.
    terr = _layer(tmp_path, "terrain", metrics={"tri": 262144.0})
    verdict = await validator.run(_brief(), _with_override(tmp_path, "terrain", terr), layers_root=tmp_path)
    assert not verdict.accepted
    assert any("terrain" in i and "triangles" in i for i in verdict.issues)
    assert _failing_specialist(verdict.issues) == "terrain"


async def test_run_rejects_a_nan_metric_value(tmp_path):
    # Pydantic accepts NaN for a float field, so a NaN over_budget reaches the gate
    # where `nan > 0` is False (silently passing). The schema rejects non-finite.
    opt = _layer(tmp_path, "optimization", metrics={"over_budget": float("nan")})
    verdict = await validator.run(_brief(), _with_override(tmp_path, "optimization", opt), layers_root=tmp_path)
    assert not verdict.accepted
    assert any("optimization" in i and "over_budget" in i and "finite" in i for i in verdict.issues)
    assert _failing_specialist(verdict.issues) == "optimization"


async def test_run_rejects_a_wrong_typed_metric_value(tmp_path):
    # A string where a float is expected — bypass Pydantic (which would reject it at
    # construction) to model a metric that slipped through wrong-typed.
    bad = _layer(tmp_path, "optimization").model_copy(update={"metrics": {"over_budget": "lots"}})
    verdict = await validator.run(_brief(), _with_override(tmp_path, "optimization", bad), layers_root=tmp_path)
    assert not verdict.accepted
    assert any("optimization" in i and "over_budget" in i and "finite" in i for i in verdict.issues)
    assert _failing_specialist(verdict.issues) == "optimization"


async def test_run_accepts_int_valued_metrics(tmp_path):
    # FM3 discriminator: int metrics must NOT be flagged against the float contract.
    # Bypass Pydantic (which coerces int->float) so the values are genuinely int.
    terr = _layer(tmp_path, "terrain").model_copy(update={"metrics": {"triangles": 262144}})
    opt = _layer(tmp_path, "optimization").model_copy(update={"metrics": {"over_budget": 0}})
    layers = []
    for s in SPECIALISTS:
        layers.append(terr if s == "terrain" else opt if s == "optimization" else _layer(tmp_path, s))
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


def test_metrics_no_schema_role_imposes_no_requirement(tmp_path):
    # FM4: a role with no ROLE_METRICS entry (biome) and no metrics must return no
    # issues and must not KeyError.
    biome = _layer(tmp_path, "biome", metrics={})
    assert validator._metrics_issues(biome) == []


def test_is_finite_number_accepts_int_and_float_rejects_nonfinite_and_nonnumeric():
    # FM3: int and float are both legal; NaN, inf (the dangerous one — inf > 0 is
    # True, so dropping the isfinite check would let it through), bool, numeric-looking
    # strings, and None are not.
    assert validator._is_finite_number(0)
    assert validator._is_finite_number(262144)  # int
    assert validator._is_finite_number(-3)
    assert validator._is_finite_number(1.5)  # float
    assert not validator._is_finite_number(float("nan"))
    assert not validator._is_finite_number(float("inf"))
    assert not validator._is_finite_number(float("-inf"))
    assert not validator._is_finite_number(True)  # bool is an int subclass, not a metric
    assert not validator._is_finite_number("5")
    assert not validator._is_finite_number(None)


async def test_run_routes_to_earliest_when_metric_and_wellformedness_issues_coexist(tmp_path):
    # A metric issue on a downstream specialist (optimization missing over_budget)
    # co-occurs with a well-formedness issue on an upstream one (terrain's file is
    # garbage). Route-back must still pick the pipeline-earliest (terrain) — proving
    # metric issues participate in the same earliest-specialist routing.
    opt = _layer(tmp_path, "optimization", metrics={})
    layers = _with_override(tmp_path, "optimization", opt)
    (tmp_path / f"terrain/{REGION}.usda").write_text("garbage\n")
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("optimization" in i and "over_budget" in i for i in verdict.issues)
    assert any("terrain" in i for i in verdict.issues)
    assert _failing_specialist(verdict.issues) == "terrain"


# ---- composition conflicts (cross-layer) ----

# /World/Hub as a Mesh vs. as an Xform — the two bodies disagree only on Hub's
# type, the discriminator for an incompatible-type conflict. The prim path carries
# no specialist-name segment, so route-back keys on the named specialists alone.
HUB_AS_MESH = """#usda 1.0
(
    defaultPrim = "World"
)
def Xform "World" {
    def Mesh "Hub" {}
}
"""
HUB_AS_XFORM = """#usda 1.0
(
    defaultPrim = "World"
)
def Xform "World" {
    def Xform "Hub" {}
}
"""
# Defines /World, overrides /World/Hub — legitimate over-on-def when paired with a
# layer that defines Hub.
OVERRIDES_HUB = """#usda 1.0
(
    defaultPrim = "World"
)
over "World" {
    over "Hub" {}
}
"""
# Defines /World, then dangles an override over /Ghost that nothing defines.
DANGLES_GHOST = """#usda 1.0
(
    defaultPrim = "World"
)
def Xform "World" {}
over "Ghost" {}
"""


def _set_with(root: Path, bodies: dict[str, str]) -> list[LayerSpec]:
    return [_layer(root, s, body=bodies.get(s, VALID_BODY)) for s in SPECIALISTS]


def test_prim_specs_skips_metadata_braces_and_nests_paths():
    body = (
        '#usda 1.0\n(\n    defaultPrim = "World"\n'
        "    customLayerData = {\n        string note = \"{ not a prim }\"\n    }\n)\n"
        'def Xform "World" (\n    kind = "group"\n)\n{\n'
        '    over "Hub" {}\n    def Mesh "Rock" {}\n}\n'
    )
    assert validator._prim_specs(body) == [
        ("def", "Xform", "/World"),
        ("over", "", "/World/Hub"),
        ("def", "Mesh", "/World/Rock"),
    ]


# A brace and a fake `def` buried in a quoted string or an asset path must not
# desync the scanner — each below carries one such trap and must still resolve to
# exactly /World and /World/Hub (a Mesh), the genuine prims.
def test_prim_specs_skips_triple_quoted_strings():
    body = (
        '#usda 1.0\n(\n    defaultPrim = "World"\n)\ndef Xform "World" {\n'
        '    string doc = """has } brace and def Foo "X" """\n    def Mesh "Hub" {}\n}\n'
    )
    assert validator._prim_specs(body) == [("def", "Xform", "/World"), ("def", "Mesh", "/World/Hub")]


def test_prim_specs_skips_single_quoted_strings():
    body = (
        "#usda 1.0\n(\n    defaultPrim = \"World\"\n)\ndef Xform \"World\" {\n"
        "    string s = 'has } brace'\n    def Mesh \"Hub\" {}\n}\n"
    )
    assert validator._prim_specs(body) == [("def", "Xform", "/World"), ("def", "Mesh", "/World/Hub")]


def test_prim_specs_skips_asset_paths():
    # An asset path can hold (, ), #, and " — none may be read as structure.
    body = (
        '#usda 1.0\n(\n    defaultPrim = "World"\n)\ndef Xform "World" {\n'
        '    custom asset thumb = @logo(1)#x".usd@\n    def Mesh "Hub" {}\n}\n'
    )
    assert validator._prim_specs(body) == [("def", "Xform", "/World"), ("def", "Mesh", "/World/Hub")]


# A variantSet's `{ }` is a composition scope, not a prim scope: a prim defined
# inside a variant attributes to the enclosing prim (no phantom /World//Mat), and
# the variant braces still balance so a following sibling sits at the right depth.
def test_prim_specs_variant_block_attributes_to_enclosing_prim():
    body = (
        '#usda 1.0\n(\n    defaultPrim = "World"\n)\n'
        'def Xform "World" {\n'
        '    variantSet "look" = {\n'
        '        "red" {\n'
        '            def Material "Mat" {}\n'
        "        }\n"
        "    }\n"
        '    def Mesh "Rock" {}\n'
        "}\n"
    )
    assert validator._prim_specs(body) == [
        ("def", "Xform", "/World"),
        ("def", "Material", "/World/Mat"),
        ("def", "Mesh", "/World/Rock"),
    ]


# Nested variantSets (a variantSet inside a variant inside a variantSet): the deep
# Light still resolves under its enclosing PRIM, and the trailing Tail is back at
# /World — proving every transparent scope popped cleanly (FM3 brace accounting).
def test_prim_specs_nested_variant_blocks_stay_aligned():
    body = (
        '#usda 1.0\n(\n    defaultPrim = "World"\n)\n'
        'def Xform "World" {\n'
        '    def Scope "Rig" {\n'
        '        variantSet "fx" = {\n'
        '            "on" {\n'
        '                variantSet "level" = {\n'
        '                    "hi" { def Light "Key" {} }\n'
        "                }\n"
        "            }\n"
        "        }\n"
        "    }\n"
        '    def Mesh "Tail" {}\n'
        "}\n"
    )
    assert validator._prim_specs(body) == [
        ("def", "Xform", "/World"),
        ("def", "Scope", "/World/Rig"),
        ("def", "Light", "/World/Rig/Key"),
        ("def", "Mesh", "/World/Tail"),
    ]


# The keyword only counts as USD syntax: a literal "variantSet" in a string value
# and a `# variant` comment must not open a transparent scope (FM2 — the same
# string/comment/asset skip that shields a fake `def` shields these too).
def test_prim_specs_quoted_or_commented_variant_keyword_is_inert():
    body = (
        '#usda 1.0\n(\n    defaultPrim = "World"\n)\n'
        'def Xform "World" {\n'
        '    string note = "a variantSet { here is only text"\n'
        '    # variant "x" = { also ignored\n'
        '    def Mesh "Hub" {}\n'
        "}\n"
    )
    assert validator._prim_specs(body) == [
        ("def", "Xform", "/World"),
        ("def", "Mesh", "/World/Hub"),
    ]


# End-to-end through the conflict detector: two specialists define <World/Mat>
# (incompatible types) inside variants. The conflict must surface at the REAL path
# /World/Mat — which only holds when variant scopes inject no phantom segment;
# pre-fix both sat at /World///Mat and this assertion fails.
def test_variant_nested_prims_conflict_at_their_real_path():
    a = '#usda 1.0\ndef Xform "World" {\n  variantSet "look" = { "red" { def Material "Mat" {} } }\n}\n'
    b = '#usda 1.0\ndef Xform "World" {\n  variantSet "look" = { "red" { def Scope "Mat" {} } }\n}\n'
    tagged = [("biome", *s) for s in validator._prim_specs(a)]
    tagged += [("prop", *s) for s in validator._prim_specs(b)]
    issues = validator._conflicts_from_specs(tagged)
    assert len(issues) == 1
    assert "</World/Mat>" in issues[0]
    assert "Material" in issues[0] and "Scope" in issues[0]


def test_same_type_redefinition_is_not_a_conflict():
    # Two specialists defining the same path with the same type is a legal USD
    # opinion-merge, not a conflict (FM1: don't false-positive on layering).
    tagged = [("terrain", "def", "Xform", "/X"), ("biome", "def", "Xform", "/X")]
    assert validator._conflicts_from_specs(tagged) == []


def test_typeless_def_carries_no_type_opinion():
    tagged = [("terrain", "def", "", "/X"), ("biome", "def", "Mesh", "/X")]
    assert validator._conflicts_from_specs(tagged) == []


def test_incompatible_type_redefinition_is_flagged():
    tagged = [("terrain", "def", "Mesh", "/World/Hub"), ("prop", "def", "Xform", "/World/Hub")]
    issues = validator._conflicts_from_specs(tagged)
    assert len(issues) == 1
    assert "prop" in issues[0] and "terrain" in issues[0]
    assert _failing_specialist(issues) == "terrain"


def test_over_on_def_is_legal():
    tagged = [("terrain", "def", "Mesh", "/World/Hub"), ("prop", "over", "", "/World/Hub")]
    assert validator._conflicts_from_specs(tagged) == []


def test_dangling_override_is_flagged():
    tagged = [("biome", "over", "", "/World/Ghost")]
    issues = validator._conflicts_from_specs(tagged)
    assert len(issues) == 1
    assert "biome" in issues[0] and "dangling" in issues[0]


def test_composition_check_runs_without_pxr(tmp_path):
    # The gate venv ships without usd-core, so the structural scan must catch a
    # conflict on its own. _composition_conflicts reads the files and flags it.
    layers = _set_with(tmp_path, {"terrain": HUB_AS_MESH, "prop": HUB_AS_XFORM})
    issues = validator._composition_conflicts(layers, tmp_path)
    assert any("incompatible types" in issue for issue in issues)


async def test_run_accepts_a_legitimate_over_on_def(tmp_path):
    # terrain defines /World/Hub, lighting only overrides it — legal layering, not
    # a conflict; the world is accepted.
    layers = _set_with(tmp_path, {"terrain": HUB_AS_MESH, "lighting": OVERRIDES_HUB})
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_rejects_incompatible_type_conflict_and_routes_to_earliest(tmp_path):
    # terrain and prop both define /World/Hub but disagree on its type. Reject and
    # route back to terrain (the pipeline-earliest of the two), which re-runs prop.
    layers = _set_with(tmp_path, {"terrain": HUB_AS_MESH, "prop": HUB_AS_XFORM})
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("incompatible types" in issue for issue in verdict.issues)
    assert _failing_specialist(verdict.issues) == "terrain"


async def test_run_rejects_dangling_override_and_routes_to_offender(tmp_path):
    layers = _set_with(tmp_path, {"biome": DANGLES_GHOST})
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("dangling override" in issue for issue in verdict.issues)
    assert _failing_specialist(verdict.issues) == "biome"
