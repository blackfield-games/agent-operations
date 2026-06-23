"""Director intent emission: the per-region faction roster is deterministic and
region-variable, drawn only from the combat-faction vocabulary, and written into the
layer's intent:factions verbatim so npc can read the seeded roster back.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_director.py -v
"""

import re

import pytest

from common.types import RegionCoord, WorldBrief
from director import director


def _factions_from_usd(text: str) -> list[str]:
    m = re.search(r'intent:factions\s*=\s*"([^"]*)"', text)
    assert m, "emitted director USD has no intent:factions"
    return m.group(1).split(",") if m.group(1) else []


def _must_not_from_usd(text: str) -> list[str]:
    m = re.search(r'intent:must_not\s*=\s*"([^"]*)"', text)
    assert m, "emitted director USD has no intent:must_not"
    return m.group(1).split(",") if m.group(1) else []


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
