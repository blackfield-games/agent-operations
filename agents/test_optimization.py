"""Budget-resolving optimization: an over-budget world is LOD-shed back under the
triangle budget by a deterministic, convergent, heaviest-first policy that never
decimates the terrain floor or strips a layer to nothing — and the emitted layer's
over_budget verdict is consistent with the reduction it records (no number changed
in isolation). A world that still overflows once shed-able geometry is exhausted
stays terminally over budget rather than faking a resolution.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_optimization.py -v
"""

import re

import pytest

from common.compose import compose_world
from common.types import LayerSpec, RegionCoord, WorldBrief
from optimization import optimization
from validator import validator


def _layer(spec: str, tris: float, rid: str = "r0", path: str | None = None) -> LayerSpec:
    return LayerSpec(
        specialist=spec,
        region_id=rid,
        path=path or f"{spec}/{rid}.usda",
        summary="",
        metrics={"triangles": float(tris)},
    )


def _scalar(text: str, key: str) -> str:
    m = re.search(rf"\b{key}\s*=\s*([^\n]+)", text)
    assert m, f"emitted optimization layer has no {key}"
    return m.group(1).strip()


def _directives(text: str) -> list[dict]:
    """Each Lod_N child of LodDirectives as a dict of its custom attributes."""
    out = []
    for block in re.finditer(r'def Scope "Lod_\d+"\s*\{(.*?)\}', text, re.DOTALL):
        body = block.group(1)
        out.append(
            {
                "specialist": re.search(r'specialist\s*=\s*"([^"]*)"', body).group(1),
                "layer": re.search(r'layer\s*=\s*"([^"]*)"', body).group(1),
                "lodLevel": int(_scalar(body, "lodLevel")),
                "lodScale": float(_scalar(body, "lodScale")),
                "authoredTriangles": float(_scalar(body, "authoredTriangles")),
                "effectiveTriangles": float(_scalar(body, "effectiveTriangles")),
            }
        )
    return out


async def _run(prior: list[LayerSpec], tmp_path, monkeypatch, biome="forest", region=RegionCoord(x=0, y=0)):
    tmp_path.mkdir(parents=True, exist_ok=True)
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome=biome, region=region)
    layer = await optimization.run(brief, prior)
    text = (tmp_path / "layers" / layer.path).read_text()
    return layer, text


@pytest.mark.asyncio
async def test_under_budget_passes_through_untouched(tmp_path, monkeypatch):
    # An already-fitting world is reported under budget with NO shedding: no
    # directives, observed == authored, forcedLodCollapse false. The resolver must
    # not "act" on a world that needs no action.
    layer, text = await _run([_layer("terrain", 262144), _layer("biome", 400000)], tmp_path, monkeypatch)
    assert layer.metrics["over_budget"] == 0.0
    assert _directives(text) == []
    assert _scalar(text, "forcedLodCollapse") == "false"
    assert float(_scalar(text, "observedTriangles")) == float(_scalar(text, "authoredTriangles"))


@pytest.mark.asyncio
async def test_over_budget_world_is_resolved_by_shedding(tmp_path, monkeypatch):
    # The core unblock: a world over the raw budget is LOD-shed back under it, so the
    # verdict is RESOLVED (over_budget 0), not the old terminal fast-fail.
    prior = [_layer("terrain", 262144), _layer("biome", 4_000_000)]
    layer, text = await _run(prior, tmp_path, monkeypatch)
    assert sum(p.metrics["triangles"] for p in prior) > optimization.TRIANGLE_BUDGET  # raw over budget
    assert layer.metrics["over_budget"] == 0.0  # but resolved
    assert float(_scalar(text, "observedTriangles")) <= optimization.TRIANGLE_BUDGET
    sheds = _directives(text)
    assert [d["specialist"] for d in sheds] == ["biome"]  # the non-floor layer was shed
    assert sheds[0]["lodLevel"] >= 1


@pytest.mark.asyncio
async def test_emission_is_deterministic(tmp_path, monkeypatch):
    # FM2: same prior + brief -> byte-identical layer + identical metric across runs,
    # so a pipeline re-run reproduces the resolution exactly (no salted hash, no time).
    prior = [_layer("terrain", 262144), _layer("biome", 4_000_000)]
    a, text_a = await _run(prior, tmp_path / "a", monkeypatch)
    b, text_b = await _run(prior, tmp_path / "b", monkeypatch)
    assert text_a == text_b
    assert a.metrics == b.metrics


def test_shedding_is_heaviest_first_and_deterministic():
    # FM2 at the policy level: a multi-layer over-budget set sheds the same layers to
    # the same LOD levels every run, picking the heaviest current contributor first.
    prior = [
        _layer("terrain", 262144),
        _layer("biome", 3_000_000, path="biome/r0.usda"),
        _layer("prop", 2_500_000, path="prop/r0.usda"),
        _layer("npc", 900_000, path="npc/r0.usda"),
    ]
    r1 = optimization._resolve(prior)
    r2 = optimization._resolve(prior)
    assert r1 == r2
    _, effective, _, directives = r1
    assert effective <= optimization.TRIANGLE_BUDGET  # converged under budget
    by_spec = {d[0]: d[3] for d in directives}
    # the heavier authored layers are driven to deeper LODs than the lightest
    assert by_spec["biome"] >= by_spec["npc"]
    assert by_spec["prop"] >= by_spec["npc"]


def test_resolve_is_bounded_and_converges():
    # FM1: with many heavy shed-able layers the loop still terminates — passes are
    # bounded by len(sheddable) * MAX_LOD (and well under the hard cap), never spinning.
    prior = [_layer("terrain", 262144)] + [
        _layer("biome", 2_000_000, path=f"biome/r{i}.usda") for i in range(12)
    ]
    _, effective, passes, directives = optimization._resolve(prior)
    sheddable = 12
    assert passes <= sheddable * optimization.MAX_LOD
    assert passes < optimization.MAX_RESOLVE_PASSES
    # 12 layers * 2M shed to 1/8 = 3M floor + 262k terrain > budget -> terminal, but
    # bounded: every layer reached MAX_LOD and the loop stopped, it did not spin.
    assert all(d[3] == optimization.MAX_LOD for d in directives)
    assert effective > optimization.TRIANGLE_BUDGET  # genuinely unresolvable, reported as such


def test_each_pass_advances_exactly_one_lod_step():
    # FM1: every resolve pass strictly shrinks the total by collapsing one layer one
    # LOD — so the pass count equals the summed LOD increments and the final total is
    # below the initial. A pass that didn't strictly reduce would break this identity.
    prior = [
        _layer("terrain", 262144),
        _layer("biome", 5_000_000, path="biome/r0.usda"),
        _layer("prop", 4_000_000, path="prop/r0.usda"),
    ]
    _, effective, passes, directives = optimization._resolve(prior)
    assert effective < 262144 + 5_000_000 + 4_000_000
    assert passes == sum(d[3] for d in directives)  # one strict LOD step per pass


def test_terrain_floor_is_never_shed_even_when_over_budget():
    # FM4: the base terrain heightfield is load-bearing — resolving must reduce the
    # scatter, never the floor, so the walkable surface survives.
    _, _, _, directives = optimization._resolve([_layer("terrain", 1_000_000), _layer("biome", 4_000_000)])
    assert "terrain" not in {d[0] for d in directives}


@pytest.mark.asyncio
async def test_floor_alone_over_budget_is_terminal_not_a_false_green(tmp_path, monkeypatch):
    # FM3 + FM4: when the do-not-shed floor ALONE exceeds the budget, no amount of
    # shedding the scatter can fit it — the world must stay over_budget=1.0 (terminal),
    # not report a fake resolution. The scatter is still driven to its MAX_LOD floor.
    prior = [_layer("terrain", 2_000_000), _layer("biome", 4_000_000)]
    layer, text = await _run(prior, tmp_path, monkeypatch)
    assert layer.metrics["over_budget"] == 1.0  # genuinely unresolvable -> terminal
    assert float(_scalar(text, "observedTriangles")) > optimization.TRIANGLE_BUDGET
    sheds = _directives(text)
    assert {d["specialist"] for d in sheds} == {"biome"}  # terrain (floor) untouched
    assert sheds[0]["lodLevel"] == optimization.MAX_LOD  # scatter shed as far as allowed


@pytest.mark.asyncio
async def test_nothing_sheddable_over_budget_does_not_fake_resolve(tmp_path, monkeypatch):
    # FM3: a world whose only geometry is the floor and is over budget has NO shed-able
    # surface — it must report terminal with zero directives, never a silent no-op
    # that claims resolved while changing nothing.
    layer, text = await _run([_layer("terrain", 2_000_000)], tmp_path, monkeypatch)
    assert layer.metrics["over_budget"] == 1.0
    assert _directives(text) == []
    assert _scalar(text, "resolvePasses") == "0"


@pytest.mark.asyncio
async def test_shed_layer_floors_at_max_lod_never_to_zero(tmp_path, monkeypatch):
    # FM4: even a wildly over-budget scatter is never collapsed below 1/8 of its
    # authored triangles, so resolving can't strip the world to nothing to hit a number.
    layer, text = await _run([_layer("terrain", 262144), _layer("biome", 50_000_000)], tmp_path, monkeypatch)
    sheds = _directives(text)
    assert sheds[0]["lodLevel"] == optimization.MAX_LOD
    assert sheds[0]["effectiveTriangles"] == pytest.approx(50_000_000 / (1 << optimization.MAX_LOD))
    assert sheds[0]["effectiveTriangles"] > 0.0


@pytest.mark.asyncio
async def test_emitted_layer_is_self_consistent_no_phantom(tmp_path, monkeypatch):
    # No-phantom: the verdict must be reconstructable from the emitted layer alone —
    # observed == authored minus the reductions the directives record, over_budget
    # consistent with observed, and each directive internally exact. A number changed
    # without a matching recorded reduction would fail this.
    prior = [
        _layer("terrain", 262144),
        _layer("biome", 3_000_000, path="biome/r0.usda"),
        _layer("prop", 2_500_000, path="prop/r0.usda"),
    ]
    layer, text = await _run(prior, tmp_path, monkeypatch)
    authored = float(_scalar(text, "authoredTriangles"))
    observed = float(_scalar(text, "observedTriangles"))
    sheds = _directives(text)
    reductions = sum(d["authoredTriangles"] - d["effectiveTriangles"] for d in sheds)
    assert observed == pytest.approx(authored - reductions)
    assert layer.metrics["over_budget"] == float(observed > optimization.TRIANGLE_BUDGET)
    assert _scalar(text, "overBudget") == str(observed > optimization.TRIANGLE_BUDGET).lower()
    for d in sheds:
        assert 1 <= d["lodLevel"] <= optimization.MAX_LOD
        assert d["lodScale"] == pytest.approx(1.0 / (1 << d["lodLevel"]))
        assert d["effectiveTriangles"] == pytest.approx(d["authoredTriangles"] * d["lodScale"])


@pytest.mark.asyncio
async def test_resolved_layer_composes_and_validates(tmp_path, monkeypatch):
    # Integration: the resolved optimization layer (with its nested LodDirectives) is
    # well-formed, composes into world.usda, and the validator ACCEPTS it — the new
    # nested scopes don't trip the composition-conflict or dangling-override scan.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=0, y=0))
    layers = []
    for spec in ("director", "terrain", "biome", "prop", "lighting", "npc"):
        mod = __import__(spec, fromlist=[spec])
        got = await getattr(mod, spec).run(brief, layers)
        if got:
            layers.append(got)
    # force an over-budget world so the optimizer actually sheds, then validate
    layers = [ly for ly in layers if ly.specialist != "biome"]
    layers.append(_layer("biome", 4_000_000, rid=brief.region.region_id, path=f"biome/{brief.region.region_id}.usda"))
    # write the heavy biome layer to disk so well-formedness/composition can open it
    (tmp_path / "layers" / "biome").mkdir(parents=True, exist_ok=True)
    (tmp_path / "layers" / f"biome/{brief.region.region_id}.usda").write_text(
        '#usda 1.0\n(\n    defaultPrim = "Biome"\n)\n\ndef PointInstancer "Scatter"\n{\n    custom int instanceCount = 16667\n}\n'
    )
    opt = await optimization.run(brief, layers)
    layers.append(opt)
    assert opt.metrics["over_budget"] == 0.0  # resolved
    compose_world(layers, tmp_path / "world")
    verdict = await validator.run(brief, layers)
    assert verdict.accepted, verdict.issues


def test_unset_env_resolves_to_module_defaults(monkeypatch):
    # FM1 (default back-compat): with no override the resolvers return the module
    # constants verbatim, so the unconfigured pipeline is byte-identical to before the
    # override existed (and the existing monkeypatch.setattr(TRIANGLE_BUDGET) tests still
    # bind, since the resolver falls back to the module global).
    monkeypatch.delenv("BLACKFIELD_TRIANGLE_BUDGET", raising=False)
    monkeypatch.delenv("BLACKFIELD_GEOMETRY_FLOOR", raising=False)
    assert optimization._resolve_budget() == optimization.TRIANGLE_BUDGET == 1_500_000
    assert optimization._resolve_floor() == optimization.GEOMETRY_FLOOR == {"terrain"}


@pytest.mark.asyncio
async def test_unset_env_run_emits_default_budget(tmp_path, monkeypatch):
    # FM1: a run with no override emits the module-default budget into the layer.
    monkeypatch.delenv("BLACKFIELD_TRIANGLE_BUDGET", raising=False)
    _, text = await _run([_layer("terrain", 262144), _layer("biome", 400000)], tmp_path, monkeypatch)
    assert int(_scalar(text, "triangleBudget")) == optimization.TRIANGLE_BUDGET


@pytest.mark.asyncio
async def test_tighter_budget_override_now_sheds_a_world_that_fit(tmp_path, monkeypatch):
    # FM2: a world that fit untouched under the default budget is driven over (and shed)
    # by a tighter override — the env value, not the constant, gates the resolve.
    prior = [_layer("terrain", 262144), _layer("biome", 400000, path="biome/r0.usda")]
    monkeypatch.delenv("BLACKFIELD_TRIANGLE_BUDGET", raising=False)
    _, text_default = await _run(prior, tmp_path / "d", monkeypatch)
    assert _scalar(text_default, "forcedLodCollapse") == "false"  # fits under the default
    monkeypatch.setenv("BLACKFIELD_TRIANGLE_BUDGET", "500000")
    layer, text = await _run(prior, tmp_path / "t", monkeypatch)
    assert int(_scalar(text, "triangleBudget")) == 500000
    assert _scalar(text, "forcedLodCollapse") == "true"  # the tighter budget sheds it
    assert float(_scalar(text, "observedTriangles")) <= 500000
    assert layer.metrics["over_budget"] == 0.0


@pytest.mark.asyncio
async def test_looser_budget_override_now_fits_a_world_that_shed(tmp_path, monkeypatch):
    # FM2 (other direction): a world the default budget could only shed now fits
    # untouched under a looser override, observed == authored (no reduction).
    prior = [_layer("terrain", 262144), _layer("biome", 3_000_000, path="biome/r0.usda")]
    monkeypatch.delenv("BLACKFIELD_TRIANGLE_BUDGET", raising=False)
    _, text_default = await _run(prior, tmp_path / "d", monkeypatch)
    assert _scalar(text_default, "forcedLodCollapse") == "true"  # default must shed it
    monkeypatch.setenv("BLACKFIELD_TRIANGLE_BUDGET", "100000000")
    layer, text = await _run(prior, tmp_path / "l", monkeypatch)
    assert int(_scalar(text, "triangleBudget")) == 100000000
    assert _scalar(text, "forcedLodCollapse") == "false"  # fits untouched
    assert float(_scalar(text, "observedTriangles")) == 262144 + 3_000_000
    assert layer.metrics["over_budget"] == 0.0


@pytest.mark.parametrize("bad", ["abc", "0", "-5", "1.5", "   "])
def test_malformed_budget_override_falls_back_to_default(bad, monkeypatch):
    # FM3: a non-numeric/zero/negative/blank override is rejected back to the default
    # (warned, not raised) — never a 0 that sheds everything or an inf that sheds nothing.
    monkeypatch.setenv("BLACKFIELD_TRIANGLE_BUDGET", bad)
    assert optimization._resolve_budget() == optimization.TRIANGLE_BUDGET


def test_floor_override_is_honored_and_always_includes_terrain(monkeypatch):
    # FM4: an explicit floor set is honored, and terrain is ALWAYS in it even when the
    # override omits it (the walkable base is never decimable).
    monkeypatch.setenv("BLACKFIELD_GEOMETRY_FLOOR", "prop,biome")
    assert optimization._resolve_floor() == {"terrain", "prop", "biome"}
    monkeypatch.setenv("BLACKFIELD_GEOMETRY_FLOOR", "prop")  # omits terrain
    assert "terrain" in optimization._resolve_floor()


def test_floor_override_drops_unknown_specialists(monkeypatch):
    # FM4: a name not in the known specialist set is dropped (warned), not silently
    # admitted as a phantom floor entry that could never match a layer.
    monkeypatch.setenv("BLACKFIELD_GEOMETRY_FLOOR", "terrain,bogus_xyz")
    assert optimization._resolve_floor() == {"terrain"}


@pytest.mark.asyncio
async def test_floored_specialist_is_not_shed(tmp_path, monkeypatch):
    # FM4 (integration): a specialist named in the floor override is load-bearing — never
    # decimated — even when flooring it makes the world terminal. Proves the floor
    # override actually moves a layer out of the shed-able set.
    monkeypatch.setenv("BLACKFIELD_TRIANGLE_BUDGET", "500000")
    monkeypatch.setenv("BLACKFIELD_GEOMETRY_FLOOR", "terrain,biome")
    prior = [_layer("terrain", 262144), _layer("biome", 1_000_000, path="biome/r0.usda")]
    layer, text = await _run(prior, tmp_path, monkeypatch)
    assert "biome" not in {d["specialist"] for d in _directives(text)}  # floored, never shed
    assert layer.metrics["over_budget"] == 1.0  # floor alone over budget -> terminal
