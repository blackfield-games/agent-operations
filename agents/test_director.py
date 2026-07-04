"""Director intent emission: the per-region faction roster AND mood beats are
deterministic and region-variable, drawn only from their fixed vocabularies, and written
into the layer's intent:factions / intent:beats so npc reads the seeded roster back and
lighting reads the seeded mood back as atmospheric fog. Every beat phrase anchors a token
lighting models, so every region drives fog (the director<->lighting contract).

Run from the agents/ dir:
    .venv/bin/python -m pytest test_director.py -v
"""

import re

import pytest

from common.types import RegionCoord, WorldBrief
from director import director
from lighting import lighting


def _factions_from_usd(text: str) -> list[str]:
    m = re.search(r'intent:factions\s*=\s*"([^"]*)"', text)
    assert m, "emitted director USD has no intent:factions"
    return m.group(1).split(",") if m.group(1) else []


def _must_not_from_usd(text: str) -> list[str]:
    m = re.search(r'intent:must_not\s*=\s*"([^"]*)"', text)
    assert m, "emitted director USD has no intent:must_not"
    return m.group(1).split(",") if m.group(1) else []


def _beats_from_usd(text: str) -> str:
    m = re.search(r'intent:beats\s*=\s*"([^"]*)"', text)
    assert m, "emitted director USD has no intent:beats"
    return m.group(1)


def test_roster_is_deterministic_no_hash_salt():
    # hashlib, not the per-process-salted builtin hash(), so the roster survives a
    # fresh interpreter — a pipeline re-run reproduces the layer byte-for-byte.
    rid = RegionCoord(x=3, y=7).region_id
    assert director._faction_roster(rid) == director._faction_roster(rid)


def test_roster_size_and_membership():
    # Each region fields exactly ROSTER_SIZE distinct factions, all from the combat
    # vocabulary, returned sorted (a canonical layer npc can parse stably).
    for x in range(12):
        roster = director._faction_roster(RegionCoord(x=x, y=0).region_id)
        assert len(roster) == director.ROSTER_SIZE
        assert len(set(roster)) == director.ROSTER_SIZE  # distinct, no repeats
        assert set(roster) <= set(director.FACTION_ARCHETYPES)
        assert roster == sorted(roster)


def test_factions_are_parse_safe_identifiers():
    # npc parses intent:factions with a simple quoted-CSV regex that only round-trips
    # bare identifiers: a name carrying a comma, quote, backslash, or whitespace would
    # be split or truncated on read (usd_str escapes a quote to \" and the capture stops
    # there). Pin the vocabulary parse-safe so a future faction can't silently corrupt
    # the director→npc hand-off — the assumption the regex relies on, enforced.
    assert all(re.fullmatch(r"[a-z][a-z_]*", f) for f in director.FACTION_ARCHETYPES)


def test_distinct_regions_can_field_distinct_rosters():
    # A constant roster would leave npc one effective choice everywhere; the director's
    # intent must actually vary region to region.
    rosters = {tuple(director._faction_roster(RegionCoord(x=x, y=0).region_id)) for x in range(16)}
    assert len(rosters) > 1


def test_full_vocabulary_is_reachable():
    # No faction is dead: every archetype in the vocabulary surfaces in some region's
    # roster across a sweep, so the seeded intent exercises the whole set.
    seen: set[str] = set()
    for x in range(-8, 9):
        for y in range(-8, 9):
            seen.update(director._faction_roster(RegionCoord(x=x, y=y).region_id))
    assert seen == set(director.FACTION_ARCHETYPES)


@pytest.mark.asyncio
async def test_emitted_layer_carries_the_roster(tmp_path, monkeypatch):
    # The roster the helper computes is exactly what lands in intent:factions, so npc
    # parses back the same set the director seeded.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    layer = await director.run(brief, [])
    text = (tmp_path / "layers" / layer.path).read_text()
    assert _factions_from_usd(text) == director._faction_roster(brief.region.region_id)


@pytest.mark.asyncio
async def test_emitted_layer_carries_parse_safe_must_not(tmp_path, monkeypatch):
    # biome parses intent:must_not with the same quoted-CSV regex npc uses for factions,
    # so the seeded constraint must be non-empty and parse-safe bare identifiers — a token
    # carrying a comma/quote/whitespace would be split or truncated on read, silently
    # corrupting the director->biome cap hand-off. Pin the vocabulary parse-safe at the
    # source, the sibling of test_factions_are_parse_safe_identifiers.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    layer = await director.run(brief, [])
    tokens = _must_not_from_usd((tmp_path / "layers" / layer.path).read_text())
    assert tokens  # a non-empty constraint
    assert all(re.fullmatch(r"[a-z][a-z_]*", t) for t in tokens)


@pytest.mark.asyncio
async def test_emission_is_deterministic(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    a = await director.run(brief, [])
    usd_a = (tmp_path / "layers" / a.path).read_text()
    b = await director.run(brief, [])
    usd_b = (tmp_path / "layers" / b.path).read_text()
    assert usd_a == usd_b


def test_region_beats_deterministic_no_hash_salt():
    # hashlib, not the per-process-salted builtin hash(), so the per-region mood survives a
    # fresh interpreter — a pipeline re-run reproduces the layer and every downstream beats
    # consumer (lighting fog) byte-for-byte. The beats sibling of the roster salt pin.
    rid = RegionCoord(x=3, y=7).region_id
    assert director._region_beats(rid) == director._region_beats(rid)


def test_beats_size_and_membership():
    # Each region fields exactly BEATS_SIZE distinct beats, all from the mood vocabulary,
    # returned sorted (a canonical layer lighting reads back stably).
    for x in range(12):
        beats = director._region_beats(RegionCoord(x=x, y=0).region_id)
        assert len(beats) == director.BEATS_SIZE
        assert len(set(beats)) == director.BEATS_SIZE  # distinct, no repeats
        assert set(beats) <= set(director.BEAT_VOCAB)
        assert beats == sorted(beats)


def test_beats_vocab_is_parse_safe_prose():
    # beats is interpolated into the layer and read back via intent:beats "([^"]*)"; a
    # phrase carrying a quote/newline would truncate the literal or the capture, and a comma
    # is reserved as the CSV separator the other intents use. Pin the vocabulary bare
    # lowercase prose (words single-spaced) so the free-form line round-trips and tokenizes
    # cleanly — the beats sibling of test_factions_are_parse_safe_identifiers.
    assert all(re.fullmatch(r"[a-z]+( [a-z]+)*", b) for b in director.BEAT_VOCAB)


def test_distinct_regions_field_distinct_beats():
    # A constant beats line would render identical fog everywhere; the director's mood must
    # actually vary region to region — the whole point of this slice.
    moods = {tuple(director._region_beats(RegionCoord(x=x, y=0).region_id)) for x in range(16)}
    assert len(moods) > 1


def test_full_beat_vocabulary_is_reachable():
    # No mood phrase is dead: every beat in the vocabulary surfaces in some region's mood
    # across a sweep, so the seeded intent exercises the whole set.
    seen: set[str] = set()
    for x in range(-8, 9):
        for y in range(-8, 9):
            seen.update(director._region_beats(RegionCoord(x=x, y=y).region_id))
    assert seen == set(director.BEAT_VOCAB)


def test_every_vocab_phrase_anchors_a_recognized_lighting_beat():
    # The structural guarantee at the source: each BEAT_VOCAB phrase tokenizes to at least
    # one lighting-recognized token, so ANY non-empty per-region subset drives fog. The
    # contract can't break by an unlucky region selection — only by adding an unanchored
    # phrase to the vocab, caught HERE.
    for phrase in director.BEAT_VOCAB:
        assert lighting._recognized_beats(phrase), f"vocab phrase {phrase!r} anchors no recognized lighting beat"


def test_every_region_carries_a_recognized_lighting_beat():
    # The director<->lighting contract, swept: lighting only fogs a region whose beats carry
    # a token it models. A varied vocab that selected only unmodeled words for some region
    # would silently stop that region's fog. Pin that EVERY region's emitted beats line
    # carries >=1 recognized lighting beat — the sweep-wide sibling of lighting's
    # single-region test_director_beats_carry_a_recognized_lighting_beat.
    for x in range(-8, 9):
        for y in range(-8, 9):
            beats = director._region_beats(RegionCoord(x=x, y=y).region_id)
            line = ". ".join(beats) + "."
            assert lighting._recognized_beats(line), f"region ({x},{y}) emits no recognized lighting beat"


@pytest.mark.asyncio
async def test_emitted_layer_carries_the_region_beats(tmp_path, monkeypatch):
    # The beats the helper computes are exactly what lands in intent:beats (joined as the
    # prose mood line with a trailing period), so lighting reads back the same mood the
    # director seeded — the beats sibling of test_emitted_layer_carries_the_roster.
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    layer = await director.run(brief, [])
    line = _beats_from_usd((tmp_path / "layers" / layer.path).read_text())
    assert line == ". ".join(director._region_beats(brief.region.region_id)) + "."


@pytest.mark.asyncio
async def test_customlayerdata_is_byte_identical_for_a_safe_brief(tmp_path, monkeypatch):
    # aesthetic/biome/region_id now route through usd_str like intent:beats/factions, but an
    # injection-free brief must serialize to exactly the bytes it did before — usd_str is
    # transparent for a value with no USD-significant char, so the default brief is unchanged.
    # This is the regression floor the escaping fix must not churn (compose/pipeline goldens).
    monkeypatch.chdir(tmp_path)
    brief = WorldBrief(biome="forest", region=RegionCoord(x=3, y=7))
    layer = await director.run(brief, [])
    text = (tmp_path / "layers" / layer.path).read_text()
    assert '        string aesthetic = "scorched-modern, post-conflict, cinematic"' in text
    assert '        string biome = "forest"' in text
    assert '        string region_id = "r+0003_+0007_l0"' in text


@pytest.mark.asyncio
async def test_free_form_brief_fields_cannot_forge_a_prim(tmp_path, monkeypatch):
    # aesthetic and biome are FREE-FORM brief text — unlike the fixed factions/beats
    # vocabularies that test_*_are_parse_safe pin at the source — so they must be escaped at
    # emission or a quote+newline closes the customLayerData literal and the trailing text
    # parses as prims, forging a `def` the validator attributes to the director's intent
    # (the exact prim-forgery usd_str's docstring names). Pin that the injectable fields route
    # through usd_str: the attack survives only as one escaped literal, never a real prim.
    monkeypatch.chdir(tmp_path)
    attack = 'forest"\n    }\n)\ndef Scope "Injected"\n{\n    custom string owned = "'
    brief = WorldBrief(aesthetic=attack, biome=attack, region=RegionCoord(x=3, y=7))
    layer = await director.run(brief, [])
    text = (tmp_path / "layers" / layer.path).read_text()
    # Only the director's own prim survives at column 0 — nothing broke out of the header.
    prims = [ln for ln in text.splitlines() if re.match(r"(def|over|class)\s", ln)]
    assert prims == ['def Scope "Director"']
    # The attack text is present, but only as an escaped literal (" -> \", newline -> \n).
    assert 'def Scope "Injected"' not in text
    assert '\\"Injected\\"' in text
    assert "\\n" in text
