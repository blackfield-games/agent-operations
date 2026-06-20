"""Unit tests for the validator route-back logic in the supervisor.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_routing.py -v
"""

from common.types import ValidatorVerdict
from runtime.supervisor import _failing_specialist, _route_back_target


def test_missing_layers_routes_to_earliest_missing():
    # validator phrasing: "missing specialist layers: ['biome', 'prop']"
    issue = "missing specialist layers: ['biome', 'prop']"
    assert _failing_specialist([issue]) == "biome"


def test_missing_metric_routes_to_optimization():
    # The scanner's route-to-optimization branch keys on a RE-RUNNABLE optimization
    # issue — a missing required metric (the optimizer must re-emit it). over_budget
    # itself no longer routes here: it is a terminal rejection that names no
    # specialist (see the validator gate + the terminal tests below), so it never
    # reaches this text scan.
    issue = "optimization layer layers/optimization.usda is missing the required metric 'over_budget' its role must emit"
    assert _failing_specialist([issue]) == "optimization"


def test_earliest_specialist_wins_across_issues():
    issues = [
        "optimization layer layers/optimization.usda is missing the required metric 'over_budget' its role must emit",
        "missing specialist layers: ['terrain']",
    ]
    # terrain is upstream of optimization → repair the cause first
    assert _failing_specialist(issues) == "terrain"


def test_unattributable_failure_falls_back_to_director():
    # a pure style rejection that names no specialist
    assert _failing_specialist(["style cosine similarity 0.61 below threshold 0.72"]) == "director"


def test_empty_issues_falls_back_to_director():
    assert _failing_specialist([]) == "director"


def test_wellformedness_issue_routes_to_named_specialist():
    # validator phrasing: "{specialist} layer {path} {reason}". The leading name is
    # the culprit; word boundaries must still match it in the existing format.
    issue = "biome layer layers/biome.usda declares no defaultPrim"
    assert _failing_specialist([issue]) == "biome"


def test_conflict_embedded_path_segment_routes_to_named_culprit():
    # composition-conflict phrasing names biome + prop as the true culprits but
    # embeds a prim path whose segment "terrain_lod" contains the pipeline-earlier
    # name "terrain". Substring matching misrouted to terrain (an innocent upstream
    # node); word boundaries route to the earliest *named* culprit, biome.
    issue = (
        "composition conflict: specialists biome, prop define the same prim "
        "</World/terrain_lod/Lamp> with incompatible types ['Mesh', 'Xform']"
    )
    assert _failing_specialist([issue]) == "biome"


def test_dangling_override_routes_to_named_specialist_not_path_segment():
    # dangling-override phrasing names prop; the path segment "director_cam" must not
    # pull route-back to the pipeline-earliest director.
    issue = (
        "composition conflict: specialist prop overrides prim "
        "</World/director_cam/Rig> but no specialist defines it (dangling override)"
    )
    assert _failing_specialist([issue]) == "prop"


def test_embedded_substring_names_do_not_trigger_routeback():
    # propeller/npcs/terrain_lod each embed a specialist name but are not that
    # specialist (`_` and trailing letters are word chars, so the boundary fails).
    # No name matches → unattributable → director fallback, not the embedded node.
    issues = [
        "mesh propeller exceeds the poly budget",
        "spawned npcs clip through terrain_lod meshes",
    ]
    assert _failing_specialist(issues) == "director"


def test_exact_path_segment_equal_to_specialist_still_routes():
    # The text scan's documented residual: a prim path segment *exactly* equal to a
    # specialist name still matches under word boundaries, so the pure text scan over
    # a conflict named on biome + prop at a prim literally </World/terrain> routes to
    # terrain (earliest). _route_back_target eliminates this end-to-end by preferring
    # the structured attribution (see test_structured_attribution_* below); this pins
    # that the FALLBACK scan still behaves as documented when no structured field.
    issue = (
        "composition conflict: specialists biome, prop define the same prim "
        "</World/terrain> with incompatible types ['Mesh', 'Xform']"
    )
    assert _failing_specialist([issue]) == "terrain"


# ---- _route_back_target: structured attribution preferred over the text scan ----


def _verdict(
    issues: list[str], failing: list[str] | None = None, terminal: bool = False
) -> ValidatorVerdict:
    kwargs = {"accepted": False, "issues": issues, "terminal": terminal}
    if failing is not None:
        kwargs["failing_specialists"] = failing
    return ValidatorVerdict(**kwargs)


def test_structured_attribution_overrides_text_scan_residual():
    # FM1: the exact-path-segment residual, fixed. A conflict at a prim literally
    # </World/terrain> authored by biome + prop. The text scan still misroutes to the
    # innocent terrain; the structured attribution names the real authors, so route-
    # back goes to biome (pipeline-earliest). The two assertions together prove the
    # structured field is what flips the result.
    issue = (
        "composition conflict: specialists biome, prop define the same prim "
        "</World/terrain> with incompatible types ['Mesh', 'Xform']"
    )
    verdict = _verdict([issue], failing=["biome", "prop"])
    assert _failing_specialist(verdict.issues) == "terrain"  # residual, still present
    assert _route_back_target(verdict) == "biome"  # structured kills it


def test_structured_attribution_picks_pipeline_earliest():
    # List order is irrelevant; the earliest in PIPELINE_ORDER wins.
    verdict = _verdict(["whatever"], failing=["prop", "biome", "lighting"])
    assert _route_back_target(verdict) == "biome"


def test_fieldless_verdict_falls_back_to_text_scan():
    # FM2: a verdict with no structured attribution (older/synthesized validator)
    # routes via the word-boundary text scan exactly as before.
    verdict = _verdict(["missing specialist layers: ['prop']"])  # field defaults to []
    assert verdict.failing_specialists == []
    assert _route_back_target(verdict) == "prop"


def test_empty_structured_list_falls_back_to_text_scan():
    # An explicitly-empty field is the same as absent → text scan.
    verdict = _verdict(["biome layer layers/biome.usda declares no defaultPrim"], failing=[])
    assert _route_back_target(verdict) == "biome"


def test_unknown_attributed_name_falls_back_to_director():
    # FM3: the field is present but names only non-route-back targets (the validator
    # itself). Ignore the unknowns; with nothing known left, route to director — NOT
    # the text-scan result, even though the text names a real specialist (terrain).
    verdict = _verdict(
        ["terrain layer layers/terrain.usda declares no defaultPrim"],
        failing=["validator"],
    )
    assert _failing_specialist(verdict.issues) == "terrain"  # text would say terrain
    assert _route_back_target(verdict) == "director"  # present-but-unknown → director


def test_unknown_names_ignored_but_known_one_still_routes():
    # A mix of known + unknown: drop the unknowns, route to the earliest known.
    verdict = _verdict(["x"], failing=["validator", "prop"])
    assert _route_back_target(verdict) == "prop"


def test_structured_and_text_disagree_structured_wins():
    # FM4: the text names biome, the structured field names prop. The contract is
    # that structured wins, with no ambiguity.
    verdict = _verdict(
        ["biome layer layers/biome.usda declares no defaultPrim"],
        failing=["prop"],
    )
    assert _failing_specialist(verdict.issues) == "biome"
    assert _route_back_target(verdict) == "prop"


# ---- _route_back_target: terminal verdicts END when nothing fixable remains ----

# An over-budget world's real issue text names "optimization" (it explains the
# budget was exceeded AFTER optimization), so a text scan over it WOULD route back
# to the optimizer. The terminal flag is what stops that futile re-run.
OVER_BUDGET_ISSUE = (
    "triangle budget exceeded — the world is over the triangle budget after "
    "optimization (the terminal LOD authority); a re-run recomputes the same "
    "result, so it must shed source geometry rather than be revised"
)


def test_terminal_verdict_with_no_fixable_ends():
    # FM1: a pure over-budget rejection — terminal, no failing specialist. Re-running
    # the deterministic optimizer can't help, so route-back is None (the supervisor
    # ENDs in one pass instead of looping to MAX_ROUNDS).
    verdict = _verdict([OVER_BUDGET_ISSUE], failing=[], terminal=True)
    assert _route_back_target(verdict) is None


def test_terminal_flag_is_what_ends_it_not_the_text():
    # Discriminator for the test above: the SAME issue text without the terminal flag
    # still falls to the text scan, which routes to optimization (the word is in the
    # text). So it is `terminal`, not the wording, that converts the loop into an END.
    routed = _verdict([OVER_BUDGET_ISSUE], failing=[], terminal=False)
    assert _route_back_target(routed) == "optimization"
    ended = _verdict([OVER_BUDGET_ISSUE], failing=[], terminal=True)
    assert _route_back_target(ended) is None


def test_terminal_verdict_repairs_co_occurring_fixable_first():
    # FM2: terminal does NOT short-circuit a fixable issue. A terminal over-budget
    # verdict that also names a re-runnable biome routes back to biome — the fixable
    # cause is repaired before the loop can ever reach the terminal END.
    verdict = _verdict([OVER_BUDGET_ISSUE, "missing specialist layers: ['biome']"], failing=["biome"], terminal=True)
    assert _route_back_target(verdict) == "biome"


def test_terminal_verdict_with_only_unknown_attributed_ends():
    # A terminal verdict naming only non-route-back targets (the validator itself) has
    # no fixable specialist left, so it ENDs — unlike the NON-terminal present-but-
    # unknown case, which falls back to director (asserted alongside to prove terminal
    # is what flips the branch).
    assert _route_back_target(_verdict(["x"], failing=["validator"], terminal=True)) is None
    assert _route_back_target(_verdict(["x"], failing=["validator"], terminal=False)) == "director"
