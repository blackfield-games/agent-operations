"""Validator — last gate, strongest layer. Combines:

  - NVIDIA Omni Asset Validator (schema, geometry, materials, performance)
  - Style classifier (embed render against moodboard anchors, cosine sim ≥ threshold)
  - Playability gates: no enclosed interiors in a frontier region (enforced — the
    intent gate rejects an `enclosedInterior` marker when the director forbids
    `interior_volumes`); spawn reachability (deferred — needs a navmesh)

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
from director import director
from prop import prop
from biome import biome
from lighting import lighting
from terrain import terrain
from npc import npc
from optimization import optimization

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

# The director's intent:must_not token that forbids enclosed interiors — the frontier
# "no enclosed interiors" rule the module docstring names. A prim declares itself an
# enclosed-interior volume with the boolean marker below (biome's vegetationCapped
# idiom); the intent gate rejects any well-formed layer carrying it when the director
# declared this token. No specialist authors interiors today, so the marker appears in
# no current layer — the gate is a pure additive no-op until one does (or a
# tampered/desynced layer smuggles one in): defense-in-depth, not a new producer contract.
INTERIOR_INTENT = "interior_volumes"

# Matches `custom bool enclosedInterior = true` at any inter-token spacing — the marker a
# prim carries to declare an enclosed interior. Mirrors biome's `vegetationCapped = true`.
ENCLOSED_INTERIOR_RE = re.compile(r"\benclosedInterior\s*=\s*true\b")

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
        for message in _budget_self_consistency(
            opt,
            opt_text,
            _summed_prior_triangles(layers),
            _prior_triangles_by_specialist(layers),
        ):
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

    # Terrain gridResolution vs the brief — the brief-re-derivation twin of the biome scatter-count
    # gate below. The triangle gate above pins triangles↔gridResolution, but the grid ITSELF is a
    # deterministic terrain._grid_resolution(region); a stale/tampered grid with a re-synced triangles
    # metric passes both the triangle gate and the budget sum, shipping a wrong-resolution floor.
    # Re-derive it off the brief and reject a mismatch, routing back to terrain. Gated on the layer
    # being well-formed only (NOT metrics_ok — this compares body↔brief directly, no metric involved),
    # mirroring the biome scatter gate; a body with no positive-integer gridResolution is skipped.
    if terrain_layer is not None and terrain_layer in wellformed:
        terrain_text = (layers_root / terrain_layer.path).read_text()
        for message in _terrain_grid_resolution_consistency(brief, terrain_layer, terrain_text):
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

    # Biome UNCAPPED scatter-COUNT vs the brief — the else-branch twin of the CAPPED magnitude gate
    # (_intent_attributions). The triangle gate above pins triangles↔instanceCount and the cap gate
    # pins instanceCount↔brief but ONLY under a vegetationCapped marker; an uncapped region's count
    # is re-derivable from the brief too, yet a stale/tampered count with a consistent metric passes
    # both. Re-derive it and reject a mismatch, routing back to biome. Gated on NOT-capped so the cap
    # gate keeps sole ownership of the capped count (no double-report) and — critically — a correctly-
    # capped count (below the uncapped scatter) is never false-rejected here. Wellformed-only; a body
    # with no parseable instanceCount is skipped (field-presence is the well-formedness gate's concern).
    if (
        biome_layer is not None
        and biome_layer in wellformed
        and not biome._caps_vegetation(_director_intent(layers, layers_root, "must_not"))
    ):
        biome_text = (layers_root / biome_layer.path).read_text()
        for message in _biome_scatter_count_consistency(brief, biome_layer, biome_text):
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

    # NPC spawnCount vs the brief — the brief-re-derivation twin of the terrain gridResolution / biome
    # uncapped scatter-count gates. The triangle gate above pins triangles↔(spawnCount × _character_tris),
    # but the spawnCount ITSELF is a deterministic npc._spawn_count(region); a stale/tampered count with a
    # re-synced triangles metric passes both the triangle gate and the budget sum, shipping the wrong crowd
    # size. Re-derive it off the brief and reject a mismatch, routing back to npc. Gated on the layer being
    # well-formed only (NOT metrics_ok — body↔brief directly, no metric involved), mirroring the terrain/
    # biome scatter gates; a spawnCount of 0 is a valid empty crowd (like biome's instanceCount, unlike
    # terrain's >=1 grid), so only an absent/negative/non-integer count is skipped.
    if npc_layer is not None and npc_layer in wellformed:
        npc_text = (layers_root / npc_layer.path).read_text()
        for message in _npc_spawn_count_consistency(brief, npc_layer, npc_text):
            issues.append(message)
            failing.add("npc")

    # Prop placement triangle self-consistency — the placement-emitter sibling of the terrain/biome/
    # npc gates above. prop meters count*_asset_tris(propAsset) + sum(_asset_tris(a) for a in the
    # requiredAsset markers); the budget gate SUMS that metric trusting it, so a stale/tampered prop
    # metric corrupts the sum. Re-derive from the emitted placementCount + propAsset + requiredAssets
    # and reject a mismatch, routing back to prop. Same trust preconditions as terrain/biome/npc; a
    # body missing placementCount/propAsset is skipped, an unbudgetable asset is reported once.
    prop_layer = next((layer for layer in layers if layer.specialist == "prop"), None)
    if prop_layer is not None and metrics_ok and prop_layer in wellformed:
        prop_text = (layers_root / prop_layer.path).read_text()
        for message in _prop_triangle_consistency(prop_layer, prop_text):
            issues.append(message)
            failing.add("prop")

    # Prop placementCount vs the brief — the brief-re-derivation twin of the terrain gridResolution /
    # biome uncapped scatter-count / npc spawnCount gates (the 4th and last geometry emitter). The
    # triangle gate above pins triangles↔(placementCount × _asset_tris + Σ required), but the
    # placementCount ITSELF is a deterministic prop._placement_count(region); a stale/tampered count with
    # a re-synced triangles metric passes both the triangle gate and the budget sum, shipping the wrong
    # prop density. Re-derive it off the brief and reject a mismatch, routing back to prop. Gated on the
    # layer being well-formed only (NOT metrics_ok — body↔brief directly, no metric involved), mirroring
    # the npc/terrain/biome scatter gates; a placementCount of 0 is a valid empty scatter (like biome's
    # instanceCount, unlike terrain's >=1 grid), so only an absent/negative/non-integer count is skipped.
    if prop_layer is not None and prop_layer in wellformed:
        prop_text = (layers_root / prop_layer.path).read_text()
        for message in _prop_placement_count_consistency(brief, prop_layer, prop_text):
            issues.append(message)
            failing.add("prop")

    # Prop FILL propAsset vs the brief — the SELECTION twin of the four count/grid brief-re-derivation
    # gates (terrain grid / biome scatter / npc spawn / prop placement). The triangle gate pins
    # triangles↔(placementCount × _asset_tris(propAsset) + Σ required) but TRUSTS the emitted asset to
    # price the fill, and the count gate re-derives only the count; a fill swapped in isolation to a
    # cheaper known asset, with the triangles metric re-synced, passes both the triangle gate and the
    # budget sum while shipping the region's fill as the wrong prop at the wrong per-asset budget. Re-
    # derive WHICH asset via prop._select_asset off the brief and reject a mismatch, routing back to
    # prop. Wellformed-only (body↔brief, no metric); an absent propAsset is skipped and an unknown asset
    # is left to the triangle/metrics gates (no double-report) — this gate owns a wrong-but-valid pick.
    if prop_layer is not None and prop_layer in wellformed:
        prop_text = (layers_root / prop_layer.path).read_text()
        for message in _prop_asset_consistency(brief, prop_layer, prop_text):
            issues.append(message)
            failing.add("prop")

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
    for specialist, message in _intent_attributions(brief, wellformed, layers_root):
        issues.append(message)
        failing.add(specialist)

    # Director intent:factions re-derivation — the PRODUCER twin of the four count/grid brief-re-derivation
    # gates and of the npc factions/selection CONSUMERS above. Those consumers re-derive npc's PICK off the
    # director's roster, so a STALE director roster that npc faithfully mirrored is invisible to them (the
    # pick is a member of, and the selection from, the stale roster) and ships the wrong region's factions
    # accepted. Re-derive the roster ITSELF via director._faction_roster off the brief and reject a
    # mismatch, naming director so route-back re-runs it. Runs over the well-formed director only (a
    # missing/malformed director is already rejected + attributed above, so this never double-reports);
    # silent on an absent roster (the empty-roster fallback the npc gate handles).
    director_layer = next((layer for layer in layers if layer.specialist == "director"), None)
    if director_layer is not None and director_layer in wellformed:
        for message in _director_factions_consistency(brief, layers, layers_root):
            issues.append(message)
            failing.add("director")
        # The beats twin: the director's intent:beats mood line drives lighting's Atmosphere, and the
        # lighting-beats gates re-check lighting's fog against that SAME line — so a stale line lighting
        # faithfully followed is invisible to them (drivenBy and density both agree with it). Re-derive
        # the line itself via director._region_beats and reject a mismatch, likewise naming director.
        # Same well-formed-director guard; silent on an absent line (the fog-less fallback lighting owns).
        for message in _director_beats_consistency(brief, layers, layers_root):
            issues.append(message)
            failing.add("director")

    # NPC archetype vs the brief + director roster — the SELECTION twin of the four count/grid brief-re-
    # derivation gates (terrain grid / biome scatter / npc spawn / prop placement) and the npc sibling of
    # the prop propAsset gate. The triangle gate pins triangles↔(spawnCount × _character_tris(archetype))
    # but TRUSTS the emitted archetype to price the crowd, and the spawn-count gate re-derives only the
    # count; an archetype swapped in isolation to a cheaper allowed one, with the triangles metric re-
    # synced, passes both while shipping the region's crowd as the wrong archetype at the wrong per-
    # character budget. Re-derive WHICH archetype via npc._select_archetype off the brief + the director
    # roster and reject a mismatch, routing back to npc. Placed AFTER the _intent_attributions loop so a
    # NON-member archetype (its intent:factions loop) and a FORBIDDEN one (its intent:must_not loop) are
    # already owned there — this gate defers both (no double-report) and fires ONLY for a legal-but-off-
    # selection pick. Wellformed-only (body↔brief, no metric); an absent archetype is skipped.
    if npc_layer is not None and npc_layer in wellformed:
        npc_text = (layers_root / npc_layer.path).read_text()
        for message in _npc_archetype_consistency(brief, layers, layers_root, npc_layer, npc_text):
            issues.append(message)
            failing.add("npc")

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
    brief: WorldBrief, layers: list[LayerSpec], layers_root: Path
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

    The biome branch then verifies the cap MAGNITUDE, not just the marker's presence — the
    biome twin of lighting's density check over its drivenBy-correct branch. When the
    ``vegetationCapped`` marker IS present (the cap is confirmed applied), a stale or tampered
    ``instanceCount`` — the UNCAPPED scatter count slapped under the marker — still ships
    accepted, the gap a presence-only check structurally can't see. The gate re-derives the
    count biome should have emitted via biome's OWN ``_scatter_count`` + ``VEGETATION_CAP_DENSITY``
    (lock-step, off the same ``brief`` biome.run consumed — the validator already holds it) and
    rejects a present ``instanceCount`` that disagrees, keyed off ``intent:must_not`` and naming
    biome. Checked ONLY on the marker-present branch (a missing marker stays the single
    violation, no density-style double-route) and ONLY for a present non-negative integer count
    (an absent/garbage count degrades to the well-formedness gate, mirroring lighting density's
    missing-field hole).

    The npc loop mirrors these for ``intent:factions``: npc draws its single emitted
    ``archetype`` from the director's faction roster (``npc._select_archetype`` over the
    same comma-list ``_director_intent`` parses), so the gate rejects an npc layer whose
    archetype is NOT a roster member — a stale/desynced/tampered pick — keying the message
    off ``intent:factions`` and naming npc. It fires ONLY for a present archetype against a
    non-empty roster (an empty roster means npc legitimately fell back to its default, so
    the gate stays silent), preserving the FM1/FM2/FM3 contract above.

    A SECOND must_not consumer follows the factions branch: ``intent:must_not`` can bar an
    npc archetype outright (the frontier forbids ``civilians``), which npc HONORS by
    excluding it from the pick — so a layer that still spawns one is rejected, named npc,
    keyed off ``intent:must_not``. The barred set is recomputed with npc's OWN
    ``_forbidden_archetypes`` (the ``MUST_NOT_ARCHETYPE`` bridge) so the gate tracks npc in
    lock-step, and it is NOT redundant with the factions membership check: a roster that
    wrongly includes ``civilian`` passes membership but is caught here, and an empty roster
    (factions silent) still bars a tampered civilian pick.

    The lighting loop closes the fourth intent (``intent:beats``): lighting turns
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

    The interiors loop closes the FIFTH intent (``intent:must_not = interior_volumes`` — the
    "no enclosed interiors" frontier rule): the ONE director intent no specialist consumes
    (biome/npc punt it) and, until now, no gate verified. It is therefore a pure VERIFY
    gate — when the director declared ``interior_volumes``, any well-formed layer carrying
    the ``enclosedInterior=true`` marker is rejected, named by its authoring specialist off
    ``intent:must_not`` so the supervisor routes the fix back (pipeline-earliest of several).
    No specialist emits the marker today, so it is additive (a correct world is byte-identical
    accepted) and silent without the intent (a non-frontier region may legitimately carry
    interiors) — defense-in-depth against a tampered/desynced/future layer, not a producer
    desync check like the four above (there is no producer helper to re-derive), holding the
    same FM1/FM2/FM3 contract.
    """
    out: list[tuple[str, str]] = []

    # Deliberately UNGUARDED on a non-empty `required` (the lighting hoist at the Atmosphere
    # check below, not a `if required:` branch): prop._required_block emits "" for an empty
    # asset set, so an empty `required` demands exactly ZERO Required prims — a real
    # expectation, not "nothing to check". The multiset comparison degenerates to it on its
    # own (`dropped` iterates an empty required_counts, so it stays []; `extra` becomes every
    # placed marker), which is why this needs no empty-case elif of its own. Gating the loop
    # on `required` instead left the `extra` check dark exactly when the director seeded no
    # must_have — or named only tokens with no MUST_HAVE_ASSET mapping — and a fabricated
    # Required prim (real `references` arc, `triangles` honestly inflated to match) then
    # cleared every other gate: the triangle gate prices whatever markers are in the body, so
    # it reconciles; the count and fill gates never look at required assets. `required` is the
    # MAPPED set (prop._required_assets skips vocabulary drift with a warning), so a director
    # naming only unmappable tokens still lands here with prop having placed nothing.
    required = prop._required_assets(_director_intent(layers, layers_root, "must_have"))
    prop_text = _layer_text(layers, "prop", layers_root)
    if prop_text is not None:
        required_counts = Counter(required)
        placed = Counter(re.findall(r'requiredAsset\s*=\s*"([^"]+)"', prop_text))
        dropped = sorted(a for a, n in required_counts.items() if placed[a] < n)
        if dropped:
            out.append((
                "prop",
                f"intent:must_have unmet: prop must place must-have asset(s) {dropped} "
                f"but its layer carries no Required prim referencing them",
            ))
        # The equality complement of `dropped`: the re-derived multiset is EXACT, so a marker
        # over the must-have set (a duplicated hero, or an unrequested asset the director never
        # demanded) is as much a desync as a dropped one — the must-have gate alone passes it
        # (all demanded present) and the triangle gate prices whatever markers are in the body,
        # so an honestly-inflated metric ships doubled/unrequested hero geometry accepted. Name
        # prop, not the director, or the route-back misfires to the director loop.
        extra = sorted(a for a, n in placed.items() if n > required_counts[a])
        if extra:
            out.append((
                "prop",
                f"intent:must_have over-placed: prop placed unrequested or duplicate "
                f"required asset(s) {extra} — its layer carries more Required prims than the "
                f"must-have set demands; re-run prop",
            ))

    if biome._caps_vegetation(_director_intent(layers, layers_root, "must_not")):
        biome_text = _layer_text(layers, "biome", layers_root)
        if biome_text is not None:
            if not re.search(r"vegetationCapped\s*=\s*true", biome_text):
                out.append((
                    "biome",
                    "intent:must_not unmet: biome must cap vegetation density "
                    "(intent:must_not forbids dense vegetation) but its layer carries no "
                    "vegetationCapped marker",
                ))
            else:
                # The cap marker is present (the cap is confirmed applied), so the SET/presence
                # half passes — now verify the MAGNITUDE, the biome twin of lighting's density
                # check over its drivenBy-correct branch. The presence check can't see a marker
                # slapped over a stale (pre-cap) or tampered instanceCount — the UNCAPPED scatter
                # count under a vegetationCapped=true line — which ships an over-scattered region
                # accepted. Re-derive the count biome SHOULD have emitted via biome's OWN
                # _scatter_count + VEGETATION_CAP_DENSITY (lock-step: a cap/jitter/density change
                # flips the expectation automatically), off the brief the validator already holds
                # (the same brief biome.run consumed — biome.run called _scatter_count(brief.biome,
                # brief.region.region_id, cap)), and reject a present count that disagrees. A body
                # with no parseable instanceCount degrades (skips): biome always co-emits it with
                # the marker, and field-presence is the well-formedness gate's concern — the
                # asymmetric gap to the present-marker check (mirroring lighting density's missing-
                # field hole). The guard is terrain/biome's: skip an absent/negative/non-integer
                # count, but a present 0 still verifies (a capped region never scatters 0, so a
                # tampered 0 under the marker is caught, not degraded past).
                count = _opt_number(biome_text, "instanceCount")
                if count is not None and count >= 0 and count == int(count):
                    expected = biome._scatter_count(
                        brief.biome, brief.region.region_id, biome.VEGETATION_CAP_DENSITY
                    )
                    if int(count) != expected:
                        out.append((
                            "biome",
                            f"intent:must_not unmet: biome's capped instanceCount {int(count)} != "
                            f"the {expected} a cap-reduced scatter yields (cap "
                            f"{biome.VEGETATION_CAP_DENSITY}) — a stale or tampered count under the "
                            f"vegetationCapped marker; re-run biome",
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

    # The must_not->npc loop: intent:must_not can bar an npc archetype outright (the frontier
    # forbids "civilians"). npc HONORS it by excluding the barred archetype from its pick, so a
    # layer that STILL spawns one is a tampered/desynced npc layer — reject it, naming npc,
    # keyed off intent:must_not. `forbidden` is recomputed with npc's OWN _forbidden_archetypes
    # (the MUST_NOT_ARCHETYPE bridge) so the gate tracks npc's vocabulary in lock-step — it can
    # never demand a ban npc no longer applies, nor miss a newly-barred archetype. This is
    # NOT redundant with the factions branch above: a roster that wrongly INCLUDES civilian
    # passes membership yet must be rejected here, and an empty roster (the factions branch
    # stays silent) still bars a tampered civilian pick. Fires ONLY for a present archetype in
    # the forbidden set; no barred token, a non-forbidden archetype, or a missing npc layer
    # yields no pair (deferring to the well-formedness / missing-layer gates), holding the same
    # FM1/FM2/FM3 contract.
    forbidden = npc._forbidden_archetypes(_director_intent(layers, layers_root, "must_not"))
    if forbidden:
        npc_text = _layer_text(layers, "npc", layers_root)
        if npc_text is not None:
            match = re.search(r'archetype\s*=\s*"([^"]+)"', npc_text)
            if match and match.group(1) in forbidden:
                out.append((
                    "npc",
                    f"intent:must_not unmet: npc spawned forbidden archetype "
                    f"{match.group(1)!r} (intent:must_not forbids {sorted(forbidden)}); re-run npc",
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
    lighting_text = _layer_text(layers, "lighting", layers_root)
    if recognized:
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
    elif lighting_text is not None and re.search(r'def Volume "Atmosphere"', lighting_text):
        # The empty-recognized mirror of the drivenBy equality above: with no modeled beat,
        # lighting.run emits the fog-less palette (no Atmosphere), so a layer that carries one
        # fabricates fog for a mood no beat seeded — a stale/tampered layer. The drivenBy check
        # guards only the non-empty case (recorded != set(recognized)); this closes the empty
        # case so the gate enforces the Atmosphere↔recognized equality both ways. Keyed off
        # intent:beats and names lighting (never a pipeline-earlier specialist) so the text-scan
        # fallback agrees with the structured attribution.
        out.append((
            "lighting",
            "intent:beats unmet: lighting drives an Atmosphere volume but no modeled beat is "
            "recognized — a stale or tampered fog layer; re-run lighting",
        ))

    # The must_not->interiors gate closes the FIFTH intent — the "no enclosed interiors"
    # frontier rule (intent:must_not = interior_volumes) the module docstring lists as a
    # playability gate but which, unlike every other intent, NO specialist consumes and
    # (until now) NO gate verified. It is the ONE director intent with no producer: biome
    # and npc explicitly punt interior_volumes as another specialist's constraint, and
    # nothing authors interiors today. So this is a pure VERIFY gate — defense-in-depth
    # against a tampered/desynced/future layer that declares an enclosed-interior volume
    # (the `enclosedInterior=true` marker) in a region the director marked off-limits.
    # Fires ONLY when the director DECLARED interior_volumes: a non-frontier region that
    # omits it may legitimately carry interiors (a premium hub), so a bare marker with no
    # declaration is not a violation. Each offender is named by the AUTHORING specialist
    # (route-back target) so the supervisor repairs whoever emitted it — pipeline-earliest
    # wins when several layers carry it — and the message keys off intent:must_not, never
    # the word "director" (pipeline-earlier), so the text-scan fallback agrees with the
    # structured attribution. Additive: no current well-formed layer carries the marker,
    # so a correct world is byte-identical accepted, and the gate is silent without the
    # intent. Scans the WELL-FORMED `layers` only (a missing/malformed layer is already
    # rejected + attributed above, so this never double-reports), reading each through
    # `_layer_text` so a file that vanished after the well-formedness check degrades to a
    # skip rather than raising through the final gate (FM3).
    if INTERIOR_INTENT in _director_intent(layers, layers_root, "must_not"):
        for layer in layers:
            text = _layer_text(layers, layer.specialist, layers_root)
            if text is not None and ENCLOSED_INTERIOR_RE.search(text):
                out.append((
                    layer.specialist,
                    f"intent:must_not unmet: {layer.specialist} layer declares an enclosed "
                    f"interior (enclosedInterior=true) but intent:must_not forbids "
                    f"interior_volumes (the frontier rule); re-run {layer.specialist}",
                ))
    return out


def _director_factions_consistency(
    brief: WorldBrief, layers: list[LayerSpec], layers_root: Path
) -> list[str]:
    """Why the director's emitted ``intent:factions`` roster disagrees with the roster the brief's
    region determines, or [] when they agree — the PRODUCER-side re-derivation the npc factions and
    selection gates structurally cannot do.

    ``director.run`` emits ``_faction_roster(brief.region.region_id)`` (a stable per-region hash-ranked
    subset of ``FACTION_ARCHETYPES``); npc draws its single ``archetype`` from that roster, and the npc
    factions membership + selection gates re-derive npc's PICK *off the director's own roster*. So a
    STALE director layer — a roster authored for another region, or tampered — is INVISIBLE to those
    gates: npc's pick is a member of, and the region-true selection FROM, the stale roster, so both stay
    silent while the wrong region's factions ship accepted. Only re-deriving the roster ITSELF off the
    brief (the same brief ``director.run`` consumed) catches it — the producer twin of the four
    terrain/biome/npc/prop count/selection brief-re-derivation gates. Reuse director's OWN
    ``_faction_roster`` so a ``FACTION_ARCHETYPES``/``ROSTER_SIZE``/salt change tracks in lock-step, and
    reject a present roster that is not byte-identical to it (the director emits a canonical SORTED
    roster, so any member/order/duplicate deviation is not what ``director.run`` produces). The message
    NAMES director — unlike the downstream gates, director IS the route-back target here, so both the
    structured attribution and the text-scan fallback correctly re-run the pipeline-earliest node. Fires
    ONLY for a PRESENT ``intent:factions`` that disagrees: an absent roster is the empty-roster fallback
    the npc gate already owns (demanding its presence would false-reject every placeholder / pre-factions
    director), and a missing/unreadable director degrades to [] via ``_director_intent`` (already rejected
    by the missing-specialist gate, so this never double-reports — FM3)."""
    emitted = _director_intent(layers, layers_root, "factions")
    if not emitted:
        return []
    expected = director._faction_roster(brief.region.region_id)
    if emitted == expected:
        return []
    return [
        f"intent:factions stale: director rostered {emitted} but the brief's region determines "
        f"{expected} (director._faction_roster) — a stale or tampered director roster; re-run director"
    ]


def _director_beats_consistency(
    brief: WorldBrief, layers: list[LayerSpec], layers_root: Path
) -> list[str]:
    """Why the director's emitted ``intent:beats`` mood line disagrees with the line the brief's region
    determines, or [] when they agree — the PRODUCER-side re-derivation the lighting beats gates cannot
    do, the beats twin of ``_director_factions_consistency``.

    ``director.run`` seeds ``". ".join(_region_beats(region)) + "."`` (a stable per-region subset of
    ``BEAT_VOCAB`` joined into one free-form line); lighting reads that SAME line and re-derives its
    Atmosphere ``drivenBy``/``density`` off it, and the validator's lighting-beats gates re-check
    lighting's fog *against the director's own line*. So a STALE director layer — a mood line authored for
    another region, or tampered — is INVISIBLE to those gates: lighting faithfully drove its fog from the
    stale line, so both ``drivenBy`` and ``density`` agree with it and stay silent while the wrong region's
    mood ships accepted. Only re-deriving the line ITSELF off the brief (the same brief ``director.run``
    consumed) catches it — the producer twin of the factions roster gate and of the four count/selection
    gates. Reuse director's OWN ``_region_beats`` and the exact ``". ".join(...) + "."`` join so a
    ``BEAT_VOCAB``/``BEATS_SIZE``/salt change tracks in lock-step, and compare the whole LINE byte-for-byte
    — never a recognized-token SET, which a stale line whose modeled tokens coincidentally match would slip,
    and the ``". "`` separator + trailing period are part of what ``director.run`` produces. The message
    NAMES director — unlike the downstream lighting gates, director IS the route-back target here, so both
    the structured attribution and the text-scan fallback re-run the pipeline-earliest node (director leads
    ``PIPELINE_ORDER``, so even a beat phrase that some day collided with a later specialist's name could
    not misroute). Fires ONLY for a PRESENT ``intent:beats`` that disagrees: an absent line is the fog-less
    fallback lighting already owns (demanding its presence would false-reject every placeholder / pre-beats
    director), and a missing/unreadable director degrades to [] (already rejected by the missing-specialist
    gate, so this never double-reports — FM3). Reads the raw line rather than ``_director_intent`` (which
    comma-splits for the token-list intents): beats is a free-form line, so the byte-exact compare needs it
    whole."""
    director_spec = next((layer for layer in layers if layer.specialist == "director"), None)
    if director_spec is None:
        return []
    try:
        text = (layers_root / director_spec.path).read_text()
    except OSError:
        return []
    match = re.search(r'intent:beats\s*=\s*"([^"]*)"', text)
    if not match or not match.group(1):
        return []
    emitted = match.group(1)
    expected = ". ".join(director._region_beats(brief.region.region_id)) + "."
    if emitted == expected:
        return []
    return [
        f"intent:beats stale: director seeded {emitted!r} but the brief's region determines "
        f"{expected!r} (director._region_beats) — a stale or tampered director beats line; re-run director"
    ]


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


def _opt_string(text: str, key: str) -> str | None:
    """Optimization body string field `key` with its USD double-quotes stripped, or None
    when absent or not a quoted literal — how a directive's `specialist`/`layer` identity
    is read back (the optimizer writes them via `usd_str`)."""
    raw = _opt_scalar(text, key)
    if raw is None or len(raw) < 2 or raw[0] != '"' or raw[-1] != '"':
        return None
    return raw[1:-1]


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


def _lod_directive_legality(
    opt_path: str, text: str, prior_by_specialist: dict[str, float] | None = None
) -> list[str]:
    """Why a recorded LodDirective is not a shed the optimizer could legally have made,
    or [] when every one is — the per-directive twin of the sum re-derivation above.

    _opt_reductions SUMS (authored − effective) trusting each directive's effective
    figure, and _budget_self_consistency checks that sum reconciles observedTriangles; but
    neither verifies the reduction is one the optimizer could produce. A shed-able layer
    only collapses to 1/(1<<lodLevel) of its authored triangles with 1 <= lodLevel <=
    MAX_LOD — a 1/8 floor, never emptied to nothing (optimization's own invariant,
    ``_lod_scale``/``MAX_LOD``). A stale or tampered directive claiming an effectiveTriangles
    below that floor fabricates a reduction, so observed == authored − Σreduction still
    balances and overBudget reads false while the world is genuinely over budget — the
    "number changed in isolation" the recorded directives exist to make checkable. Re-derive
    each directive's legal scale + effective across the trust boundary with the producer's
    own ``optimization._lod_scale``/``MAX_LOD`` (the lock-step reuse the terrain/biome/npc/
    prop/lighting gates use), and reject a mismatch, naming optimization so it re-runs. An
    absent or non-numeric lodLevel/lodScale is an illegal directive, not a crash.

    When ``prior_by_specialist`` is supplied (the run() path), a shed that is otherwise legal
    is ALSO grounded against the layer it names: its authoredTriangles must equal that
    specialist's real triangle metric. The effective check above pins effective to authored,
    and ``_budget_self_consistency`` ties only the TOTAL authored to the summed geometry — so
    an inflated per-directive authored (a shed crediting a bigger layer than the geometry
    holds) with a ratio-legal effective fabricates a reduction the sum reconciles to, and an
    over-budget world ships accepted. Match each directive's ``specialist`` to a current prior
    layer and reject an authoredTriangles that is not that layer's metric (or a ``specialist``
    naming no prior layer — a phantom shed crediting geometry that isn't there). It also rejects
    a shed naming a FLOOR-set specialist (``optimization._resolve_floor()``, the do-not-shed set
    — terrain by default, env-widenable): floor layers route into optimization's ``floor_total``
    and never into the sheddable candidates, so a recorded floor shed is a fabricated reduction
    of always-present geometry that grounds cleanly (terrain's authored legally equals its
    metric, so the check above stays silent). Checked first, before phantom/authored, so a
    floored name resolves to that one specific reason.

    Finally, as the trailing else of that grounded chain, it rejects a directive whose
    ``specialist`` was already shed by an earlier one. ``_opt_reductions`` SUMS (authored −
    effective) over every ``Lod_N`` block with no dedup and no per-layer cap, and
    ``_budget_self_consistency`` ties only the TOTAL to the summed geometry — so two
    ratio-legal directives naming the same layer double-count its reduction (Σreductions
    un-caps past the geometry the layer can physically give) and the over-budget world
    reconciles to accepted. ``optimization._resolve`` enumerates each sheddable index exactly
    once (one directive per specialist), so a repeat is a tamper/desync the ``seen`` set
    catches. Tracked per-run (initialised before the loop) so a later validation of a
    legitimately single shed of the same layer is not false-rejected. Runs only for
    a shed already legal by scale + effective (an illegal one is rejected regardless) and only
    when the map is given, so the direct-call unit path (no map) keeps its legality-only
    contract — the per-layer twin of the sum's ``authored == prior_triangles`` grounding."""
    out: list[str] = []
    floor = 1 << optimization.MAX_LOD
    seen: set[str] = set()
    for block in re.finditer(r'def Scope "(Lod_\d+)"\s*\{(.*?)\}', text, re.DOTALL):
        name, body = block.group(1), block.group(2)
        level = _opt_number(body, "lodLevel")
        scale = _opt_number(body, "lodScale")
        authored = _opt_number(body, "authoredTriangles")
        effective = _opt_number(body, "effectiveTriangles")
        if level is None or scale is None or authored is None or effective is None:
            out.append(
                f"optimization layer {opt_path} directive {name} is malformed: lodLevel/"
                f"lodScale/authoredTriangles/effectiveTriangles missing or non-numeric — "
                f"cannot re-derive the recorded reduction; re-run optimization"
            )
            continue
        if not level.is_integer() or not (1 <= level <= optimization.MAX_LOD):
            out.append(
                f"optimization layer {opt_path} directive {name} is an illegal shed: "
                f"lodLevel {level:g} is outside the legal range 1..={optimization.MAX_LOD} "
                f"(a shed layer floors at 1/{floor}, never below); re-run optimization"
            )
            continue
        legal_scale = optimization._lod_scale(int(level))
        if not math.isclose(scale, legal_scale, rel_tol=1e-9, abs_tol=1e-6):
            out.append(
                f"optimization layer {opt_path} directive {name} is an illegal shed: "
                f"lodScale {scale:g} != {legal_scale:g} for lodLevel {int(level)} — the "
                f"recorded scale does not match the level; re-run optimization"
            )
            continue
        if not math.isclose(effective, authored * legal_scale, rel_tol=1e-9, abs_tol=1e-6):
            out.append(
                f"optimization layer {opt_path} directive {name} is an illegal shed: "
                f"effectiveTriangles {effective:.0f} != {authored * legal_scale:.0f} "
                f"(authoredTriangles {authored:.0f} * lodScale {legal_scale:g} at lodLevel "
                f"{int(level)}) — a fabricated reduction the optimizer's 1/{floor} floor "
                f"could not produce; re-run optimization"
            )
            continue
        if prior_by_specialist is None:
            continue
        specialist = _opt_string(body, "specialist")
        if specialist in optimization._resolve_floor():
            out.append(
                f"optimization layer {opt_path} directive {name} names {specialist!r}, a "
                f"floor layer the optimizer never sheds — a fabricated reduction of "
                f"always-present geometry; re-run optimization"
            )
        elif specialist not in prior_by_specialist:
            out.append(
                f"optimization layer {opt_path} directive {name} names {specialist!r}, not a "
                f"current prior layer — a phantom shed crediting a reduction to geometry that "
                f"isn't there; re-run optimization"
            )
        elif not math.isclose(
            authored, prior_by_specialist[specialist], rel_tol=1e-9, abs_tol=1e-6
        ):
            out.append(
                f"optimization layer {opt_path} directive {name} authoredTriangles "
                f"{authored:.0f} != the {prior_by_specialist[specialist]:.0f} triangles the "
                f"{specialist} layer holds — an inflated authored base fabricating a reduction "
                f"the summed budget cannot see; re-run optimization"
            )
        elif specialist in seen:
            out.append(
                f"optimization layer {opt_path} directive {name} sheds {specialist!r} "
                f"already shed by an earlier directive — a double-counted reduction the "
                f"optimizer never emits (one directive per layer); re-run optimization"
            )
        else:
            seen.add(specialist)
    return out


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


def _prior_triangles_by_specialist(layers: list[LayerSpec]) -> dict[str, float]:
    """{specialist: triangles} over the current geometry layers — the per-layer breakdown
    each recorded LodDirective's authoredTriangles must match the one it names, the
    component twin of _summed_prior_triangles's total (sum of these == that total). Skips
    optimization (it sheds the others, not itself) and any non-finite metric (already
    rejected upstream), exactly as the sum does."""
    out: dict[str, float] = {}
    for layer in layers:
        if layer.specialist == "optimization":
            continue
        value = layer.metrics.get("triangles", 0.0)
        if _is_finite_number(value):
            out[layer.specialist] = value
    return out


def _budget_self_consistency(
    opt: LayerSpec,
    text: str,
    prior_triangles: float,
    prior_by_specialist: dict[str, float] | None = None,
) -> list[str]:
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
    field is reported as MALFORMED, not raised (FM4). The sum re-derivation is paired with
    a per-directive halving check (``_lod_directive_legality``): reconciling
    observedTriangles to authored − Σreduction proves the total balances, but a directive
    claiming an effectiveTriangles below the 1/8 shed floor fabricates a reduction that
    balances anyway — so each directive is also re-derived against the optimizer's own
    legal scale, closing the number-changed-in-isolation the sum alone cannot see."""
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
    out.extend(_lod_directive_legality(opt.path, text, prior_by_specialist))
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


def _terrain_grid_resolution_consistency(brief: WorldBrief, layer: LayerSpec, text: str) -> list[str]:
    """Why terrain's emitted ``gridResolution`` disagrees with the resolution the brief's
    region determines, or [] when they agree — the brief-re-derivation twin of
    ``_biome_scatter_count_consistency`` over terrain's un-sheddable geometry floor.

    ``_terrain_triangle_consistency`` pins triangles↔gridResolution (metric↔body), but the
    gridResolution ITSELF is a deterministic function of the region — ``terrain.run`` emits
    ``_grid_resolution(brief.region.region_id)`` (``BASE_GRID_RESOLUTION`` scaled by a stable
    per-region hash) — that nothing re-derived. A STALE (authored for another region, pre-
    route-back) or tampered grid carrying a triangles metric re-synced to it passes BOTH the
    triangle gate AND the optimizer's budget sum, shipping a heightfield at the WRONG resolution
    for its region (the number-changed-in-isolation the sum alone cannot see). Re-derive via
    terrain's OWN ``_grid_resolution`` off the brief the validator already holds (the same brief
    ``terrain.run`` consumed) and reject a mismatch, naming terrain so route-back targets it.
    Same skip discipline as the triangle gate: a gridResolution that is absent, below 1, or non-
    integer is unverifiable (field-presence is the well-formedness gate's concern) — SKIP; a
    present positive-integer grid that disagrees is a real violation."""
    grid = _opt_number(text, "gridResolution")
    if grid is None or grid < 1 or grid != int(grid):
        return []
    expected = terrain._grid_resolution(brief.region.region_id)
    if int(grid) == expected:
        return []
    return [
        f"terrain layer {layer.path} gridResolution {int(grid)} != the {expected} the brief's "
        f"region determines (terrain._grid_resolution) — a stale or tampered grid; re-run terrain"
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


def _biome_scatter_count_consistency(brief: WorldBrief, layer: LayerSpec, text: str) -> list[str]:
    """Why biome's emitted UNCAPPED ``instanceCount`` disagrees with the scatter the brief
    determines, or [] when they agree — the brief-re-derivation twin of the CAPPED magnitude
    gate (``_intent_attributions``), closing the same body↔brief check for the uncapped case.

    ``_biome_triangle_consistency`` pins triangles↔instanceCount and the cap magnitude gate pins
    instanceCount↔brief, but ONLY under a ``vegetationCapped`` marker. An uncapped region's count
    is a deterministic function of the brief too (``biome.run`` emits ``_scatter_count(brief.biome,
    region, None)``), yet nothing re-derived it — a STALE (pre-route-back, authored for another
    region/brief) or tampered count carrying a CONSISTENT triangles metric passes both the triangle
    gate and the budget sum, shipping an under/over-scattered region accepted (the number-changed-
    in-isolation the sum alone cannot see). Re-derive via biome's OWN ``_scatter_count`` (cap
    ``None``) off the brief the validator already holds (the same brief ``biome.run`` consumed) and
    reject a mismatch, naming biome so route-back targets it. The CALLER gates this on NOT-capped,
    so the cap magnitude gate keeps sole ownership of the capped count (no double-report) and a
    correctly-capped count — strictly below the uncapped scatter — is never false-rejected here.
    Same skip discipline as the triangle gate: an absent/negative/non-integer count is unverifiable
    (field-presence is the well-formedness gate's concern) — SKIP; but a present count that
    disagrees (including a tampered 0 over a nonzero-scatter region) is a real violation."""
    count = _opt_number(text, "instanceCount")
    if count is None or count < 0 or count != int(count):
        return []
    expected = biome._scatter_count(brief.biome, brief.region.region_id, None)
    if int(count) == expected:
        return []
    return [
        f"biome layer {layer.path} uncapped instanceCount {int(count)} != the {expected} the "
        f"brief's region scatters (biome._scatter_count, cap none) — a stale or tampered scatter "
        f"count; re-run biome"
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


def _npc_spawn_count_consistency(brief: WorldBrief, layer: LayerSpec, text: str) -> list[str]:
    """Why npc's emitted ``spawnCount`` disagrees with the crowd size the brief's region
    determines, or [] when they agree — the brief-re-derivation twin of
    ``_terrain_grid_resolution_consistency`` / ``_biome_scatter_count_consistency`` over npc's
    other geometry half.

    ``_npc_triangle_consistency`` pins triangles↔(spawnCount × ``_character_tris(archetype)``)
    (metric↔body), but the spawnCount ITSELF is a deterministic function of the region — ``npc.run``
    emits ``_spawn_count(brief.region.region_id)`` (``BASE_SPAWNS`` scaled by a stable per-region
    hash), independent of the roster-dependent archetype — that nothing re-derived. A STALE (authored
    for another region, pre-route-back) or tampered count carrying a triangles metric re-synced to it
    passes BOTH the triangle gate AND the optimizer's budget sum, shipping a region at the WRONG crowd
    size (the number-changed-in-isolation the sum alone cannot see). Re-derive via npc's OWN
    ``_spawn_count`` off the brief the validator already holds (the same brief ``npc.run`` consumed)
    and reject a mismatch, naming npc so route-back targets it. Unlike terrain's grid (a heightfield
    needs >=1), a spawnCount of 0 is a VALID empty-crowd region metering 0 npc triangles — like biome's
    instanceCount — so SKIP only a count that is absent, negative, or non-integer (field-presence is
    the well-formedness gate's concern); a present count that disagrees (including a tampered 0 over a
    nonzero-spawn region) is a real violation."""
    count = _opt_number(text, "spawnCount")
    if count is None or count < 0 or count != int(count):
        return []
    expected = npc._spawn_count(brief.region.region_id)
    if int(count) == expected:
        return []
    return [
        f"npc layer {layer.path} spawnCount {int(count)} != the {expected} the brief's region "
        f"determines (npc._spawn_count) — a stale or tampered spawn count; re-run npc"
    ]


def _prop_triangle_consistency(layer: LayerSpec, text: str) -> list[str]:
    """Why prop's reported ``triangles`` metric disagrees with the placement geometry its body
    declares, or [] when they agree — the placement-emitter sibling of the terrain/biome/npc
    triangle gates over the same geometry trust boundary.

    prop meters ``placementCount * _asset_tris(propAsset)`` (the hash-selected FILL asset placed
    ``count`` times) PLUS ``sum(_asset_tris(a) for a in requiredAssets)`` (one of each director-
    required hero, emitted as ``requiredAsset`` markers) — a fill term plus a variable required sum,
    unlike the others' single term. ``_budget_self_consistency`` SUMS this metric in trusting it, so
    a STALE (pre-route-back count/required-set) or tampered prop metric silently corrupts the budget
    sum. Re-derive from the body's ``placementCount`` + ``propAsset`` + every ``requiredAsset`` marker
    — reusing ``prop._asset_tris`` / ``prop.ASSET_TRIS`` so a per-asset budget change tracks in lock-
    step — and reject a mismatch, naming prop so route-back targets it. Needs the fill fields to
    verify: an absent/negative/non-integer ``placementCount`` or an absent ``propAsset`` leaves the
    metric unverifiable and is SKIPPED (field-presence is the schema gate's concern; the real
    ``prop.run`` always emits both, and an empty required set is legal — a region with no must-have).
    Any placed asset (fill or required) absent from ``ASSET_TRIS`` cannot be budgeted — ``prop.run``
    would have raised at emission — so it is REPORTED (once), membership checked BEFORE
    ``_asset_tris`` so its ``KeyError`` never escapes through ``run()``. The reported metric is read
    straight from ``layer.metrics`` (finite by the caller's metrics_ok precondition)."""
    count = _opt_number(text, "placementCount")
    fill = re.search(r'propAsset\s*=\s*"([^"]*)"', text)
    if count is None or count < 0 or count != int(count) or fill is None:
        return []
    required = re.findall(r'requiredAsset\s*=\s*"([^"]*)"', text)
    placed = [fill.group(1), *required]
    unknown = [asset for asset in placed if asset not in prop.ASSET_TRIS]
    if unknown:
        return [
            f"prop layer {layer.path} places asset(s) {unknown} with no triangle budget in "
            f"ASSET_TRIS — unbudgetable props; re-run prop"
        ]
    reported = layer.metrics.get("triangles", 0.0)
    expected = float(
        int(count) * prop._asset_tris(fill.group(1)) + sum(prop._asset_tris(a) for a in required)
    )
    if math.isclose(reported, expected, rel_tol=1e-9, abs_tol=1e-6):
        return []
    return [
        f"prop layer {layer.path} triangles metric {reported:.0f} != the {expected:.0f} its body "
        f"declares ({int(count)} x {fill.group(1)} + must-have {required}) "
        f"— a stale or tampered prop metric; re-run prop"
    ]


def _prop_placement_count_consistency(brief: WorldBrief, layer: LayerSpec, text: str) -> list[str]:
    """Why prop's emitted ``placementCount`` disagrees with the placement density the brief's region
    determines, or [] when they agree — the brief-re-derivation twin of
    ``_terrain_grid_resolution_consistency`` / ``_biome_scatter_count_consistency`` /
    ``_npc_spawn_count_consistency``, the 4th and last geometry emitter's brief-determined dimension.

    ``_prop_triangle_consistency`` pins triangles↔(placementCount × ``_asset_tris(propAsset)`` + Σ
    required), but the placementCount ITSELF is a deterministic function of the region — ``prop.run``
    emits ``_placement_count(brief.region.region_id)`` (``BASE_PLACEMENTS`` scaled by a stable per-
    region hash), independent of the director-required hero set and the hash-selected fill asset — that
    nothing re-derived. A STALE (authored for another region, pre-route-back) or tampered count carrying
    a triangles metric re-synced to it passes BOTH the triangle gate AND the optimizer's budget sum,
    shipping a region at the WRONG prop density (the number-changed-in-isolation the sum alone cannot
    see). Re-derive via prop's OWN ``_placement_count`` off the brief the validator already holds (the
    same brief ``prop.run`` consumed) and reject a mismatch, naming prop so route-back targets it. This
    gate owns only the FILL count; the required-asset set is director-driven (the intent gate's concern),
    not brief-region-derived. Unlike terrain's grid (a heightfield needs >=1), a placementCount of 0 is a
    VALID empty-scatter region metering 0 fill triangles — like biome's instanceCount and npc's
    spawnCount — so SKIP only a count that is absent, negative, or non-integer (field-presence is the
    well-formedness gate's concern); a present count that disagrees (including a tampered 0 over a
    nonzero-placement region) is a real violation."""
    count = _opt_number(text, "placementCount")
    if count is None or count < 0 or count != int(count):
        return []
    expected = prop._placement_count(brief.region.region_id)
    if int(count) == expected:
        return []
    return [
        f"prop layer {layer.path} placementCount {int(count)} != the {expected} the brief's region "
        f"determines (prop._placement_count) — a stale or tampered placement count; re-run prop"
    ]


def _prop_asset_consistency(brief: WorldBrief, layer: LayerSpec, text: str) -> list[str]:
    """Why prop's emitted fill ``propAsset`` disagrees with the library asset the brief's region
    deterministically selects, or [] when they agree — the SELECTION twin of the four count/grid
    brief-re-derivation gates (terrain gridResolution / biome scatter-count / npc spawnCount / prop
    placementCount) over prop's OTHER brief-determined dimension: not how MANY props, but WHICH one.

    prop places one hash-selected fill asset ``placementCount`` times — ``prop.run`` emits
    ``propAsset = _select_asset(brief.region.region_id)`` (a stable per-region hash over the SORTED
    ``ASSET_TRIS`` keys, salted distinctly from the count so asset and count vary independently).
    ``_prop_triangle_consistency`` pins triangles↔(placementCount × ``_asset_tris(propAsset)`` + Σ
    required) but TRUSTS the emitted asset to PRICE the fill, and the placement-count gate re-derives
    only the count — so a fill swapped in isolation to a CHEAPER known asset, with the triangles metric
    re-synced to that asset, passes the triangle gate AND ``_budget_self_consistency`` while shipping
    the region's fill as the wrong prop at the wrong per-asset budget (a hero comms_tower_01 at 12k tris
    silently down-swapped to an ammo_crate_01 at 600 — a 20× geometry under-count the sum cannot see).
    Re-derive WHICH asset via prop's OWN ``_select_asset`` off the brief the validator already holds (the
    same brief ``prop.run`` consumed) and reject a mismatch, naming prop so route-back targets it.

    Owns only the deterministic FILL pick: the director-required hero set is intent-driven (the intent
    gate's concern), not brief-region-derived, so it stays out of this gate. Fires ONLY for a present
    fill that IS budgetable (a member of ``ASSET_TRIS``): an absent ``propAsset`` is skipped (field-
    presence is the well-formedness gate's concern; ``prop.run`` always co-emits it), and an UNKNOWN
    asset is left to ``_prop_triangle_consistency``'s unbudgetable report (under metrics_ok) / the
    metrics-schema gate (which routes prop back when the metric is unreadable and the triangle gate is
    skipped) — never double-reported here. This gate's unique concern is a wrong-but-VALID pick, the
    hole the budget checks structurally cannot see."""
    fill = re.search(r'propAsset\s*=\s*"([^"]*)"', text)
    if fill is None or fill.group(1) not in prop.ASSET_TRIS:
        return []
    expected = prop._select_asset(brief.region.region_id)
    if fill.group(1) == expected:
        return []
    return [
        f"prop layer {layer.path} fill propAsset {fill.group(1)!r} != the {expected!r} the brief's "
        f"region deterministically selects (prop._select_asset) — a stale or tampered fill asset; "
        f"re-run prop"
    ]


def _npc_archetype_consistency(
    brief: WorldBrief, layers: list[LayerSpec], layers_root: Path, layer: LayerSpec, text: str
) -> list[str]:
    """Why npc's emitted ``archetype`` disagrees with the one the brief's region + the director's roster
    deterministically select, or [] when they agree — the SELECTION twin of the four count/grid brief-re-
    derivation gates (terrain gridResolution / biome scatter-count / npc spawnCount / prop placementCount)
    and the npc sibling of ``_prop_asset_consistency``, over npc's OTHER brief-determined dimension: not
    how MANY npcs, but WHICH archetype.

    npc spawns one hash-selected archetype ``spawnCount`` times — ``npc.run`` emits
    ``archetype = _select_archetype(region_id, roster, forbidden)`` (a stable per-region hash over the
    SORTED faction roster the director seeded, salted distinctly from the spawn count so archetype and
    count vary independently; members the director's ``intent:must_not`` forbids are excluded first; falls
    back to ``NPC_ARCHETYPE`` when the roster is empty or wholly forbidden). ``_npc_triangle_consistency``
    pins triangles↔(spawnCount × ``_character_tris(archetype)``) but TRUSTS the emitted archetype to PRICE
    the crowd, and the spawn-count gate re-derives only the count — so an archetype swapped in isolation to
    a cheaper allowed one, with the triangles metric re-synced, passes both gates while shipping the
    region's crowd as the wrong archetype at the wrong per-character budget. Re-derive WHICH archetype via
    npc's OWN ``_select_archetype`` off the brief + the director roster the validator already holds (the
    same inputs ``npc.run`` consumed) and reject a mismatch, naming npc so route-back targets it.

    DEFERS the two cases the existing intent gates own, so it never double-reports: a NON-member archetype
    (a non-empty roster that does not contain it) is ``_intent_attributions``' ``intent:factions`` loop,
    and a FORBIDDEN archetype is its ``intent:must_not`` loop. The complement of those two — a LEGAL pick
    (in the roster, or the empty-roster fallback) that is NOT forbidden yet is still not the region's
    deterministic selection — is the hole NEITHER intent gate sees and this gate's unique concern. An
    absent archetype is skipped (field-presence is the well-formedness gate's concern; ``npc.run`` always
    co-emits it). ``roster``/``forbidden`` are recomputed from prop/biome's own ``_director_intent`` and
    npc's own ``_forbidden_archetypes`` so a roster or MUST_NOT_ARCHETYPE change flips the expectation in
    lock-step with the intent gates and the producer."""
    match = re.search(r'archetype\s*=\s*"([^"]+)"', text)
    if match is None:
        return []
    archetype = match.group(1)
    roster = _director_intent(layers, layers_root, "factions")
    forbidden = npc._forbidden_archetypes(_director_intent(layers, layers_root, "must_not"))
    if not ((not roster or archetype in set(roster)) and archetype not in forbidden):
        return []
    expected = npc._select_archetype(brief.region.region_id, roster, forbidden)
    if archetype == expected:
        return []
    return [
        # No pipeline-earlier specialist name (director/terrain/biome/prop/lighting) in the message: the
        # roster comes from the director but the word-boundary route-back text scan would misroute a
        # "director" mention to that pipeline-earlier node — so phrase it "faction roster", naming only npc.
        f"npc layer {layer.path} archetype {archetype!r} != the {expected!r} the region + faction roster "
        f"deterministically select (npc._select_archetype) — a stale or tampered archetype; re-run npc"
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
