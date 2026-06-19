"""The arena_client state machine — a transport-agnostic client of the arena-01
Gateway. An agent supplies a `Transport` (a pair of recv/send over a connection)
and a `Policy` (an `Observation` → `ActionIntent` decision); the client owns the
protocol: handshake with a version check, then per tick a parity-bounded
observation in and a validated action out, until the match ends.

Three failure modes the protocol exists to prevent are handled here, not assumed
away:

- **Rejection is a normal outcome, not a desync (FM1).** The server silently
  forfeits an invalid/late action — the next observation is the ground truth, so
  the client never predicts its action took effect. A `Reject` frame (only the
  handshake sends one in arena-01, but a server may surface one mid-match) is
  recorded on `rejections` and stepped past, never raised mid-match.
- **A late answer forfeits the tick (FM2).** The policy is timed against the
  tick's `deadline_micros`; an over-budget answer is dropped rather than sent to a
  stale tick. The client never blocks the match on a slow policy.
- **A version skew is refused at connect (FM3).** `connect` checks the Welcome's
  `protocol_version` before any match state exists, so a drifted agent is rejected
  cleanly instead of mis-simulated.
"""

from __future__ import annotations

import time
from collections.abc import Callable
from typing import Protocol

from .proto import (
    PROTOCOL_VERSION,
    Action,
    ActionButtons,
    ActionIntent,
    Challenge,
    MatchConfig,
    MatchResult,
    Observation,
    ProtocolError,
    Reject,
    Start,
    Vec2,
    Welcome,
    act_frame,
    decode_gateway,
    join_frame,
)

Policy = Callable[[Observation], ActionIntent]


class Transport(Protocol):
    """One agent's connection to the Gateway: blocking recv of the next server
    frame and send of an agent frame, both as plain JSON-shaped dicts."""

    def recv(self) -> dict: ...
    def send(self, frame: dict) -> None: ...


class HandshakeRejected(Exception):
    """The server refused the seat at the handshake — version mismatch, full
    match, or an unauthenticated ranked seat. Terminal for this connection."""


class VersionMismatch(HandshakeRejected):
    def __init__(self, ours: int, theirs: int) -> None:
        super().__init__(f"gateway protocol version mismatch: ours={ours}, theirs={theirs}")
        self.ours = ours
        self.theirs = theirs


def check_version(theirs: int) -> None:
    """Reject a Welcome whose version is not exactly ours, mirroring
    `arena_proto::check_version`. Run before any match state exists so a drifted
    agent never plays under a divergent contract."""
    if theirs != PROTOCOL_VERSION:
        raise VersionMismatch(PROTOCOL_VERSION, theirs)


def _hold(facing: int) -> ActionIntent:
    return ActionIntent(
        move_dir=Vec2(x=0, y=0),
        aim=facing,
        buttons=ActionButtons(fire=False, jump=False, ability=False, reload=False),
    )


class ArenaClient:
    def __init__(
        self,
        transport: Transport,
        agent_id: str,
        signature_hex: str = "",
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.transport = transport
        self.agent_id = agent_id
        self.signature_hex = signature_hex
        self._clock = clock
        self.match_id: str | None = None
        self.seat: int | None = None
        self.config: MatchConfig | None = None
        self.nonce: str | None = None
        self.result: MatchResult | None = None
        self.done = False
        self.connected = False
        self.rejections: list[str] = []
        self.forfeits = 0

    def connect(self) -> ArenaClient:
        """Run the handshake: receive the connection challenge, send Join, and on a
        Welcome check the version and record the assigned seat + rules. Raises
        `HandshakeRejected`/`VersionMismatch` on refusal — before any match loop."""
        first = decode_gateway(self.transport.recv())
        if not isinstance(first, Challenge):
            raise ProtocolError(f"expected challenge first, got {type(first).__name__}")
        self.nonce = first.nonce
        self.transport.send(join_frame(self.agent_id, self.signature_hex))
        reply = decode_gateway(self.transport.recv())
        if isinstance(reply, Reject):
            raise HandshakeRejected(reply.reason)
        if not isinstance(reply, Welcome):
            raise ProtocolError(f"expected welcome or reject, got {type(reply).__name__}")
        check_version(reply.protocol_version)
        self.match_id = reply.match_id
        self.seat = reply.seat
        start = decode_gateway(self.transport.recv())
        if not isinstance(start, Start):
            raise ProtocolError(f"expected start, got {type(start).__name__}")
        self.config = start.config
        self.connected = True
        return self

    def poll(self, policy: Policy) -> MatchResult | None:
        """Process exactly one inbound frame. Returns the MatchResult on End (and
        marks the client done), else None. Connects lazily on first call."""
        if not self.connected:
            self.connect()
        msg = decode_gateway(self.transport.recv())
        if isinstance(msg, MatchResult):
            self.result = msg
            self.done = True
            return msg
        if isinstance(msg, Reject):
            self.rejections.append(msg.reason)
            return None
        if isinstance(msg, Observation):
            self._respond(msg, policy)
            return None
        raise ProtocolError(f"unexpected mid-match frame {type(msg).__name__}")

    def _respond(self, obs: Observation, policy: Policy) -> None:
        if not obs.own.alive:
            # A corpse's action is rejected (SeatDown); answer with a passive hold so
            # a lock-step transport stays in frame, and never fire.
            self._send(obs, _hold(obs.own.facing))
            return
        start = self._clock()
        intent = policy(obs)
        elapsed_micros = (self._clock() - start) * 1_000_000
        if elapsed_micros > obs.deadline_micros:
            # FM2: by the time a late answer reaches the server it answers a stale
            # tick and is rejected — so drop it here and forfeit the tick rather than
            # send an action bound to a tick that has passed.
            self.forfeits += 1
            return
        self._send(obs, intent)

    def _send(self, obs: Observation, intent: ActionIntent) -> None:
        assert self.match_id is not None and self.seat is not None
        action = Action(
            protocol_version=PROTOCOL_VERSION,
            match_id=self.match_id,
            seat=self.seat,
            tick=obs.tick,
            intent=intent,
        )
        self.transport.send(act_frame(action))

    def run(self, policy: Policy) -> MatchResult:
        """Drive the connection to the match's End and return the result. For a
        dedicated per-connection transport (a real socket, or the harness-driven
        single seat); a shared multiplexed transport drives `poll` round-robin."""
        if not self.connected:
            self.connect()
        while not self.done:
            self.poll(policy)
        assert self.result is not None
        return self.result
