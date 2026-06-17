"""Unit tests for the validator route-back logic in the supervisor.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_routing.py -v
"""

from runtime.supervisor import _failing_specialist


def test_missing_layers_routes_to_earliest_missing():
    # validator phrasing: "missing specialist layers: ['biome', 'prop']"
    issue = "missing specialist layers: ['biome', 'prop']"
    assert _failing_specialist([issue]) == "biome"


def test_triangle_budget_routes_to_optimization():
    issue = "triangle budget exceeded — re-run optimization with stricter LODs"
    assert _failing_specialist([issue]) == "optimization"


def test_earliest_specialist_wins_across_issues():
    issues = [
        "triangle budget exceeded — re-run optimization with stricter LODs",
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
    # documented residual (FM2): a prim path segment *exactly* equal to a specialist
    # name still matches under word boundaries. A conflict named on biome + prop at a
    # prim literally </World/terrain> still routes to terrain (earliest) — the narrow,
    # accepted residual word boundaries do not eliminate.
    issue = (
        "composition conflict: specialists biome, prop define the same prim "
        "</World/terrain> with incompatible types ['Mesh', 'Xform']"
    )
    assert _failing_specialist([issue]) == "terrain"
