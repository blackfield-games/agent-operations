"""End-to-end test for the runtime CLI entry point.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_cli.py -v
"""

import json

from common.types import LayerSpec, ValidatorVerdict
from runtime import cli
from runtime.cli import main

# The 7 authoring specialists (validator emits no layer of its own).
AUTHORING = ("director", "terrain", "biome", "prop", "lighting", "npc", "optimization")


def test_main_runs_end_to_end(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)

    code = main(["--x", "42", "--y", "-17", "--biome", "scorched_grassland", "--out", "out"])

    assert code == 0  # validator accepted

    world = tmp_path / "out" / "world.usda"
    assert world.exists()
    text = world.read_text()
    # validator does not emit its own layer; the 7 authoring specialists do.
    assert "validator" not in text
    for specialist in ("director", "terrain", "biome", "prop", "lighting", "npc", "optimization"):
        assert specialist in text

    out = capsys.readouterr().out
    assert "r+0042_-0017_l0" in out
    assert "ACCEPTED" in out
    assert "7 emitted" in out
    # one validation round on the happy path (accepted on the first pass)
    assert "rounds:    1" in out
    # report lists every authoring specialist
    for specialist in ("director", "terrain", "biome", "prop", "lighting", "npc", "optimization"):
        assert specialist in out
    # report names the written world.usda (path as returned by compose_world)
    assert "out/world.usda" in out


def test_aesthetic_override(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)

    code = main(["--x", "1", "--y", "2", "--aesthetic", "neon-overgrown", "--out", "world_out"])

    assert code == 0
    director_layer = tmp_path / "layers" / "director" / "r+0001_+0002_l0.usda"
    assert director_layer.exists()
    assert "neon-overgrown" in director_layer.read_text()


def test_json_output(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)

    code = main(["--x", "1", "--y", "2", "--json", "--out", "out"])
    assert code == 0  # exit code is unchanged by --json

    report = json.loads(capsys.readouterr().out)
    assert report["region_id"] == "r+0001_+0002_l0"
    assert report["accepted"] is True
    assert isinstance(report["issues"], list)
    # rounds: number of validator passes; 1 on the happy path (accepted first round).
    assert isinstance(report["rounds"], int)
    assert report["rounds"] == 1

    # One layer per authoring specialist, each carrying specialist + summary.
    assert len(report["layers"]) == len(AUTHORING)
    assert {layer["specialist"] for layer in report["layers"]} == set(AUTHORING)
    assert all("summary" in layer for layer in report["layers"])

    # layer_counts: strongest-first (STRENGTH_ORDER minus the layerless validator),
    # one layer each.
    assert list(report["layer_counts"].keys()) == [
        "optimization", "lighting", "prop", "npc", "biome", "terrain", "director"
    ]
    assert all(count == 1 for count in report["layer_counts"].values())

    assert report["world"].endswith("out/world.usda")


def _stub_run_graph(result: dict):
    """Replacement for cli._run_graph that yields a fixed graph result, so main()'s
    verdict -> exit-code + report logic is tested without running the real
    LangGraph (covered end-to-end, including route-back, in test_supervisor.py)."""
    async def _run(brief):
        return result

    return _run


def test_main_exits_1_and_reports_rejected_on_validator_rejection(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    layers = [
        LayerSpec(specialist="terrain", region_id="r+0042_-0017_l0", path="terrain/a.usda", summary="scorched ridge", metrics={}),
    ]
    result = {
        "verdict": ValidatorVerdict(accepted=False, issues=["style score 0.41 < 0.60 threshold"]),
        "layers": layers,
        "rounds": 3,
    }
    monkeypatch.setattr(cli, "_run_graph", _stub_run_graph(result))

    code = main(["--x", "42", "--y", "-17", "--out", "out"])

    # Rejection -> non-zero exit so the CLI is usable as a CI gate.
    assert code == 1

    out = capsys.readouterr().out
    assert "REJECTED" in out
    assert "rounds:    3" in out
    # The issues block is only rendered when verdict.issues is non-empty.
    assert "issues:" in out
    assert "style score 0.41 < 0.60 threshold" in out
    # world.usda is still composed from the emitted layers even on rejection.
    assert (tmp_path / "out" / "world.usda").exists()


def test_json_output_reflects_rejection(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    layers = [
        LayerSpec(specialist="terrain", region_id="r+0001_+0002_l0", path="terrain/a.usda", summary="t", metrics={}),
    ]
    result = {
        "verdict": ValidatorVerdict(accepted=False, issues=["missing specialist layers: ['biome']"]),
        "layers": layers,
        "rounds": 2,
    }
    monkeypatch.setattr(cli, "_run_graph", _stub_run_graph(result))

    code = main(["--x", "1", "--y", "2", "--json", "--out", "out"])
    assert code == 1  # --json does not change the rejection exit code

    report = json.loads(capsys.readouterr().out)
    assert report["accepted"] is False
    assert report["issues"] == ["missing specialist layers: ['biome']"]
    assert report["rounds"] == 2
    assert report["region_id"] == "r+0001_+0002_l0"
