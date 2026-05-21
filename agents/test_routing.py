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
