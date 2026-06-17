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

import re
from pathlib import Path

from common.types import WorldBrief, LayerSpec, ValidatorVerdict

STYLE_SIM_THRESHOLD = 0.72

# USD text layers must begin with this cookie; pxr refuses to open a file without
# it and the engine silently skips a sublayer it cannot parse.
USDA_MAGIC = "#usda"

# A USD prim declaration: a specifier, an optional type name, a quoted prim name.
# Drives the structural composition-conflict scan when usd-core isn't installed.
_PRIM_DECL = re.compile(r'(def|over|class)\b[ \t]+(?:([A-Za-z_]\w*)[ \t]+)?"([^"]+)"')


async def run(
    brief: WorldBrief,
    layers: list[LayerSpec],
    layers_root: Path = Path("layers"),
) -> ValidatorVerdict:
    issues: list[str] = []

    expected = {"director", "terrain", "biome", "prop", "lighting", "npc", "optimization"}
    got = {layer.specialist for layer in layers}
    missing = expected - got
    if missing:
        issues.append(f"missing specialist layers: {sorted(missing)}")

    opt = next((layer for layer in layers if layer.specialist == "optimization"), None)
    if opt and opt.metrics.get("over_budget", 0.0) > 0:
        issues.append("triangle budget exceeded — re-run optimization with stricter LODs")

    # Open every emitted layer and confirm the engine could too: a garbled header,
    # a missing defaultPrim, or a dangling file composes into a world.usda that
    # fails silently at load. Each issue leads with the specialist name so the
    # supervisor's _failing_specialist routes the fix back to the offending node.
    wellformed: list[LayerSpec] = []
    for layer in layers:
        reason = _layer_wellformedness(layers_root / layer.path)
        if reason:
            issues.append(f"{layer.specialist} layer {layer.path} {reason}")
        else:
            wellformed.append(layer)

    # Per-layer well-formedness isn't composability: two specialists can define the
    # same prim path with incompatible types, or dangle an override over a prim no
    # one defines, and the layers still open individually while the composed stage
    # is silently broken. Check the layers that passed the per-layer gate.
    issues.extend(_composition_conflicts(wellformed, layers_root))

    # style check: TODO call sidecar with brief.style_anchors + rendered preview
    # for now, accept if no other issues
    accepted = len(issues) == 0

    return ValidatorVerdict(accepted=accepted, issues=issues)


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


def _composition_conflicts(layers: list[LayerSpec], layers_root: Path) -> list[str]:
    """Cross-layer composition conflicts across the well-formed specialist layers.

    Structural scan of every layer's prim specs, fed to `_conflicts_from_specs`.
    A missing/unreadable layer is skipped — the well-formedness gate already
    flagged it, so it isn't re-reported here.
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
    return _conflicts_from_specs(tagged)


def _prim_specs(text: str) -> list[tuple[str, str, str]]:
    """Every prim spec in a USD text layer as ``(specifier, type_name, prim_path)``.

    Structural heuristic, no pxr: prim scopes are the ``{ }`` blocks at paren-depth
    zero. Dictionary braces in layer/prim metadata always sit inside ``( )``, so a
    paren-aware counter tells the two apart. Quoted strings (single ``'`` / double
    ``"`` / triple ``'''`` / ``\"\"\"``), ``@...@`` asset paths, and ``#`` comments
    are skipped whole, so a brace or keyword inside any of them is never read as
    structure. ``type_name`` is ``""`` for a typeless prim (``def "X"``), which
    carries no type opinion.
    """
    specs: list[tuple[str, str, str]] = []
    stack: list[str] = []  # ancestor prim names → current path
    pending: str | None = None  # a declared prim awaiting its opening brace
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
            stack.append(pending or "")
            pending = None
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
                specs.append((specifier, type_name, "/" + "/".join([*stack, name])))
                pending = name
                i = m.end()
            else:
                i += 1
    return specs


def _conflicts_from_specs(tagged: list[tuple[str, str, str, str]]) -> list[str]:
    """Composition conflicts from ``(specialist, specifier, type_name, prim_path)``.

    Two classes — the ones provable without resolving full USD composition:
      - a prim path *defined* (``def``/``class``, not ``over``) with two or more
        incompatible non-empty type names across specialists; the composed prim's
        type is ambiguous. Same-type redefinition is a legal opinion-merge and a
        typeless def carries no type opinion, so neither is flagged;
      - a *dangling override*: a path overridden by some specialist but defined by
        none, so the opinion composes onto nothing.

    Each issue names the conflicting specialists (sorted, for stable output) so the
    supervisor's ``_failing_specialist`` routes back to the pipeline-earliest. The
    prim path is included for diagnostics; because ``_failing_specialist`` substring-
    matches the whole issue, a path segment containing a pipeline-earlier specialist
    name (e.g. ``terrain_lod`` → terrain) can over-trigger route-back to that
    innocent node. That wastes its re-run but still replays the true culprits
    downstream (so a non-deterministic specialist can converge), and is bounded by
    the round cap. The clean fix — word-boundary matching in ``_failing_specialist``
    — lives in the supervisor, outside this gate.
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

    issues: list[str] = []
    for path in sorted(types_by_path):
        by_specialist = types_by_path[path]
        all_types = {t for types in by_specialist.values() for t in types}
        if len(all_types) > 1:
            who = ", ".join(sorted(by_specialist))
            issues.append(
                f"composition conflict: specialists {who} define the same prim "
                f"<{path}> with incompatible types {sorted(all_types)}"
            )

    defined = set(types_by_path)
    for path in sorted(overs_by_path):
        if path not in defined:
            who = ", ".join(sorted(overs_by_path[path]))
            issues.append(
                f"composition conflict: specialist {who} overrides prim <{path}> "
                f"but no specialist defines it (dangling override)"
            )
    return issues


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
