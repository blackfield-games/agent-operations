"""End-to-end test for the runtime CLI entry point.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_cli.py -v
"""

from pathlib import Path

from runtime.cli import main


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
