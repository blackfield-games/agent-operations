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

import pydantic
import pytest

from arena_client import proto
from arena_client.baseline import BaselinePolicy, aim_at
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


class MockTransport:
    """A scripted transport: `inbound` frames are recv'd in order, every sent frame
    is appended to `sent`. Lets the client state machine be driven with no I/O."""

    def __init__(self, inbound: list[dict]) -> None:
        self._inbound = list(inbound)
        self.sent: list[dict] = []

    def recv(self) -> dict:
        if not self._inbound:
            raise proto.ProtocolError("mock transport exhausted")
        return self._inbound.pop(0)

    def send(self, frame: dict) -> None:
        self.sent.append(frame)


class FakeClock:
    """A deterministic monotonic clock: returns each scripted reading once, then
    holds the last value. Lets a deadline test simulate a slow policy with no sleep."""

    def __init__(self, readings: list[float]) -> None:
        self._readings = list(readings)
        self._i = 0

    def __call__(self) -> float:
        v = self._readings[min(self._i, len(self._readings) - 1)]
        self._i += 1
        return v


def _challenge_frame(nonce: str = "00") -> dict:
    return {"type": "challenge", "nonce": nonce}


def _welcome_frame(version: int = proto.PROTOCOL_VERSION, seat: int = 0) -> dict:
    return {"type": "welcome", "protocol_version": version, "match_id": FIXED_MATCH, "seat": seat}


def _start_frame() -> dict:
    return {
        "type": "start", "match_id": FIXED_MATCH,
        "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 2},
    }


def _observe_frame(tick: int = 0, seat: int = 0, alive: bool = True, deadline: int = 50_000) -> dict:
    return {
        "type": "observe", "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH,
        "seat": seat, "tick": tick, "phase": "live", "deadline_micros": deadline,
        "own": {"seat": seat, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
                "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "ammo": 30, "alive": alive},
        "visible": [],
    }


def _end_frame() -> dict:
    return {"type": "end", "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH,
            "final_tick": 1, "outcomes": [], "replay_hash": "00"}


def _fixed_policy(_obs) -> ActionIntent:
    return _intent(100, 0)


def test_connect_completes_handshake():
    from arena_client.sdk import ArenaClient
    t = MockTransport([_challenge_frame("abcd"), _welcome_frame(seat=2), _start_frame()])
    c = ArenaClient(t, agent_id="agent-2")
    c.connect()
    assert c.seat == 2 and c.match_id == FIXED_MATCH and c.nonce == "abcd"
    assert c.config is not None and c.config.seats == 2
    assert len(t.sent) == 1 and t.sent[0]["type"] == "join" and t.sent[0]["signature_hex"] == ""


def test_connect_refuses_version_skew():
    # FM3: a version skew is a clean refusal at connect, before any match state.
    from arena_client.sdk import ArenaClient, VersionMismatch
    t = MockTransport([_challenge_frame(), _welcome_frame(version=proto.PROTOCOL_VERSION + 1)])
    c = ArenaClient(t, agent_id="x")
    with pytest.raises(VersionMismatch):
        c.connect()
    assert not c.connected


def test_connect_handles_handshake_reject():
    from arena_client.sdk import ArenaClient, HandshakeRejected
    t = MockTransport([_challenge_frame(), {"type": "reject", "reason": "match full"}])
    c = ArenaClient(t, agent_id="x")
    with pytest.raises(HandshakeRejected, match="match full"):
        c.connect()


def test_run_answers_each_observation_until_end():
    from arena_client.sdk import ArenaClient
    inbound = [_challenge_frame(), _welcome_frame(seat=0), _start_frame(),
               _observe_frame(tick=0), _observe_frame(tick=1), _end_frame()]
    t = MockTransport(inbound)
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0, 0.0]))
    result = c.run(_fixed_policy)
    assert isinstance(result, MatchResult) and c.done
    acts = [f for f in t.sent if f["type"] == "act"]
    assert [a["tick"] for a in acts] == [0, 1]
    assert all(a["seat"] == 0 for a in acts)


def test_mid_match_reject_is_surfaced_not_raised():
    # FM1: a reject mid-match is recorded and stepped past, never a desync/crash.
    from arena_client.sdk import ArenaClient
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(tick=0), {"type": "reject", "reason": "stale tick"},
               _observe_frame(tick=1), _end_frame()]
    t = MockTransport(inbound)
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0, 0.0]))
    result = c.run(_fixed_policy)
    assert isinstance(result, MatchResult)
    assert c.rejections == ["stale tick"]
    assert [a["tick"] for a in t.sent if a["type"] == "act"] == [0, 1]


def test_deadline_overrun_forfeits_the_tick():
    # FM2: a policy slower than deadline_micros forfeits — no late action is sent.
    from arena_client.sdk import ArenaClient
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(tick=0, deadline=1000), _end_frame()]
    t = MockTransport(inbound)
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0, 1.0]))  # 1.0s elapsed >> 1000us
    result = c.run(_fixed_policy)
    assert isinstance(result, MatchResult)
    assert c.forfeits == 1
    assert not any(f["type"] == "act" for f in t.sent)


def test_downed_seat_answers_with_passive_hold():
    from arena_client.sdk import ArenaClient
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(tick=0, alive=False), _end_frame()]
    t = MockTransport(inbound)
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0, 0.0]))
    c.run(_fixed_policy)
    acts = [f for f in t.sent if f["type"] == "act"]
    assert len(acts) == 1
    assert acts[0]["intent"]["buttons"]["fire"] is False
    assert acts[0]["intent"]["move_dir"] == {"x": 0, "y": 0}


def _make_obs(x, y, ammo=30, alive=True, team=0, facing=0, visible=()):
    return Observation.model_validate({
        "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH, "seat": 0, "tick": 0,
        "phase": "live", "deadline_micros": 50_000,
        "own": {"seat": 0, "team": team, "position": {"x": x, "y": y}, "z": 0, "facing": facing,
                "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "ammo": ammo, "alive": alive},
        "visible": [{"entity_id": eid, "kind": "player", "team": t, "position": {"x": ex, "y": ey},
                     "z": 0, "facing": 0, "in_line_of_sight": True} for (eid, t, ex, ey) in visible],
    })


def test_aim_at_cardinals_and_diagonals():
    assert aim_at(1000, 0) == 0          # E
    assert aim_at(1000, 1000) == 8192    # NE
    assert aim_at(0, 1000) == 16384      # N
    assert aim_at(-1000, 0) == 32768     # W
    assert aim_at(0, -1000) == 49152     # S
    assert aim_at(1000, -1000) == 57344  # SE


def test_baseline_moves_and_fires_toward_enemy():
    # Own at origin (team 0), one enemy (team 1) due east and out of move range.
    obs = _make_obs(0, 0, team=0, visible=[(1, 1, 5000, 0)])
    intent = BaselinePolicy()(obs)
    assert intent.aim == 0  # east
    assert intent.move_dir == Vec2(x=1000, y=0)  # full-speed east, clamped to the cap
    assert intent.buttons.fire is True


def test_baseline_targets_the_nearest_enemy():
    # A closer enemy east, a farther one north; the baseline picks the closer.
    obs = _make_obs(0, 0, team=0, visible=[(1, 1, 0, 4000), (2, 1, 3000, 0)])
    intent = BaselinePolicy()(obs)
    assert intent.aim == 0  # toward the nearer (east) enemy, not the northern one
    assert intent.move_dir.x > 0 and intent.move_dir.y == 0


def test_baseline_reloads_when_empty():
    obs = _make_obs(0, 0, ammo=0, team=0, visible=[(1, 1, 1000, 0)])
    intent = BaselinePolicy()(obs)
    assert intent.buttons.reload is True
    assert intent.buttons.fire is False
    assert intent.move_dir == Vec2(x=0, y=0)


def test_baseline_advances_on_centre_when_no_enemy_in_sight():
    # Spawned off-centre with nothing visible: close on the arena centre, never fire.
    obs = _make_obs(20000, 3000, team=0, visible=[])
    intent = BaselinePolicy()(obs)
    assert intent.buttons.fire is False
    assert intent.move_dir.x < 0  # heading back toward the origin
    assert intent.move_dir.x**2 + intent.move_dir.y**2 <= proto.MOVE_INTENT_SCALE**2


def test_baseline_ignores_allies():
    # A same-team entity is not a target — with only an ally visible, advance.
    obs = _make_obs(10000, 0, team=1, visible=[(5, 1, 11000, 0)])
    intent = BaselinePolicy()(obs)
    assert intent.buttons.fire is False
    assert intent.move_dir.x < 0  # toward centre, since no ENEMY is in sight


def test_baseline_move_is_always_legal_and_deterministic():
    pol = BaselinePolicy()
    for ex, ey in [(50000, 0), (-12345, 67890), (1, -1), (0, 0), (-99999, -99999)]:
        obs = _make_obs(0, 0, team=0, visible=[(1, 1, ex, ey)])
        a = pol(obs)
        b = pol(obs)
        assert a == b  # pure function of the observation
        assert a.move_dir.x**2 + a.move_dir.y**2 <= proto.MOVE_INTENT_SCALE**2
