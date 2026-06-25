"""Validator — last gate, strongest layer. Combines:

  - NVIDIA Omni Asset Validator (schema, geometry, materials, performance)
  - Style classifier (embed render against moodboard anchors, cosine sim ≥ threshold)
  - Playability gates (no enclosed interiors in frontier, spawn reachability)

This stub runs the cheapest checks inline. Real impl shells out to
`omni.asset_validator` and a sidecar style-classifier service via MCP.

Returns a ValidatorVerdict; supervisor reads `.accepted` and decides whether
to commit the composed world or route back to a specialist.
"""

from __future__ import annotations

import math
import re
from collections import Counter
from pathlib import Path

from common.types import WorldBrief, LayerSpec, ValidatorVerdict
from prop import prop
from biome import biome
from lighting import lighting
from terrain import terrain
from npc import npc

STYLE_SIM_THRESHOLD = 0.72

# Metrics a specialist's role is contracted to ALWAYS emit, keyed by role. The
# value is the set of required metric keys; every metric is a finite number
# (LayerSpec.metrics is dict[str, float], so the value type is uniform — the
# contract is "present and finite", with int and float both legal). Only roles
# with a real downstream consumer are listed: the optimizer's budget verdict
# (read by run() below) and the triangle count the optimizer sums across prior
# layers. A role with no entry imposes NO requirement, so the specialists that
# legitimately emit no metrics (director, lighting, npc) are
# unaffected — keeping the schema to what a role genuinely always emits.
#
# NOTE: the optimizer sums `triangles` across ALL prior layers; terrain, biome, prop,
# and npc all emit geometry, so all four are contracted emitters — a mis-spelled key
# in any of them can't silently under-count the budget. If a further stub (lighting)
# starts contributing geometry, add `triangles` to its contract here.
ROLE_METRICS: dict[str, set[str]] = {
    "optimization": {"over_budget"},
    "terrain": {"triangles"},
    "biome": {"triangles"},
    "prop": {"triangles"},
    "npc": {"triangles"},
}

# USD text layers must begin with this cookie; pxr refuses to open a file without
# it and the engine silently skips a sublayer it cannot parse.
USDA_MAGIC = "#usda"

# A USD prim declaration: a specifier, an optional type name, a quoted prim name.
# Drives the structural composition-conflict scan when usd-core isn't installed.
# Inter-token whitespace spans newlines: USDA treats `\n` as an ordinary separator,
# so a head wrapped across lines (`def\nMesh\n"Hub"`) is one declaration and must
# still resolve. The type's negative lookahead keeps a specifier keyword from being
# read as a type name, so a malformed bare `over`/`def` with no name never swallows
# the following `def "X"` as its type (FM2 — no fabricated prim).
_PRIM_DECL = re.compile(
    r"(def|over|class)\b[ \t\r\n]+"
    r"(?:(?!(?:def|over|class)\b)([A-Za-z_]\w*)[ \t\r\n]+)?"
    r'"([^"]+)"'
)
# A variantSet / variant block opener. Its `{ }` is a composition scope, not a
# prim scope, so it must not contribute a path segment — but prims nested inside
# (a variant can legally hold prim children) still attribute to the enclosing
# prim. `variantSet` is matched before `variant` so the longer keyword wins.
_VARIANT_DECL = re.compile(r"(?:variantSet|variant)\b")


async def run(
    brief: WorldBrief,
    layers: list[LayerSpec],
    layers_root: Path = Path("layers"),
) -> ValidatorVerdict:
    issues: list[str] = []
    # Specialists each issue is attributed to, recorded HERE at the point of
    # rejection (not parsed back out of the text). The supervisor prefers this over
    # scanning `issues` for names, so a prim path segment that merely looks like a
    # specialist name can't misroute route-back. Surfaced as
    # `ValidatorVerdict.failing_specialists`.
    failing: set[str] = set()
    # Set when a rejection cannot be repaired by re-running any specialist (an
    # over-budget world; see the gate below). The supervisor ends the revision loop
    # rather than routing back when this is set and no fixable specialist remains.
    terminal = False

    expected = {"director", "terrain", "biome", "prop", "lighting", "npc", "optimization"}
    got = {layer.specialist for layer in layers}
    missing = expected - got
    if missing:
        issues.append(f"missing specialist layers: {sorted(missing)}")
        failing.update(missing)  # the missing specialists must each re-run

    # Per-role metrics contract: a specialist whose role must report a metric (the
    # optimizer's over_budget verdict, terrain's triangle count the optimizer sums)
    # but emits it missing, mis-spelled, or non-finite silently drops the signal —
    # the gate below (or the optimizer's sum) never sees it and the world ships
    # over-budget. Reject and route back, naming the specialist. Checked before the
    # over_budget gate so a missing/garbage metric is reported as such, not masked
    # by the gate's `.get(..., 0.0)` default.
    metrics_ok = True
    for layer in layers:
        layer_metrics = _metrics_issues(layer)
        if layer_metrics:
            issues.extend(layer_metrics)
            failing.add(layer.specialist)
            metrics_ok = False

    # Read the gate only on a finite numeric value: a missing/garbage over_budget is
    # already reported by the metrics schema above, and comparing a non-number here
    # would raise (e.g. "str" > 0). A missing metric defaults to 0.0 (gate passes;
    # the schema is what rejects it).
    opt = next((layer for layer in layers if layer.specialist == "optimization"), None)
    over_budget = opt.metrics.get("over_budget", 0.0) if opt else 0.0
    if _is_finite_number(over_budget) and over_budget > 0:
        # Terminal, NOT routed back to optimization: optimization is the last,
        # deterministic LOD authority — re-running it recomputes the same triangle
        # sum and re-reports over_budget, so a route-back can only loop to MAX_ROUNDS.
        # The world is genuinely over budget and must shed source geometry; we reject
        # and let the supervisor end the loop (it still routes back for any co-occurring
        # FIXABLE issue, since over_budget no longer blames a specialist here).
        issues.append(
            "triangle budget exceeded — the world is over the triangle budget after "
            "optimization (the terminal LOD authority); a re-run recomputes the same "
            "result, so it must shed source geometry rather than be revised"
        )
        terminal = True

    # Open every emitted layer and confirm the engine could too: a garbled header,
    # a missing defaultPrim, or a dangling file composes into a world.usda that
    # fails silently at load. Each issue is attributed to the layer's own specialist.
    wellformed: list[LayerSpec] = []
    for layer in layers:
        reason = _layer_wellformedness(layers_root / layer.path)
        if reason:
            issues.append(f"{layer.specialist} layer {layer.path} {reason}")
            failing.add(layer.specialist)
        else:
            wellformed.append(layer)

    # Budget self-consistency — defense-in-depth across the optimizer trust boundary.
    # The over_budget gate above trusts optimization's self-reported metric; the
    # validator is the last gate, so re-derive the verdict from the emitted body + the
    # current summed geometry and reject a STALE or desynced optimization layer (its
    # metric/body predate a route-back's re-authored geometry, or a number was changed in
    # isolation) that would otherwise ship an over-budget world accepted — the
    # cross-trust-boundary check that until now lived only in the optimizer's own tests.
    # Runs ONLY when the inputs are trustworthy: every specialist present (complete
    # geometry sum), no metrics-schema issue (over_budget + every triangles finite), and
    # optimization's own layer well-formed — each is already rejected + routed above, so
    # re-deriving on top would only double-report. A disagreement routes back to
    # optimization to re-run (a re-emittable desync, NOT the genuine terminal over-budget
    # verdict the metric gate owns when metric and body agree).
    if opt is not None and not missing and metrics_ok and opt in wellformed:
        opt_text = (layers_root / opt.path).read_text()
        for message in _budget_self_consistency(opt, opt_text, _summed_prior_triangles(layers)):
            issues.append(message)
            failing.add("optimization")

    # Terrain triangle self-consistency — defense-in-depth across the terrain trust boundary,
    # the geometry-emitter twin of the budget gate above. _budget_self_consistency re-derives
    # the optimizer's verdict but SUMS terrain's `triangles` metric trusting it; a stale or
    # tampered terrain metric corrupts that sum and ships an over/under-counted world. Re-derive
    # the count from the emitted gridResolution and reject a mismatch, routing back to terrain.
    # Runs only when the metric is trustworthy to read (every metric clean) and terrain's layer
    # is well-formed — both already rejected + attributed above, so re-deriving on top would
    # only double-report; a body with no positive-integer gridResolution is skipped, not blamed.
    terrain_layer = next((layer for layer in layers if layer.specialist == "terrain"), None)
    if terrain_layer is not None and metrics_ok and terrain_layer in wellformed:
        terrain_text = (layers_root / terrain_layer.path).read_text()
        for message in _terrain_triangle_consistency(terrain_layer, terrain_text):
            issues.append(message)
            failing.add("terrain")

    # Biome scatter triangle self-consistency — the scatter-emitter sibling of the terrain
    # gate above. biome meters instanceCount * TRIS_PER_INSTANCE; the budget gate SUMS that
    # metric trusting it, so a stale/tampered biome metric corrupts the sum just as a terrain
    # one does. Re-derive from the emitted instanceCount and reject a mismatch, routing back
    # to biome. Same trust preconditions as terrain (metrics clean + biome's layer
    # well-formed, both already attributed above so re-deriving here only double-reports); a
    # body with no instanceCount is skipped, not blamed.
    biome_layer = next((layer for layer in layers if layer.specialist == "biome"), None)
    if biome_layer is not None and metrics_ok and biome_layer in wellformed:
        biome_text = (layers_root / biome_layer.path).read_text()
        for message in _biome_triangle_consistency(biome_layer, biome_text):
            issues.append(message)
            failing.add("biome")

    # NPC character triangle self-consistency — the character-emitter sibling of the terrain/biome
    # gates above. npc meters spawnCount * _character_tris(archetype); the budget gate SUMS that
    # metric trusting it, so a stale/tampered npc metric (a count not updated when the archetype
    # changed under a re-rostered director) corrupts the sum just as a terrain/biome one does.
    # Re-derive from the emitted spawnCount + archetype and reject a mismatch, routing back to npc.
    # Same trust preconditions as terrain/biome (metrics clean + npc's layer well-formed, both
    # already attributed above); a body missing either field is skipped, an unbudgetable archetype
    # is reported once (never raised through run()).
    npc_layer = next((layer for layer in layers if layer.specialist == "npc"), None)
    if npc_layer is not None and metrics_ok and npc_layer in wellformed:
        npc_text = (layers_root / npc_layer.path).read_text()
        for message in _npc_triangle_consistency(npc_layer, npc_text):
            issues.append(message)
            failing.add("npc")

    # Per-layer well-formedness isn't composability: two specialists can define the
    # same prim path with incompatible types, or dangle an override over a prim no
    # one defines, and the layers still open individually while the composed stage
    # is silently broken. Each conflict is attributed to the specialists that
    # authored it (from the layer tags, not the prim-path text).
    for specialists, message in _composition_attributions(wellformed, layers_root):
        issues.append(message)
        failing.update(specialists)

    # Director-intent gate — close the loop the rest of the pipeline only half-builds:
    # the director DECLARES hard intent (must_have, must_not) and prop/biome HONOR it,
    # but nothing VERIFIED they did. Reject a world where prop dropped a must-have asset
    # it should have placed, or biome left vegetation uncapped under a capping director,
    # naming the offending specialist so the supervisor routes the fix back. Runs over
    # the WELL-FORMED layers only: a missing/malformed director, prop, or biome layer is
    # already rejected + attributed by the gates above, so the intent check simply skips
    # it (no double-report, no crash) rather than re-deriving that failure (FM3).
    for specialist, message in _intent_attributions(wellformed, layers_root):
        issues.append(message)
        failing.add(specialist)

    # style check: TODO call sidecar with brief.style_anchors + rendered preview
    # for now, accept if no other issues
    accepted = len(issues) == 0

    return ValidatorVerdict(
        accepted=accepted, issues=issues, failing_specialists=sorted(failing), terminal=terminal
    )


def _director_intent(layers: list[LayerSpec], layers_root: Path, field: str) -> list[str]:
    """The director's ``intent:<field>`` tokens (``must_have`` / ``must_not``), parsed
    off the director layer under `layers_root`, or ``[]`` when no director layer
    contributed or it seeded no such intent.

    Mirrors ``prop._must_have_from_director`` / ``biome._must_not_from_director`` EXACTLY
    — same regex, same comma split, same empty-token filter — so the gate reads the same
    tokens the specialists honored, but off the validator's `layers_root` rather than the
    ``Path("layers")`` those hardcode (the validator is root-relative for testability).
    Degrades to ``[]`` on an absent/unreadable director layer: the intent gate then does
    not fire — a missing director is already rejected by the missing-specialist gate — so
    a director hiccup never raises through the final gate (FM3).
    """
    director = next((layer for layer in layers if layer.specialist == "director"), None)
    if director is None:
        return []
    try:
        text = (layers_root / director.path).read_text()
    except OSError:
        return []
    match = re.search(rf'intent:{field}\s*=\s*"([^"]*)"', text)
    if not match or not match.group(1):
        return []
    return [token for token in match.group(1).split(",") if token]


def _layer_text(layers: list[LayerSpec], specialist: str, layers_root: Path) -> str | None:
    """`specialist`'s on-disk layer text, or ``None`` when its spec is absent or the file
    can't be read. The intent gate then defers to the missing-specialist / well-formedness
    gates (which already reject and attribute the layer) instead of re-reporting it or
    crashing the final gate with a KeyError/OSError (FM3)."""
    spec = next((layer for layer in layers if layer.specialist == specialist), None)
    if spec is None:
        return None
    try:
        return (layers_root / spec.path).read_text()
    except OSError:
        return None


def _intent_attributions(
    layers: list[LayerSpec], layers_root: Path
) -> list[tuple[str, str]]:
    """Director-intent violations as ``(specialist, message)`` pairs over the well-formed
    layers — the VERIFY half of declare → honor → verify.

    Reuses prop's and biome's OWN mapping helpers — ``prop._required_assets`` (the
    ``MUST_HAVE_ASSET`` token→asset bridge, which SKIPS an unmappable token exactly as
    prop does) and ``biome._caps_vegetation`` (the ``MUST_NOT_VEGETATION_CAP``
    recognizer) — rather than re-deriving the mapping, so a future vocabulary change in a
    specialist can't desync the gate from what it emits. The gate therefore requires ONLY
    the assets prop would actually place and the cap ONLY when biome would actually cap: a
    token prop legitimately skips is absent from ``required`` and never demanded, so a
    correctly-built world is never false-rejected (FM1). A real violation — a mapped
    must-have with no Required prim, or a capping director with no ``vegetationCapped``
    marker — yields a pair naming the offending specialist so route-back targets it (FM2);
    the message keys off ``intent:must_have`` / ``intent:must_not`` (not the word
    "director") so the supervisor's text-scan fallback agrees with the structured
    attribution. No director intent, or a missing/malformed (hence not-well-formed) prop
    or biome layer, yields no pairs — the world validates exactly as before (FM3).

    The npc loop mirrors these for ``intent:factions``: npc draws its single emitted
    ``archetype`` from the director's faction roster (``npc._select_archetype`` over the
    same comma-list ``_director_intent`` parses), so the gate rejects an npc layer whose
    archetype is NOT a roster member — a stale/desynced/tampered pick — keying the message
    off ``intent:factions`` and naming npc. It fires ONLY for a present archetype against a
    non-empty roster (an empty roster means npc legitimately fell back to its default, so
    the gate stays silent), preserving the FM1/FM2/FM3 contract above.

    The lighting loop closes the fourth and last intent (``intent:beats``): lighting turns
    the director's free-form mood line into atmospheric fog and, for every beat it MODELS,
    emits a ``def Volume "Atmosphere"`` whose ``drivenBy`` lists those recognized tokens.
    The gate recomputes the recognized set with lighting's OWN ``_recognized_beats`` (so it
    can never demand an atmosphere for a beat lighting no longer models, nor miss one it
    newly does) and rejects a well-formed lighting layer whose Atmosphere ``drivenBy`` does
    not match it — a dropped volume or a desynced ``drivenBy`` — naming lighting off
    ``intent:beats``. When ``drivenBy`` DOES match (the beats are the right set), it then
    verifies the fog MAGNITUDE: lighting emits ``inputs:density = _fog_density(recognized)``,
    so a density desynced from that sum (stale from a pre-route beat set, or tampered in
    isolation while ``drivenBy`` still looks right) is rejected too, re-derived via lighting's
    OWN ``_fog_density`` for lock-step. It fires ONLY when >=1 modeled beat is present (a beats
    line with no modeled keyword imposes no atmosphere, so the pre-beats palette validates),
    and density is checked ONLY on the ``drivenBy``-correct branch (a wrong ``drivenBy`` is the
    single violation, never also a density complaint), holding the same FM1/FM2/FM3 contract.
    """
    out: list[tuple[str, str]] = []

    required = prop._required_assets(_director_intent(layers, layers_root, "must_have"))
    if required:
        prop_text = _layer_text(layers, "prop", layers_root)
        if prop_text is not None:
            placed = Counter(re.findall(r'requiredAsset\s*=\s*"([^"]+)"', prop_text))
            dropped = sorted(a for a, n in Counter(required).items() if placed[a] < n)
            if dropped:
                out.append((
                    "prop",
                    f"intent:must_have unmet: prop must place must-have asset(s) {dropped} "
                    f"but its layer carries no Required prim referencing them",
                ))

    if biome._caps_vegetation(_director_intent(layers, layers_root, "must_not")):
        biome_text = _layer_text(layers, "biome", layers_root)
        if biome_text is not None and not re.search(r"vegetationCapped\s*=\s*true", biome_text):
            out.append((
                "biome",
                "intent:must_not unmet: biome must cap vegetation density "
                "(intent:must_not forbids dense vegetation) but its layer carries no "
                "vegetationCapped marker",
            ))

    # The npc loop: npc draws its single emitted `archetype` from the director's faction
    # roster (intent:factions), so a present archetype that is NOT a roster member is a
    # stale/desynced/tampered npc layer — reject it, naming npc. Fires ONLY for a non-empty
    # roster (an empty roster means npc legitimately fell back to its default, nothing to
    # verify) and a present archetype (a layer missing it is a structural fault the
    # well-formedness gate owns, not this membership check). set(roster) is the exact list
    # npc selected from, parsed by the same `_director_intent` the roster comes through.
    roster = _director_intent(layers, layers_root, "factions")
    if roster:
        npc_text = _layer_text(layers, "npc", layers_root)
        if npc_text is not None:
            match = re.search(r'archetype\s*=\s*"([^"]+)"', npc_text)
            if match and match.group(1) not in set(roster):
                out.append((
                    # The message names npc but NOT "director" (which is pipeline-earlier),
                    # so the supervisor's word-boundary route-back scan targets npc, not the
                    # director — mirroring the prop/biome messages' intent:* keying.
                    "npc",
                    f"intent:factions unmet: npc spawned archetype "
                    f"{match.group(1)!r} not in the faction roster "
                    f"{sorted(set(roster))}; re-run npc",
                ))

    # The lighting loop closes the FOURTH and last intent (intent:beats): lighting turns the
    # director's free-form mood line into atmospheric fog — the one director intent no other
    # specialist consumes. When the line names >=1 beat lighting MODELS it emits a
    # `def Volume "Atmosphere"` carrying `drivenBy = "<recognized tokens>"`; with no modeled
    # beat it emits the bare pre-beats palette (no Atmosphere). So a non-empty recognized set
    # demands an Atmosphere driven by exactly those tokens — a stale lighting layer (authored
    # before beats drove fog) or a desynced/tampered drivenBy is the violation, named lighting.
    # `recognized` is computed with lighting's OWN _recognized_beats so the gate tracks
    # lighting's vocabulary in lock-step (a BEAT_FOG_DENSITY change can't desync it). beats is
    # free-form, not a comma-list, so _director_intent's comma-split is rejoined into one line
    # for the recognizer — lossless, since _recognized_beats re-tokenizes on every
    # non-alphanumeric (comma included), so the split-then-join round-trips to the raw line.
    recognized = lighting._recognized_beats(" ".join(_director_intent(layers, layers_root, "beats")))
    if recognized:
        lighting_text = _layer_text(layers, "lighting", layers_root)
        if lighting_text is not None:
            atmosphere = re.search(r'def Volume "Atmosphere"\s*\{(.*?)\}', lighting_text, re.DOTALL)
            driven = re.search(r'drivenBy\s*=\s*"([^"]*)"', atmosphere.group(1)) if atmosphere else None
            recorded = {t for t in driven.group(1).split(",") if t} if driven else set()
            if recorded != set(recognized):
                out.append((
                    # Names lighting (route-back target) but no pipeline-earlier specialist,
                    # keyed off intent:beats — the text-scan fallback agrees with the
                    # structured attribution, mirroring the prop/biome/npc messages.
                    "lighting",
                    f"intent:beats unmet: lighting must drive an Atmosphere volume from "
                    f"recognized beats {recognized} but its layer records drivenBy "
                    f"{sorted(recorded)}; re-run lighting",
                ))
            elif atmosphere is not None:
                # drivenBy is confirmed right, so the beats are the correct SET — now verify the
                # fog MAGNITUDE they accumulate to. lighting emits `inputs:density` =
                # _fog_density(recognized); a density frozen from a pre-route-back beat set, or one
                # tampered in isolation, desyncs from that sum while drivenBy still looks right (the
                # gap the drivenBy SET check structurally can't see). Re-derive via lighting's OWN
                # _fog_density so a BEAT_FOG_DENSITY change tracks in lock-step; abs_tol absorbs the
                # 2-decimal `{density:.2f}` emission (the float sum is imprecise vs its rounded
                # string — a tighter tol false-rejects a correct layer). Density is checked ONLY on
                # the drivenBy-correct branch, so a wrong drivenBy is the single violation, never
                # also a density complaint (no double-route of one root cause). A body with no
                # parseable density degrades to the well-formedness gate's concern — lighting always
                # co-emits density with drivenBy, so this skips only a deeper malformation.
                density = re.search(r"inputs:density\s*=\s*([0-9]*\.?[0-9]+)", atmosphere.group(1))
                expected = lighting._fog_density(recognized)
                if density is not None and not math.isclose(
                    float(density.group(1)), expected, abs_tol=5e-3
                ):
                    out.append((
                        "lighting",
                        f"intent:beats unmet: lighting's Atmosphere density {density.group(1)} != "
                        f"the {expected:.2f} its recognized beats {recognized} sum to "
                        f"(a stale or tampered fog magnitude); re-run lighting",
                    ))
    return out


def _is_finite_number(value: object) -> bool:
    """True for a real finite numeric metric: an int or float (int is legal against
    the float contract — FM3) that is neither NaN nor inf. bool is an int subclass
    but never a valid metric, so it's excluded. isinstance is checked first so
    math.isfinite never sees a non-number."""
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
    )


def _metrics_issues(layer: LayerSpec) -> list[str]:
    """Why `layer` violates its role's metrics contract, or [] if it satisfies it
    (or its role declares none — degrade cleanly, never KeyError, FM4).

    A required key that is missing, non-numeric, or NaN/inf is reported with the
    specialist named first so the supervisor's `_failing_specialist` routes the fix
    back to it; the metric keys (`over_budget`, `triangles`) contain no specialist
    name as a word, so they can't misroute. A present, finite int-or-float value is
    accepted — an int is not a type error against the float contract (FM3).
    """
    required = ROLE_METRICS.get(layer.specialist)
    if not required:
        return []
    out: list[str] = []
    for key in sorted(required):
        if key not in layer.metrics:
            out.append(
                f"{layer.specialist} layer {layer.path} is missing the required "
                f"metric '{key}' its role must emit"
            )
        elif not _is_finite_number(layer.metrics[key]):
            out.append(
                f"{layer.specialist} layer {layer.path} metric '{key}' must be a "
                f"finite number, got {layer.metrics[key]!r}"
            )
    return out


def _opt_scalar(text: str, key: str) -> str | None:
    """The raw value of optimization body scalar `key` (`key = <value>` to end of line),
    or None when absent. The optimizer writes one scalar per line, so the rest of the
    line is the value; the caller decides whether it parses as a number or a bool."""
    m = re.search(rf"\b{key}\s*=\s*([^\n]+)", text)
    return m.group(1).strip() if m else None


def _opt_number(text: str, key: str) -> float | None:
    """Optimization body field `key` as a FINITE float, or None if absent, non-numeric,
    or non-finite — so a missing/garbage figure is reported as malformed, never trusted,
    never raised (FM4). USD scalars here carry no type suffix, so float() parses the int
    budget and the double triangle counts alike, but float() also accepts 'inf'/'nan'/
    '1e999' — and an inf budget would make observedTriangles > triangleBudget always
    false, silently accepting an over-budget world. Reject non-finite here, mirroring the
    metrics path's `_is_finite_number`, so a tampered or stale body can't defeat the audit
    with the very kind of number-changed-in-isolation this gate exists to catch."""
    raw = _opt_scalar(text, key)
    if raw is None:
        return None
    try:
        value = float(raw)
    except ValueError:
        return None
    return value if math.isfinite(value) else None


def _opt_bool(text: str, key: str) -> bool | None:
    """Optimization body bool field `key`, or None if absent or not a USD bool literal."""
    raw = _opt_scalar(text, key)
    return True if raw == "true" else False if raw == "false" else None


def _opt_reductions(text: str) -> float | None:
    """Σ(authoredTriangles − effectiveTriangles) over the emitted LodDirectives — the
    geometry the recorded LOD shedding removed. None when a directive is malformed (a
    Scope missing/garbage authored/effective); no directives ⇒ 0.0 (nothing was shed)."""
    total = 0.0
    for block in re.finditer(r'def Scope "Lod_\d+"\s*\{(.*?)\}', text, re.DOTALL):
        body = block.group(1)
        authored = _opt_number(body, "authoredTriangles")
        effective = _opt_number(body, "effectiveTriangles")
        if authored is None or effective is None:
            return None
        total += authored - effective
    return total


def _summed_prior_triangles(layers: list[LayerSpec]) -> float:
    """Σ the current geometry layers' triangle metrics — the figure optimization's
    authoredTriangles must equal if it ran on THIS geometry. Mirrors
    optimization._resolve's own sum (every prior layer's `triangles`, missing → 0); a
    non-finite value is skipped (the metrics gate already rejected any contracted layer
    carrying one, and this only runs when that gate is clean)."""
    total = 0.0
    for layer in layers:
        if layer.specialist == "optimization":
            continue
        value = layer.metrics.get("triangles", 0.0)
        if _is_finite_number(value):
            total += value
    return total


def _budget_self_consistency(opt: LayerSpec, text: str, prior_triangles: float) -> list[str]:
    """Why optimization's emitted artifact disagrees with its over_budget metric or the
    current geometry, or [] when they are mutually consistent — the validator re-deriving
    the budget verdict instead of trusting the metric across the optimizer trust boundary.

    The optimizer is the deterministic budget authority, but a route-back can leave a
    STALE optimization layer whose cached metric/body reflect the pre-route geometry, or
    a number can be changed in isolation; either ships a genuinely-over-budget world as
    accepted because the gate above believes the metric. Re-derive from the body and the
    summed prior geometry and reject the mismatch, naming optimization so it re-runs (the
    desync is a re-emittable flaw — the metric gate keeps ownership of the genuine,
    terminal over-budget verdict when metric and body agree). A missing/non-numeric budget
    field is reported as MALFORMED, not raised (FM4)."""
    budget = _opt_number(text, "triangleBudget")
    authored = _opt_number(text, "authoredTriangles")
    observed = _opt_number(text, "observedTriangles")
    body_over = _opt_bool(text, "overBudget")
    reductions = _opt_reductions(text)
    malformed = sorted(
        name
        for name, value in (
            ("triangleBudget", budget),
            ("authoredTriangles", authored),
            ("observedTriangles", observed),
            ("overBudget", body_over),
            ("LodDirectives", reductions),
        )
        if value is None
    )
    if malformed:
        return [
            f"optimization layer {opt.path} budget body is malformed: cannot re-derive "
            f"the over-budget verdict — field(s) {malformed} missing or non-numeric"
        ]

    out: list[str] = []
    metric_over = opt.metrics.get("over_budget", 0.0) > 0
    rederived_over = observed > budget
    if metric_over != rederived_over:
        out.append(
            f"optimization layer {opt.path} over_budget metric disagrees with its body: "
            f"the metric says {'over' if metric_over else 'within'} budget but "
            f"observedTriangles {observed:.0f} vs triangleBudget {budget:.0f} re-derives "
            f"{'over' if rederived_over else 'within'} — a desynced or stale optimization "
            f"layer; re-run optimization"
        )
    if body_over != rederived_over:
        out.append(
            f"optimization layer {opt.path} body is internally inconsistent: overBudget="
            f"{str(body_over).lower()} but observedTriangles {observed:.0f} vs "
            f"triangleBudget {budget:.0f} re-derives {str(rederived_over).lower()}"
        )
    if not math.isclose(observed, authored - reductions, rel_tol=1e-9, abs_tol=1e-6):
        out.append(
            f"optimization layer {opt.path} body is internally inconsistent: "
            f"observedTriangles {observed:.0f} != {authored - reductions:.0f} "
            f"(authoredTriangles {authored:.0f} minus {reductions:.0f} of recorded LOD "
            f"reductions) — a phantom triangle figure"
        )
    if not math.isclose(authored, prior_triangles, rel_tol=1e-9, abs_tol=1e-6):
        out.append(
            f"optimization layer {opt.path} is stale: its authoredTriangles {authored:.0f} "
            f"!= the {prior_triangles:.0f} summed from the current prior layers — geometry "
            f"changed after optimization last ran; re-run optimization"
        )
    return out


def _terrain_triangle_consistency(layer: LayerSpec, text: str) -> list[str]:
    """Why terrain's reported ``triangles`` metric disagrees with the heightfield its body
    declares, or [] when they agree — defense-in-depth across the terrain trust boundary,
    the geometry-emitter twin of ``_budget_self_consistency`` over the optimizer.

    terrain meters ``_heightfield_triangles(gridResolution)``; ``_budget_self_consistency``
    re-derives the optimizer's verdict but SUMS this metric in trusting it, so a STALE
    (pre-route-back grid) or tampered terrain metric silently corrupts that budget sum and
    ships an over/under-counted world. Re-derive the count from the body's gridResolution —
    reusing ``terrain._heightfield_triangles`` so a change to the triangulation tracks in
    lock-step (the very coupling terrain's own helper docstring promises) — and reject a
    mismatch, naming terrain so route-back targets it. A gridResolution that is absent or not
    a positive integer leaves the metric unverifiable: SKIP (return []) rather than report it
    — field-presence is the schema gate's concern and the real ``terrain.run`` always emits a
    sane grid, so the VALID_BODY placeholder terrain layers other gates' tests use (no
    gridResolution) are never false-rejected (FM3). The reported metric is read straight from
    ``layer.metrics`` (the caller runs this only when the metrics schema is clean, so it is a
    finite number)."""
    grid = _opt_number(text, "gridResolution")
    if grid is None or grid < 1 or grid != int(grid):
        return []
    reported = layer.metrics.get("triangles", 0.0)
    expected = float(terrain._heightfield_triangles(int(grid)))
    if math.isclose(reported, expected, rel_tol=1e-9, abs_tol=1e-6):
        return []
    return [
        f"terrain layer {layer.path} triangles metric {reported:.0f} != the {expected:.0f} "
        f"its body declares (a {int(grid)}x{int(grid)} heightfield triangulates to "
        f"2*({int(grid)}-1)^2 triangles) — a stale or tampered terrain metric; re-run terrain"
    ]


def _biome_triangle_consistency(layer: LayerSpec, text: str) -> list[str]:
    """Why biome's reported ``triangles`` metric disagrees with the instanceCount its
    body declares, or [] when they agree — the scatter-emitter sibling of
    ``_terrain_triangle_consistency`` over the same geometry trust boundary.

    biome meters ``instanceCount * TRIS_PER_INSTANCE``; ``_budget_self_consistency`` SUMS
    this metric in trusting it, so a STALE (pre-route-back count) or tampered biome metric
    silently corrupts the budget sum and ships an over/under-counted world. Re-derive the
    count from the body's instanceCount — reusing ``biome.TRIS_PER_INSTANCE`` so a change to
    the per-instance triangle cost tracks in lock-step — and reject a mismatch, naming biome
    so route-back targets it. Unlike terrain's grid (a heightfield needs >=1), a biome count
    of 0 is a VALID empty-scatter region metering 0 triangles, so SKIP only a count that is
    absent, negative, or non-integer — a 0 still VERIFIES (a tampered ``instanceCount = 0``
    over a nonzero metric is a real violation to catch, not an unverifiable body to degrade
    on). The reported metric is read straight from ``layer.metrics`` (the caller runs this
    only when the metrics schema is clean, so it is a finite number)."""
    count = _opt_number(text, "instanceCount")
    if count is None or count < 0 or count != int(count):
        return []
    reported = layer.metrics.get("triangles", 0.0)
    expected = float(int(count) * biome.TRIS_PER_INSTANCE)
    if math.isclose(reported, expected, rel_tol=1e-9, abs_tol=1e-6):
        return []
    return [
        f"biome layer {layer.path} triangles metric {reported:.0f} != the {expected:.0f} "
        f"its body declares ({int(count)} instances * {biome.TRIS_PER_INSTANCE} tris each) "
        f"— a stale or tampered biome metric; re-run biome"
    ]


def _npc_triangle_consistency(layer: LayerSpec, text: str) -> list[str]:
    """Why npc's reported ``triangles`` metric disagrees with the character geometry its body
    declares, or [] when they agree — the character-emitter sibling of
    ``_terrain_triangle_consistency`` / ``_biome_triangle_consistency`` over the same geometry
    trust boundary.

    npc meters ``spawnCount * _character_tris(archetype)``; ``_budget_self_consistency`` SUMS this
    metric in trusting it, so a STALE (pre-route-back count, or a metric not updated when the
    archetype changed under a re-rostered director) or tampered npc metric silently corrupts the
    budget sum. Re-derive from the body's ``spawnCount`` + ``archetype`` — reusing
    ``npc._character_tris`` / ``npc.CHARACTER_TRIS`` so a per-archetype budget change tracks in
    lock-step — and reject a mismatch, naming npc so route-back targets it. Verifying needs BOTH
    fields: a ``spawnCount`` that is absent, negative, or non-integer, or an absent ``archetype``,
    leaves the metric unverifiable and is SKIPPED (field-presence is the schema gate's concern;
    the real ``npc.run`` always emits both). An ``archetype`` PRESENT but absent from
    ``CHARACTER_TRIS`` cannot be budgeted — ``npc.run`` would have raised at emission — so it is
    REPORTED (once), the membership checked BEFORE calling ``_character_tris`` so the helper's
    ``KeyError`` never escapes through ``run()`` (FM3). The reported metric is read straight from
    ``layer.metrics`` (the caller runs this only when the metrics schema is clean, so it is a
    finite number)."""
    count = _opt_number(text, "spawnCount")
    archetype = re.search(r'archetype\s*=\s*"([^"]*)"', text)
    if count is None or count < 0 or count != int(count) or archetype is None:
        return []
    name = archetype.group(1)
    if name not in npc.CHARACTER_TRIS:
        return [
            f"npc layer {layer.path} declares archetype {name!r} which has no triangle budget "
            f"in CHARACTER_TRIS — an unbudgetable character; re-run npc"
        ]
    reported = layer.metrics.get("triangles", 0.0)
    expected = float(int(count) * npc._character_tris(name))
    if math.isclose(reported, expected, rel_tol=1e-9, abs_tol=1e-6):
        return []
    return [
        f"npc layer {layer.path} triangles metric {reported:.0f} != the {expected:.0f} its body "
        f"declares ({int(count)} x {name} @ {npc._character_tris(name)} tris each) "
        f"— a stale or tampered npc metric; re-run npc"
    ]


def _layer_wellformedness(path: Path) -> str | None:
    """Why `path` is not a well-formed USD layer, or None if it is.

    Deterministic — no LLM, no GPU, no UsdStage. The structural checks are
    authoritative on their own (the gate venv has no usd-core); when pxr *is*
    installed it adds a strict parse on top, so a syntactically broken body that
    still has a header and a defaultPrim is caught in production.
    """
    try:
        text = path.read_text()
    except FileNotFoundError:
        return "is missing — no file on disk"
    except OSError as e:
        return f"is unreadable: {e}"

    if not text.strip():
        return "is empty"
    if not text.startswith(USDA_MAGIC):
        return f"does not start with the {USDA_MAGIC} header"
    if "defaultPrim" not in text:
        return "declares no defaultPrim"
    return _pxr_parse_issue(path)


def _pxr_parse_issue(path: Path) -> str | None:
    """Strict parse via pxr when available, else None. Guarded import so the gate
    venv — which ships without usd-core — degrades to the structural checks above
    instead of raising ImportError through the whole agents suite (FM4)."""
    try:
        from pxr import Sdf
    except ImportError:
        return None
    try:
        layer = Sdf.Layer.OpenAsAnonymous(str(path))
    except Exception:
        # pxr surfaces a malformed body as a Tf runtime error, not a return value.
        return "is not parseable as a USD layer"
    if not layer:
        return "is not parseable as a USD layer"
    if not layer.defaultPrim:
        return "declares no defaultPrim"
    return None


def _composition_attributions(
    layers: list[LayerSpec], layers_root: Path
) -> list[tuple[list[str], str]]:
    """Cross-layer composition conflicts as ``(specialists, message)`` across the
    well-formed specialist layers.

    Structural scan of every layer's prim specs, fed to `_conflict_attributions`.
    A missing/unreadable layer is skipped — the well-formedness gate already
    flagged it, so it isn't re-reported here. The single source of truth behind
    both the verdict's `issues` text and its structured `failing_specialists`.
    """
    tagged: list[tuple[str, str, str, str]] = []
    for layer in layers:
        full = layers_root / layer.path
        specs = _pxr_layer_specs(full)
        if specs is None:
            try:
                specs = _prim_specs(full.read_text())
            except OSError:
                continue
        for specifier, type_name, path in specs:
            tagged.append((layer.specialist, specifier, type_name, path))
    return _conflict_attributions(tagged)


def _composition_conflicts(layers: list[LayerSpec], layers_root: Path) -> list[str]:
    """Composition-conflict messages only — the text view over
    `_composition_attributions`."""
    return [message for _, message in _composition_attributions(layers, layers_root)]


def _prim_specs(text: str) -> list[tuple[str, str, str]]:
    """Every prim spec in a USD text layer as ``(specifier, type_name, prim_path)``.

    Structural heuristic, no pxr: prim scopes are the ``{ }`` blocks at paren-depth
    zero. Dictionary braces in layer/prim metadata always sit inside ``( )``, so a
    paren-aware counter tells the two apart. Quoted strings (single ``'`` / double
    ``"`` / triple ``'''`` / ``\"\"\"``), ``@...@`` asset paths, and ``#`` comments
    are skipped whole, so a brace or keyword inside any of them is never read as
    structure. ``type_name`` is ``""`` for a typeless prim (``def "X"``), which
    carries no type opinion.

    ``variantSet "x" = { "v" { ... } }`` braces are composition scopes, not prim
    scopes: they are tracked (so brace accounting stays aligned) but contribute no
    path segment, so a prim defined inside a variant attributes to the enclosing
    prim — not a phantom ``/Prim//x`` path, and not dropped.
    """
    specs: list[tuple[str, str, str]] = []
    # Each scope is (kind, name); only "prim" scopes contribute a path segment.
    # "variant" scopes (a variantSet block and each variant inside it) are
    # transparent — their braces are balanced but skipped when building a path.
    stack: list[tuple[str, str]] = []
    pending: str | None = None  # a declared prim awaiting its opening brace
    pending_variant = False  # a variantSet/variant keyword awaiting its brace
    paren = 0
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c == '"' or c == "'":
            if text[i : i + 3] == c * 3:  # triple-quoted: runs to the next triple
                end = text.find(c * 3, i + 3)
                i = n if end < 0 else end + 3
            else:
                i += 1
                while i < n and text[i] != c:
                    i += 2 if text[i] == "\\" else 1
                i += 1
        elif c == "@":  # asset path @...@ / @@@...@@@ — may hold braces, #, quotes
            marker = "@@@" if text[i : i + 3] == "@@@" else "@"
            end = text.find(marker, i + len(marker))
            i = n if end < 0 else end + len(marker)
        elif c == "#":
            while i < n and text[i] != "\n":
                i += 1
        elif c == "(":
            paren += 1
            i += 1
        elif c == ")":
            paren = max(paren - 1, 0)
            i += 1
        elif paren:
            i += 1
        elif c == "{":
            if pending is not None:
                stack.append(("prim", pending))
                pending = None
            elif pending_variant:
                stack.append(("variant", ""))  # a variantSet block
                pending_variant = False
            elif stack and stack[-1][0] == "variant":
                stack.append(("variant", ""))  # a bare variant inside a variantSet
            else:
                stack.append(("prim", ""))  # an unrecognized brace, treated as a scope
            i += 1
        elif c == "}":
            if stack:
                stack.pop()
            i += 1
        else:
            left_ok = i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")
            m = _PRIM_DECL.match(text, i) if left_ok else None
            if m:
                specifier, type_name, name = m.group(1), m.group(2) or "", m.group(3)
                prim_names = [nm for kind, nm in stack if kind == "prim"]
                specs.append((specifier, type_name, "/" + "/".join([*prim_names, name])))
                pending = name
                pending_variant = False
                i = m.end()
            elif left_ok and (vm := _VARIANT_DECL.match(text, i)):
                pending_variant = True
                i = vm.end()
            else:
                i += 1
    return specs


def _conflict_attributions(
    tagged: list[tuple[str, str, str, str]],
) -> list[tuple[list[str], str]]:
    """Composition conflicts from ``(specialist, specifier, type_name, prim_path)``
    as ``(specialists, message)`` pairs.

    Two classes — the ones provable without resolving full USD composition:
      - a prim path *defined* (``def``/``class``, not ``over``) with two or more
        incompatible non-empty type names across specialists; the composed prim's
        type is ambiguous. Same-type redefinition is a legal opinion-merge and a
        typeless def carries no type opinion, so neither is flagged;
      - a *dangling override*: a path overridden by some specialist but defined by
        none, so the opinion composes onto nothing.

    ``specialists`` is the sorted set the conflict implicates (the co-definers, or
    the lone author of a dangling override) — drawn from the layer TAGS, not the
    prim-path text. The supervisor routes back to the pipeline-earliest of them via
    this structured attribution, so a prim path that merely embeds (or equals) a
    specialist name can't pull route-back to an innocent node. The path is still
    embedded in the message for diagnostics, but it no longer drives routing.
    """
    types_by_path: dict[str, dict[str, set[str]]] = {}
    overs_by_path: dict[str, set[str]] = {}
    for specialist, specifier, type_name, path in tagged:
        if specifier == "over":
            overs_by_path.setdefault(path, set()).add(specialist)
            continue
        per_specialist = types_by_path.setdefault(path, {}).setdefault(specialist, set())
        if type_name:
            per_specialist.add(type_name)

    out: list[tuple[list[str], str]] = []
    for path in sorted(types_by_path):
        by_specialist = types_by_path[path]
        all_types = {t for types in by_specialist.values() for t in types}
        if len(all_types) > 1:
            who = sorted(by_specialist)
            out.append((
                who,
                f"composition conflict: specialists {', '.join(who)} define the same "
                f"prim <{path}> with incompatible types {sorted(all_types)}",
            ))

    defined = set(types_by_path)
    for path in sorted(overs_by_path):
        if path not in defined:
            who = sorted(overs_by_path[path])
            out.append((
                who,
                f"composition conflict: specialist {', '.join(who)} overrides prim "
                f"<{path}> but no specialist defines it (dangling override)",
            ))
    return out


def _conflicts_from_specs(tagged: list[tuple[str, str, str, str]]) -> list[str]:
    """Composition-conflict messages only — the text view over
    `_conflict_attributions`."""
    return [message for _, message in _conflict_attributions(tagged)]


def _pxr_layer_specs(path: Path) -> list[tuple[str, str, str]] | None:
    """A layer's prim specs as ``(specifier, type_name, prim_path)`` via pxr, or
    None when usd-core isn't importable / the layer won't open.

    Guarded like `_pxr_parse_issue`: returns None on any failure so
    `_composition_conflicts` falls back to the structural `_prim_specs` scan
    instead of raising through the gate (FM4). When pxr is present, USD's own
    parser supersedes the regex heuristic — exact specifier, type, and namespaced
    path for every prim spec in the layer. This resolves each layer authoritatively
    but not the full composed stage: reference/variant/instance-mediated conflicts
    still need the Pcp resolution that remains a TODO here.
    """
    try:
        from pxr import Sdf
    except ImportError:
        return None
    try:
        layer = Sdf.Layer.OpenAsAnonymous(str(path))
    except Exception:
        return None
    if not layer:
        return None
    specifier = {Sdf.SpecifierDef: "def", Sdf.SpecifierClass: "class", Sdf.SpecifierOver: "over"}
    specs: list[tuple[str, str, str]] = []
    stack = [layer.pseudoRoot]
    while stack:
        for child in stack.pop().nameChildren:
            specs.append(
                (specifier.get(child.specifier, "over"), child.typeName or "", child.path.pathString)
            )
            stack.append(child)
    return specs
