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
from optimization import optimization
from runtime.supervisor import _failing_specialist, _route_back_target
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


# Metrics each role is contracted to emit (mirrors the real terrain/biome/
# optimization stubs); other specialists emit none. _full_set applies these so a
# fully-formed world satisfies the validator's per-role metrics schema.
_ROLE_METRICS = {
    "terrain": {"triangles": 262144.0},
    "biome": {"triangles": 100000.0},
    "prop": {"triangles": 96000.0},
    "npc": {"triangles": 144000.0},
    "optimization": {"over_budget": 0.0},
}

# The geometry the optimizer sees: the triangle metrics of every prior layer summed
# the way optimization._resolve sums them. The validator's budget re-derivation checks
# the optimization body's authoredTriangles against this, so a realistic optimization
# fixture must record exactly this figure (else it would read as a STALE layer).
SUMMED_PRIOR = float(sum(m.get("triangles", 0.0) for m in _ROLE_METRICS.values()))


def _opt_body(
    *,
    budget: int | str = optimization.TRIANGLE_BUDGET,
    authored: float = SUMMED_PRIOR,
    observed: float | None = None,
    over_budget: bool | None = None,
    directives: str = "",
) -> str:
    """A realistic optimization layer body for the validator's budget re-derivation.

    Defaults are mutually CONSISTENT: observed == authored (no shedding), overBudget
    re-derived from observed vs budget, authored == the summed prior geometry. A test
    overrides one field to model the desync (a stale layer, a phantom figure, a metric
    that disagrees with the body) the validator must catch."""
    if observed is None:
        observed = authored
    if over_budget is None:
        over_budget = observed > budget
    return (
        "#usda 1.0\n(\n    defaultPrim = \"Optimization\"\n)\n\n"
        'def Scope "Optimization"\n{\n'
        f"    custom int triangleBudget = {budget}\n"
        f"    custom double authoredTriangles = {authored}\n"
        f"    custom double observedTriangles = {observed}\n"
        f"    custom bool overBudget = {str(bool(over_budget)).lower()}\n"
        f"    custom int resolvePasses = 0{directives}\n"
        "}\n"
    )


def _layer(
    root: Path,
    specialist: str,
    body: str | None = VALID_BODY,
    metrics: dict | None = None,
) -> LayerSpec:
    """Write a layer file for `specialist` under `root` (skipped when body is
    None, to model a LayerSpec pointing at a missing file) and return its spec.
    `metrics` defaults to the role's contracted metrics so a layer satisfies the
    schema unless a test deliberately overrides it. The optimization layer defaults to
    a realistic optimizer body (consistent with its over_budget metric) so the
    validator's budget self-consistency re-derivation has a real artifact to audit;
    pass an explicit body to model a malformed/garbage optimization file."""
    rel = f"{specialist}/{REGION}.usda"
    if metrics is None:
        metrics = dict(_ROLE_METRICS.get(specialist, {}))
    if specialist == "optimization" and body is VALID_BODY:
        ob = metrics.get("over_budget", 0.0)
        over = isinstance(ob, (int, float)) and not isinstance(ob, bool) and ob > 0
        body = _opt_body(budget=500_000 if over else optimization.TRIANGLE_BUDGET)
    if body is not None:
        full = root / rel
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(body)
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


async def test_run_marks_over_budget_terminal_and_blames_no_specialist(tmp_path):
    # over_budget>0 is the genuine budget-exceeded REJECTION (the case the nan/missing/
    # garbage tests above deliberately make the gate SKIP). Optimization is the last,
    # deterministic LOD authority, so a re-run recomputes the same sum — the verdict is
    # terminal and names NO failing specialist (re-running can't help). With nothing
    # fixable left, route-back ENDs instead of looping to MAX_ROUNDS.
    opt = _layer(tmp_path, "optimization", metrics={"over_budget": 1.0})
    verdict = await validator.run(_brief(), _with_override(tmp_path, "optimization", opt), layers_root=tmp_path)
    assert not verdict.accepted
    assert verdict.terminal
    assert any("triangle budget" in i for i in verdict.issues)
    assert verdict.failing_specialists == []  # over_budget blames nobody — not optimization
    assert _route_back_target(verdict) is None  # terminal + no fixable → END


async def test_run_over_budget_with_co_occurring_fixable_still_routes_back(tmp_path):
    # FM2: a fixable issue co-occurring with over_budget must NOT be short-circuited.
    # A missing biome layer (re-runnable) alongside an over-budget world: the verdict
    # is terminal AND blames biome; route-back repairs the fixable biome first, so the
    # terminal flag never strands the recoverable issue.
    opt = _layer(tmp_path, "optimization", metrics={"over_budget": 1.0})
    layers = [layer for layer in _with_override(tmp_path, "optimization", opt) if layer.specialist != "biome"]
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert verdict.terminal
    assert "biome" in verdict.failing_specialists
    assert "optimization" not in verdict.failing_specialists
    assert _route_back_target(verdict) == "biome"


async def test_run_over_budget_with_malformed_optimization_file_routes_back_to_fix_it(tmp_path):
    # The terminal flag is about the over_budget METRIC (a deterministic recompute),
    # NOT about file I/O. A co-occurring MALFORMED optimization FILE is a separate,
    # genuinely re-runnable issue — the wellformedness gate routes any bad-file
    # specialist back to re-run (a write can fail transiently). So a verdict that is
    # BOTH terminal AND blames optimization for its malformed file routes back to
    # optimization to repair the file; terminal does not strand it (FM2 applied to
    # optimization's own fixable flaw). A re-run that writes a valid file then sees
    # over_budget alone → terminal, no fixable → END (converges in two rounds); a
    # persistently garbage-writing optimizer is bounded by MAX_ROUNDS like any broken
    # specialist — NOT the deterministic-over-budget-alone world the slice converges.
    opt = _layer(tmp_path, "optimization", body="garbage\n", metrics={"over_budget": 1.0})
    verdict = await validator.run(_brief(), _with_override(tmp_path, "optimization", opt), layers_root=tmp_path)
    assert not verdict.accepted
    assert verdict.terminal  # the over-budget signal is still terminal
    assert "optimization" in verdict.failing_specialists  # but the malformed file IS re-runnable
    assert _route_back_target(verdict) == "optimization"  # repair the file; terminal doesn't strand it


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
    # FM4: a role with no ROLE_METRICS entry (director) and no metrics must return no
    # issues and must not KeyError.
    director = _layer(tmp_path, "director", metrics={})
    assert validator._metrics_issues(director) == []


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


# ---- budget self-consistency re-derivation (defense-in-depth across the optimizer
# trust boundary) ----
#
# The over_budget gate trusts optimization's self-reported metric; these pin that the
# validator — the last gate — independently re-derives the verdict from the emitted body
# (triangleBudget/observedTriangles/overBudget/LodDirectives) + the summed current prior
# geometry, so a STALE or desynced optimization layer can't ship an over-budget world
# accepted. A mismatch routes back to optimization to re-run; a malformed body field is
# reported, not raised.


def _opt_override(root: Path, *, body: str, over_budget: float = 0.0) -> list[LayerSpec]:
    """A full well-formed default-geometry set (summed prior triangles == SUMMED_PRIOR)
    with the optimization layer carrying `body` and the given over_budget metric."""
    opt = _layer(root, "optimization", body=body, metrics={"over_budget": over_budget})
    return _with_override(root, "optimization", opt)


async def test_run_rejects_over_budget_body_with_within_budget_metric(tmp_path):
    # FM1: optimization reports over_budget=0.0 but its body records observedTriangles
    # over triangleBudget (overBudget=true). The metric the gate trusts and the body
    # disagree; the validator re-derives observed>budget and rejects, routing back to
    # optimization — the over-budget world is NOT shipped accepted on the stale metric.
    layers = _opt_override(tmp_path, body=_opt_body(budget=500_000), over_budget=0.0)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("over_budget metric disagrees with its body" in i for i in verdict.issues)
    assert "optimization" in verdict.failing_specialists
    assert _route_back_target(verdict) == "optimization"


async def test_run_rejects_body_overbudget_flag_inconsistent_with_its_numbers(tmp_path):
    # FM1 variant: the body's overBudget flag claims over budget while its own
    # observedTriangles sits within triangleBudget (and the metric agrees: within). The
    # flag contradicts the numbers printed beside it — a value changed in isolation — so
    # the re-derivation rejects it even though metric and re-derived numeric verdict match.
    body = _opt_body(budget=1_500_000, observed=SUMMED_PRIOR, over_budget=True)
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert any("overBudget=true" in i and "internally inconsistent" in i for i in verdict.issues)
    assert _route_back_target(verdict) == "optimization"


async def test_run_rejects_a_stale_optimization_layer(tmp_path):
    # FM2: the geometry re-ran heavier after a route-back but optimization's cached layer
    # did not — its authoredTriangles records the OLD lighter geometry and its metric says
    # within budget, while the current prior layers sum OVER the budget it used. The cached
    # metric would ship the over-budget world accepted; the validator re-derives authored
    # against the current geometry and rejects the stale layer.
    stale = 300_000.0
    body = _opt_body(budget=500_000, authored=stale, observed=stale, over_budget=False)
    assert SUMMED_PRIOR > 500_000  # the current geometry genuinely exceeds the used budget
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert any("stale" in i and "summed from the current prior layers" in i for i in verdict.issues)
    assert _route_back_target(verdict) == "optimization"


async def test_run_rejects_a_phantom_observed_triangle_figure(tmp_path):
    # FM3: the body claims observedTriangles far under authoredTriangles but records NO LOD
    # reductions to account for the drop — a number changed in isolation that hides a
    # genuinely over-budget world (authored 602144 over the 500k budget) behind a fake
    # observed. The metric, computed from the same phantom, says within. The re-derivation
    # reconstructs observed from authored minus the recorded reductions and rejects it.
    body = _opt_body(budget=500_000, authored=SUMMED_PRIOR, observed=400_000, over_budget=False)
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert any("phantom triangle figure" in i for i in verdict.issues)
    assert _route_back_target(verdict) == "optimization"


async def test_run_reports_a_malformed_optimization_body_not_raises(tmp_path):
    # FM4: a well-formed USD optimization layer (header + defaultPrim) whose budget body is
    # missing observedTriangles. The re-derivation must report MALFORMED with a clear
    # reason and route back to optimization — never crash the final gate on the parse.
    body = (
        '#usda 1.0\n(\n    defaultPrim = "Optimization"\n)\n\n'
        'def Scope "Optimization"\n{\n'
        "    custom int triangleBudget = 1500000\n"
        "    custom double authoredTriangles = 602144.0\n"
        "    custom bool overBudget = false\n"
        "}\n"
    )
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert any("malformed" in i and "observedTriangles" in i for i in verdict.issues)
    assert _route_back_target(verdict) == "optimization"


async def test_run_reports_a_non_finite_budget_field_as_malformed(tmp_path):
    # FM4 hardening: float() accepts 'inf'/'nan'/'1e999', so a body whose triangleBudget
    # parses to a non-finite value would make observedTriangles > triangleBudget always
    # false and silently accept an over-budget world. A tampered/stale body's non-finite
    # figure must be MALFORMED, not trusted — the over_budget metric path already rejects
    # non-finite, and the body path must too.
    body = _opt_body(budget="1e999", over_budget=False)  # 1e999 overflows to inf
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert any("malformed" in i and "triangleBudget" in i for i in verdict.issues)
    assert _route_back_target(verdict) == "optimization"


async def test_run_accepts_a_resolved_world_with_recorded_lod_reductions(tmp_path):
    # A genuinely over-authored world the optimizer RESOLVED by shedding: observed sits
    # under budget, the drop is fully accounted for by recorded LodDirectives, and authored
    # matches the summed geometry. The re-derivation reconstructs observed from authored
    # minus the reductions, agrees on every check, and stays silent — accepted.
    directives = (
        '\n\n    def Scope "LodDirectives"\n    {\n'
        '        def Scope "Lod_0"\n        {\n'
        '            custom string specialist = "biome"\n'
        '            custom string layer = "biome/r.usda"\n'
        "            custom int lodLevel = 1\n"
        "            custom double lodScale = 0.5\n"
        "            custom double authoredTriangles = 202144.0\n"
        "            custom double effectiveTriangles = 101072.0\n"
        "        }\n    }"
    )
    # authored 602144, shed 202144 -> 101072 (reductions 101072), observed 501072 (within).
    body = _opt_body(
        budget=1_500_000, authored=SUMMED_PRIOR, observed=501_072.0, over_budget=False,
        directives=directives,
    )
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_skips_re_derivation_when_a_geometry_layer_is_missing(tmp_path):
    # The re-derivation runs only when the inputs are trustworthy: a missing geometry
    # specialist makes the summed prior incomplete, so the stale-check would spuriously
    # fire. With biome absent, the missing-layer gate owns the rejection and routes to
    # biome; the budget re-derivation does not pile on a bogus optimization blame.
    layers = [layer for layer in _opt_override(tmp_path, body=_opt_body()) if layer.specialist != "biome"]
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert "optimization" not in verdict.failing_specialists
    assert _route_back_target(verdict) == "biome"


def test_summed_prior_triangles_sums_geometry_skips_optimization_and_nonfinite():
    layers = [
        LayerSpec(specialist="terrain", region_id="r", path="t", summary="", metrics={"triangles": 262144.0}),
        LayerSpec(specialist="prop", region_id="r", path="p", summary="", metrics={"triangles": 96000.0}),
        # optimization's own triangles (if any) must not count toward the prior sum.
        LayerSpec(specialist="optimization", region_id="r", path="o", summary="", metrics={"triangles": 9.9, "over_budget": 0.0}),
        LayerSpec(specialist="director", region_id="r", path="d", summary="", metrics={}),
    ]
    assert validator._summed_prior_triangles(layers) == 358144.0


def test_opt_number_and_bool_parse_or_none_on_garbage():
    # FM4 at the parse layer: a missing or non-numeric field returns None (so the caller
    # reports MALFORMED) instead of raising.
    text = (
        "custom int triangleBudget = 1500000\n"
        "custom double observedTriangles = 602144.0\n"
        "custom bool overBudget = false\n"
        "custom double garbage = lots\n"
    )
    assert validator._opt_number(text, "triangleBudget") == 1500000.0
    assert validator._opt_number(text, "observedTriangles") == 602144.0
    assert validator._opt_number(text, "missing") is None
    assert validator._opt_number(text, "garbage") is None
    assert validator._opt_bool(text, "overBudget") is False
    assert validator._opt_bool(text, "missing") is None


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


# USDA treats newlines as token separators, so a declaration head may wrap across
# lines. Pre-fix the [ \t]-only separators missed a wrapped `def`/`over` head and
# the prim vanished (and `{` opened a phantom empty scope); the four below resolve
# to the same paths a single-line head would. DISCRIMINATING vs the pre-fix scanner.
def test_prim_specs_resolves_a_newline_split_declaration_head():
    body = (
        '#usda 1.0\n(\n    defaultPrim = "World"\n)\n'
        'def Xform "World" {\n'
        "    def\n    Mesh\n    \"Hub\" {}\n"
        "    over\n    \"Patch\" {}\n"
        "}\n"
    )
    assert validator._prim_specs(body) == [
        ("def", "Xform", "/World"),
        ("def", "Mesh", "/World/Hub"),
        ("over", "", "/World/Patch"),
    ]


def test_prim_specs_newline_split_decl_under_variant_attributes_to_parent():
    # The wrapped child sits inside a variant: it must attribute to the enclosing
    # PRIM (/World/Mat), not a phantom path and not dropped — composing the newline
    # head with the transparent-variant-scope logic (FM3 brace/scope accounting).
    body = (
        '#usda 1.0\n(\n    defaultPrim = "World"\n)\n'
        'def Xform "World" {\n'
        '    variantSet "look" = {\n'
        '        "red" {\n'
        "            def\n            Material\n            \"Mat\" {}\n"
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


def test_prim_specs_bare_specifier_does_not_swallow_following_decl():
    # A malformed bare `over` (no name) must not consume the following `def "Real"`
    # as its type/name: the head's negative lookahead forbids a specifier keyword as
    # a type, so Real resolves as a typeless def, not type="def" under `over` (FM2).
    body = '#usda 1.0\ndef Xform "World" {\n    over\n    def "Real" {}\n}\n'
    assert validator._prim_specs(body) == [
        ("def", "Xform", "/World"),
        ("def", "", "/World/Real"),
    ]


def test_prim_specs_multiline_metadata_block_is_not_a_prim():
    # A prim's ( ) metadata spanning lines — a nested dict brace, a list, and a
    # string holding a fake `def` — opens no prim even with newlines now matchable
    # (it sits at paren depth > 0, where a decl is never attempted). Guard for FM2.
    body = (
        '#usda 1.0\n(\n    defaultPrim = "World"\n)\n'
        'def Xform "World" (\n'
        '    kind = "group"\n'
        "    customData = {\n"
        '        string note = "def Fake \\"X\\""\n'
        "    }\n"
        '    prepend apiSchemas = [\n        "PhysicsCollisionAPI",\n    ]\n'
        ")\n{\n"
        '    def Mesh "Hub" {}\n'
        "}\n"
    )
    assert validator._prim_specs(body) == [
        ("def", "Xform", "/World"),
        ("def", "Mesh", "/World/Hub"),
    ]


# End-to-end: two specialists define <World/Mat> with incompatible types, each via a
# newline-split declaration head. Pre-fix neither head matched, so /World/Mat carried
# only one type (or none) and no conflict surfaced — accept-when-broken. Post-fix the
# conflict is detected at the real path and routes to biome (pipeline-earliest).
def test_newline_split_decls_conflict_at_real_path_and_route_earliest():
    a = '#usda 1.0\ndef Xform "World" {\n  def\n  Material\n  "Mat" {}\n}\n'
    b = '#usda 1.0\ndef Xform "World" {\n  def Scope "Mat" {}\n}\n'
    tagged = [("biome", *s) for s in validator._prim_specs(a)]
    tagged += [("prop", *s) for s in validator._prim_specs(b)]
    issues = validator._conflicts_from_specs(tagged)
    assert len(issues) == 1
    assert "</World/Mat>" in issues[0]
    assert "Material" in issues[0] and "Scope" in issues[0]
    assert _failing_specialist(issues) == "biome"


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


# A prim literally named "terrain" under /World, defined with incompatible types by
# two specialists — the exact-path-segment residual the text scan cannot resolve.
TERRAIN_PRIM_AS_MESH = """#usda 1.0
(
    defaultPrim = "World"
)
def Xform "World" {
    def Mesh "terrain" {}
}
"""
TERRAIN_PRIM_AS_XFORM = """#usda 1.0
(
    defaultPrim = "World"
)
def Xform "World" {
    def Xform "terrain" {}
}
"""


# ---- structured failing-specialist attribution on the verdict (end to end) ----


async def test_run_attributes_conflict_to_authors_not_path_segment(tmp_path):
    # The headline fix, end to end: biome + prop both define </World/terrain> with
    # incompatible types. The verdict's structured attribution names the AUTHORS
    # (biome, prop), drawn from the layer tags — not the path segment "terrain" — so
    # route-back repairs biome, while the legacy text scan over the same issues still
    # misroutes to the innocent terrain. This is the residual the slice eliminates.
    layers = _set_with(tmp_path, {"biome": TERRAIN_PRIM_AS_MESH, "prop": TERRAIN_PRIM_AS_XFORM})
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert verdict.failing_specialists == ["biome", "prop"]
    assert _failing_specialist(verdict.issues) == "terrain"  # residual in the text
    assert _route_back_target(verdict) == "biome"  # structured fixes it


async def test_run_attributes_missing_layer_and_metric_to_their_specialists(tmp_path):
    # A missing biome layer + optimization missing its required over_budget metric:
    # each is attributed structurally to the owning specialist (missing-layer and
    # metrics-schema sources), and route-back picks the pipeline-earliest (biome).
    layers = [
        _layer(tmp_path, "director"),
        _layer(tmp_path, "terrain"),
        # biome omitted → missing layer
        _layer(tmp_path, "prop"),
        _layer(tmp_path, "lighting"),
        _layer(tmp_path, "npc"),
        _layer(tmp_path, "optimization", metrics={}),  # drop the required over_budget
    ]
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert "biome" in verdict.failing_specialists  # missing layer
    assert "optimization" in verdict.failing_specialists  # missing required metric
    assert _route_back_target(verdict) == "biome"


async def test_run_attributes_malformed_layer_to_its_specialist(tmp_path):
    # Well-formedness source: a malformed biome layer is attributed to biome and
    # nothing else, so the structured field is exactly ["biome"].
    layers = _set_with(tmp_path, {"biome": "not a usda file"})
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert verdict.failing_specialists == ["biome"]
    assert _route_back_target(verdict) == "biome"


# ---- director-intent gate (declare → honor → VERIFY) ----
#
# The director DECLARES hard intent (intent:must_have, intent:must_not) and prop/biome
# HONOR it; these pin that the validator — the last gate — VERIFIES they did, reusing
# prop's/biome's own mapping helpers so it demands ONLY what those specialists would
# actually emit (no false reject of a correctly-skipped unmappable token), and routes a
# real violation back to the offending specialist.


def _director_body(*, must_have: str | None = None, must_not: str | None = None) -> str:
    """A well-formed director layer seeding the given intent. An omitted field emits no
    attribute (the director predates it / seeded nothing) so the gate stays silent."""
    lines = ["#usda 1.0", "(", '    defaultPrim = "Director"', ")", "", 'def Scope "Director"', "{"]
    if must_have is not None:
        lines.append(f'    custom string intent:must_have = "{must_have}"')
    if must_not is not None:
        lines.append(f'    custom string intent:must_not = "{must_not}"')
    lines.append("}")
    return "\n".join(lines) + "\n"


def _prop_body(required_assets: list[str]) -> str:
    """A prop layer placing one Required prim per asset (the `requiredAsset` marker the
    gate keys on), beside the fill PointInstancer — mirroring prop._required_block."""
    prims = "".join(
        f'\ndef Xform "Required_{i}"\n{{\n    custom string requiredAsset = "{a}"\n}}\n'
        for i, a in enumerate(required_assets)
    )
    return (
        '#usda 1.0\n(\n    defaultPrim = "Props"\n)\n\n'
        'def PointInstancer "Props"\n{\n    custom string propAsset = "barricade_01"\n}\n'
        + prims
    )


def _biome_body(*, capped: bool) -> str:
    """A biome layer that emits the vegetationCapped marker iff `capped` — mirroring
    biome.run's cap_line."""
    cap = "\n    custom bool vegetationCapped = true" if capped else ""
    return (
        '#usda 1.0\n(\n    defaultPrim = "Biome"\n)\n\n'
        f'def PointInstancer "Scatter"\n{{\n    custom int instanceCount = 100{cap}\n}}\n'
    )


def _set_with_missing(root: Path, bodies: dict[str, str], missing: str) -> list[LayerSpec]:
    """A full set with `bodies` applied and `missing`'s file left unwritten (its spec
    still present) — a specialist that failed/timed out, leaving a dangling layer."""
    return [
        _layer(root, s, body=None if s == missing else bodies.get(s, VALID_BODY))
        for s in SPECIALISTS
    ]


async def test_run_accepts_a_world_that_honors_director_intent(tmp_path):
    # The real pipeline's shape: director seeds both intents, prop places every mapped
    # must-have, biome caps. The gate finds nothing unmet — accepted, byte-clean.
    layers = _set_with(tmp_path, {
        "director": _director_body(must_have="comms_tower,convoy_wreck", must_not="dense_vegetation"),
        "prop": _prop_body(["comms_tower_01", "convoy_wreck_01"]),
        "biome": _biome_body(capped=True),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues
    assert verdict.issues == []


async def test_run_rejects_a_dropped_must_have_and_routes_to_prop(tmp_path):
    # FM2 (false accept): prop places comms_tower_01 but DROPS the convoy_wreck_01 the
    # director marked must_have. The gate must catch the real violation and name prop.
    layers = _set_with(tmp_path, {
        "director": _director_body(must_have="comms_tower,convoy_wreck"),
        "prop": _prop_body(["comms_tower_01"]),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:must_have" in i and "convoy_wreck_01" in i for i in verdict.issues)
    assert "prop" in verdict.failing_specialists
    # the message names no other specialist, so even the text-scan fallback routes right.
    assert _failing_specialist(verdict.issues) == "prop"
    assert _route_back_target(verdict) == "prop"


async def test_run_rejects_an_uncapped_biome_under_a_capping_director(tmp_path):
    # FM2 (false accept, biome twin): the director's must_not carries the cap token but
    # biome emits no vegetationCapped marker — rejected and routed back to biome.
    layers = _set_with(tmp_path, {
        "director": _director_body(must_not="dense_vegetation"),
        "biome": _biome_body(capped=False),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:must_not" in i and "vegetationCapped" in i for i in verdict.issues)
    assert "biome" in verdict.failing_specialists
    # the message keys off intent:must_not, not the word "director", so the text-scan
    # fallback agrees with the structured attribution (no misroute to director).
    assert _failing_specialist(verdict.issues) == "biome"
    assert _route_back_target(verdict) == "biome"


async def test_run_does_not_false_reject_an_unmappable_must_have_token(tmp_path):
    # FM1 (the worst failure — an infinite revision loop): the director names a token
    # with NO MUST_HAVE_ASSET mapping. prop skips it (places nothing), so the gate must
    # require only the MAPPED asset — demanding a Required prim for the unmappable token
    # would route a correctly-built world back forever.
    layers = _set_with(tmp_path, {
        "director": _director_body(must_have="comms_tower,floating_citadel"),
        "prop": _prop_body(["comms_tower_01"]),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_does_not_require_a_cap_for_non_cap_must_not_tokens(tmp_path):
    # FM1 (biome twin): a must_not of other specialists' constraints (interior_volumes,
    # civilians) carries no recognized vegetation-cap token, so biome correctly does NOT
    # cap. The gate must not demand a vegetationCapped marker — the world stays accepted.
    layers = _set_with(tmp_path, {
        "director": _director_body(must_not="interior_volumes,civilians"),
        "biome": _biome_body(capped=False),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_intent_gate_silent_when_director_seeds_no_intent(tmp_path):
    # FM3 (back-compat): a director with neither must_have nor must_not leaves the gate
    # dormant — an uncapped biome and a prop with no Required prims still validate.
    layers = _set_with(tmp_path, {
        "director": _director_body(),
        "biome": _biome_body(capped=False),
        "prop": _prop_body([]),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_intent_gate_degrades_on_a_missing_prop_layer(tmp_path):
    # FM3 (missing-layer degrade): the director declares a must_have but prop's FILE is
    # gone (it failed/timed out). The intent gate must skip the unreadable layer, never
    # crash the final gate; the missing-layer gate rejects and routes back to prop.
    bodies = {"director": _director_body(must_have="comms_tower,convoy_wreck")}
    verdict = await validator.run(
        _brief(), _set_with_missing(tmp_path, bodies, missing="prop"), layers_root=tmp_path
    )
    assert not verdict.accepted
    assert "prop" in verdict.failing_specialists
    assert _route_back_target(verdict) == "prop"


async def test_run_intent_gate_degrades_on_a_missing_biome_layer(tmp_path):
    # FM3 (missing-layer degrade, biome twin): the director declares a cap token but
    # biome's FILE is gone. The gate skips it instead of crashing; biome is routed back.
    bodies = {"director": _director_body(must_not="dense_vegetation")}
    verdict = await validator.run(
        _brief(), _set_with_missing(tmp_path, bodies, missing="biome"), layers_root=tmp_path
    )
    assert not verdict.accepted
    assert "biome" in verdict.failing_specialists
    assert _route_back_target(verdict) == "biome"
