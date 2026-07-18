"""Lighting honors the director's intent:beats: a region's mood line drives an additive
atmospheric fog Volume, while the DECISIONS.md-locked overcast+inferno-rim COLOR palette
stays byte-identical. With no director (or no recognized beat) lighting falls back to the
pre-beats fog-less palette, never raising on a director hiccup.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_lighting.py -v
"""

import re

import pytest

from common.types import LayerSpec, RegionCoord, WorldBrief
from director import director
from lighting import lighting
from terrain import terrain
from validator import validator

def _density_from_usd(text: str) -> float:
    m = re.search(r"inputs:density\s*=\s*([\d.]+)", text)
    assert m, "emitted lighting USD has no Atmosphere density"
    return float(m.group(1))


@pytest.mark.asyncio
async def test_director_beats_drive_atmospheric_fog(tmp_path, monkeypatch):
    # The director->lighting flow: the director's intent:beats produce an Atmosphere
    # Volume whose density is the accumulated recognized-beat fog, tagged with the beats
    # that drove it. director intent now SHAPES lighting, not just a fixed palette.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    dl = await director.run(brief, [])
    layer = await lighting.run(brief, [dl])
    text = (tmp_path / "layers" / layer.path).read_text()
    recognized = lighting._recognized_beats(lighting._beats_from_director([dl]))
    assert recognized, "director beats carry no recognized lighting beat"
    assert 'def Volume "Atmosphere"' in text
    assert _density_from_usd(text) == float(f"{lighting._fog_density(recognized):.2f}")
    assert f'drivenBy = "{",".join(recognized)}"' in text
    assert layer.summary.endswith(f"fog {lighting._fog_density(recognized):.2f}")


def test_locked_palette_matches_decisions_md():
    # The single authoritative pin of the DECISIONS.md-locked values: lighting.run and the
    # validator's palette gate both re-derive from these constants, so a fat-finger to a
    # color/intensity/angle would pass every relationship test — this asserts the bytes
    # themselves. The one place the literal values live in a test (replacing the old
    # hand-copied LOCKED_SKY/LOCKED_SUN that shadowed the inline producer literals).
    assert lighting.LOCKED_SKY == (
        '    def DomeLight "Sky"\n'
        "    {\n"
        "        float inputs:intensity = 0.4\n"
        "        color3f inputs:color = (0.55, 0.58, 0.62)\n"
        '        custom string mood = "overcast"\n'
        "    }"
    )
    assert lighting.LOCKED_SUN == (
        '    def DistantLight "Sun"\n'
        "    {\n"
        "        float inputs:intensity = 1.2\n"
        "        color3f inputs:color = (1.0, 0.62, 0.32)\n"
        "        float inputs:angle = 0.53\n"
        '        custom string mood = "inferno_rim"\n'
        "    }"
    )
    assert lighting.PALETTE_BLOCK == f"{lighting.LOCKED_SKY}\n{lighting.LOCKED_SUN}"


@pytest.mark.asyncio
async def test_locked_palette_unchanged_only_fog_added(tmp_path, monkeypatch):
    # FM1 (palette lock): beats add ONLY the Atmosphere volume — the overcast dome and
    # inferno-rim sun (color, intensity, angle) are byte-identical with and without beats.
    # The DECISIONS.md color lock holds while the atmosphere reads the director's intent. The
    # locked blocks are re-derived from lighting's OWN constants (single source), not a copy.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    dl = await director.run(brief, [])
    with_beats = (tmp_path / "layers" / (await lighting.run(brief, [dl])).path).read_text()
    no_beats = (tmp_path / "layers" / (await lighting.run(brief, [])).path).read_text()
    for block in (lighting.LOCKED_SKY, lighting.LOCKED_SUN):
        assert block in with_beats, "beats changed the locked palette"
        assert block in no_beats
    assert 'def Volume "Atmosphere"' in with_beats
    assert "Atmosphere" not in no_beats
    # the palette prefix (everything up to the Xform close) is byte-identical — the fog
    # is appended after the Sun, never spliced into the dome/sun opinions.
    prefix = no_beats[: no_beats.rindex("\n}\n")]
    assert with_beats.startswith(prefix)


@pytest.mark.asyncio
async def test_director_absent_falls_back_byte_identical(tmp_path, monkeypatch):
    # FM2 (back-compat): with no director layer in prior, lighting emits exactly the
    # fog-less palette it did before the beats flow existed — byte-identical whether prior
    # is empty or carries non-director layers, and carrying no Atmosphere prim.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    terr = await terrain.run(brief, [])
    a = (tmp_path / "layers" / (await lighting.run(brief, [])).path).read_text()
    b = (tmp_path / "layers" / (await lighting.run(brief, [terr])).path).read_text()
    assert a == b
    assert "Atmosphere" not in a


@pytest.mark.asyncio
async def test_beats_less_director_degrades_to_fallback(tmp_path, monkeypatch):
    # FM2: a director layer carrying no intent:beats (a pre-intent record) parses to "" ->
    # the fog-less palette, byte-identical to the no-director render, not a raise that
    # would drop the lighting layer. The sibling of biome's must_not-less degrade.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    rel = f"director/{brief.region.region_id}.usda"
    dpath = tmp_path / "layers" / rel
    dpath.parent.mkdir(parents=True, exist_ok=True)
    dpath.write_text('#usda 1.0\ndef Scope "Director"\n{\n}\n')
    stub = LayerSpec(specialist="director", region_id=brief.region.region_id, path=rel, summary="no intent")
    assert lighting._beats_from_director([stub]) == ""
    usd = (tmp_path / "layers" / (await lighting.run(brief, [stub])).path).read_text()
    assert "Atmosphere" not in usd
    plain = await lighting.run(brief, [])  # overwrites the same path with the no-director render
    assert usd == (tmp_path / "layers" / plain.path).read_text()


def test_missing_director_file_degrades_to_empty(tmp_path, monkeypatch):
    # FM2: a director LayerSpec whose file is absent must degrade to "" (-> fog-less
    # palette), never raise — a raise would isolate lighting as an empty sublayer in the
    # supervisor, dropping the region's whole lighting rig over a director hiccup.
    monkeypatch.chdir(tmp_path)
    stub = LayerSpec(
        specialist="director", region_id="r+0000_+0000_l0", path="director/missing.usda", summary="gone"
    )
    assert lighting._beats_from_director([stub]) == ""


def test_unrecognized_beats_contribute_no_fog():
    # FM4 (vocabulary): a beats line of words lighting doesn't model yields no recognized
    # beat and zero fog; only a known token drives the haze, an empty line never does, and
    # a recognized token is still picked up amid unknown prose.
    assert lighting._recognized_beats("pristine. serene. untouched.") == []
    assert lighting._fog_density([]) == 0.0
    assert lighting._recognized_beats("") == []
    assert lighting._recognized_beats("a calm, smoke-wreathed ruin") == ["smoke"]


@pytest.mark.asyncio
async def test_director_beats_carry_a_recognized_lighting_beat(tmp_path, monkeypatch):
    # FM4 (contract): the director's emitted intent:beats actually carries a token lighting
    # reads as fog, so the director->lighting flow can never silently stop firing if the
    # director's vocabulary drifts. The sibling of the npc roster and biome cap-token pins.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=2, y=4))
    dl = await director.run(brief, [])
    assert lighting._recognized_beats(lighting._beats_from_director([dl]))


@pytest.mark.asyncio
async def test_hostile_beats_inject_no_prim(tmp_path, monkeypatch):
    # FM3 (injection): a director beats line carrying a USD quote/newline must not break
    # lighting's parse or forge a prim. lighting READS beats but never echoes them — only
    # vocab-controlled tokens reach the layer — so a hostile mood line yields at most the
    # intended fog, never an injected prim the validator would misattribute.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=1, y=2))
    rel = f"director/{brief.region.region_id}.usda"
    dpath = tmp_path / "layers" / rel
    dpath.parent.mkdir(parents=True, exist_ok=True)
    hostile = 'scorched"\n    }\n    def Mesh "Injected"\n    {\n        custom int n = 1'
    dpath.write_text(f'#usda 1.0\ndef Scope "Director"\n{{\n    custom string intent:beats = "{hostile}"\n}}\n')
    stub = LayerSpec(specialist="director", region_id=brief.region.region_id, path=rel, summary="hostile")
    text = (tmp_path / "layers" / (await lighting.run(brief, [stub])).path).read_text()
    prims = {path for _, _, path in validator._prim_specs(text)}
    assert prims <= {"/Lighting", "/Lighting/Sky", "/Lighting/Sun", "/Lighting/Atmosphere"}
    assert not any("Injected" in p for p in prims)


def test_fog_accumulates_and_clamps_to_ceiling():
    # FM5 (ceiling): contributions accumulate, but several heavy beats clamp to
    # FOG_DENSITY_MAX rather than exceeding a physical density; a single light beat stays
    # below the ceiling and equals its table contribution exactly.
    light = lighting._fog_density(lighting._recognized_beats("abandoned"))
    assert light == lighting.BEAT_FOG_DENSITY["abandoned"]
    assert light < lighting.FOG_DENSITY_MAX
    raw = sum(lighting.BEAT_FOG_DENSITY[t] for t in ("smoke", "conflict", "storm", "ash"))
    assert raw > lighting.FOG_DENSITY_MAX  # the sum really does exceed the ceiling
    assert lighting._fog_density(lighting._recognized_beats("smoke. conflict. storm. ash.")) == lighting.FOG_DENSITY_MAX


def test_recognized_beats_deduped_and_order_invariant():
    # FM3 (determinism): a repeated beat word and word order never change the result —
    # recognized beats are deduped and returned in table order, so the fog is byte-stable.
    assert lighting._recognized_beats("smoke smoke smoke") == ["smoke"]
    assert lighting._recognized_beats("conflict. scorched.") == lighting._recognized_beats("scorched. conflict.")
    assert lighting._recognized_beats("storm scorched") == ["scorched", "storm"]  # table order, not input order


@pytest.mark.asyncio
async def test_beats_emission_is_deterministic(tmp_path, monkeypatch):
    # FM3: the same brief + director yields byte-identical foggy USD across runs, so a
    # pipeline re-run (and a validation-revision re-run) reproduces the fog too — not just
    # the fog-less palette.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=5, y=9))
    dl = await director.run(brief, [])
    a = (tmp_path / "layers" / (await lighting.run(brief, [dl])).path).read_text()
    b = (tmp_path / "layers" / (await lighting.run(brief, [dl])).path).read_text()
    assert a == b
    assert 'def Volume "Atmosphere"' in a  # the director path actually fogged
