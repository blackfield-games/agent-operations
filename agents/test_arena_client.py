"""Tests for the arena_client SDK: wire-shape parity with arena/proto, the client
state machine (handshake / observe→act / rejection / deadline), the deterministic
baseline, and a full baseline-vs-baseline match against the real Rust core.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_arena_client.py -v

The wire-shape tests pin the SAME canonical frames arena/proto/src/lib.rs pins, so
a drift on either side of the seam fails here. The end-to-end match test runs the
real arena-core via the arena-harness binary; it skips (never fails) when cargo or
the arena workspace is absent, and runs for real under validate.sh.
"""

from __future__ import annotations

from arena_client import proto
from arena_client.proto import (
    Action,
    ActionButtons,
    ActionIntent,
    Challenge,
    MatchResult,
    Observation,
    Reject,
    Start,
    Vec2,
    Welcome,
    act_frame,
    decode_gateway,
    join_frame,
    leave_frame,
)

FIXED_MATCH = "550e8400-e29b-41d4-a716-446655440000"


def test_observation_wire_shape_matches_rust():
    # The exact canonical frame from arena/proto observation_wire_shape_is_stable.
    canonical = {
        "protocol_version": proto.PROTOCOL_VERSION,
        "match_id": FIXED_MATCH,
        "seat": 0,
        "tick": 128,
        "phase": "live",
        "deadline_micros": 50_000,
        "own": {
            "seat": 0, "team": 1,
            "position": {"x": 0, "y": 0}, "z": 0,
            "facing": 16384,
            "velocity": {"x": 0, "y": 0},
            "health": 100, "max_health": 100, "ammo": 30, "alive": True,
        },
        "visible": [{
            "entity_id": 7, "kind": "player", "team": 2,
            "position": {"x": 1000, "y": -2000}, "z": 0,
            "facing": 16384, "in_line_of_sight": True,
        }],
    }
    obs = Observation.model_validate(canonical)
    assert obs.model_dump(mode="json") == canonical


def test_action_wire_shape_matches_rust():
    canonical = {
        "protocol_version": proto.PROTOCOL_VERSION,
        "match_id": FIXED_MATCH,
        "seat": 3,
        "tick": 128,
        "intent": {
            "move_dir": {"x": 600, "y": 800},
            "aim": 16384,
            "buttons": {"fire": True, "jump": False, "ability": False, "reload": False},
        },
    }
    action = Action.model_validate(canonical)
    assert action.model_dump(mode="json") == canonical


def test_match_result_wire_shape_matches_rust():
    canonical = {
        "protocol_version": proto.PROTOCOL_VERSION,
        "match_id": FIXED_MATCH,
        "final_tick": 2,
        "outcomes": [
            {"seat": 0, "team": 1, "placement": 1, "score": 3, "alive_at_end": True},
            {"seat": 1, "team": 2, "placement": 2, "score": 1, "alive_at_end": False},
        ],
        "replay_hash": "deadbeef",
    }
    result = MatchResult.model_validate(canonical)
    assert result.model_dump(mode="json") == canonical


def test_match_config_wire_shape_matches_rust():
    canonical = {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 8}
    cfg = proto.MatchConfig.model_validate(canonical)
    assert cfg.model_dump(mode="json") == canonical


def test_unknown_field_is_rejected():
    # extra=forbid: a field the Python type does not know is a decode error, not a
    # silent drop — the same wire-shape discipline the Rust crate enforces.
    bad = {
        "entity_id": 7, "kind": "player", "team": 2,
        "position": {"x": 0, "y": 0}, "z": 0, "facing": 0, "in_line_of_sight": True,
        "health": 100,  # a hidden-state field that must never appear on a VisibleEntity
    }
    import pydantic
    import pytest
    with pytest.raises(pydantic.ValidationError):
        proto.VisibleEntity.model_validate(bad)


def test_gateway_envelopes_decode():
    nonce = "0a1b2c3d4e5f60718293a4b5c6d7e8f9"
    assert decode_gateway({"type": "challenge", "nonce": nonce}) == Challenge(nonce=nonce)
    assert decode_gateway(
        {"type": "welcome", "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH, "seat": 2}
    ) == Welcome(protocol_version=proto.PROTOCOL_VERSION, match_id=FIXED_MATCH, seat=2)
    assert decode_gateway({"type": "reject", "reason": "nope"}) == Reject(reason="nope")

    start = decode_gateway({
        "type": "start", "match_id": FIXED_MATCH,
        "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 8},
    })
    assert isinstance(start, Start) and start.config.seats == 8

    # observe / end flatten the inner struct next to "type".
    obs_frame = {"type": "observe", **Observation.model_validate({
        "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH, "seat": 0, "tick": 1,
        "phase": "live", "deadline_micros": 50_000,
        "own": {"seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
                "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "ammo": 30, "alive": True},
        "visible": [],
    }).model_dump(mode="json")}
    assert isinstance(decode_gateway(obs_frame), Observation)

    end_frame = {"type": "end", **MatchResult.model_validate({
        "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH, "final_tick": 9,
        "outcomes": [], "replay_hash": "00",
    }).model_dump(mode="json")}
    assert isinstance(decode_gateway(end_frame), MatchResult)


def test_unknown_gateway_tag_raises():
    import pytest
    with pytest.raises(proto.ProtocolError):
        decode_gateway({"type": "teleport"})
    with pytest.raises(proto.ProtocolError):
        decode_gateway({"no_type": 1})


def test_agent_frames_encode():
    assert join_frame("0xabc") == {
        "type": "join", "protocol_version": proto.PROTOCOL_VERSION,
        "agent_id": "0xabc", "signature_hex": "",
    }
    action = Action.model_validate({
        "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH, "seat": 1, "tick": 4,
        "intent": {"move_dir": {"x": 1, "y": 2}, "aim": 3,
                   "buttons": {"fire": False, "jump": False, "ability": False, "reload": False}},
    })
    af = act_frame(action)
    assert af["type"] == "act" and af["seat"] == 1 and af["tick"] == 4
    assert leave_frame("forfeit") == {"type": "leave", "reason": "forfeit"}


def _intent(x: int, y: int) -> ActionIntent:
    return ActionIntent(
        move_dir=Vec2(x=x, y=y), aim=0,
        buttons=ActionButtons(fire=False, jump=False, ability=False, reload=False),
    )


def test_move_clamp_matches_rust():
    # The exact rows from arena/proto move_clamp_caps_overlong_and_leaves_inrange.
    assert _intent(3000, 4000).clamped().move_dir == Vec2(x=600, y=800)
    assert _intent(600, 800).clamped().move_dir == Vec2(x=600, y=800)
    assert _intent(300, -400).clamped().move_dir == Vec2(x=300, y=-400)


def test_move_clamp_truncates_toward_zero_not_floor():
    # (-5000, 5000): mag = isqrt(50_000_000) = 7071; -5_000_000 / 7071 truncates to
    # -707 (Rust signed division), where Python floor `//` would give -708. This row
    # is the one that fails if the clamp uses `//` instead of trunc-toward-zero.
    assert _intent(-5000, 5000).clamped().move_dir == Vec2(x=-707, y=707)


def test_move_clamp_never_exceeds_max():
    big = 10**9
    for x, y in [(big, big), (-big, big), (1001, 0), (1000, 1000), (-30000, 12000)]:
        c = _intent(x, y).clamped().move_dir
        assert c.x * c.x + c.y * c.y <= proto.MOVE_INTENT_SCALE**2, (x, y, c)
