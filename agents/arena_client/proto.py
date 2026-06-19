"""Arena Gateway wire types — the Python mirror of `arena/proto` (arena-01).

These are the exact serde-JSON shapes the Rust Gateway emits and accepts, so a
Python agent built on this SDK is wire-compatible with the headless reference
arena (and, later, the UE5 server) without a second protocol definition. The
field names, the `snake_case` enums, and the internally-tagged `type` envelopes
match `arena/proto/src/lib.rs` byte-for-byte; `test_arena_client.py` pins the
same canonical frames the Rust crate pins, so a drift on either side breaks loud.

Like the Rust side, every spatial quantity is integer fixed-point — no floats on
the wire — so the same match hashes identically on every platform. `extra=forbid`
makes an unexpected field a decode error rather than a silent drop: within a
`PROTOCOL_VERSION` the shape is fixed, and a new field means a version bump.
"""

from __future__ import annotations

from math import isqrt
from typing import Literal

from pydantic import BaseModel, ConfigDict

PROTOCOL_VERSION = 1
POSITION_SCALE = 1000
VELOCITY_SCALE = 1000
MOVE_INTENT_SCALE = 1000

Phase = Literal["lobby", "starting", "live", "ended"]
EntityKindName = Literal["player", "projectile", "pickup"]


class _Wire(BaseModel):
    model_config = ConfigDict(extra="forbid")


class Vec2(_Wire):
    x: int
    y: int


class ActionButtons(_Wire):
    fire: bool
    jump: bool
    ability: bool
    reload: bool


def _trunc_div(a: int, b: int) -> int:
    """Integer division truncating toward zero — Rust's `/` on signed ints, NOT
    Python's floor `//`. The two diverge on a negative numerator with a remainder
    (`-7 // 2 == -4` but Rust `-7 / 2 == -3`), which would desync the clamp from
    the server for a move toward negative coordinates."""
    q = abs(a) // abs(b)
    return -q if (a < 0) != (b < 0) else q


class ActionIntent(_Wire):
    move_dir: Vec2
    aim: int
    buttons: ActionButtons

    def clamped(self) -> ActionIntent:
        """The anti-god-mode move clamp, mirroring `ActionIntent::clamped` in
        arena-proto exactly (integer `isqrt`, truncate toward zero) so a baseline
        can emit an already-legal move that the server passes through unchanged.
        An in-range request is returned untouched; an overlong one is scaled down
        to magnitude `MOVE_INTENT_SCALE`, never up."""
        x, y = self.move_dir.x, self.move_dir.y
        mag_sq = x * x + y * y
        max_sq = MOVE_INTENT_SCALE * MOVE_INTENT_SCALE
        if mag_sq <= max_sq:
            move_dir = self.move_dir
        else:
            mag = max(isqrt(mag_sq), 1)
            move_dir = Vec2(
                x=_trunc_div(x * MOVE_INTENT_SCALE, mag),
                y=_trunc_div(y * MOVE_INTENT_SCALE, mag),
            )
        return ActionIntent(move_dir=move_dir, aim=self.aim, buttons=self.buttons)


class Action(_Wire):
    protocol_version: int
    match_id: str
    seat: int
    tick: int
    intent: ActionIntent


class SelfState(_Wire):
    seat: int
    team: int
    position: Vec2
    z: int
    facing: int
    velocity: Vec2
    health: int
    max_health: int
    ammo: int
    alive: bool


class VisibleEntity(_Wire):
    entity_id: int
    kind: EntityKindName
    team: int
    position: Vec2
    z: int
    facing: int
    in_line_of_sight: bool


class Observation(_Wire):
    protocol_version: int
    match_id: str
    seat: int
    tick: int
    phase: Phase
    deadline_micros: int
    own: SelfState
    visible: list[VisibleEntity]


class SeatOutcome(_Wire):
    seat: int
    team: int
    placement: int
    score: int
    alive_at_end: bool


class MatchResult(_Wire):
    protocol_version: int
    match_id: str
    final_tick: int
    outcomes: list[SeatOutcome]
    replay_hash: str


class MatchConfig(_Wire):
    tick_hz: int
    max_ticks: int
    bounds: Vec2
    seats: int


class SeatInfo(_Wire):
    seat: int
    team: int
    controller: str


class Challenge(_Wire):
    nonce: str


class Welcome(_Wire):
    protocol_version: int
    match_id: str
    seat: int


class Reject(_Wire):
    reason: str


class Start(_Wire):
    match_id: str
    config: MatchConfig


GatewayMsg = Challenge | Welcome | Reject | Start | Observation | MatchResult


class ProtocolError(Exception):
    """An incoming frame is malformed or not a frame the agent expects here."""


def decode_gateway(frame: dict) -> GatewayMsg:
    """Decode one server→agent frame. Envelopes are internally tagged on `type`
    (`snake_case`), and the two newtype variants — `observe` / `end` — carry the
    `Observation` / `MatchResult` fields flattened next to `type`, exactly as the
    Rust `GatewayMsg` serializes them."""
    if not isinstance(frame, dict) or "type" not in frame:
        raise ProtocolError(f"frame is not a tagged gateway message: {frame!r}")
    tag = frame["type"]
    body = {k: v for k, v in frame.items() if k != "type"}
    match tag:
        case "challenge":
            return Challenge.model_validate(body)
        case "welcome":
            return Welcome.model_validate(body)
        case "reject":
            return Reject.model_validate(body)
        case "start":
            return Start.model_validate(body)
        case "observe":
            return Observation.model_validate(body)
        case "end":
            return MatchResult.model_validate(body)
        case _:
            raise ProtocolError(f"unknown gateway frame type {tag!r}")


def join_frame(agent_id: str, signature_hex: str = "") -> dict:
    """An `AgentMsg::Join` frame. An empty `signature_hex` is an unranked seat —
    matchmaking decides whether that is allowed; a ranked seat signs the
    `join_digest` (a keccak/secp256k1 follow-up, deferred with on-chain identity)."""
    return {
        "type": "join",
        "protocol_version": PROTOCOL_VERSION,
        "agent_id": agent_id,
        "signature_hex": signature_hex,
    }


def act_frame(action: Action) -> dict:
    return {"type": "act", **action.model_dump(mode="json")}


def leave_frame(reason: str) -> dict:
    return {"type": "leave", "reason": reason}
