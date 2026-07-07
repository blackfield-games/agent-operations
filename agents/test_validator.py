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

from biome import biome
from common.types import LayerSpec, RegionCoord, WorldBrief
from director import director
from lighting import lighting
from npc import npc
from optimization import optimization
from prop import prop
from runtime.supervisor import _failing_specialist, _route_back_target
from terrain import terrain
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
        "            custom double authoredTriangles = 100000.0\n"
        "            custom double effectiveTriangles = 50000.0\n"
        "        }\n    }"
    )
    # authored 602144; biome (its real 100000) shed to 50000 (reductions 50000), observed 552144
    # (within). The directive's authoredTriangles == biome's real metric, so the per-directive
    # grounding stays silent alongside the sum re-derivation.
    body = _opt_body(
        budget=1_500_000, authored=SUMMED_PRIOR, observed=552_144.0, over_budget=False,
        directives=directives,
    )
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


def _lod_directive(*, level, scale, authored, effective, name="Lod_0", drop=(), specialist=None):
    """One LodDirectives block for the halving-legality unit test. `drop` omits fields
    (to model a malformed directive); `specialist` adds the named-layer identity the
    per-directive authored grounding reads back."""
    lines = [
        f'        custom int lodLevel = {level}',
        f'        custom double lodScale = {scale}',
        f'        custom double authoredTriangles = {authored}',
        f'        custom double effectiveTriangles = {effective}',
    ]
    if specialist is not None:
        lines.insert(0, f'        custom string specialist = "{specialist}"')
    kept = "\n".join(line for line in lines if not any(f" {d} =" in line for d in drop))
    return f'    def Scope "{name}"\n    {{\n{kept}\n    }}'


def test_lod_directive_legality_passes_legal_sheds_and_flags_each_illegal_kind():
    # The per-directive re-derivation: a legal shed (effective == authored/(1<<level),
    # 1<=level<=MAX_LOD) is silent; each illegal kind — below-floor fabricated effective,
    # a level out of range, a scale that mismatches its level, a malformed directive — is
    # named for route-back. This is the branch-level twin of the end-to-end run() tests.
    legal = _lod_directive(level=2, scale=0.25, authored=80000.0, effective=20000.0)
    assert validator._lod_directive_legality("optimization/r.usda", legal) == []

    below_floor = _lod_directive(level=3, scale=0.125, authored=80000.0, effective=1.0)
    msgs = validator._lod_directive_legality("optimization/r.usda", below_floor)
    assert len(msgs) == 1 and "effectiveTriangles" in msgs[0] and "fabricated" in msgs[0]

    over_max = _lod_directive(level=4, scale=0.0625, authored=80000.0, effective=5000.0)
    msgs = validator._lod_directive_legality("optimization/r.usda", over_max)
    assert len(msgs) == 1 and "lodLevel 4" in msgs[0] and "1..=3" in msgs[0]

    below_min = _lod_directive(level=0, scale=1.0, authored=80000.0, effective=80000.0)
    msgs = validator._lod_directive_legality("optimization/r.usda", below_min)
    assert len(msgs) == 1 and "lodLevel 0" in msgs[0]

    # scale 0.125 matches its OWN effective (80000*0.125=10000) but not level 1's legal 0.5,
    # so the level<->scale check fires before the effective check — a subtler tamper.
    bad_scale = _lod_directive(level=1, scale=0.125, authored=80000.0, effective=10000.0)
    msgs = validator._lod_directive_legality("optimization/r.usda", bad_scale)
    assert len(msgs) == 1 and "lodScale" in msgs[0] and "0.5" in msgs[0]

    malformed = _lod_directive(level=1, scale=0.5, authored=80000.0, effective=40000.0, drop=("lodLevel",))
    msgs = validator._lod_directive_legality("optimization/r.usda", malformed)
    assert len(msgs) == 1 and "malformed" in msgs[0]


async def test_run_rejects_a_below_floor_fabricated_lod_reduction(tmp_path):
    # The exploit the sum re-derivation alone cannot see: a directive claims to shed biome
    # (202144) to effectiveTriangles 1.0 — far below the 1/8 floor (25268) — fabricating a
    # 202143 reduction. observed is set to authored - that reduction so observed == authored
    # - Σreduction still balances, and at budget 410000 the fabricated observed 400001 reads
    # UNDER budget (overBudget false). Every existing _budget_self_consistency check passes,
    # so absent the halving check this genuinely-over-budget world (even biome's floor shed
    # leaves 425268 > 410000) ships ACCEPTED. The per-directive legality rejects it.
    reductions = 202144.0 - 1.0
    observed = SUMMED_PRIOR - reductions
    directives = (
        '\n\n    def Scope "LodDirectives"\n    {\n'
        '        def Scope "Lod_0"\n        {\n'
        '            custom string specialist = "biome"\n'
        '            custom string layer = "biome/r.usda"\n'
        "            custom int lodLevel = 3\n"
        "            custom double lodScale = 0.125\n"
        "            custom double authoredTriangles = 202144.0\n"
        "            custom double effectiveTriangles = 1.0\n"
        "        }\n    }"
    )
    body = _opt_body(budget=410_000, authored=SUMMED_PRIOR, observed=observed, over_budget=False, directives=directives)
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert "optimization" in verdict.failing_specialists
    assert _route_back_target(verdict) == "optimization"
    assert any("fabricated reduction" in issue for issue in verdict.issues), verdict.issues


async def test_run_rejects_a_lod_directive_whose_scale_mismatches_its_level(tmp_path):
    # A different illegal kind through the full run(): lodScale 0.125 on a lodLevel 1
    # directive (legal scale 0.5). effectiveTriangles 25268 matches the WRONG scale so the
    # sum reconciles and the world sits under the default budget — accepted absent the
    # check. The level<->scale re-derivation catches the mismatch and routes back.
    reductions = 202144.0 - 25268.0
    observed = SUMMED_PRIOR - reductions
    directives = (
        '\n\n    def Scope "LodDirectives"\n    {\n'
        '        def Scope "Lod_0"\n        {\n'
        '            custom string specialist = "biome"\n'
        '            custom string layer = "biome/r.usda"\n'
        "            custom int lodLevel = 1\n"
        "            custom double lodScale = 0.125\n"
        "            custom double authoredTriangles = 202144.0\n"
        "            custom double effectiveTriangles = 25268.0\n"
        "        }\n    }"
    )
    body = _opt_body(authored=SUMMED_PRIOR, observed=observed, over_budget=False, directives=directives)
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert _route_back_target(verdict) == "optimization"
    assert any("lodScale" in issue for issue in verdict.issues), verdict.issues


async def test_run_rejects_an_inflated_directive_authored_the_sum_cannot_see(tmp_path):
    # FM2 (the false-accept neither the sum re-derivation NOR the per-directive scale/effective
    # legality can see): a directive claims to shed biome at authoredTriangles=400000 — 4x biome's
    # real 100000 — at a RATIO-LEGAL effective (400000*0.125=50000), fabricating a 350000 reduction
    # off a phantom-sized base. observed is set to authored_top - that reduction so observed ==
    # authored - Σreduction still balances, and at budget 300000 the fabricated observed 252144 reads
    # UNDER budget (overBudget false). The scale check (0.125==legal for level 3) and effective check
    # (50000==400000*0.125) BOTH pass, so absent the authored grounding this ships ACCEPTED — yet biome
    # really holds 100000 and floors at 12500, so the world is genuinely over budget (fully shed:
    # 262144+12500+96000+144000 = 514644 > 300000). Grounding authored to the named layer catches it.
    reductions = 400000.0 - 50000.0
    observed = SUMMED_PRIOR - reductions  # 602144 - 350000 = 252144, under budget 300000
    directives = (
        '\n\n    def Scope "LodDirectives"\n    {\n'
        '        def Scope "Lod_0"\n        {\n'
        '            custom string specialist = "biome"\n'
        '            custom string layer = "biome/r.usda"\n'
        "            custom int lodLevel = 3\n"
        "            custom double lodScale = 0.125\n"
        "            custom double authoredTriangles = 400000.0\n"
        "            custom double effectiveTriangles = 50000.0\n"
        "        }\n    }"
    )
    body = _opt_body(budget=300_000, authored=SUMMED_PRIOR, observed=observed, over_budget=False, directives=directives)
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert "optimization" in verdict.failing_specialists
    assert _route_back_target(verdict) == "optimization"
    assert any("authoredTriangles" in i and "inflated authored base" in i for i in verdict.issues), verdict.issues


async def test_run_rejects_a_directive_shedding_a_floor_layer(tmp_path):
    # The floor-layer sibling of the inflated/phantom fabricated reductions: a directive sheds
    # TERRAIN — the do-not-shed floor. authoredTriangles=262144 IS terrain's real metric (so the
    # authored grounding stays silent) and 262144*0.125=32768 is a ratio-legal level-3 effective,
    # fabricating a 229376 reduction of geometry the optimizer never touches. observed is set to
    # authored - that reduction (372768) so the sum balances and reads UNDER the 500k budget. But
    # terrain is floored and never decimated at runtime, so the world really renders at its full
    # authored 602144 (> 500000) — a genuinely over-budget world shipped accepted. Scale, effective,
    # authored==terrain's metric and the sum ALL pass; only the floor-eligibility check rejects it.
    reductions = 262144.0 - 32768.0
    observed = SUMMED_PRIOR - reductions  # 602144 - 229376 = 372768, under budget 500000
    directives = (
        '\n\n    def Scope "LodDirectives"\n    {\n'
        '        def Scope "Lod_0"\n        {\n'
        '            custom string specialist = "terrain"\n'
        '            custom string layer = "terrain/r.usda"\n'
        "            custom int lodLevel = 3\n"
        "            custom double lodScale = 0.125\n"
        "            custom double authoredTriangles = 262144.0\n"
        "            custom double effectiveTriangles = 32768.0\n"
        "        }\n    }"
    )
    body = _opt_body(budget=500_000, authored=SUMMED_PRIOR, observed=observed, over_budget=False, directives=directives)
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert "optimization" in verdict.failing_specialists
    assert _route_back_target(verdict) == "optimization"
    assert any("floor layer" in i and "terrain" in i for i in verdict.issues), verdict.issues


async def test_run_rejects_a_directive_shedding_a_layer_twice(tmp_path):
    # The multiplicity sibling of the fabricated reductions: four ratio-legal LOD3 sheds, but PROP
    # is recorded TWICE. _opt_reductions sums (authored - effective) with no dedup and no per-layer
    # cap, and _budget_self_consistency ties only the TOTAL to the summed geometry, so prop is
    # credited a 168000 reduction (84000 twice) though it can physically give only 84000. Σreductions
    # 381500 (biome 87500 + prop 84000 + npc 126000 + prop 84000); observed set to 602144 - 381500 =
    # 220644, under the 300000 budget. Every existing gate passes: each names a real sheddable,
    # authored == its metric, scale/effective ratio-legal, observed == authored - Σreduction. But the
    # world is genuinely over budget — even fully shed it renders at 262144 + 12500 + 12000 + 18000 =
    # 304644 > 300000. The multiplicity check rejects the repeat and routes back to optimization.
    directives = (
        '\n\n    def Scope "LodDirectives"\n    {\n'
        '        def Scope "Lod_0"\n        {\n'
        '            custom string specialist = "biome"\n'
        '            custom string layer = "biome/r.usda"\n'
        "            custom int lodLevel = 3\n"
        "            custom double lodScale = 0.125\n"
        "            custom double authoredTriangles = 100000.0\n"
        "            custom double effectiveTriangles = 12500.0\n"
        "        }\n"
        '        def Scope "Lod_1"\n        {\n'
        '            custom string specialist = "prop"\n'
        '            custom string layer = "prop/r.usda"\n'
        "            custom int lodLevel = 3\n"
        "            custom double lodScale = 0.125\n"
        "            custom double authoredTriangles = 96000.0\n"
        "            custom double effectiveTriangles = 12000.0\n"
        "        }\n"
        '        def Scope "Lod_2"\n        {\n'
        '            custom string specialist = "npc"\n'
        '            custom string layer = "npc/r.usda"\n'
        "            custom int lodLevel = 3\n"
        "            custom double lodScale = 0.125\n"
        "            custom double authoredTriangles = 144000.0\n"
        "            custom double effectiveTriangles = 18000.0\n"
        "        }\n"
        '        def Scope "Lod_3"\n        {\n'
        '            custom string specialist = "prop"\n'
        '            custom string layer = "prop/r.usda"\n'
        "            custom int lodLevel = 3\n"
        "            custom double lodScale = 0.125\n"
        "            custom double authoredTriangles = 96000.0\n"
        "            custom double effectiveTriangles = 12000.0\n"
        "        }\n    }"
    )
    observed = SUMMED_PRIOR - 381_500.0  # 220644, under budget
    body = _opt_body(budget=300_000, authored=SUMMED_PRIOR, observed=observed, over_budget=False, directives=directives)
    verdict = await validator.run(_brief(), _opt_override(tmp_path, body=body), layers_root=tmp_path)
    assert not verdict.accepted
    assert "optimization" in verdict.failing_specialists
    assert _route_back_target(verdict) == "optimization"
    assert any("already shed" in i and "prop" in i for i in verdict.issues), verdict.issues


def test_lod_directive_grounding_flags_inflated_and_phantom_authored():
    # The per-directive authored grounding, which runs ONLY when a prior-triangle map is passed
    # (the run() path): a directive whose authoredTriangles equals the named layer's real metric is
    # silent; an INFLATED authored with a ratio-legal effective is flagged; a directive naming no
    # prior layer is a phantom shed. Without the map the legality-only contract holds (grounding
    # skipped even when authored matches nothing) — the branch-level twin of the run() exploit above.
    prior = {"biome": 100000.0, "prop": 96000.0}

    correct = _lod_directive(level=1, scale=0.5, authored=100000.0, effective=50000.0, specialist="biome")
    assert validator._lod_directive_legality("optimization/r.usda", correct, prior) == []
    # no map -> grounding skipped even though nothing would match (the direct-call legality-only path)
    assert validator._lod_directive_legality("optimization/r.usda", correct) == []

    inflated = _lod_directive(level=3, scale=0.125, authored=400000.0, effective=50000.0, specialist="biome")
    msgs = validator._lod_directive_legality("optimization/r.usda", inflated, prior)
    assert len(msgs) == 1 and "inflated authored base" in msgs[0] and "biome" in msgs[0]

    phantom = _lod_directive(level=1, scale=0.5, authored=80000.0, effective=40000.0, specialist="ghost")
    msgs = validator._lod_directive_legality("optimization/r.usda", phantom, prior)
    assert len(msgs) == 1 and "phantom shed" in msgs[0] and "'ghost'" in msgs[0]


def test_lod_directive_grounding_rejects_a_floor_layer_shed():
    # The optimizer routes floor-set specialists (optimization._resolve_floor(), default {'terrain'})
    # into floor_total and NEVER into the sheddable set, so no honest layer records a shed of one. A
    # terrain shed is a fabricated reduction of always-present geometry — rejected even though its
    # authoredTriangles equals terrain's real metric (the authored grounding stays silent) and its
    # scale/effective are ratio-legal. The floor check runs only with the map (the grounding path) and
    # is ordered before phantom/authored so a floored name gives one clear reason.
    prior = {"terrain": 262144.0, "biome": 100000.0}

    floor_shed = _lod_directive(level=3, scale=0.125, authored=262144.0, effective=32768.0, specialist="terrain")
    msgs = validator._lod_directive_legality("optimization/r.usda", floor_shed, prior)
    assert len(msgs) == 1 and "floor layer" in msgs[0] and "terrain" in msgs[0]
    assert "inflated" not in msgs[0] and "phantom" not in msgs[0]  # authored legal + present; floor is the reason
    # no map -> legality-only, floor check skipped like the authored grounding
    assert validator._lod_directive_legality("optimization/r.usda", floor_shed) == []

    # a stale floor NAME (floored but absent from the current prior layers) resolves to the floor
    # issue, not phantom — floor is checked first so the more specific reason wins (FM4).
    stale = _lod_directive(level=1, scale=0.5, authored=50000.0, effective=25000.0, specialist="terrain")
    msgs = validator._lod_directive_legality("optimization/r.usda", stale, {"biome": 100000.0})
    assert len(msgs) == 1 and "floor layer" in msgs[0] and "phantom" not in msgs[0]


def test_lod_directive_grounding_rejects_a_double_shed():
    # The multiplicity twin of the per-directive grounding: _opt_reductions sums (authored -
    # effective) over every Lod_N block with no dedup or per-layer cap, and the sum re-derivation
    # ties only the TOTAL to the summed geometry — so two ratio-legal directives naming the SAME
    # layer double-count its reduction (a specialist is shed at most once; _resolve emits one
    # directive per sheddable index). The repeat is rejected and NAMED; the first, and a single
    # shed of a distinct layer, stay silent. Runs only with the map (the grounding path); ordered
    # last so a floored/phantom/inflated repeat is reported for its own reason, not as a dup.
    prior = {"biome": 100000.0, "prop": 96000.0}

    first = _lod_directive(level=3, scale=0.125, authored=96000.0, effective=12000.0, specialist="prop")
    repeat = _lod_directive(level=3, scale=0.125, authored=96000.0, effective=12000.0, specialist="prop", name="Lod_1")
    msgs = validator._lod_directive_legality("optimization/r.usda", f"{first}\n{repeat}", prior)
    assert len(msgs) == 1 and "already shed" in msgs[0] and "prop" in msgs[0]
    assert "Lod_1" in msgs[0]  # the repeat is flagged, not the first legal shed

    # a distinct second layer is not a repeat — both shed once, both grounded
    biome = _lod_directive(level=1, scale=0.5, authored=100000.0, effective=50000.0, specialist="biome", name="Lod_1")
    assert validator._lod_directive_legality("optimization/r.usda", f"{first}\n{biome}", prior) == []

    # no map -> legality-only, the multiplicity check skipped like the rest of the grounding
    assert validator._lod_directive_legality("optimization/r.usda", f"{first}\n{repeat}") == []


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


def _director_body(
    *,
    must_have: str | None = None,
    must_not: str | None = None,
    factions: str | None = None,
    beats: str | None = None,
) -> str:
    """A well-formed director layer seeding the given intent. An omitted field emits no
    attribute (the director predates it / seeded nothing) so the gate stays silent. `beats`
    is the director's free-form mood line (prose, no commas) — the real director joins its
    per-region beat phrases into one such line."""
    lines = ["#usda 1.0", "(", '    defaultPrim = "Director"', ")", "", 'def Scope "Director"', "{"]
    if beats is not None:
        lines.append(f'    custom string intent:beats = "{beats}"')
    if must_have is not None:
        lines.append(f'    custom string intent:must_have = "{must_have}"')
    if must_not is not None:
        lines.append(f'    custom string intent:must_not = "{must_not}"')
    if factions is not None:
        lines.append(f'    custom string intent:factions = "{factions}"')
    lines.append("}")
    return "\n".join(lines) + "\n"


def _lighting_body(driven_by: list[str] | None = None, density: float | None = None) -> str:
    """A lighting layer mirroring lighting.run: the locked overcast+inferno-rim palette,
    plus a `def Volume "Atmosphere"` whose `drivenBy` lists `driven_by` iff it is given
    (None ⇒ the pre-beats palette with no Atmosphere — the back-compat floor the gate must
    accept when no beat is recognized). `inputs:density` defaults to the consistent
    lighting._fog_density(driven_by) lighting.run emits; pass `density` to author a
    drivenBy-correct-but-density-wrong layer (the density-self-consistency reject fixture)."""
    atmosphere = ""
    if driven_by is not None:
        fog = lighting._fog_density(driven_by) if density is None else density
        atmosphere = (
            '\n    def Volume "Atmosphere"\n    {\n'
            f"        float inputs:density = {fog:.2f}\n"
            f'        custom string drivenBy = "{",".join(driven_by)}"\n'
            '        custom string mood = "haze"\n    }'
        )
    return (
        '#usda 1.0\n(\n    defaultPrim = "Lighting"\n)\n\n'
        'def Xform "Lighting"\n{\n'
        '    def DomeLight "Sky"\n    {\n        custom string mood = "overcast"\n    }'
        f"{atmosphere}\n}}\n"
    )


# The region-true FILL asset the brief determines for the test REGION — prop.run emits exactly this
# (a stable per-region hash over the sorted ASSET_TRIS keys). The run-level fixtures carry it (not an
# arbitrary asset) so the propAsset↔brief SELECTION gate stays silent on the triangle/count-gate and
# intent fixtures, reused from prop's OWN helper so an ASSET_TRIS/salt change flips it in lock-step.
_REGION_ASSET = prop._select_asset(REGION)


def _prop_body(
    required_assets: list[str], placement_count: int | None = None, fill_asset: str = _REGION_ASSET
) -> str:
    """A prop layer placing one Required prim per asset (the `requiredAsset` marker the
    gate keys on), beside the fill PointInstancer (propAsset `fill_asset`, defaulting to
    the region-true `_REGION_ASSET` pick so the selection gate stays silent) — mirroring
    prop._required_block. `placementCount` is opt-in (omit ⇒ the triangle self-consistency
    gate can't re-derive the metric and skips the layer, so the intent fixtures that don't
    declare a count degrade-skip it); pass `placement_count` for the triangle fixtures.
    Pass `fill_asset` to model a stale/tampered fill (a wrong known asset, or an unknown
    one) the propAsset↔brief selection gate must catch or defer."""
    prims = "".join(
        f'\ndef Xform "Required_{i}"\n{{\n    custom string requiredAsset = "{a}"\n}}\n'
        for i, a in enumerate(required_assets)
    )
    count_line = f"\n    custom int placementCount = {placement_count}" if placement_count is not None else ""
    return (
        '#usda 1.0\n(\n    defaultPrim = "Props"\n)\n\n'
        f'def PointInstancer "Props"\n{{\n    custom string propAsset = "{fill_asset}"{count_line}\n}}\n'
        + prims
    )


def _biome_body(*, capped: bool, instance_count: int | None = None) -> str:
    """A biome layer that emits the vegetationCapped marker iff `capped` — mirroring
    biome.run's cap_line. `instance_count`, when given, emits the `instanceCount` field
    biome.run meters its triangle count off; omitted (the default) it models the
    placeholder bodies the intent/composition tests use, which the triangle gate skips
    (no count to re-derive from) exactly like terrain's no-gridResolution placeholders."""
    cap = "\n    custom bool vegetationCapped = true" if capped else ""
    count = f"\n    custom int instanceCount = {instance_count}" if instance_count is not None else ""
    return (
        '#usda 1.0\n(\n    defaultPrim = "Biome"\n)\n\n'
        f'def PointInstancer "Scatter"\n{{\n    custom string scatterRule = "post_conflict_sparse"'
        f"{count}{cap}\n}}\n"
    )


def _npc_body(archetype: str, spawn_count: int | None = None) -> str:
    """An npc layer emitting one `archetype` marker in its Spawns block — mirroring
    npc.run, the field the intent gate keys on. `spawnCount` is opt-in (omit ⇒ the
    triangle self-consistency gate can't re-derive the metric and skips the layer, so the
    factions fixtures that don't declare a count degrade-skip it — the same placeholder
    discipline biome's no-instanceCount bodies follow); pass `spawn_count` for the
    triangle fixtures."""
    count_line = f"        custom int spawnCount = {spawn_count}\n" if spawn_count is not None else ""
    return (
        '#usda 1.0\n(\n    defaultPrim = "NPCs"\n)\n\n'
        'def Xform "NPCs"\n{\n    def Xform "Spawns"\n    {\n'
        f'        custom string archetype = "{archetype}"\n'
        f"{count_line}    }}\n}}\n"
    )


# The region-true archetype the brief determines for the test REGION under an EMPTY roster — npc.run
# falls back to NPC_ARCHETYPE ("scavenger") when the director seeds no intent:factions. The empty-roster
# run-level fixtures (the metric/count sets, whose default director carries no factions) carry exactly
# this so the archetype↔brief SELECTION gate stays silent on them; fixtures WITH a roster derive the pick
# from npc._select_archetype(REGION, roster, forbidden) in lock-step so an ARCHETYPE_TRIS/salt change
# flips them automatically rather than silently drifting into a false-reject.
_REGION_ARCHETYPE = npc._select_archetype(REGION, [], frozenset())


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


async def test_run_rejects_an_over_placed_required_asset_and_routes_to_prop(tmp_path):
    # FM1/FM2 (false accept, the equality complement of the dropped case): prop places BOTH
    # must-haves — but also a DUPLICATE comms_tower_01 and an unrequested (real, in ASSET_TRIS)
    # fuel_depot_01. The must-have gate is satisfied (all demanded present) and a triangle-honest
    # metric would price the extra markers, so only the exact-multiset check catches a region
    # carrying MORE than its must-have set. Reject and name prop — never director, or a "director"
    # mention in the message would misroute the route-back to the director loop.
    layers = _set_with(tmp_path, {
        "director": _director_body(must_have="comms_tower,convoy_wreck"),
        "prop": _prop_body(["comms_tower_01", "convoy_wreck_01", "comms_tower_01", "fuel_depot_01"]),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any(
        "over-placed" in i and "comms_tower_01" in i and "fuel_depot_01" in i for i in verdict.issues
    )
    assert "prop" in verdict.failing_specialists
    assert not any("director" in i for i in verdict.issues)  # must not misroute to the director loop
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


# ---- biome cap MAGNITUDE: the marker present AND the instanceCount cap-reduced ----
#
# The presence check above catches a missing vegetationCapped marker; these pin that the
# validator also verifies the capped COUNT — the biome twin of lighting's density check over
# its drivenBy-correct branch. A marker slapped over the uncapped scatter (cap not actually
# applied) is rejected; a count consistent with the cap-reduced scatter is accepted; the
# check fires only on the marker-present branch and skips an absent count. The set builder
# carries the count as BOTH the body's instanceCount and a matching triangles metric (and
# re-syncs the optimizer to the summed geometry), so the biome triangle + budget gates stay
# silent and ONLY the cap-magnitude check varies — no double-report.

_CAPPED_COUNT = biome._scatter_count(_brief().biome, REGION, biome.VEGETATION_CAP_DENSITY)
_UNCAPPED_COUNT = biome._scatter_count(_brief().biome, REGION, None)


def _capped_biome_set(
    root: Path, *, count: int | None, capped: bool = True, caps: bool = True
) -> list[LayerSpec]:
    """A full set under a director that caps vegetation iff `caps`, whose biome layer emits the
    vegetationCapped marker iff `capped` and (when `count` is given) `count` as BOTH its
    instanceCount body field and a matching triangles metric — the optimizer body re-synced to
    the summed geometry so the biome triangle + budget gates stay silent and only the
    cap-magnitude check varies. `count=None` models the no-instanceCount placeholder body (the
    triangle gate skips it), keeping biome's default metric."""
    biome_tris = 100000.0 if count is None else float(count * biome.TRIS_PER_INSTANCE)
    summed = biome_tris + 262144.0 + 96000.0 + 144000.0  # terrain + prop + npc default metrics
    out: list[LayerSpec] = []
    for s in SPECIALISTS:
        if s == "director":
            director = _director_body(must_not="dense_vegetation" if caps else None)
            out.append(_layer(root, "director", body=director))
        elif s == "biome":
            out.append(_layer(
                root, "biome",
                body=_biome_body(capped=capped, instance_count=count),
                metrics={"triangles": biome_tris},
            ))
        elif s == "optimization":
            body = _opt_body(authored=summed, observed=summed, over_budget=False)
            out.append(_layer(root, "optimization", body=body, metrics={"over_budget": 0.0}))
        else:
            out.append(_layer(root, s))
    return out


async def test_run_accepts_a_capped_biome_whose_count_matches_the_cap(tmp_path):
    # FM1: biome caps AND emits the cap-reduced instanceCount the brief's region yields — a
    # correctly-capped world. The gate re-derives the same count and accepts. The cap genuinely
    # bites here (scorched_grassland's grassland base 2000 > the 800 cap), so the capped count is
    # strictly below the uncapped one — a real reduction, not a no-op marker.
    assert _CAPPED_COUNT < _UNCAPPED_COUNT
    verdict = await validator.run(
        _brief(), _capped_biome_set(tmp_path, count=_CAPPED_COUNT), layers_root=tmp_path
    )
    assert verdict.accepted, verdict.issues


async def test_run_rejects_a_capped_biome_carrying_the_uncapped_count(tmp_path):
    # FM2 (false accept): biome emits the vegetationCapped marker but the UNCAPPED count under it
    # — the cap was never applied to the geometry. Rejected, routed back to biome, the message
    # keyed off intent:must_not and naming the magnitude (not just the marker).
    verdict = await validator.run(
        _brief(), _capped_biome_set(tmp_path, count=_UNCAPPED_COUNT), layers_root=tmp_path
    )
    assert not verdict.accepted
    assert any(
        "intent:must_not" in i and "instanceCount" in i and str(_UNCAPPED_COUNT) in i
        for i in verdict.issues
    )
    assert "biome" in verdict.failing_specialists
    # names no pipeline-earlier specialist, so the text-scan fallback agrees and routes to biome.
    assert _failing_specialist(verdict.issues) == "biome"
    assert _route_back_target(verdict) == "biome"


async def test_run_capped_biome_magnitude_not_checked_when_the_marker_is_absent(tmp_path):
    # FM3 (no double-route): a capping director, but biome emits NO marker AND a wrong (uncapped)
    # count. The magnitude check is gated behind the marker, so this is the SINGLE presence
    # violation — never also a magnitude complaint about the same layer.
    verdict = await validator.run(
        _brief(),
        _capped_biome_set(tmp_path, count=_UNCAPPED_COUNT, capped=False),
        layers_root=tmp_path,
    )
    assert not verdict.accepted
    must_not_issues = [i for i in verdict.issues if "intent:must_not" in i]
    assert len(must_not_issues) == 1
    assert "vegetationCapped" in must_not_issues[0] and "instanceCount" not in must_not_issues[0]


async def test_run_skips_a_capped_biome_with_no_instance_count(tmp_path):
    # FM3 (degrade): the marker is present but the body declares no instanceCount — the placeholder
    # shape other gates' fixtures use. Field-presence is the well-formedness gate's concern, so the
    # magnitude check skips and the world validates.
    verdict = await validator.run(
        _brief(), _capped_biome_set(tmp_path, count=None), layers_root=tmp_path
    )
    assert verdict.accepted, verdict.issues


async def test_run_capped_biome_magnitude_tracks_the_cap_in_lock_step(tmp_path):
    # FM4 (vocabulary/algorithm desync): the expectation IS biome._scatter_count's output — the
    # exact capped count accepts, that count off-by-one rejects. A copied cap value or a
    # re-implemented jitter would drift from biome and either false-reject or miss the desync.
    ok = await validator.run(
        _brief(), _capped_biome_set(tmp_path, count=_CAPPED_COUNT), layers_root=tmp_path
    )
    assert ok.accepted, ok.issues
    off = await validator.run(
        _brief(), _capped_biome_set(tmp_path, count=_CAPPED_COUNT + 1), layers_root=tmp_path
    )
    assert not off.accepted


# ---- biome UNCAPPED scatter COUNT vs the brief: the else-branch twin of the cap magnitude ----
#
# The cap magnitude check above pins instanceCount to the brief ONLY under a vegetationCapped
# marker; an UNCAPPED region's count is a deterministic function of the brief too (biome.run emits
# _scatter_count(brief.biome, region, None)), yet nothing pinned it — a stale/tampered count with a
# consistent triangles metric passes the triangle + budget gates and ships an under/over-scattered
# world accepted. These pin the uncapped re-derivation, gated on NOT-capped so the cap magnitude
# gate keeps sole ownership of the capped case. _capped_biome_set(capped=False, caps=False) builds
# an uncapped world (no capping director, no marker), carrying the count as BOTH instanceCount and a
# matching triangles metric with the optimizer re-synced — so only the count re-derivation varies.


async def test_run_accepts_an_uncapped_biome_whose_count_matches_the_brief(tmp_path):
    # FM1: no capping director, biome emits the region-true uncapped scatter — the gate re-derives
    # the same count off the brief and accepts.
    verdict = await validator.run(
        _brief(),
        _capped_biome_set(tmp_path, count=_UNCAPPED_COUNT, capped=False, caps=False),
        layers_root=tmp_path,
    )
    assert verdict.accepted, verdict.issues


async def test_run_rejects_an_uncapped_biome_carrying_a_tampered_count(tmp_path):
    # FM2 (false accept — the exploit): a count changed in isolation, with a CONSISTENT triangles
    # metric AND a re-synced optimizer body (so the triangle + budget gates stay silent), ships an
    # under-scattered region. Only the brief re-derivation sees it: rejected, routed back to biome,
    # the message naming instanceCount and both the tampered and the expected count.
    tampered = _UNCAPPED_COUNT + 1
    verdict = await validator.run(
        _brief(),
        _capped_biome_set(tmp_path, count=tampered, capped=False, caps=False),
        layers_root=tmp_path,
    )
    assert not verdict.accepted
    assert any(
        "instanceCount" in i and str(tampered) in i and str(_UNCAPPED_COUNT) in i
        for i in verdict.issues
    )
    assert "biome" in verdict.failing_specialists
    # names no pipeline-earlier specialist, so the text-scan fallback agrees and routes to biome.
    assert _failing_specialist(verdict.issues) == "biome"
    assert _route_back_target(verdict) == "biome"


async def test_run_uncapped_count_not_checked_under_a_capping_director(tmp_path):
    # FM3 (no double-report / no false-reject): under a capping director the cap magnitude gate owns
    # the count. A correctly-capped region carries _CAPPED_COUNT (strictly below _UNCAPPED_COUNT), so
    # the uncapped re-derivation MUST be gated off here — ungated it would expect _UNCAPPED_COUNT and
    # false-reject the correctly-capped count.
    assert _CAPPED_COUNT != _UNCAPPED_COUNT
    verdict = await validator.run(
        _brief(), _capped_biome_set(tmp_path, count=_CAPPED_COUNT), layers_root=tmp_path
    )
    assert verdict.accepted, verdict.issues


async def test_run_skips_an_uncapped_biome_with_no_instance_count(tmp_path):
    # FM3 (degrade): the body declares no instanceCount (the placeholder shape other gates' fixtures
    # use) — field-presence is the well-formedness gate's concern, so the count re-derivation skips
    # (returns []) and the world validates.
    verdict = await validator.run(
        _brief(),
        _capped_biome_set(tmp_path, count=None, capped=False, caps=False),
        layers_root=tmp_path,
    )
    assert verdict.accepted, verdict.issues


async def test_run_uncapped_count_tracks_the_brief_in_lock_step(tmp_path):
    # FM4 (vocabulary/algorithm desync): the expectation IS biome._scatter_count(cap None)'s output —
    # the exact uncapped count accepts, that count off-by-one rejects. A copied density or a
    # re-implemented jitter would drift from biome and either false-reject a legit count or miss the
    # desync.
    ok = await validator.run(
        _brief(),
        _capped_biome_set(tmp_path, count=_UNCAPPED_COUNT, capped=False, caps=False),
        layers_root=tmp_path,
    )
    assert ok.accepted, ok.issues
    off = await validator.run(
        _brief(),
        _capped_biome_set(tmp_path, count=_UNCAPPED_COUNT + 1, capped=False, caps=False),
        layers_root=tmp_path,
    )
    assert not off.accepted


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


# ---- npc loop: archetype must be drawn from the director's faction roster ----
#
# The director DECLARES intent:factions and npc HONORS it by spawning an archetype from
# that roster; these pin that the validator VERIFIES it — accepting ANY roster member,
# rejecting an off-roster pick (a stale/desynced npc layer) routed back to npc, and
# degrading silently when there is no roster or the layer is missing.


async def test_run_accepts_npc_archetype_in_the_director_roster(tmp_path):
    # FM1 (no false reject): npc draws ONE archetype from a non-empty roster by a region hash; the layer
    # emitting exactly that region-true pick validates — the factions gate sees a member and the
    # selection gate re-derives the SAME pick. The roster is the REGION-TRUE one (director's own
    # _faction_roster) so the director intent:factions re-derivation gate stays silent too; both roster
    # and pick track their producers in lock-step. (A roster member that is NOT the region-true pick is
    # now the selection gate's concern, pinned in test_npc_archetype_consistency_rejects_a_legal_but_off_selection_member.)
    roster = director._faction_roster(REGION)
    archetype = npc._select_archetype(REGION, roster, frozenset())
    layers = _set_with(tmp_path, {
        "director": _director_body(factions=",".join(roster)),
        "npc": _npc_body(archetype),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_rejects_an_off_roster_npc_archetype_and_routes_to_npc(tmp_path):
    # FM2 (false accept): the director rosters the REGION-TRUE factions but npc's layer spawns
    # "drone" — a stale/desynced/tampered pick not in the roster. The gate must catch it,
    # name npc, and key the message off intent:factions (never the word "director", which
    # is pipeline-earlier) so the text-scan fallback agrees and route-back targets npc. The roster
    # is region-true so ONLY the off-roster pick fails — the director re-derivation gate stays silent.
    roster = director._faction_roster(REGION)
    assert "drone" not in roster  # premise: drone is the off-roster pick, not a region-true member
    layers = _set_with(tmp_path, {
        "director": _director_body(factions=",".join(roster)),
        "npc": _npc_body("drone"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:factions" in i and "drone" in i for i in verdict.issues)
    assert "npc" in verdict.failing_specialists
    # the message names no pipeline-earlier specialist, so even the text-scan fallback
    # routes to npc (a "director's roster" phrasing would misroute to director).
    assert _failing_specialist(verdict.issues) == "npc"
    assert _route_back_target(verdict) == "npc"


async def test_run_npc_intent_gate_silent_when_director_seeds_no_roster(tmp_path):
    # FM3 (back-compat): with no intent:factions the FACTIONS membership gate is dormant — no roster to
    # check against. npc legitimately falls back to NPC_ARCHETYPE, which the selection gate re-derives
    # off the empty roster and accepts, so a fallback layer validates end-to-end. (A NON-fallback
    # archetype under an empty roster IS caught — the selection gate's concern, pinned separately.)
    layers = _set_with(tmp_path, {
        "director": _director_body(),
        "npc": _npc_body(_REGION_ARCHETYPE),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_npc_intent_gate_degrades_on_a_missing_npc_layer(tmp_path):
    # FM3 (missing-layer degrade): the director rosters factions but npc's FILE is gone.
    # The intent gate must skip the unreadable layer (never crash the final gate); the
    # missing-layer gate rejects and routes back to npc. The roster is region-true so the
    # director re-derivation gate stays silent — the missing npc layer is the only fault.
    bodies = {"director": _director_body(factions=",".join(director._faction_roster(REGION)))}
    verdict = await validator.run(
        _brief(), _set_with_missing(tmp_path, bodies, missing="npc"), layers_root=tmp_path
    )
    assert not verdict.accepted
    assert "npc" in verdict.failing_specialists
    assert _route_back_target(verdict) == "npc"


async def test_run_npc_intent_gate_does_not_special_case_the_fallback_archetype(tmp_path):
    # FM4 (fallback confusion): npc's no-roster fallback is "scavenger", but the gates must treat it as
    # an ordinary token — a roster that legitimately CONTAINS scavenger AND selects it is honored, NOT
    # rejected as "the fallback". The roster "scavenger,sentinel" makes scavenger the region-true pick,
    # so both the factions gate (it is a member) and the selection gate (it is the pick) accept it.
    roster = ["scavenger", "sentinel"]
    assert npc._select_archetype(REGION, roster, frozenset()) == "scavenger"  # premise: scavenger selected
    layers = _set_with(tmp_path, {
        "director": _director_body(factions=",".join(roster)),
        "npc": _npc_body("scavenger"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


# ---- director intent:factions re-derivation: the roster ITSELF must match the brief's region ----
#
# The PRODUCER twin of the npc factions/selection CONSUMERS above. Those gates re-derive npc's PICK off
# the director's roster, so a STALE director roster npc faithfully followed is invisible to them. These
# pin that the validator re-derives the roster ITSELF via director._faction_roster off the brief:
# accepting a region-true roster, rejecting a present stale one (routed to director, the pipeline-
# earliest node), and staying silent on an absent roster (the empty-roster fallback the npc gate owns).


async def test_run_accepts_a_region_true_director_factions_roster(tmp_path):
    # FM1 (no false reject): the director's roster IS what the brief's region determines
    # (director._faction_roster), so the re-derivation gate stays silent and the world — with npc's
    # region-true pick from it — validates end-to-end. The gate adds no false reject to a correct region.
    roster = director._faction_roster(REGION)
    layers = _set_with(tmp_path, {
        "director": _director_body(factions=",".join(roster)),
        "npc": _npc_body(npc._select_archetype(REGION, roster, frozenset())),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues
    assert not any("_faction_roster" in i for i in verdict.issues)


async def test_run_rejects_a_stale_director_roster_npc_faithfully_followed(tmp_path):
    # FM2 (the exploit the consumer gates structurally miss): the director ships a STALE roster (authored
    # for another region) and npc faithfully mirrors it — npc's pick is a member of, AND the region-true
    # selection FROM, that stale roster, so the factions-membership and selection gates BOTH stay silent.
    # Only re-deriving the roster itself off the brief catches the wrong region's factions. Rejected,
    # named director ALONE (npc is blameless — it followed the roster), routed to director.
    stale = ["raider", "sentinel"]
    assert stale != director._faction_roster(REGION)  # premise: a roster from another region
    pick = npc._select_archetype(REGION, stale, frozenset())  # exactly what npc.run emits reading it
    assert pick in stale  # premise: npc's pick is a faithful member of the stale roster
    layers = _set_with(tmp_path, {
        "director": _director_body(factions=",".join(stale)),
        "npc": _npc_body(pick),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:factions" in i and "director._faction_roster" in i for i in verdict.issues)
    # npc followed the roster faithfully, so ONLY the director is blamed — the proof that the consumer
    # gates stayed silent and the producer gate alone caught the stale roster.
    assert verdict.failing_specialists == ["director"]
    assert _route_back_target(verdict) == "director"


async def test_run_director_factions_gate_silent_without_a_roster(tmp_path):
    # FM3 (back-compat): a director that seeds NO intent:factions is the empty-roster fallback the npc gate
    # already owns — the re-derivation gate stays silent rather than demanding the roster's presence (which
    # would false-reject every placeholder / pre-factions director). npc's region-true fallback validates.
    layers = _set_with(tmp_path, {
        "director": _director_body(),  # no intent:factions
        "npc": _npc_body(_REGION_ARCHETYPE),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues
    assert not any("_faction_roster" in i for i in verdict.issues)


def test_director_factions_consistency_flags_a_stale_roster_and_skips_an_absent_one(tmp_path):
    # Unit-level (mirrors the selection gate's direct-call pins): the gate re-derives director._faction_roster
    # off the brief. A region-true roster AND an absent roster both yield [] (no false reject; the empty-roster
    # fallback is the npc gate's concern); a PRESENT stale roster yields exactly one director-named issue.
    def issues(factions):
        bodies = {"director": _director_body(factions=factions)} if factions is not None else {}
        layers = _set_with(tmp_path, bodies)
        return validator._director_factions_consistency(_brief(), layers, tmp_path)

    assert issues(",".join(director._faction_roster(REGION))) == []  # region-true: silent
    assert issues(None) == []  # absent roster: silent (the fallback)
    stale = issues("raider,sentinel")  # present + wrong: one issue naming director + the helper
    assert len(stale) == 1
    assert "director" in stale[0] and "_faction_roster" in stale[0]


# ---- npc must_not loop: a barred archetype (civilians) must never reach the layer ----
#
# The director DECLARES intent:must_not and npc HONORS it by excluding the barred archetype
# from its pick; these pin that the validator VERIFIES it — rejecting an npc layer that still
# spawns a forbidden archetype (routed to npc), catching the cases the factions membership
# check structurally can't (a roster that NAMES civilian, an empty roster), and staying
# silent when the token is absent, the archetype is allowed, or the layer is missing.


async def test_run_rejects_a_forbidden_civilian_npc_archetype_and_routes_to_npc(tmp_path):
    # FM2 (false accept): the director forbids civilians via intent:must_not but npc's layer
    # spawns one — a tampered/desynced pick. The gate must catch it, name npc, and key the
    # message off intent:must_not (never a pipeline-earlier specialist) so route-back targets
    # npc. The token is "civilians" alone so only the npc branch fires (dense_vegetation would
    # also trip biome's cap check and muddy the routing — biome is pipeline-earlier).
    layers = _set_with(tmp_path, {
        "director": _director_body(must_not="civilians"),
        "npc": _npc_body("civilian"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:must_not" in i and "civilian" in i for i in verdict.issues)
    assert "npc" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "npc"
    assert _route_back_target(verdict) == "npc"


async def test_run_rejects_a_civilian_even_when_the_roster_names_it(tmp_path):
    # Non-redundancy with the factions membership branch (case B): a desynced roster that WRONGLY rosters
    # civilian makes the membership check pass, yet the must_not branch must STILL reject — the ban
    # outranks roster membership. A civilian-naming roster can never be region-true (_faction_roster
    # excludes civilians), so the director intent:factions re-derivation gate ALSO fires: both npc (the
    # must_not violation) and director (the illegal roster) are blamed, and route-back targets the
    # pipeline-earliest — director, the root cause. The must_not branch's non-redundancy property is
    # still proven by the presence of its intent:must_not civilian issue.
    layers = _set_with(tmp_path, {
        "director": _director_body(factions="civilian,raider", must_not="civilians"),
        "npc": _npc_body("civilian"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:must_not" in i and "civilian" in i for i in verdict.issues)  # the ban still fires
    assert {"director", "npc"} <= set(verdict.failing_specialists)
    assert _route_back_target(verdict) == "director"  # the illegal roster is the pipeline-earliest cause


async def test_run_rejects_a_civilian_under_an_empty_roster(tmp_path):
    # Non-redundancy with the factions branch (case C): with no intent:factions the membership
    # check is silent, but a must_not that forbids civilians must still bar a tampered civilian
    # layer — the ban does not need a roster to bite.
    layers = _set_with(tmp_path, {
        "director": _director_body(must_not="civilians"),
        "npc": _npc_body("civilian"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:must_not" in i and "civilian" in i for i in verdict.issues)
    assert _route_back_target(verdict) == "npc"


async def test_run_must_not_npc_gate_accepts_a_non_forbidden_archetype(tmp_path):
    # FM1 (no false reject): the director forbids civilians but npc spawns an ALLOWED archetype
    # — the gate must stay silent. The ban bars only the named archetype, nothing else. The roster is
    # region-true and npc spawns the region-true SELECTED, non-forbidden pick, so the director
    # re-derivation and the selection gate stay silent too — isolating the must_not branch.
    roster = director._faction_roster(REGION)
    archetype = npc._select_archetype(REGION, roster, npc._forbidden_archetypes(["civilians"]))
    layers = _set_with(tmp_path, {
        "director": _director_body(factions=",".join(roster), must_not="civilians"),
        "npc": _npc_body(archetype),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_must_not_npc_gate_silent_without_the_civilians_token(tmp_path):
    # FM4 (token scope): a must_not that does NOT name civilians imposes no npc ban, so the must_not->npc
    # branch stays SILENT on an npc civilian. Isolating that branch needs civilian to be the roster's
    # region-true SELECTION pick (else the selection gate fires), which needs civilian in the roster —
    # but _faction_roster can never emit civilian, so the director intent:factions re-derivation gate now
    # independently rejects that desynced roster. The world is therefore rejected for the DIRECTOR roster,
    # NOT a must_not civilian complaint: the must_not branch's silence is proven by the ABSENCE of any
    # intent:must_not civilian issue and by director being the SOLE blame (interior_volumes maps to no
    # archetype — pinned in test_npc's _forbidden_archetypes unit test).
    roster = ["civilian", "raider"]
    assert npc._select_archetype(REGION, roster, frozenset()) == "civilian"  # premise: civilian selected
    layers = _set_with(tmp_path, {
        "director": _director_body(factions=",".join(roster), must_not="interior_volumes"),
        "npc": _npc_body("civilian"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted  # the desynced civilian-naming roster is caught by the director gate
    assert not any("intent:must_not" in i and "civilian" in i for i in verdict.issues)  # must_not silent
    assert verdict.failing_specialists == ["director"]  # only the roster; npc's must_not branch did not fire


async def test_run_must_not_npc_gate_degrades_on_a_missing_npc_layer(tmp_path):
    # FM3 (missing-layer degrade): the director forbids civilians but npc's FILE is gone. The
    # must_not branch must skip the unreadable layer (never crash the final gate); the
    # missing-layer gate rejects and routes back to npc.
    bodies = {"director": _director_body(must_not="civilians")}
    verdict = await validator.run(
        _brief(), _set_with_missing(tmp_path, bodies, missing="npc"), layers_root=tmp_path
    )
    assert not verdict.accepted
    assert "npc" in verdict.failing_specialists
    assert _route_back_target(verdict) == "npc"


# ---- must_not -> interiors: the frontier "no enclosed interiors" rule (interior_volumes) ----
#
# The director DECLARES intent:must_not = interior_volumes (the "no enclosed interiors"
# frontier rule the validator docstring names) but — unlike every other intent — NO
# specialist consumes it, so there is nothing to HONOR and, until this gate, nothing
# VERIFIED it. It is therefore a pure VERIFY gate: reject any well-formed layer that
# declares an enclosed interior (the enclosedInterior=true marker) when the director
# forbade interior_volumes, naming the authoring specialist. No specialist emits the
# marker today, so it is additive (a correct world is byte-identical accepted) and silent
# without the intent — defense-in-depth against a tampered/desynced/future layer.


def _interior_body(default_prim: str = "Structures") -> str:
    """A well-formed layer declaring an enclosed-interior volume via the
    `enclosedInterior=true` marker. No specialist emits this today, so it models the
    tampered/desynced/future layer the interiors gate must catch when the director forbids
    interior_volumes. Well-formed (magic + defaultPrim) so it clears the well-formedness
    gate and reaches the intent gate; it carries no triangle-metric fields, so the
    per-specialist triangle self-consistency gates degrade-skip it (no false reject)."""
    return (
        f'#usda 1.0\n(\n    defaultPrim = "{default_prim}"\n)\n\n'
        f'def Xform "{default_prim}"\n{{\n'
        '    def Cube "Bunker"\n    {\n        custom bool enclosedInterior = true\n    }\n}\n'
    )


async def test_run_interiors_gate_accepts_a_frontier_world_without_the_marker(tmp_path):
    # FM1 (additive / no self-trigger): the director declares interior_volumes (its own
    # layer literally carries the "interior_volumes" string) but no layer declares the
    # enclosedInterior marker — the gate must stay silent and NOT self-trigger off the
    # director's declaration. The world validates exactly as before the gate existed. Only
    # interior_volumes is declared, so no sibling must_not gate (dense_vegetation/civilians)
    # muddies the result.
    layers = _set_with(tmp_path, {"director": _director_body(must_not="interior_volumes")})
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues
    assert "director" not in verdict.failing_specialists


async def test_run_rejects_an_enclosed_interior_in_a_frontier_region_and_routes_to_the_author(tmp_path):
    # FM2 (false accept): the director forbids interior_volumes but the prop layer declares
    # an enclosed interior — a tampered/desynced/future layer. The gate must reject it, name
    # prop (the author, the route-back target), and key the message off intent:must_not, NEVER
    # the pipeline-earlier "director" (which would misroute the text-scan fallback).
    layers = _set_with(tmp_path, {
        "director": _director_body(must_not="interior_volumes"),
        "prop": _interior_body("Props"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:must_not" in i and "enclosedInterior" in i and "prop" in i for i in verdict.issues)
    assert "prop" in verdict.failing_specialists
    assert "director" not in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "prop"
    assert _route_back_target(verdict) == "prop"


async def test_run_interiors_gate_routes_to_the_earliest_of_several_offenders(tmp_path):
    # FM2 (earliest route-back): two layers smuggle an interior — terrain (pipeline-earlier)
    # and prop. Both are named, and the supervisor repairs the earliest (terrain) first, so an
    # upstream cause is fixed before its downstream replay.
    layers = _set_with(tmp_path, {
        "director": _director_body(must_not="interior_volumes"),
        "terrain": _interior_body("TerrainInterior"),
        "prop": _interior_body("PropInterior"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert {"terrain", "prop"} <= set(verdict.failing_specialists)
    assert _route_back_target(verdict) == "terrain"


async def test_run_interiors_gate_dormant_when_the_director_omits_interior_volumes(tmp_path):
    # FM3 (over-reach): a NON-frontier region — the director's must_not names other tokens but
    # NOT interior_volumes — with a layer that declares an enclosed interior must stay ACCEPTED.
    # Interiors are forbidden only where the frontier rule is declared (a premium hub is legal).
    layers = _set_with(tmp_path, {
        "director": _director_body(must_not="civilians"),
        "prop": _interior_body("Props"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_interiors_gate_dormant_without_any_must_not(tmp_path):
    # FM3 (over-reach): the director seeds NO must_not at all, so the gate cannot fire — even a
    # layer declaring an enclosed interior validates. A bare marker with no declaration is not a
    # violation.
    layers = _set_with(tmp_path, {
        "director": _director_body(),
        "prop": _interior_body("Props"),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_interiors_gate_degrades_on_a_missing_director(tmp_path):
    # FM4 (degrade, no crash / no double-report): the director layer is gone, so _director_intent
    # returns [] and the interiors gate cannot read the declaration — it must stay silent (raise
    # nothing through the final gate) even though prop carries the marker. The missing-specialist
    # gate rejects and attributes director; the interiors gate contributes no issue.
    verdict = await validator.run(
        _brief(),
        _set_with_missing(tmp_path, {"prop": _interior_body("Props")}, missing="director"),
        layers_root=tmp_path,
    )
    assert not verdict.accepted
    assert "director" in verdict.failing_specialists
    assert not any("enclosedInterior" in i for i in verdict.issues)


# ---- lighting loop: the Atmosphere must be driven by the director's recognized beats ----
#
# The director DECLARES intent:beats (a free-form mood line) and lighting HONORS it by
# emitting a `def Volume "Atmosphere"` driven by the beats it MODELS; these pin that the
# validator VERIFIES it — accepting a layer driven by exactly the recognized beats,
# rejecting one that dropped the atmosphere (routed back to lighting), staying silent when
# no modeled beat is present, degrading when the layer is missing, and tracking lighting's
# vocabulary via _recognized_beats rather than a hardcoded keyword set.


# The region-true mood line director.run seeds for the test REGION — it joins the region's per-region
# beat phrases into exactly this free-form line. The run fixtures below carry it (not an arbitrary line)
# so the director intent:beats re-derivation gate stays silent on the lighting-gate fixtures, reused from
# director's OWN _region_beats so a BEAT_VOCAB/BEATS_SIZE/salt change flips it in lock-step.
_REGION_BEATS_LINE = ". ".join(director._region_beats(REGION)) + "."


# The recognition/density VARIETY these run gates once exercised is pinned at unit level (here and in
# test_lighting.py), not through validator.run: the director intent:beats re-derivation gate below now
# demands a region-true beats line on every well-formed director, and a region's beats are a FIXED
# 3-phrase composition that reproduces neither an unmodeled-only line nor an arbitrary 2-beat density.
# So the recognizer/density facts those fixtures relied on move here, and the remaining run fixtures
# adopt _REGION_BEATS_LINE.
def test_lighting_beats_recognition_and_density_variety_are_unit_pinned():
    # An unmodeled-only line: the recognizer models nothing, so the validator's lighting loop stays
    # silent (its `if recognized:` guard is false). Unreachable at run level now — such a line is not
    # region-true, so the director-beats gate rejects it — but still the reason the loop is silent.
    assert lighting._recognized_beats("quiet meadow. gentle breeze.") == []
    assert lighting._fog_density([]) == 0.0
    # A modeled+unmodeled MIX recognizes ONLY the modeled token — the filter the drivenBy check trusts.
    assert lighting._recognized_beats("scorched earth. quiet meadow.") == ["scorched"]
    # Why the density gate compares with abs_tol: _fog_density emits `{d:.2f}`, a 2-decimal string, and
    # a sum of 2-decimal table values is float-imprecise (0.10 + 0.20 is 0.30000000000000004, emitted
    # "0.30"), so an exact compare would false-reject a correct layer.
    d = lighting._fog_density(["abandoned", "ash"])
    assert d != float(f"{d:.2f}")
    assert f"{d:.2f}" == "0.30"


async def test_run_accepts_lighting_atmosphere_driven_by_recognized_beats(tmp_path):
    # FM1 (no false reject): the director names beats lighting models and lighting emits the
    # Atmosphere driven by exactly those recognized tokens — nothing unmet, byte-clean. The
    # region-true line keeps the new director intent:beats gate silent (it re-derives the same line).
    beats = _REGION_BEATS_LINE
    layers = _set_with(tmp_path, {
        "director": _director_body(beats=beats),
        "lighting": _lighting_body(driven_by=lighting._recognized_beats(beats)),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues
    assert verdict.issues == []


async def test_run_rejects_a_dropped_atmosphere_and_routes_to_lighting(tmp_path):
    # FM2 (false accept): the director names recognized beats but lighting's layer carries no
    # Atmosphere (a stale layer authored before beats drove fog). The gate must catch it,
    # name lighting, and key the message off intent:beats (never "director", pipeline-earlier)
    # so the text-scan fallback agrees with the structured attribution and routes to lighting.
    # The director line is region-true, so only the dropped-atmosphere (lighting) violation fires.
    layers = _set_with(tmp_path, {
        "director": _director_body(beats=_REGION_BEATS_LINE),
        "lighting": _lighting_body(driven_by=None),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:beats" in i and "lighting" in i for i in verdict.issues)
    assert "lighting" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "lighting"
    assert _route_back_target(verdict) == "lighting"


async def test_run_lighting_intent_gate_degrades_on_a_missing_lighting_layer(tmp_path):
    # FM3 (missing-layer degrade): the director names recognized beats but lighting's FILE is
    # gone. The intent gate must skip the unreadable layer (never crash the final gate); the
    # missing-layer/well-formedness gate rejects and routes back to lighting.
    bodies = {"director": _director_body(beats=_REGION_BEATS_LINE)}
    verdict = await validator.run(
        _brief(), _set_with_missing(tmp_path, bodies, missing="lighting"), layers_root=tmp_path
    )
    assert not verdict.accepted
    assert "lighting" in verdict.failing_specialists
    assert _route_back_target(verdict) == "lighting"


async def test_run_lighting_intent_gate_tracks_recognized_beats_vocabulary(tmp_path):
    # FM4 (vocabulary desync): the gate recognizes beats via lighting's OWN _recognized_beats,
    # so its expectation is exactly that helper's output — never a hardcoded keyword set.
    # Recompute the recognized tokens from the helper: a layer driven by them is accepted; one
    # driven by a STRICT SUBSET (a stale layer that lost a beat the director still names) is
    # rejected and routed back to lighting. Both halves recompute via the helper, so a future
    # BEAT_FOG_DENSITY change flips the fixture's expectation in lock-step. The director line is
    # region-true (the new director-beats gate stays silent), so lighting is the only route-back target.
    beats = _REGION_BEATS_LINE
    recognized = lighting._recognized_beats(beats)
    assert len(recognized) >= 2  # need a token to drop for the reject half

    layers = _set_with(tmp_path, {
        "director": _director_body(beats=beats),
        "lighting": _lighting_body(driven_by=recognized),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues

    layers = _set_with(tmp_path, {
        "director": _director_body(beats=beats),
        "lighting": _lighting_body(driven_by=recognized[:-1]),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:beats" in i and "lighting" in i for i in verdict.issues)
    assert _route_back_target(verdict) == "lighting"


# ---- lighting fog-DENSITY self-consistency: the Atmosphere's inputs:density must match the fog
# its recognized beats sum to via lighting._fog_density. Closes lighting-beats FM2 — the drivenBy
# SET check confirms the right beats but not the MAGNITUDE they accumulate to, so a stale/tampered
# density desynced from that sum (while drivenBy still looks right) shipped accepted. Density is
# verified ONLY on the drivenBy-correct branch (a wrong drivenBy is the single violation).


async def test_run_rejects_a_stale_lighting_density_and_routes_to_lighting(tmp_path):
    # FM2 (false accept): the right drivenBy but a density desynced from what the beats sum to (a
    # stale/tampered fog magnitude). The gate must catch it, name lighting, and key the message off
    # intent:beats (never a pipeline-earlier specialist) so the text-scan fallback agrees with the
    # structured attribution and routes back to lighting.
    beats = _REGION_BEATS_LINE  # region-true, so only the density (lighting) violation fires
    recognized = lighting._recognized_beats(beats)
    wrong = lighting._fog_density(recognized) + 0.15  # well past abs_tol from what the beats sum to
    layers = _set_with(tmp_path, {
        "director": _director_body(beats=beats),
        "lighting": _lighting_body(driven_by=recognized, density=wrong),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:beats" in i and "lighting" in i and "density" in i for i in verdict.issues)
    assert "lighting" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "lighting"
    assert _route_back_target(verdict) == "lighting"


async def test_run_lighting_density_not_reported_when_drivenby_already_wrong(tmp_path):
    # FM3 (no double-report): a desynced drivenBy is the ONE violation — density is verified only on
    # the drivenBy-correct branch, so a layer with BOTH a stale drivenBy and a wrong density yields a
    # single lighting issue (the drivenBy one), never also a density complaint that double-routes the
    # same root cause.
    beats = _REGION_BEATS_LINE  # region-true, so the director-beats gate adds no competing issue
    recognized = lighting._recognized_beats(beats)
    layers = _set_with(tmp_path, {
        "director": _director_body(beats=beats),
        "lighting": _lighting_body(driven_by=recognized[:-1], density=0.99),  # drivenBy AND density wrong
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    lighting_issues = [i for i in verdict.issues if "intent:beats" in i and "lighting" in i]
    assert len(lighting_issues) == 1
    assert "drivenBy" in lighting_issues[0] and "density" not in lighting_issues[0]
    assert _route_back_target(verdict) == "lighting"


async def test_run_lighting_density_tracks_the_fog_table_in_lock_step(tmp_path):
    # FM4 (vocabulary desync): the expected density is re-derived via lighting's OWN _fog_density,
    # never a re-summed BEAT_FOG_DENSITY copy — a layer emitting exactly _fog_density(recognized) is
    # accepted while one off by a hundredth (> abs_tol) is rejected, both recomputed via the helper
    # so a future fog-table change flips the fixture's expectation in lock-step.
    beats = _REGION_BEATS_LINE  # region-true, so lighting is the only route-back target on the reject half
    recognized = lighting._recognized_beats(beats)
    expected = lighting._fog_density(recognized)
    assert len(recognized) >= 2  # premise: a non-trivial accumulated sum

    layers = _set_with(tmp_path, {
        "director": _director_body(beats=beats),
        "lighting": _lighting_body(driven_by=recognized, density=expected),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues

    layers = _set_with(tmp_path, {
        "director": _director_body(beats=beats),
        "lighting": _lighting_body(driven_by=recognized, density=expected + 0.01),  # > abs_tol 5e-3
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:beats" in i and "lighting" in i and "density" in i for i in verdict.issues)
    assert _route_back_target(verdict) == "lighting"


async def test_run_rejects_a_fabricated_atmosphere_when_no_beat_is_recognized(tmp_path):
    # The OVER-emission complement of the drivenBy equality: the director seeds NO intent:beats, so the
    # validator's recognized set is empty and lighting.run emits the fog-less palette (no Atmosphere). A
    # stale/tampered lighting layer that FABRICATES a `def Volume "Atmosphere"` anyway — fog for a mood no
    # beat seeded — is the empty-set mirror the non-empty drivenBy check (recorded != set(recognized))
    # structurally cannot see (that branch is skipped when recognized is empty). This is the exact
    # complement of test_run_director_beats_gate_silent_without_a_line (same fixture, but lighting now
    # carries an Atmosphere). Rejected, named lighting ALONE — the director-beats producer gate stays
    # silent on an absent line, so no pipeline-earlier specialist co-fires — keyed off intent:beats.
    layers = _set_with(tmp_path, {
        "director": _director_body(),  # no intent:beats -> the validator's recognized set is empty
        "lighting": _lighting_body(driven_by=["smoke"]),  # a fabricated Atmosphere no beat justifies
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:beats" in i and "lighting" in i and "no modeled beat" in i for i in verdict.issues)
    assert verdict.failing_specialists == ["lighting"]  # no director-beats co-fire; lighting is the sole target
    assert _route_back_target(verdict) == "lighting"


# ---- director intent:beats re-derivation: the PRODUCER twin of the lighting-beats CONSUMER gates ----
#
# The lighting gates above re-check lighting's fog against the director's OWN mood line, so a STALE line
# lighting faithfully followed is invisible to them. These pin that the validator re-derives the line
# ITSELF via director._region_beats off the brief: accepting a region-true line, rejecting a present stale
# one (routed to director, the pipeline-earliest node), and staying silent on an absent line (the fog-less
# fallback lighting owns). The beats twin of the director intent:factions re-derivation gate.


async def test_run_accepts_a_region_true_director_beats_line(tmp_path):
    # FM1 (no false reject): the director's mood line IS what the brief's region determines
    # (director._region_beats joined), so the re-derivation gate stays silent and the world — with
    # lighting's Atmosphere driven from it — validates end-to-end. The gate adds no false reject.
    layers = _set_with(tmp_path, {
        "director": _director_body(beats=_REGION_BEATS_LINE),
        "lighting": _lighting_body(driven_by=lighting._recognized_beats(_REGION_BEATS_LINE)),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues
    assert not any("_region_beats" in i for i in verdict.issues)


async def test_run_rejects_a_stale_director_beats_line_lighting_faithfully_followed(tmp_path):
    # FM2 (the exploit the lighting gates structurally miss): the director ships a STALE mood line (authored
    # for another region) and lighting faithfully drives its Atmosphere from it — drivenBy is exactly the
    # stale line's recognized beats AND density is what they sum to — so the lighting drivenBy and density
    # gates BOTH stay silent. Only re-deriving the line itself off the brief catches the wrong region's
    # mood. Rejected, named director ALONE (lighting is blameless — it followed the line), routed to director.
    stale = ". ".join(director._region_beats("r+0000_+0000_l0")) + "."
    assert stale != _REGION_BEATS_LINE  # premise: a mood line from another region
    recognized = lighting._recognized_beats(stale)  # exactly what lighting.run drives from reading it
    assert recognized  # premise: lighting faithfully models the stale line (a non-empty Atmosphere)
    layers = _set_with(tmp_path, {
        "director": _director_body(beats=stale),
        "lighting": _lighting_body(driven_by=recognized),  # density defaults to _fog_density(recognized)
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:beats" in i and "director._region_beats" in i for i in verdict.issues)
    # lighting followed the line faithfully, so ONLY the director is blamed — the proof that the lighting
    # gates stayed silent and the producer gate alone caught the stale line.
    assert verdict.failing_specialists == ["director"]
    assert _route_back_target(verdict) == "director"


async def test_run_director_beats_gate_silent_without_a_line(tmp_path):
    # FM3 (back-compat): a director that seeds NO intent:beats is the fog-less fallback lighting already
    # owns — the re-derivation gate stays silent rather than demanding the line's presence (which would
    # false-reject every placeholder / pre-beats director). Lighting's no-Atmosphere palette validates.
    layers = _set_with(tmp_path, {
        "director": _director_body(),  # no intent:beats
        "lighting": _lighting_body(driven_by=None),
    })
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues
    assert not any("_region_beats" in i for i in verdict.issues)


def test_director_beats_consistency_flags_a_stale_line_and_skips_an_absent_one(tmp_path):
    # Unit-level (mirrors the factions gate's direct-call pin): the gate re-derives director._region_beats
    # off the brief. A region-true line AND an absent line both yield [] (no false reject; the fog-less
    # fallback is lighting's concern); a PRESENT stale line yields exactly one director-named issue.
    def issues(beats):
        bodies = {"director": _director_body(beats=beats)} if beats is not None else {}
        layers = _set_with(tmp_path, bodies)
        return validator._director_beats_consistency(_brief(), layers, tmp_path)

    assert issues(_REGION_BEATS_LINE) == []  # region-true: silent
    assert issues(None) == []  # absent line: silent (the fallback)
    other = ". ".join(director._region_beats("r+0000_+0000_l0")) + "."
    assert other != _REGION_BEATS_LINE  # premise: a genuinely different region's line
    stale = issues(other)  # present + wrong: one issue naming director + the helper
    assert len(stale) == 1
    assert "director" in stale[0] and "_region_beats" in stale[0]


# ---- terrain triangle self-consistency: the metric must match the declared heightfield ----
#
# Defense-in-depth across the terrain trust boundary, the geometry-emitter twin of the budget
# self-consistency gate: _budget_self_consistency re-derives the optimizer's verdict but SUMS
# terrain's `triangles` metric trusting it, so a stale/tampered terrain metric corrupts the
# budget sum. These pin that the validator re-derives terrain._heightfield_triangles from the
# body's gridResolution and rejects a metric that disagrees (routed back to terrain), skips a
# body with no positive-integer gridResolution, and tracks terrain's own formula in lock-step.

# The region-true gridResolution the brief determines for the test REGION — terrain.run emits
# exactly this. The run-level fixtures carry it (not an arbitrary grid) so the gridResolution↔brief
# gate stays silent on them; the brief-re-derivation tests offset from it to model a stale grid.
_REGION_GRID = terrain._grid_resolution(REGION)


def _terrain_body(grid) -> str:
    """A terrain layer declaring a `grid`x`grid` heightfield — mirroring terrain.run, the
    gridResolution the triangle self-consistency gate re-derives the metric from. `grid` is
    formatted verbatim so a test can model a non-integer / non-positive value the gate skips."""
    return (
        '#usda 1.0\n(\n    defaultPrim = "Terrain"\n)\n\n'
        'def Mesh "Terrain" (\n    kind = "component"\n)\n{\n'
        '    custom string biome = "scorched"\n'
        f"    custom int gridResolution = {grid}\n"
        '    custom string heightfieldSource = "placeholder"\n}\n'
    )


def _terrain_spec(triangles: float) -> LayerSpec:
    return LayerSpec(
        specialist="terrain", region_id=REGION, path=f"terrain/{REGION}.usda",
        summary="t", metrics={"triangles": triangles},
    )


def test_terrain_triangle_consistency_accepts_a_metric_matching_its_grid():
    # FM1 (no false reject): metric == _heightfield_triangles(grid) — nothing to report. The
    # grid is non-trivial so a hardcoded count couldn't accidentally satisfy it.
    grid = 600
    spec = _terrain_spec(float(terrain._heightfield_triangles(grid)))
    assert validator._terrain_triangle_consistency(spec, _terrain_body(grid)) == []


def test_terrain_triangle_consistency_rejects_a_metric_that_disagrees_with_its_grid():
    # FM2 (false accept): a terrain metric that drifts from its declared grid — exactly the
    # hardcoded-262144-regardless-of-the-declared-512 bug terrain's own helper docstring warns
    # of. One issue, naming terrain and no pipeline-later specialist, so the text-scan fallback
    # routes back to terrain.
    grid = 600
    assert terrain._heightfield_triangles(grid) != 262144  # premise: the grid implies another count
    issues = validator._terrain_triangle_consistency(_terrain_spec(262144.0), _terrain_body(grid))
    assert len(issues) == 1
    assert "terrain" in issues[0] and "triangles metric" in issues[0]
    assert _failing_specialist(issues) == "terrain"


def test_terrain_triangle_consistency_skips_an_unverifiable_body():
    # FM3 (degrade, no false reject): a body with no gridResolution (the VALID_BODY placeholder
    # the _full_set tests use) is unverifiable — SKIP, never reject. A non-positive or
    # non-integer grid is likewise unverifiable, not a false rejection.
    spec = _terrain_spec(999.0)
    assert validator._terrain_triangle_consistency(spec, VALID_BODY) == []
    assert validator._terrain_triangle_consistency(spec, _terrain_body(0)) == []
    assert validator._terrain_triangle_consistency(spec, _terrain_body("512.5")) == []


def test_terrain_triangle_consistency_tracks_the_heightfield_formula_in_lock_step():
    # FM4 (formula desync): the expectation is recomputed via terrain._heightfield_triangles,
    # never a hardcoded count — a metric correct for one grid is rejected against a different
    # grid, so a future change to the triangulation flips the gate's verdict in lock-step.
    g1, g2 = 400, 800
    spec = _terrain_spec(float(terrain._heightfield_triangles(g1)))
    assert validator._terrain_triangle_consistency(spec, _terrain_body(g1)) == []
    assert validator._terrain_triangle_consistency(spec, _terrain_body(g2)) != []


# gridResolution↔brief re-derivation — the brief-re-derivation twin of the biome uncapped
# scatter-count gate. terrain.run emits _grid_resolution(region), a region-varying deterministic
# value, and the triangle gate above pins triangles↔gridResolution — but the gridResolution itself
# is trusted blindly. A stale/tampered grid with a re-synced triangles metric passes the triangle
# gate AND the budget sum; these pin that the validator re-derives it off the brief, rejects a
# disagreeing grid (routed to terrain), skips an unverifiable body, and tracks the formula in
# lock-step. _terrain_spec's metric is irrelevant here (this gate reads only the body's grid).


def test_terrain_grid_resolution_consistency_accepts_the_region_true_grid():
    # FM1 (no false reject): the grid terrain.run emits for this brief's region — nothing to report.
    spec = _terrain_spec(0.0)
    assert validator._terrain_grid_resolution_consistency(_brief(), spec, _terrain_body(_REGION_GRID)) == []


def test_terrain_grid_resolution_consistency_rejects_a_grid_that_disagrees_with_the_brief():
    # FM2: a stale/tampered grid (authored for another region) — rejected, naming terrain, the
    # message carrying both the emitted grid and the region-true expectation.
    spec = _terrain_spec(0.0)
    tampered = _REGION_GRID + 1
    issues = validator._terrain_grid_resolution_consistency(_brief(), spec, _terrain_body(tampered))
    assert len(issues) == 1
    assert "gridResolution" in issues[0]
    assert str(tampered) in issues[0] and str(_REGION_GRID) in issues[0]
    assert "terrain" in issues[0]


def test_terrain_grid_resolution_consistency_skips_an_unverifiable_body():
    # FM3 (degrade, no false reject): no gridResolution (the VALID_BODY placeholder other gates'
    # fixtures use), or a <1 / non-integer grid, leaves the value unverifiable — SKIP, mirroring
    # _terrain_triangle_consistency's own guard exactly so no other fixture is newly false-rejected.
    spec = _terrain_spec(0.0)
    assert validator._terrain_grid_resolution_consistency(_brief(), spec, VALID_BODY) == []
    assert validator._terrain_grid_resolution_consistency(_brief(), spec, _terrain_body(0)) == []
    assert validator._terrain_grid_resolution_consistency(_brief(), spec, _terrain_body("512.5")) == []


def test_terrain_grid_resolution_consistency_tracks_the_brief_in_lock_step():
    # FM4 (desync): the expectation IS terrain._grid_resolution(region)'s output — the exact grid
    # accepts, off-by-one rejects. A copied BASE_GRID_RESOLUTION or a re-implemented jitter would
    # drift and either false-reject a legit grid or miss a tampered one.
    spec = _terrain_spec(0.0)
    assert validator._terrain_grid_resolution_consistency(_brief(), spec, _terrain_body(_REGION_GRID)) == []
    assert validator._terrain_grid_resolution_consistency(_brief(), spec, _terrain_body(_REGION_GRID + 1)) != []


def _terrain_metric_set(root: Path, *, grid, terrain_tris: float) -> list[LayerSpec]:
    """A full well-formed set with terrain declaring `grid` and metering `terrain_tris`, and the
    optimization body re-synced to the resulting summed geometry so the budget gate stays silent
    — only the terrain triangle self-consistency check is exercised (no double-report)."""
    summed = terrain_tris + 100000.0 + 96000.0 + 144000.0  # biome + prop + npc default metrics
    out: list[LayerSpec] = []
    for s in SPECIALISTS:
        if s == "terrain":
            out.append(_layer(root, "terrain", body=_terrain_body(grid), metrics={"triangles": terrain_tris}))
        elif s == "optimization":
            body = _opt_body(authored=summed, observed=summed, over_budget=False)
            out.append(_layer(root, "optimization", body=body, metrics={"over_budget": 0.0}))
        else:
            out.append(_layer(root, s))
    return out


async def test_run_accepts_a_terrain_metric_consistent_with_its_grid(tmp_path):
    # FM1 at run() level: a correct world (terrain metric == its grid's heightfield, optimizer
    # body re-synced) validates clean — the metric AND gridResolution gates both stay silent.
    grid = _REGION_GRID
    layers = _terrain_metric_set(tmp_path, grid=grid, terrain_tris=float(terrain._heightfield_triangles(grid)))
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_rejects_a_stale_terrain_metric_and_routes_to_terrain(tmp_path):
    # FM2 at run() level: terrain's metric is stale/tampered (doesn't match its gridResolution),
    # with the optimizer body re-synced to that metric so the BUDGET gate stays silent — terrain
    # is the ONLY issue (no double-report) and route-back targets it. The grid stays region-true
    # so the gridResolution↔brief gate is silent — only the tampered METRIC trips the triangle gate.
    grid = _REGION_GRID
    terrain_tris = 262144.0
    assert terrain._heightfield_triangles(grid) != terrain_tris
    layers = _terrain_metric_set(tmp_path, grid=grid, terrain_tris=terrain_tris)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("terrain" in i and "triangles metric" in i for i in verdict.issues)
    assert "terrain" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "terrain"
    assert _route_back_target(verdict) == "terrain"


async def test_run_rejects_a_terrain_grid_that_disagrees_with_the_brief(tmp_path):
    # FM2 at run() level (the exploit the gate exists to catch): a gridResolution changed in
    # isolation, with the triangles metric re-synced to it (_terrain_metric_set co-emits
    # _heightfield_triangles(grid)) AND the optimizer body re-synced — so the triangle gate and the
    # budget gate stay silent — ships a heightfield at the wrong resolution for its region. Only the
    # brief re-derivation sees it: rejected, routed back to terrain, the message naming gridResolution
    # and both the tampered and the region-true grid.
    tampered = _REGION_GRID + 1
    layers = _terrain_metric_set(
        tmp_path, grid=tampered, terrain_tris=float(terrain._heightfield_triangles(tampered))
    )
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any(
        "gridResolution" in i and str(tampered) in i and str(_REGION_GRID) in i
        for i in verdict.issues
    )
    assert "terrain" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "terrain"
    assert _route_back_target(verdict) == "terrain"


# ---- biome scatter triangle self-consistency: the metric must match the declared instanceCount ----
#
# The scatter-emitter sibling of the terrain gate above: _budget_self_consistency SUMS biome's
# `triangles` metric trusting it, so a stale/tampered biome metric corrupts the budget sum. These
# pin that the validator re-derives count * biome.TRIS_PER_INSTANCE from the body's instanceCount
# and rejects a metric that disagrees (routed back to biome), skips a body with no/negative/non-
# integer count, VERIFIES a 0 count (the boundary that differs from terrain's >=1 grid), and tracks
# biome's own per-instance cost in lock-step.


def _biome_spec(triangles: float) -> LayerSpec:
    return LayerSpec(
        specialist="biome", region_id=REGION, path=f"biome/{REGION}.usda",
        summary="t", metrics={"triangles": triangles},
    )


def test_biome_triangle_consistency_accepts_a_metric_matching_its_count():
    # FM1 (no false reject): metric == count * TRIS_PER_INSTANCE — nothing to report. The count is
    # non-trivial so a hardcoded figure couldn't accidentally satisfy it.
    count = 500
    spec = _biome_spec(float(count * biome.TRIS_PER_INSTANCE))
    assert validator._biome_triangle_consistency(spec, _biome_body(capped=False, instance_count=count)) == []


def test_biome_triangle_consistency_rejects_a_metric_that_disagrees_with_its_count():
    # FM2 (false accept): a biome metric that drifts from its declared instanceCount. One issue,
    # naming biome and no pipeline-later specialist, so the text-scan fallback routes back to biome.
    count = 500
    bad = float(count * biome.TRIS_PER_INSTANCE) - 1000.0
    issues = validator._biome_triangle_consistency(_biome_spec(bad), _biome_body(capped=False, instance_count=count))
    assert len(issues) == 1
    assert "biome" in issues[0] and "triangles metric" in issues[0]
    assert _failing_specialist(issues) == "biome"


def test_biome_triangle_consistency_skips_an_unverifiable_body():
    # FM3 (degrade, no false reject): a body with no instanceCount (the VALID_BODY placeholder the
    # _full_set tests use), a negative count, or a non-integer count is unverifiable — SKIP.
    spec = _biome_spec(999.0)
    assert validator._biome_triangle_consistency(spec, VALID_BODY) == []
    assert validator._biome_triangle_consistency(spec, _biome_body(capped=False, instance_count=-5)) == []
    noninteger = (
        '#usda 1.0\n(\n    defaultPrim = "Biome"\n)\n\n'
        'def PointInstancer "Scatter"\n{\n    custom int instanceCount = 240.5\n}\n'
    )
    assert validator._biome_triangle_consistency(spec, noninteger) == []


def test_biome_triangle_consistency_verifies_a_zero_count_empty_scatter():
    # The boundary that differs from terrain (grid >= 1): a count of 0 is a VALID empty-scatter
    # region metering 0 triangles, so it is VERIFIED not skipped — a 0-metric body accepts, and a
    # tampered instanceCount = 0 over a nonzero metric is REJECTED, never hidden behind a degrade.
    assert validator._biome_triangle_consistency(_biome_spec(0.0), _biome_body(capped=False, instance_count=0)) == []
    issues = validator._biome_triangle_consistency(_biome_spec(5000.0), _biome_body(capped=False, instance_count=0))
    assert len(issues) == 1
    assert _failing_specialist(issues) == "biome"


def test_biome_triangle_consistency_tracks_the_per_instance_cost_in_lock_step():
    # FM4 (formula desync): the expectation is recomputed via biome.TRIS_PER_INSTANCE, never a
    # hardcoded count — a metric correct for one count is rejected against a different count, so a
    # future change to the per-instance triangle cost flips the gate's verdict in lock-step.
    c1, c2 = 400, 800
    spec = _biome_spec(float(c1 * biome.TRIS_PER_INSTANCE))
    assert validator._biome_triangle_consistency(spec, _biome_body(capped=False, instance_count=c1)) == []
    assert validator._biome_triangle_consistency(spec, _biome_body(capped=False, instance_count=c2)) != []


def _biome_metric_set(root: Path, *, count, biome_tris: float) -> list[LayerSpec]:
    """A full well-formed set with biome declaring `count` instances and metering `biome_tris`, the
    optimization body re-synced to the resulting summed geometry so the budget gate stays silent —
    only the biome triangle self-consistency check is exercised (no double-report)."""
    summed = biome_tris + 262144.0 + 96000.0 + 144000.0  # terrain + prop + npc default metrics
    out: list[LayerSpec] = []
    for s in SPECIALISTS:
        if s == "biome":
            out.append(
                _layer(root, "biome", body=_biome_body(capped=False, instance_count=count), metrics={"triangles": biome_tris})
            )
        elif s == "optimization":
            body = _opt_body(authored=summed, observed=summed, over_budget=False)
            out.append(_layer(root, "optimization", body=body, metrics={"over_budget": 0.0}))
        else:
            out.append(_layer(root, s))
    return out


async def test_run_accepts_a_biome_metric_consistent_with_its_count(tmp_path):
    # FM1 at run() level: a correct world — biome emits the region-true uncapped scatter and a
    # metric consistent with it, optimizer body re-synced — validates clean: neither the triangle
    # self-consistency gate nor the brief re-derivation false-rejects it.
    count = _UNCAPPED_COUNT
    layers = _biome_metric_set(tmp_path, count=count, biome_tris=float(count * biome.TRIS_PER_INSTANCE))
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_rejects_a_stale_biome_metric_and_routes_to_biome(tmp_path):
    # FM2 at run() level: biome's METRIC is stale/tampered (doesn't match its instanceCount) while
    # the count itself stays region-true (so the brief re-derivation is silent) and the optimizer
    # body is re-synced to that metric (so the BUDGET gate stays silent) — the triangle gate makes
    # biome the ONLY issue (no double-report) and route-back targets it.
    count = _UNCAPPED_COUNT
    biome_tris = float(count * biome.TRIS_PER_INSTANCE) + 999.0
    layers = _biome_metric_set(tmp_path, count=count, biome_tris=biome_tris)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("biome" in i and "triangles metric" in i for i in verdict.issues)
    assert "biome" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "biome"
    assert _route_back_target(verdict) == "biome"


# ---- npc character triangle self-consistency: the metric must match spawnCount × _character_tris --
#
# The character-emitter sibling of the terrain/biome triangle gates: _budget_self_consistency SUMS
# npc's `triangles` metric trusting it, so a stale/tampered npc metric corrupts the budget sum. The
# gate re-derives spawnCount × npc._character_tris(archetype) from the body and rejects a mismatch,
# naming npc. Unlike terrain/biome (one numeric field), npc needs BOTH spawnCount AND a budgetable
# archetype — an unknown archetype is reported once (membership checked before _character_tris, so
# its KeyError never escapes through run()).


def _npc_spec(triangles: float) -> LayerSpec:
    return LayerSpec(
        specialist="npc", region_id=REGION, path=f"npc/{REGION}.usda",
        summary="t", metrics={"triangles": triangles},
    )


def test_npc_triangle_consistency_accepts_a_metric_matching_its_spawn_and_archetype():
    # FM1 (no false reject): metric == spawnCount × _character_tris(archetype) — nothing to report.
    count, archetype = 8, "raider"
    spec = _npc_spec(float(count * npc._character_tris(archetype)))
    assert validator._npc_triangle_consistency(spec, _npc_body(archetype, spawn_count=count)) == []


def test_npc_triangle_consistency_rejects_a_metric_that_disagrees():
    # FM2 (missed violation): a metric desynced from spawnCount × _character_tris (a stale count, or
    # a metric not updated when the archetype changed under a re-rostered director) is reported once,
    # naming npc.
    count, archetype = 8, "raider"
    bad = float(count * npc._character_tris(archetype)) - 5000.0
    issues = validator._npc_triangle_consistency(_npc_spec(bad), _npc_body(archetype, spawn_count=count))
    assert len(issues) == 1 and "npc" in issues[0] and "triangles metric" in issues[0]


def test_npc_triangle_consistency_skips_an_unverifiable_body():
    # FM3 (degrade, silent): a body missing spawnCount or archetype, or with a negative/non-integer
    # count, leaves the metric unverifiable — SKIP (field-presence is the schema gate's job), never
    # crash. The factions-suite npc bodies (no spawnCount) ride this branch and stay green.
    spec = _npc_spec(144000.0)
    assert validator._npc_triangle_consistency(spec, _npc_body("raider")) == []  # no spawnCount
    assert validator._npc_triangle_consistency(spec, _npc_body("raider", spawn_count=-3)) == []
    no_archetype = (
        '#usda 1.0\n(\n    defaultPrim = "NPCs"\n)\n\n'
        'def Xform "NPCs"\n{\n    custom int spawnCount = 6\n}\n'
    )
    assert validator._npc_triangle_consistency(spec, no_archetype) == []


def test_npc_triangle_consistency_reports_an_unbudgetable_archetype_once_without_raising():
    # FM3 (unknown archetype): an archetype not in CHARACTER_TRIS can't be budgeted — npc.run would
    # have raised at emission. The gate REPORTS it once (membership checked BEFORE _character_tris, so
    # the helper's KeyError never escapes through run()), never skips or crashes.
    assert "wraith" not in npc.CHARACTER_TRIS  # premise
    issues = validator._npc_triangle_consistency(_npc_spec(50000.0), _npc_body("wraith", spawn_count=6))
    assert len(issues) == 1 and "npc" in issues[0] and "CHARACTER_TRIS" in issues[0]


def test_npc_triangle_consistency_tracks_the_per_archetype_budget_in_lock_step():
    # FM4 (vocabulary desync): the expectation recomputes via npc._character_tris, never a copied
    # table — so a per-archetype budget change flips the fixture in lock-step. A heavier archetype at
    # the same count meters more triangles; the gate accepts each at its own budget and rejects the
    # crossed pair (a light metric against a heavy archetype's body).
    count = 6
    light = _npc_spec(float(count * npc._character_tris("drone")))
    heavy = _npc_spec(float(count * npc._character_tris("sentinel")))
    assert validator._npc_triangle_consistency(light, _npc_body("drone", spawn_count=count)) == []
    assert validator._npc_triangle_consistency(heavy, _npc_body("sentinel", spawn_count=count)) == []
    assert validator._npc_triangle_consistency(light, _npc_body("sentinel", spawn_count=count)) != []


# The region-true spawn count the brief determines for the test REGION — npc.run emits exactly this
# (BASE_SPAWNS scaled by npc's stable per-region hash). The run-level fixtures carry it (not an
# arbitrary count) so the spawnCount↔brief gate stays silent on the triangle-gate fixtures.
_REGION_SPAWNS = npc._spawn_count(REGION)


def _npc_metric_set(root: Path, *, spawn_count, archetype, npc_tris: float) -> list[LayerSpec]:
    """A full well-formed set with npc declaring `spawn_count` × `archetype` and metering `npc_tris`,
    the optimization body re-synced to the resulting summed geometry so the budget gate stays silent
    — only the npc triangle self-consistency check is exercised (no double-report)."""
    summed = 262144.0 + 100000.0 + 96000.0 + npc_tris  # terrain + biome + prop + npc metrics
    out: list[LayerSpec] = []
    for s in SPECIALISTS:
        if s == "npc":
            out.append(
                _layer(root, "npc", body=_npc_body(archetype, spawn_count=spawn_count), metrics={"triangles": npc_tris})
            )
        elif s == "optimization":
            body = _opt_body(authored=summed, observed=summed, over_budget=False)
            out.append(_layer(root, "optimization", body=body, metrics={"over_budget": 0.0}))
        else:
            out.append(_layer(root, s))
    return out


async def test_run_accepts_an_npc_metric_consistent_with_its_spawn_and_archetype(tmp_path):
    # FM1 at run() level: a correct world (npc metric == spawnCount × _character_tris, optimizer body
    # re-synced) validates clean — the new gate adds no false rejection.
    count, archetype = _REGION_SPAWNS, _REGION_ARCHETYPE
    npc_tris = float(count * npc._character_tris(archetype))
    layers = _npc_metric_set(tmp_path, spawn_count=count, archetype=archetype, npc_tris=npc_tris)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_rejects_a_stale_npc_metric_and_routes_to_npc(tmp_path):
    # FM2 at run() level: npc's metric is stale/tampered (doesn't match its spawnCount × archetype),
    # the optimizer body re-synced to that metric so the BUDGET gate stays silent — npc is the ONLY
    # issue (no double-report) and route-back targets it.
    count, archetype = _REGION_SPAWNS, _REGION_ARCHETYPE
    npc_tris = float(count * npc._character_tris(archetype)) + 7000.0
    layers = _npc_metric_set(tmp_path, spawn_count=count, archetype=archetype, npc_tris=npc_tris)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("npc" in i and "triangles metric" in i for i in verdict.issues)
    assert "npc" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "npc"
    assert _route_back_target(verdict) == "npc"


# ---- npc spawnCount vs the brief: the re-derived crowd size must match npc._spawn_count(region) ----
#
# The brief-re-derivation twin of the terrain gridResolution / biome uncapped scatter-count gates.
# npc.run emits _spawn_count(region), a region-varying deterministic count, and the triangle gate
# above pins triangles↔(spawnCount × _character_tris) — but the spawnCount ITSELF is trusted blindly.
# A stale/tampered count with a re-synced triangles metric passes the triangle gate AND the budget
# sum; these pin that the validator re-derives it off the brief, rejects a disagreeing count (routed
# to npc), skips an unverifiable body, and — unlike terrain's >=1 grid — treats a PRESENT 0 that
# disagrees as a real violation (npc's floor is >=0, like biome's instanceCount). _npc_spec's metric
# is irrelevant here (this gate reads only the body's spawnCount).


def test_npc_spawn_count_consistency_accepts_the_region_true_count():
    # FM1 (no false reject): the count npc.run emits for this brief's region — nothing to report.
    spec = _npc_spec(0.0)
    body = _npc_body("raider", spawn_count=_REGION_SPAWNS)
    assert validator._npc_spawn_count_consistency(_brief(), spec, body) == []


def test_npc_spawn_count_consistency_rejects_a_count_that_disagrees_with_the_brief():
    # FM2: a stale/tampered count (authored for another region) — rejected, naming npc, the message
    # carrying both the emitted count and the region-true expectation.
    spec = _npc_spec(0.0)
    tampered = _REGION_SPAWNS + 1
    issues = validator._npc_spawn_count_consistency(_brief(), spec, _npc_body("raider", spawn_count=tampered))
    assert len(issues) == 1
    assert "spawnCount" in issues[0]
    assert str(tampered) in issues[0] and str(_REGION_SPAWNS) in issues[0]
    assert "npc" in issues[0]


def test_npc_spawn_count_consistency_skips_an_unverifiable_body():
    # FM3 (degrade, no false reject): no spawnCount (the factions-suite placeholder shape), or a
    # negative / non-integer count, leaves it unverifiable — SKIP, mirroring _npc_triangle_consistency's
    # own count guard. A spawnCount of 0 is NOT skipped here (unlike terrain's grid < 1) — see below.
    spec = _npc_spec(0.0)
    assert validator._npc_spawn_count_consistency(_brief(), spec, _npc_body("raider")) == []  # no spawnCount
    assert validator._npc_spawn_count_consistency(_brief(), spec, _npc_body("raider", spawn_count=-3)) == []
    assert validator._npc_spawn_count_consistency(_brief(), spec, _npc_body("raider", spawn_count="8.5")) == []


def test_npc_spawn_count_consistency_verifies_a_present_zero_that_disagrees():
    # The biome-not-terrain boundary: npc's floor is >=0 (a 0-spawn region is a valid empty crowd, like
    # biome's instanceCount, UNLIKE terrain's gridResolution which skips <1). So a PRESENT 0 that
    # disagrees with the brief (a tampered 0 over this nonzero-spawn region) is a REAL violation, not a
    # skip — the exact case a `count <= 0` skip would wrongly swallow.
    assert _REGION_SPAWNS > 0  # premise: this region spawns a nonzero crowd
    spec = _npc_spec(0.0)
    issues = validator._npc_spawn_count_consistency(_brief(), spec, _npc_body("raider", spawn_count=0))
    assert len(issues) == 1 and "spawnCount" in issues[0] and "npc" in issues[0]


def test_npc_spawn_count_consistency_tracks_the_brief_in_lock_step():
    # FM4 (desync): the expectation IS npc._spawn_count(region)'s output — the exact count accepts,
    # off-by-one rejects. A copied BASE_SPAWNS or a re-implemented jitter would drift and either
    # false-reject a legit count or miss a tampered one.
    spec = _npc_spec(0.0)
    ok = _npc_body("raider", spawn_count=_REGION_SPAWNS)
    off = _npc_body("raider", spawn_count=_REGION_SPAWNS + 1)
    assert validator._npc_spawn_count_consistency(_brief(), spec, ok) == []
    assert validator._npc_spawn_count_consistency(_brief(), spec, off) != []


async def test_run_rejects_an_npc_spawn_count_that_disagrees_with_the_brief(tmp_path):
    # FM4 at run() level (the exploit the gate exists to catch): a spawnCount changed in isolation, with
    # the triangles metric re-synced to it (spawn_count × _character_tris) AND the optimizer body
    # re-synced (_npc_metric_set does both) — so the triangle gate and the budget gate stay silent —
    # ships the wrong crowd size for the region. Only the brief re-derivation sees it: rejected, routed
    # back to npc, the message naming spawnCount and both the tampered and the region-true count.
    tampered = _REGION_SPAWNS + 1
    npc_tris = float(tampered * npc._character_tris(_REGION_ARCHETYPE))
    layers = _npc_metric_set(tmp_path, spawn_count=tampered, archetype=_REGION_ARCHETYPE, npc_tris=npc_tris)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any(
        "spawnCount" in i and str(tampered) in i and str(_REGION_SPAWNS) in i
        for i in verdict.issues
    )
    assert "npc" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "npc"
    assert _route_back_target(verdict) == "npc"


# ---- npc archetype vs the brief + roster: the emitted archetype must match npc._select_archetype -----
#
# The SELECTION twin of the terrain gridResolution / biome scatter-count / npc spawnCount / prop
# placementCount gates and the npc sibling of the prop propAsset gate — npc's OTHER brief-determined
# dimension (not how MANY npcs, but WHICH archetype). npc.run emits archetype = _select_archetype(region,
# roster, forbidden), and _npc_triangle_consistency pins triangles↔(spawnCount × _character_tris(archetype))
# — but it TRUSTS the emitted archetype to PRICE the crowd. An archetype swapped in isolation to another
# ALLOWED roster member with the metric re-synced passes the triangle gate, the spawn-count gate, AND the
# budget sum; these pin that the validator re-derives WHICH archetype off the brief + roster, rejects a
# disagreeing LEGAL pick (routed to npc), and — deferring to the existing intent gates — SKIPS a non-member
# (the intent:factions loop's concern) and a forbidden archetype (the intent:must_not loop's) rather than
# double-reporting. The message names npc, never "director" (pipeline-earlier), so route-back targets npc.


def _archetype_issues(root: Path, *, archetype: str, factions: str | None = None, must_not: str | None = None):
    """Drive _npc_archetype_consistency over a layer set: an npc layer spawning `archetype`, plus a director
    seeding `factions`/`must_not` when either is given (else the default empty-roster director). Returns the
    gate's issues — the layers/layers_root it needs to re-derive the roster are built under `root`."""
    bodies = {"npc": _npc_body(archetype)}
    if factions is not None or must_not is not None:
        bodies["director"] = _director_body(factions=factions, must_not=must_not)
    layers = _set_with(root, bodies)
    npc_layer = next(layer for layer in layers if layer.specialist == "npc")
    return validator._npc_archetype_consistency(
        _brief(), layers, root, npc_layer, (root / npc_layer.path).read_text()
    )


def _npc_roster_metric_set(root: Path, *, roster: list[str], archetype: str, npc_tris: float) -> list[LayerSpec]:
    """A full well-formed set: the director seeds `roster` (intent:factions), npc spawns `archetype` at the
    region-true spawnCount metering `npc_tris`, and the optimizer body re-syncs to the summed geometry so
    the budget gate stays silent — isolating the archetype selection gate (no double-report from the
    triangle / spawn-count / budget gates)."""
    summed = 262144.0 + 100000.0 + 96000.0 + npc_tris  # terrain + biome + prop + npc metrics
    out: list[LayerSpec] = []
    for s in SPECIALISTS:
        if s == "npc":
            out.append(_layer(
                root, "npc", body=_npc_body(archetype, spawn_count=_REGION_SPAWNS), metrics={"triangles": npc_tris}
            ))
        elif s == "director":
            out.append(_layer(root, "director", body=_director_body(factions=",".join(roster))))
        elif s == "optimization":
            body = _opt_body(authored=summed, observed=summed, over_budget=False)
            out.append(_layer(root, "optimization", body=body, metrics={"over_budget": 0.0}))
        else:
            out.append(_layer(root, s))
    return out


def test_npc_archetype_consistency_accepts_the_region_true_pick(tmp_path):
    # FM1 (no false reject): the archetype npc.run selects for this region + roster — nothing to report.
    roster = ["raider", "sentinel"]
    archetype = npc._select_archetype(REGION, roster, frozenset())
    assert _archetype_issues(tmp_path, archetype=archetype, factions=",".join(roster)) == []


def test_npc_archetype_consistency_rejects_a_legal_but_off_selection_member(tmp_path):
    # FM2 (the exploit): a LEGAL roster member that is NOT the region-true pick (a down-swap to a cheaper
    # allowed archetype, or any stale/tampered legal pick) — rejected, naming npc, the message carrying BOTH
    # the emitted and the region-true archetype. The hole the factions/must_not/triangle/budget gates miss.
    roster = ["raider", "sentinel"]
    picked = npc._select_archetype(REGION, roster, frozenset())
    off = next(a for a in roster if a != picked)  # a legal member, not the selection
    issues = _archetype_issues(tmp_path, archetype=off, factions=",".join(roster))
    assert len(issues) == 1
    assert "archetype" in issues[0]
    assert off in issues[0] and picked in issues[0]
    assert "npc" in issues[0]


def test_npc_archetype_consistency_empty_roster_accepts_the_fallback_rejects_others(tmp_path):
    # The empty-roster case: with no roster npc falls back to NPC_ARCHETYPE, so the gate re-derives that
    # fallback and accepts it — but rejects any OTHER non-forbidden archetype (npc would never emit it with
    # no roster). The selection gate OWNS the no-roster case the factions membership gate is dormant on.
    assert _archetype_issues(tmp_path, archetype=npc.NPC_ARCHETYPE) == []
    other = next(a for a in sorted(npc.CHARACTER_TRIS) if a != npc.NPC_ARCHETYPE)  # a different KNOWN archetype
    assert _archetype_issues(tmp_path, archetype=other) != []


def test_npc_archetype_consistency_defers_a_non_member_and_a_forbidden_pick(tmp_path):
    # FM3 (no double-report): a NON-member archetype is owned by _intent_attributions' intent:factions loop
    # and a FORBIDDEN one by its intent:must_not loop — this gate SKIPS both so neither is reported twice.
    # It fires only for the complement: a legal, non-forbidden, off-selection pick.
    assert _archetype_issues(tmp_path, archetype="drone", factions="raider,sentinel") == []  # non-member
    assert _archetype_issues(  # forbidden (even though the roster names it)
        tmp_path, archetype="civilian", factions="civilian,raider", must_not="civilians"
    ) == []


def test_npc_archetype_consistency_skips_an_absent_archetype(tmp_path):
    # FM (degrade): an npc body with no archetype field is skipped (field-presence is the well-formedness
    # gate's concern; npc.run always co-emits it) — even under a non-empty roster.
    no_arch = '#usda 1.0\n(\n    defaultPrim = "NPCs"\n)\n\ndef Xform "NPCs"\n{\n}\n'
    bodies = {"npc": no_arch, "director": _director_body(factions="raider,sentinel")}
    layers = _set_with(tmp_path, bodies)
    npc_layer = next(layer for layer in layers if layer.specialist == "npc")
    assert validator._npc_archetype_consistency(_brief(), layers, tmp_path, npc_layer, no_arch) == []


def test_npc_archetype_consistency_tracks_the_selection_in_lock_step(tmp_path):
    # FM4 (desync): the expectation IS npc._select_archetype(region, roster, forbidden)'s output — the exact
    # region-true archetype accepts, any other roster member rejects. A copied sorted(roster) index or a
    # re-implemented hash/salt would drift and either false-reject the legit pick or miss a tampered one.
    roster = ["raider", "sentinel"]
    picked = npc._select_archetype(REGION, roster, frozenset())
    other = next(a for a in roster if a != picked)
    assert _archetype_issues(tmp_path, archetype=picked, factions=",".join(roster)) == []
    assert _archetype_issues(tmp_path, archetype=other, factions=",".join(roster)) != []


async def test_run_rejects_an_npc_archetype_that_disagrees_with_the_brief(tmp_path):
    # The exploit at run() level: the archetype swapped IN ISOLATION to a different ALLOWED roster member
    # with the triangles metric re-synced to spawnCount × _character_tris(swapped) AND the optimizer body
    # re-synced (_npc_roster_metric_set does both) — so the triangle gate, the spawn-count gate, AND the
    # budget gate stay silent — ships the region's crowd as the wrong archetype at the wrong per-character
    # budget. Only the brief re-derivation sees it: rejected, routed to npc, naming both archetypes.
    roster = director._faction_roster(REGION)  # region-true, so only the off-SELECTION swap fails
    picked = npc._select_archetype(REGION, roster, frozenset())  # the region-true pick
    off = next(a for a in roster if a != picked)  # a legal member, not the selection
    assert npc._character_tris(off) != npc._character_tris(picked)  # premise: a real per-character budget swap
    npc_tris = float(_REGION_SPAWNS * npc._character_tris(off))  # metric re-synced to the swapped archetype
    layers = _npc_roster_metric_set(tmp_path, roster=roster, archetype=off, npc_tris=npc_tris)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("npc._select_archetype" in i and off in i and picked in i for i in verdict.issues)
    assert "npc" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "npc"
    assert _route_back_target(verdict) == "npc"


async def test_run_leaves_a_non_member_npc_archetype_to_the_factions_gate_not_the_selection_gate(tmp_path):
    # No double-report at run() level: a NON-member archetype (not in a non-empty roster) is owned by the
    # intent:factions loop; the selection gate SKIPS it. The run rejects (factions) but carries NO
    # _select_archetype selection message — the two gates never both fire on the same off-roster pick.
    npc_tris = float(_REGION_SPAWNS * npc._character_tris("drone"))
    roster = list(director._faction_roster(REGION))  # region-true, so the director gate is silent
    assert "drone" not in roster  # premise: drone is the non-member the factions gate owns
    layers = _npc_roster_metric_set(tmp_path, roster=roster, archetype="drone", npc_tris=npc_tris)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("intent:factions" in i and "drone" in i for i in verdict.issues)  # the factions gate owns it
    assert not any("_select_archetype" in i for i in verdict.issues)  # the selection gate stayed silent
    assert "npc" in verdict.failing_specialists


# ---- prop placement triangle self-consistency: the metric must match count*fill + Σ required ------
#
# The placement-emitter sibling of the terrain/biome/npc gates, but prop's metric is a fill TERM plus
# a variable REQUIRED-ASSET SUM (count*_asset_tris(propAsset) + Σ _asset_tris(requiredAsset)). The
# gate re-derives both from the body and rejects a mismatch, naming prop. The fill is the region-true
# _REGION_ASSET (_prop_body's pick); an asset absent from ASSET_TRIS is reported once (no _asset_tris
# raise).


def _prop_spec(triangles: float) -> LayerSpec:
    return LayerSpec(
        specialist="prop", region_id=REGION, path=f"prop/{REGION}.usda",
        summary="t", metrics={"triangles": triangles},
    )


def _prop_expected(count: int, required: list[str]) -> float:
    return float(count * prop._asset_tris(_REGION_ASSET) + sum(prop._asset_tris(a) for a in required))


def test_prop_triangle_consistency_accepts_a_metric_matching_its_placements():
    # FM1 (no false reject): metric == count*fill + Σ required — both the scatter fill and the
    # must-have heroes summed, nothing to report.
    count, required = 8, ["comms_tower_01", "convoy_wreck_01"]
    spec = _prop_spec(_prop_expected(count, required))
    assert validator._prop_triangle_consistency(spec, _prop_body(required, placement_count=count)) == []


def test_prop_triangle_consistency_rejects_a_metric_that_disagrees():
    # FM2 (missed violation): a metric desynced from count*fill + Σ required (a stale count or a
    # required set changed under a re-rostered director) is reported once, naming prop.
    count, required = 8, ["comms_tower_01"]
    bad = _prop_expected(count, required) - 3000.0
    issues = validator._prop_triangle_consistency(_prop_spec(bad), _prop_body(required, placement_count=count))
    assert len(issues) == 1 and "prop" in issues[0] and "triangles metric" in issues[0]


def test_prop_triangle_consistency_skips_an_unverifiable_body():
    # FM3 (degrade, silent): a body missing placementCount or propAsset, or with a negative/non-integer
    # count, leaves the metric unverifiable — SKIP. The intent fixtures (no placementCount) ride this.
    spec = _prop_spec(96000.0)
    assert validator._prop_triangle_consistency(spec, _prop_body(["comms_tower_01"])) == []  # no count
    assert validator._prop_triangle_consistency(spec, _prop_body(["comms_tower_01"], placement_count=-2)) == []
    no_propasset = (
        '#usda 1.0\n(\n    defaultPrim = "Props"\n)\n\n'
        'def PointInstancer "Props"\n{\n    custom int placementCount = 5\n}\n'
    )
    assert validator._prop_triangle_consistency(spec, no_propasset) == []


def test_prop_triangle_consistency_reports_an_unbudgetable_asset_once_without_raising():
    # FM3 (unknown asset): a placed asset (here a required hero) not in ASSET_TRIS can't be budgeted —
    # prop.run would have raised at emission. The gate REPORTS it once (membership checked BEFORE
    # _asset_tris, so the helper's KeyError never escapes through run()), never skips or crashes.
    assert "phantom_01" not in prop.ASSET_TRIS  # premise
    issues = validator._prop_triangle_consistency(
        _prop_spec(50000.0), _prop_body(["phantom_01"], placement_count=5)
    )
    assert len(issues) == 1 and "prop" in issues[0] and "ASSET_TRIS" in issues[0]


def test_prop_triangle_consistency_tracks_the_per_asset_budget_in_lock_step():
    # FM4 (vocabulary desync): the expectation recomputes via prop._asset_tris over BOTH the fill and
    # the required sum, never a copied table — so a per-asset budget change flips the fixture in lock-
    # step. A heavier required set at the same count meters more; the gate accepts each and rejects
    # the crossed pair (a light metric against a heavier required body).
    count = 4
    light = _prop_spec(_prop_expected(count, ["ammo_crate_01"]))
    heavy = _prop_spec(_prop_expected(count, ["comms_tower_01"]))
    assert validator._prop_triangle_consistency(light, _prop_body(["ammo_crate_01"], placement_count=count)) == []
    assert validator._prop_triangle_consistency(heavy, _prop_body(["comms_tower_01"], placement_count=count)) == []
    assert validator._prop_triangle_consistency(light, _prop_body(["comms_tower_01"], placement_count=count)) != []


# The region-true placement count the brief determines for the test REGION — prop.run emits exactly
# this (BASE_PLACEMENTS scaled by prop's stable per-region hash). The run-level fixtures carry it (not
# an arbitrary count) so the placementCount↔brief gate stays silent on the triangle-gate fixtures.
_REGION_PLACEMENTS = prop._placement_count(REGION)


def _prop_metric_set(
    root: Path, *, placement_count, required, prop_tris: float, fill_asset: str = _REGION_ASSET
) -> list[LayerSpec]:
    """A full well-formed set with prop declaring `placement_count` × the `fill_asset` fill + `required`
    heroes and metering `prop_tris`, the optimization body re-synced to the resulting summed geometry so
    the budget gate stays silent — only the prop self-consistency checks are exercised. `fill_asset`
    defaults to the region-true pick (selection gate silent); pass a wrong/unknown asset to drive it."""
    summed = 262144.0 + 100000.0 + 144000.0 + prop_tris  # terrain + biome + npc + prop metrics
    out: list[LayerSpec] = []
    for s in SPECIALISTS:
        if s == "prop":
            out.append(
                _layer(root, "prop", body=_prop_body(required, placement_count=placement_count, fill_asset=fill_asset), metrics={"triangles": prop_tris})
            )
        elif s == "optimization":
            body = _opt_body(authored=summed, observed=summed, over_budget=False)
            out.append(_layer(root, "optimization", body=body, metrics={"over_budget": 0.0}))
        else:
            out.append(_layer(root, s))
    return out


async def test_run_accepts_a_prop_metric_consistent_with_its_placements(tmp_path):
    # FM1 at run() level: a correct world (prop metric == count*fill + Σ required, optimizer re-synced)
    # validates clean — the new gate adds no false rejection.
    count, required = _REGION_PLACEMENTS, ["comms_tower_01", "convoy_wreck_01"]
    layers = _prop_metric_set(tmp_path, placement_count=count, required=required, prop_tris=_prop_expected(count, required))
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert verdict.accepted, verdict.issues


async def test_run_rejects_a_stale_prop_metric_and_routes_to_prop(tmp_path):
    # FM2 at run() level: prop's metric is stale/tampered, the optimizer body re-synced to it so the
    # BUDGET gate stays silent — prop is the ONLY issue (no double-report) and route-back targets it.
    count, required = _REGION_PLACEMENTS, ["comms_tower_01"]
    prop_tris = _prop_expected(count, required) + 4000.0
    layers = _prop_metric_set(tmp_path, placement_count=count, required=required, prop_tris=prop_tris)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("prop" in i and "triangles metric" in i for i in verdict.issues)
    assert "prop" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "prop"
    assert _route_back_target(verdict) == "prop"


# ---- prop placementCount vs the brief: the re-derived density must match prop._placement_count -------
#
# The brief-re-derivation twin of the terrain gridResolution / biome uncapped scatter-count / npc
# spawnCount gates — the 4th and last geometry emitter's brief-determined dimension. prop.run emits
# _placement_count(region), a region-varying deterministic count, and the triangle gate above pins
# triangles↔(placementCount × _asset_tris + Σ required) — but the placementCount ITSELF is trusted
# blindly. A stale/tampered count with a re-synced triangles metric passes the triangle gate AND the
# budget sum; these pin that the validator re-derives it off the brief, rejects a disagreeing count
# (routed to prop), skips an unverifiable body, and — unlike terrain's >=1 grid — treats a PRESENT 0
# that disagrees as a real violation (prop's floor is >=0, like biome/npc). _prop_spec's metric is
# irrelevant here (this gate reads only the body's placementCount).


def test_prop_placement_count_consistency_accepts_the_region_true_count():
    # FM1 (no false reject): the count prop.run emits for this brief's region — nothing to report.
    spec = _prop_spec(0.0)
    body = _prop_body([], placement_count=_REGION_PLACEMENTS)
    assert validator._prop_placement_count_consistency(_brief(), spec, body) == []


def test_prop_placement_count_consistency_rejects_a_count_that_disagrees_with_the_brief():
    # FM2: a stale/tampered count (authored for another region) — rejected, naming prop, the message
    # carrying both the emitted count and the region-true expectation.
    spec = _prop_spec(0.0)
    tampered = _REGION_PLACEMENTS + 1
    issues = validator._prop_placement_count_consistency(_brief(), spec, _prop_body([], placement_count=tampered))
    assert len(issues) == 1
    assert "placementCount" in issues[0]
    assert str(tampered) in issues[0] and str(_REGION_PLACEMENTS) in issues[0]
    assert "prop" in issues[0]


def test_prop_placement_count_consistency_skips_an_unverifiable_body():
    # FM3 (degrade, no false reject): no placementCount (the intent-suite placeholder shape), or a
    # negative / non-integer count, leaves it unverifiable — SKIP, mirroring _prop_triangle_consistency's
    # own count guard. A placementCount of 0 is NOT skipped here (unlike terrain's grid < 1) — see below.
    spec = _prop_spec(0.0)
    assert validator._prop_placement_count_consistency(_brief(), spec, _prop_body([])) == []  # no count
    assert validator._prop_placement_count_consistency(_brief(), spec, _prop_body([], placement_count=-4)) == []
    assert validator._prop_placement_count_consistency(_brief(), spec, _prop_body([], placement_count="27.5")) == []


def test_prop_placement_count_consistency_verifies_a_present_zero_that_disagrees():
    # The biome-not-terrain boundary: prop's floor is >=0 (a 0-placement region is a valid empty scatter
    # metering 0 fill triangles, like biome's instanceCount / npc's spawnCount, UNLIKE terrain's grid
    # which skips <1). So a PRESENT 0 that disagrees with the brief (a tampered 0 over this nonzero-
    # placement region) is a REAL violation, not a skip — the exact case a `count <= 0` skip would swallow.
    assert _REGION_PLACEMENTS > 0  # premise: this region places a nonzero scatter
    spec = _prop_spec(0.0)
    issues = validator._prop_placement_count_consistency(_brief(), spec, _prop_body([], placement_count=0))
    assert len(issues) == 1 and "placementCount" in issues[0] and "prop" in issues[0]


def test_prop_placement_count_consistency_tracks_the_brief_in_lock_step():
    # FM4 (desync): the expectation IS prop._placement_count(region)'s output — the exact count accepts,
    # off-by-one rejects. A copied BASE_PLACEMENTS or a re-implemented jitter would drift and either
    # false-reject a legit count or miss a tampered one.
    spec = _prop_spec(0.0)
    ok = _prop_body([], placement_count=_REGION_PLACEMENTS)
    off = _prop_body([], placement_count=_REGION_PLACEMENTS + 1)
    assert validator._prop_placement_count_consistency(_brief(), spec, ok) == []
    assert validator._prop_placement_count_consistency(_brief(), spec, off) != []


async def test_run_rejects_a_prop_placement_count_that_disagrees_with_the_brief(tmp_path):
    # FM4 at run() level (the exploit the gate exists to catch): a placementCount changed in isolation,
    # with the triangles metric re-synced to it (count × fill + Σ required) AND the optimizer body
    # re-synced (_prop_metric_set does both) — so the triangle gate and the budget gate stay silent —
    # ships the wrong prop density for the region. Only the brief re-derivation sees it: rejected, routed
    # back to prop, the message naming placementCount and both the tampered and the region-true count.
    tampered = _REGION_PLACEMENTS + 1
    layers = _prop_metric_set(tmp_path, placement_count=tampered, required=[], prop_tris=_prop_expected(tampered, []))
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any(
        "placementCount" in i and str(tampered) in i and str(_REGION_PLACEMENTS) in i
        for i in verdict.issues
    )
    assert "prop" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "prop"
    assert _route_back_target(verdict) == "prop"


# ---- prop FILL propAsset vs the brief: the emitted fill must match prop._select_asset ----------------
#
# The SELECTION twin of the terrain gridResolution / biome scatter-count / npc spawnCount / prop
# placementCount gates — prop's OTHER brief-determined dimension (not how MANY props, but WHICH one).
# prop.run emits propAsset = _select_asset(region), and _prop_triangle_consistency pins triangles↔(count
# × _asset_tris(propAsset) + Σ required) — but it TRUSTS the emitted asset to PRICE the fill. A fill
# swapped in isolation to a cheaper KNOWN asset with the metric re-synced passes the triangle gate AND
# the budget sum; these pin that the validator re-derives WHICH asset off the brief, rejects a
# disagreeing KNOWN pick (routed to prop), and — deferring to the triangle/metrics gates — SKIPS an
# absent or UNKNOWN (unbudgetable) asset rather than double-reporting it. _prop_spec's metric is
# irrelevant here (this gate reads only the body's propAsset).

# A KNOWN asset that is NOT the region's deterministic pick — a stale/tampered fill's payload.
_WRONG_ASSET = next(a for a in sorted(prop.ASSET_TRIS) if a != _REGION_ASSET)


def test_prop_asset_consistency_accepts_the_region_true_asset():
    # FM1 (no false reject): the fill prop.run selects for this brief's region — nothing to report.
    assert validator._prop_asset_consistency(_brief(), _prop_spec(0.0), _prop_body([])) == []


def test_prop_asset_consistency_rejects_a_different_known_asset():
    # FM2 (the exploit): a fill swapped to a different BUDGETABLE asset (a down-swap to a cheaper prop, or
    # any stale/tampered pick) — rejected, naming prop, the message carrying BOTH the emitted and the
    # region-true asset. This is the wrong-but-VALID pick the triangle/budget checks structurally miss.
    issues = validator._prop_asset_consistency(_brief(), _prop_spec(0.0), _prop_body([], fill_asset=_WRONG_ASSET))
    assert len(issues) == 1
    assert "propAsset" in issues[0]
    assert _WRONG_ASSET in issues[0] and _REGION_ASSET in issues[0]
    assert "prop" in issues[0]


def test_prop_asset_consistency_skips_an_absent_or_unknown_asset():
    # FM3 (degrade / no double-report): an absent propAsset (field-presence is the well-formedness gate's
    # concern) OR an UNKNOWN asset (not in ASSET_TRIS — owned by _prop_triangle_consistency's unbudgetable
    # report / the metrics-schema gate) is SKIPPED here, so a known-but-unbudgetable fill is never double-
    # reported alongside the triangle gate. This gate fires only for a wrong-but-BUDGETABLE pick.
    spec = _prop_spec(0.0)
    no_propasset = (
        '#usda 1.0\n(\n    defaultPrim = "Props"\n)\n\n'
        'def PointInstancer "Props"\n{\n    custom int placementCount = 5\n}\n'
    )
    assert validator._prop_asset_consistency(_brief(), spec, no_propasset) == []  # absent
    assert "phantom_01" not in prop.ASSET_TRIS  # premise: unknown
    assert validator._prop_asset_consistency(_brief(), spec, _prop_body([], fill_asset="phantom_01")) == []


def test_prop_asset_consistency_tracks_the_selection_in_lock_step():
    # FM4 (desync): the expectation IS prop._select_asset(region)'s output — the exact region-true asset
    # accepts, any other KNOWN asset rejects. A copied sorted(ASSET_TRIS) index or a re-implemented
    # hash/salt would drift and either false-reject the legit pick or miss a tampered one.
    spec = _prop_spec(0.0)
    assert validator._prop_asset_consistency(_brief(), spec, _prop_body([])) == []
    assert validator._prop_asset_consistency(_brief(), spec, _prop_body([], fill_asset=_WRONG_ASSET)) != []


async def test_run_rejects_a_prop_fill_asset_that_disagrees_with_the_brief(tmp_path):
    # The exploit at run() level: the fill DOWN-SWAPPED to the cheapest known asset with the triangles
    # metric re-synced to count × _asset_tris(cheaper) AND the optimizer body re-synced (_prop_metric_set
    # does both) — so the triangle gate and the budget gate stay silent — ships the region's fill as the
    # wrong prop at the wrong per-asset budget (the 20x under-count). Only the brief re-derivation sees
    # it: rejected, routed back to prop, the message naming propAsset and BOTH assets.
    cheaper = min(prop.ASSET_TRIS, key=prop._asset_tris)
    assert cheaper != _REGION_ASSET and prop._asset_tris(cheaper) < prop._asset_tris(_REGION_ASSET)  # premise
    count = _REGION_PLACEMENTS
    prop_tris = float(count * prop._asset_tris(cheaper))  # re-synced to the cheaper fill, no required
    layers = _prop_metric_set(tmp_path, placement_count=count, required=[], prop_tris=prop_tris, fill_asset=cheaper)
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("propAsset" in i and cheaper in i and _REGION_ASSET in i for i in verdict.issues)
    assert "prop" in verdict.failing_specialists
    assert _failing_specialist(verdict.issues) == "prop"
    assert _route_back_target(verdict) == "prop"


async def test_run_leaves_an_unknown_prop_fill_to_the_triangle_gate_not_the_selection_gate(tmp_path):
    # No double-report at run() level: an UNKNOWN fill (not in ASSET_TRIS) is owned by
    # _prop_triangle_consistency's unbudgetable report; the selection gate SKIPS it. The run rejects
    # (unbudgetable) but carries NO _select_asset selection message — the two gates never both fire on
    # the same unknown asset.
    assert "phantom_01" not in prop.ASSET_TRIS  # premise
    layers = _prop_metric_set(
        tmp_path, placement_count=_REGION_PLACEMENTS, required=[], prop_tris=50000.0, fill_asset="phantom_01"
    )
    verdict = await validator.run(_brief(), layers, layers_root=tmp_path)
    assert not verdict.accepted
    assert any("ASSET_TRIS" in i for i in verdict.issues)  # the triangle gate owns it
    assert not any("_select_asset" in i for i in verdict.issues)  # the selection gate stayed silent
    assert "prop" in verdict.failing_specialists
