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
from pathlib import Path

from common.types import WorldBrief, LayerSpec, ValidatorVerdict

STYLE_SIM_THRESHOLD = 0.72

# Metrics a specialist's role is contracted to ALWAYS emit, keyed by role. The
# value is the set of required metric keys; every metric is a finite number
# (LayerSpec.metrics is dict[str, float], so the value type is uniform — the
# contract is "present and finite", with int and float both legal). Only roles
# with a real downstream consumer are listed: the optimizer's budget verdict
# (read by run() below) and the triangle count the optimizer sums across prior
# layers. A role with no entry imposes NO requirement, so the specialists that
# legitimately emit no metrics (director, prop, lighting, npc) are
# unaffected — keeping the schema to what a role genuinely always emits.
#
# NOTE: the optimizer sums `triangles` across ALL prior layers; terrain and biome
# both emit geometry, so both are contracted emitters. If a further stub
# (prop/…) starts contributing geometry, add `triangles` to its contract here so a
# mis-spelled key there can't silently under-count the budget.
ROLE_METRICS: dict[str, set[str]] = {
    "optimization": {"over_budget"},
    "terrain": {"triangles"},
    "biome": {"triangles"},
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
    for layer in layers:
        layer_metrics = _metrics_issues(layer)
        if layer_metrics:
            issues.extend(layer_metrics)
            failing.add(layer.specialist)

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

    # Per-layer well-formedness isn't composability: two specialists can define the
    # same prim path with incompatible types, or dangle an override over a prim no
    # one defines, and the layers still open individually while the composed stage
    # is silently broken. Each conflict is attributed to the specialists that
    # authored it (from the layer tags, not the prim-path text).
    for specialists, message in _composition_attributions(wellformed, layers_root):
        issues.append(message)
        failing.update(specialists)

    # style check: TODO call sidecar with brief.style_anchors + rendered preview
    # for now, accept if no other issues
    accepted = len(issues) == 0

    return ValidatorVerdict(
        accepted=accepted, issues=issues, failing_specialists=sorted(failing), terminal=terminal
    )


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
