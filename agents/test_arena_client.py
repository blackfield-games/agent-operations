"""Tests for the arena_client SDK: wire-shape parity with arena/proto, the client
state machine (handshake / observe→act / rejection / deadline), the deterministic
baseline, and a full baseline-vs-baseline match against the real Rust core.

Run from the agents/ dir:
    .venv/bin/python -m pytest test_arena_client.py -v

The wire-shape tests pin the SAME canonical frames arena/proto/src/lib.rs pins, so
a drift on either side of the seam fails here. The end-to-end match test runs the
real arena-core via the arena-harness binary. A plain local `pytest` without the
Rust toolchain skips it; CI and validate.sh build the harness first and set
ARENA_E2E_REQUIRED, which turns a missing harness into a hard failure so a broken
A2A path can never pass CI by silently skipping.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

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
            "health": 100, "max_health": 100, "shield": 40, "ammo": 30, "cooldown": 5, "dash_cooldown": 0, "alive": True,
        },
        "visible": [{
            "entity_id": 7, "kind": "player", "team": 2,
            "position": {"x": 1000, "y": -2000}, "z": 0,
            "facing": 16384, "in_line_of_sight": True,
        }],
    }
    obs = Observation.model_validate(canonical)
    assert obs.model_dump(mode="json") == canonical


def test_self_state_carries_cooldown_but_visible_entity_does_not():
    # FM3 parity: the Rust SelfState emits the seat's own fire cooldown, so the
    # model must accept it — and require it, since the wire always carries it, so a
    # Rust/Python drift fails loud instead of silently defaulting.
    own = {
        "seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
        "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
        "ammo": 30, "cooldown": 3, "dash_cooldown": 0, "alive": True,
    }
    assert proto.SelfState.model_validate(own).cooldown == 3
    with pytest.raises(pydantic.ValidationError):
        proto.SelfState.model_validate({k: v for k, v in own.items() if k != "cooldown"})

    # FM2 parity bound: cooldown is private HUD state — VisibleEntity must reject it
    # (extra=forbid mirrors the Rust wire-pin exclusion), so an enemy's fire timing
    # is never readable off another pawn.
    with pytest.raises(pydantic.ValidationError):
        proto.VisibleEntity.model_validate({
            "entity_id": 1, "kind": "player", "team": 2,
            "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
            "in_line_of_sight": True, "cooldown": 3,
        })


def test_self_state_carries_shield_but_visible_entity_does_not():
    # FM2 parity bound: shield is private-HUD armor state. SelfState REQUIRES it (the
    # Rust wire always carries it, so a drift fails loud instead of silently
    # defaulting), and VisibleEntity must REJECT it (extra=forbid mirrors the Rust
    # wire-pin exclusion), so an enemy's remaining mitigation is never an x-ray.
    own = {
        "seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
        "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 40,
        "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "alive": True,
    }
    assert proto.SelfState.model_validate(own).shield == 40
    with pytest.raises(pydantic.ValidationError):
        proto.SelfState.model_validate({k: v for k, v in own.items() if k != "shield"})
    with pytest.raises(pydantic.ValidationError):
        proto.VisibleEntity.model_validate({
            "entity_id": 1, "kind": "player", "team": 2,
            "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
            "in_line_of_sight": True, "shield": 40,
        })


def test_self_state_carries_dash_cooldown_but_visible_entity_does_not():
    # FM3 parity bound: the dash cooldown is private-HUD readiness like the fire
    # cooldown. SelfState REQUIRES it (the Rust wire always carries it, so a drift
    # fails loud instead of silently defaulting), and VisibleEntity must REJECT it
    # (extra=forbid mirrors the Rust wire-pin exclusion), so an enemy's dash
    # readiness is never an x-ray.
    own = {
        "seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
        "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
        "ammo": 30, "cooldown": 0, "dash_cooldown": 7, "alive": True,
    }
    assert proto.SelfState.model_validate(own).dash_cooldown == 7
    with pytest.raises(pydantic.ValidationError):
        proto.SelfState.model_validate({k: v for k, v in own.items() if k != "dash_cooldown"})
    with pytest.raises(pydantic.ValidationError):
        proto.VisibleEntity.model_validate({
            "entity_id": 1, "kind": "player", "team": 2,
            "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
            "in_line_of_sight": True, "dash_cooldown": 7,
        })


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


def test_out_of_domain_fields_are_rejected():
    # Integer fields mirror the Rust wire width and match_id is UUID-shaped, so a
    # value the server cannot hold fails here with a parity error rather than
    # panicking the Rust harness on deserialize.
    with pytest.raises(pydantic.ValidationError):
        proto.ActionIntent.model_validate({  # aim is u16; 70000 overflows it
            "move_dir": {"x": 0, "y": 0}, "aim": 70_000,
            "buttons": {"fire": False, "jump": False, "ability": False, "reload": False},
        })
    with pytest.raises(pydantic.ValidationError):
        proto.SelfState.model_validate({  # seat is u8
            "seat": -1, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
            "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0, "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "alive": True,
        })
    with pytest.raises(pydantic.ValidationError):
        proto.Welcome.model_validate(  # match_id must be a UUID
            {"protocol_version": proto.PROTOCOL_VERSION, "match_id": "not-a-uuid", "seat": 0}
        )


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
                "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0, "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "alive": True},
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
                "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0, "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "alive": alive},
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


def test_deadline_not_enforced_still_answers():
    # The loopback path disables enforcement (no wall-clock; the harness blocks for
    # one frame per seat). A slow policy must then still answer — never drop a frame.
    from arena_client.sdk import ArenaClient
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(tick=0, deadline=1000), _end_frame()]
    t = MockTransport(inbound)
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0, 1.0]), enforce_deadline=False)
    c.run(_fixed_policy)
    assert c.forfeits == 0
    assert [a["tick"] for a in t.sent if a["type"] == "act"] == [0]


def test_forfeit_does_not_livelock_the_next_tick_acts():
    # FM3: a dropped (late) action must not wedge the agent — the receive loop
    # resumes and the next observation is answered normally. obs0's policy runs over
    # budget (forfeit); obs1's runs within budget (sent).
    from arena_client.sdk import ArenaClient
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(tick=0, deadline=1000), _observe_frame(tick=1, deadline=1000),
               _end_frame()]
    t = MockTransport(inbound)
    # obs0: 0.0 -> 1.0 (1s elapsed >> 1000us, forfeit); obs1: 1.0 -> 1.0 (0 elapsed, on time).
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0, 1.0, 1.0, 1.0]))
    c.run(_fixed_policy)
    assert c.forfeits == 1
    assert [a["tick"] for a in t.sent if a["type"] == "act"] == [1], (
        "the dropped tick sends nothing, but the next tick still acts"
    )


def test_deadline_is_relative_to_the_observation_not_a_fixed_threshold():
    # FM2: enforcement is measured against each observation's own deadline_micros,
    # not a constant. The SAME 1s policy time is on time under a 2s budget (sent) but
    # late under a 1ms budget (forfeit) — a fixed threshold could not produce this
    # split, so this pins enforcement to the observation's deadline field.
    from arena_client.sdk import ArenaClient
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(tick=0, deadline=2_000_000), _observe_frame(tick=1, deadline=1000),
               _end_frame()]
    t = MockTransport(inbound)
    # obs0: 0.0 -> 1.0 (1s elapsed < 2s budget, sent); obs1: 1.0 -> 2.0 (1s elapsed >> 1ms, forfeit).
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0, 1.0, 1.0, 2.0]))
    c.run(_fixed_policy)
    assert c.forfeits == 1
    assert [a["tick"] for a in t.sent if a["type"] == "act"] == [0], (
        "the large-deadline tick is sent; the same elapsed forfeits under the small deadline"
    )


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
                "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0, "ammo": ammo, "cooldown": 0, "dash_cooldown": 0, "alive": alive},
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
    # A far enemy east, a closer one north; the baseline picks the closer and aims
    # NORTH (not east) — so a constant/east aim would fail this, not pass by chance.
    obs = _make_obs(0, 0, team=0, visible=[(1, 1, 5000, 0), (2, 1, 0, 3000)])
    intent = BaselinePolicy()(obs)
    assert intent.aim == 16384  # north, toward the nearer enemy
    assert intent.move_dir.y > 0 and intent.move_dir.x == 0


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


def _arena_harness() -> str | None:
    """The built arena-harness binary, or None (skip) when cargo / the arena
    workspace is unavailable. Builds on demand if cargo is present so a plain
    `pytest` is self-sufficient; under validate.sh the arena build already produced
    it, so this just locates it."""
    arena = Path(__file__).resolve().parent.parent / "arena"
    for profile in ("release", "debug"):
        candidate = arena / "target" / profile / "arena-harness"
        if candidate.exists():
            return str(candidate)
    if shutil.which("cargo") and (arena / "Cargo.toml").exists():
        subprocess.run(
            ["cargo", "build", "-q", "-p", "arena-harness", "--manifest-path", str(arena / "Cargo.toml")],
            check=True,
        )
        candidate = arena / "target" / "debug" / "arena-harness"
        if candidate.exists():
            return str(candidate)
    return None


def _require_or_skip_harness() -> str:
    """Resolve the arena-harness binary. CI and validate.sh build it first and set
    ARENA_E2E_REQUIRED, so a missing harness there is a hard failure — the A2A path
    must stay continuously exercised, never skipped. A plain local `pytest` without
    the Rust toolchain leaves the flag unset and skips."""
    harness = _arena_harness()
    if harness is not None:
        return harness
    reason = "arena-harness unavailable (needs cargo + the arena workspace)"
    if os.environ.get("ARENA_E2E_REQUIRED"):
        pytest.fail(f"{reason}; ARENA_E2E_REQUIRED is set so the e2e match must run")
    pytest.skip(reason)


def test_baseline_vs_baseline_runs_a_real_decisive_deterministic_match():
    # The headline of arena-03: a full agent-vs-agent match through the SDK against
    # the real arena-02 core, gradeable and reproducible. Required (fails, not skips)
    # under CI/validate.sh; skips only on a plain local pytest without the toolchain.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    seed = 12345
    match_id = "11111111-2222-4333-8444-555555555555"
    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    results = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id)

    assert set(results) == {0, 1}
    r0, r1 = results[0], results[1]
    assert r0 == r1, "every seat sees the same canonical result"
    assert r0.match_id == match_id
    assert isinstance(r0, MatchResult)

    # A real, decisive duel: it ended before the tick cap and exactly one seat is
    # alive (the seat-order tie-break makes a symmetric match decisive, not a draw).
    assert 0 < r0.final_tick < 3600
    alive = [o for o in r0.outcomes if o.alive_at_end]
    assert len(alive) == 1
    assert alive[0].placement == 1 and alive[0].score > 0
    assert len(r0.replay_hash) == 64  # keccak256, lowercase hex

    # Determinism: the same seed + match id re-runs byte-for-byte, replay hash incl.
    again = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id)
    assert again[0] == r0
    assert again[0].replay_hash == r0.replay_hash

    # A different seed perturbs the opening, so the match is genuinely seed-driven
    # (not a fixed script): the replay hash differs.
    other = run_local_match(harness, [0, 1], policies, seed=seed + 1, match_id=match_id)
    assert other[0].replay_hash != r0.replay_hash


def test_e2e_match_is_required_in_ci_not_skipped(monkeypatch):
    # FM1: with the harness absent but ARENA_E2E_REQUIRED set (CI / validate.sh),
    # the e2e match MUST be a hard failure, never a silent skip — else a broken A2A
    # path passes CI unnoticed.
    monkeypatch.setattr(sys.modules[__name__], "_arena_harness", lambda: None)

    monkeypatch.setenv("ARENA_E2E_REQUIRED", "1")
    try:
        _require_or_skip_harness()
    except pytest.skip.Exception:
        pytest.fail("required harness was skipped, not failed (FM1 regression)")
    except pytest.fail.Exception:
        pass
    else:
        pytest.fail("a missing required harness must raise")

    # Unset (a plain local pytest with no Rust toolchain): skip for dev ergonomics.
    monkeypatch.delenv("ARENA_E2E_REQUIRED", raising=False)
    with pytest.raises(pytest.skip.Exception):
        _require_or_skip_harness()


def test_subprocess_gateway_reaps_the_harness_when_the_match_body_raises():
    # FM3: a failure mid-match must not leak the harness subprocess. The gateway is a
    # context manager whose __exit__ closes stdin then waits/kills, so the process is
    # reaped even when the with-body raises. A portable dummy stdin-reader stands in
    # for the harness (no cargo needed), so this guard runs everywhere.
    from arena_client.sdk import SubprocessGateway

    proc = None
    with pytest.raises(RuntimeError):
        with SubprocessGateway([sys.executable, "-c", "import sys; sys.stdin.read()"]) as gw:
            proc = gw._proc
            assert proc.poll() is None  # alive inside the block
            raise RuntimeError("match body blew up")
    assert proc is not None
    assert proc.poll() is not None  # reaped on __exit__, not leaked


def test_subprocess_gateway_watchdog_kills_a_silent_harness():
    # FM2 / anti-hang: readiness is the first frame on the pipe, awaited by a blocking
    # readline (never a sleep), and a watchdog Timer kills a harness that never speaks
    # so a wedged handshake fails loudly instead of blocking forever. A dummy that
    # sleeps without writing stands in for a hung harness.
    from arena_client.sdk import GatewayClosed, SubprocessGateway

    with SubprocessGateway([sys.executable, "-c", "import time; time.sleep(30)"], timeout=0.5) as gw:
        start = time.monotonic()
        with pytest.raises(GatewayClosed):
            gw.recv(0)
        # The 0.5s watchdog must fire — not the dummy's 30s sleep. The wide bound
        # keeps it non-flaky while still failing loudly if the watchdog is removed.
        assert time.monotonic() - start < 10.0
