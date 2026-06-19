"""End-to-end test for the runtime CLI entry point.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_cli.py -v
"""

import json

import pytest

from common.types import LayerSpec, ValidatorVerdict
from runtime import cli
from runtime.cli import main
from runtime.poster import PostOutcome, PostResult

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

    # The seam: an accepted region surfaces coordinator-ready render jobs in the
    # exact POST /jobs body shape (kind as the snake_case wire string). The whole-
    # tile diffusion job leads, then one per renderable specialist layer the graph
    # authored (terrain/biome/npc/optimization -> the 4 non-diffusion kinds, by enum
    # order); director/lighting/prop carry no render kind.
    assert [job["kind"] for job in report["render_jobs"]] == [
        "diffusion_tile", "terrain", "foliage", "npc_tick", "optimization"
    ]
    job = report["render_jobs"][0]
    assert job["region"] == {"x": 1, "y": 2, "layer": 0}
    assert set(job) == {"kind", "region", "deadline_secs", "max_payout_wei", "inputs"}
    assert job["inputs"]["region_id"] == "r+0001_+0002_l0"


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
    # A rejected region emits no render jobs (it must not be dispatched for render).
    assert report["render_jobs"] == []


def test_missing_required_coord_exits_2():
    # --x and --y are required ints: argparse must exit(2) when either is absent,
    # never silently default the region coords. The CLI is a CI entrypoint, so a
    # silent default would render the wrong region (0,0) without failing the run.
    # _parse_args runs first in main(), so this exits before the graph is built.
    for argv in (["--y", "1"], ["--x", "1"]):
        with pytest.raises(SystemExit) as exc_info:
            main(argv)
        assert exc_info.value.code == 2


def test_layer_arg_flows_into_region_id():
    # A non-default --layer must reach the region_id (_lN), and --x/--y map to the
    # grid coords. Every other test uses the default layer 0; this locks that the
    # arg isn't dropped. Tests the arg->brief mapping directly, without the graph.
    brief = cli._build_brief(cli._parse_args(["--x", "3", "--y", "-4", "--layer", "2"]))
    assert brief.region.region_id == "r+0003_-0004_l2"
    assert (brief.region.x, brief.region.y, brief.region.layer) == (3, -4, 2)


# --- --post-to: ship an accepted region's render jobs to the coordinator ---

def _accepted_result(region_id="r+0042_-0017_l0"):
    return {
        "verdict": ValidatorVerdict(accepted=True, issues=[]),
        "layers": [
            LayerSpec(specialist="terrain", region_id=region_id, path="terrain/a.usda", summary="ridge", metrics={}),
        ],
        "rounds": 1,
    }


def _stub_post(monkeypatch, make_results, store=None):
    """Replace cli.post_jobs with a fake that records the call (token/base_url/jobs)
    and returns canned results, so the CLI wiring is tested without a coordinator
    (the transport itself is covered in test_poster.py)."""
    def fake(jobs, *, base_url, token, client):
        if store is not None:
            store.update(jobs=jobs, base_url=base_url, token=token)
        return make_results(jobs)
    monkeypatch.setattr(cli, "post_jobs", fake)


def _created(jobs, job_id="srv-7"):
    return [
        PostResult(j.region.region_id, j.kind.wire_name, PostOutcome.CREATED, status_code=201, job_id=job_id)
        for j in jobs
    ]


def test_post_to_ships_accepted_jobs_and_threads_token(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("COORDINATOR_INGEST_TOKEN", "tok-123")
    monkeypatch.setattr(cli, "_run_graph", _stub_run_graph(_accepted_result()))
    seen: dict = {}
    _stub_post(monkeypatch, _created, store=seen)

    code = main(["--x", "42", "--y", "-17", "--post-to", "http://coord:8080", "--out", "out"])

    assert code == 0
    assert seen["base_url"] == "http://coord:8080"
    assert seen["token"] == "tok-123"  # threaded from the env var
    # The one terrain layer fans out to the whole-tile diffusion job + a TERRAIN job.
    assert [job.kind.wire_name for job in seen["jobs"]] == ["diffusion_tile", "terrain"]
    out = capsys.readouterr().out
    assert "posted:    2/2 accepted" in out
    assert "id=srv-7" in out


def test_post_to_token_never_printed(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    secret = "TOPSECRET-ingest-9f3"
    monkeypatch.setenv("COORDINATOR_INGEST_TOKEN", secret)
    monkeypatch.setattr(cli, "_run_graph", _stub_run_graph(_accepted_result()))
    _stub_post(monkeypatch, _created)

    main(["--x", "42", "--y", "-17", "--post-to", "http://c", "--json", "--out", "out"])
    assert secret not in capsys.readouterr().out


def test_post_failure_returns_exit_2(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("COORDINATOR_INGEST_TOKEN", "t")
    monkeypatch.setattr(cli, "_run_graph", _stub_run_graph(_accepted_result()))
    _stub_post(
        monkeypatch,
        lambda jobs: [PostResult(jobs[0].region.region_id, jobs[0].kind.wire_name, PostOutcome.UNAVAILABLE, status_code=503)],
    )

    code = main(["--x", "42", "--y", "-17", "--post-to", "http://c", "--out", "out"])
    assert code == 3  # distinct from rejection (1) and argparse usage (2)


def test_no_post_to_does_not_post(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(cli, "_run_graph", _stub_run_graph(_accepted_result()))

    def boom(*a, **k):
        raise AssertionError("post_jobs must not run without --post-to")

    monkeypatch.setattr(cli, "post_jobs", boom)

    code = main(["--x", "42", "--y", "-17", "--out", "out"])
    assert code == 0
    assert "posted:" not in capsys.readouterr().out


def test_post_to_without_token_warns_and_posts_unauthenticated(tmp_path, monkeypatch, capsys, caplog):
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("COORDINATOR_INGEST_TOKEN", raising=False)
    monkeypatch.setattr(cli, "_run_graph", _stub_run_graph(_accepted_result()))
    seen: dict = {}
    _stub_post(monkeypatch, _created, store=seen)

    with caplog.at_level("WARNING"):
        main(["--x", "42", "--y", "-17", "--post-to", "http://c", "--out", "out"])
    assert seen["token"] is None  # empty env -> unauthenticated post
    assert "unauthenticated" in caplog.text


def test_post_to_rejected_region_posts_nothing(tmp_path, monkeypatch, capsys):
    # A rejected region emits no jobs, so --post-to must not reach the transport.
    monkeypatch.chdir(tmp_path)
    result = {
        "verdict": ValidatorVerdict(accepted=False, issues=["bad"]),
        "layers": [LayerSpec(specialist="terrain", region_id="r+0042_-0017_l0", path="terrain/a.usda", summary="t", metrics={})],
        "rounds": 1,
    }
    monkeypatch.setattr(cli, "_run_graph", _stub_run_graph(result))

    def boom(*a, **k):
        raise AssertionError("must not post a rejected region")

    monkeypatch.setattr(cli, "post_jobs", boom)

    code = main(["--x", "42", "--y", "-17", "--post-to", "http://c", "--out", "out"])
    assert code == 1  # rejection still dominates the exit code
    assert "posted:" not in capsys.readouterr().out


def test_post_to_json_includes_posted(tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("COORDINATOR_INGEST_TOKEN", "t")
    monkeypatch.setattr(cli, "_run_graph", _stub_run_graph(_accepted_result()))
    _stub_post(monkeypatch, lambda jobs: _created(jobs, job_id="J1"))

    code = main(["--x", "42", "--y", "-17", "--post-to", "http://c", "--json", "--out", "out"])
    assert code == 0
    report = json.loads(capsys.readouterr().out)
    assert "posted" in report
    assert report["posted"][0]["outcome"] == "created"
    assert report["posted"][0]["job_id"] == "J1"
    assert report["posted"][0]["region_id"] == "r+0042_-0017_l0"
