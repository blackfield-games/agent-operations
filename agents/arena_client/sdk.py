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

import json
import subprocess
import threading
import time
from collections import defaultdict, deque
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
        enforce_deadline: bool = True,
    ) -> None:
        self.transport = transport
        self.agent_id = agent_id
        self.signature_hex = signature_hex
        self._clock = clock
        # A real transport has a server-side wall-clock deadline, so dropping a late
        # action (and sending nothing) is correct — the server forfeits the tick and
        # streams the next observation. A lock-step in-process transport (the loopback
        # harness) has NO wall-clock and blocks until it gets one frame per seat, so a
        # dropped frame would deadlock; that path disables enforcement instead.
        self.enforce_deadline = enforce_deadline
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
        if self.enforce_deadline and elapsed_micros > obs.deadline_micros:
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


class GatewayClosed(ProtocolError):
    """The harness stream ended before the match did."""


class SubprocessGateway:
    """Spawns a gateway harness and multiplexes the seat-tagged transport frames it
    speaks (`{"seat": u8, "frame": <arena-01>}`) into per-seat queues, so several
    ArenaClients can share one stdio pipe. A real networked gateway is one
    connection per seat; this is the local loopback twin used to run a match
    against the reference core. A watchdog kills a silent harness so a hang fails
    the caller loudly instead of blocking forever."""

    def __init__(self, argv: list[str], timeout: float = 30.0) -> None:
        self._proc = subprocess.Popen(
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1
        )
        self._queues: dict[int, deque[dict]] = defaultdict(deque)
        self._timeout = timeout

    def _readline(self) -> str:
        timer = threading.Timer(self._timeout, self._proc.kill)
        timer.start()
        try:
            assert self._proc.stdout is not None
            return self._proc.stdout.readline()
        finally:
            timer.cancel()

    def recv(self, seat: int) -> dict:
        while not self._queues[seat]:
            line = self._readline()
            if not line:
                raise GatewayClosed("harness closed the stream")
            env = json.loads(line)
            self._queues[env["seat"]].append(env["frame"])
        return self._queues[seat].popleft()

    def send(self, seat: int, frame: dict) -> None:
        assert self._proc.stdin is not None
        self._proc.stdin.write(json.dumps({"seat": seat, "frame": frame}) + "\n")
        self._proc.stdin.flush()

    def close(self) -> None:
        try:
            if self._proc.stdin is not None:
                self._proc.stdin.close()
        except OSError:
            pass
        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            self._proc.wait()

    def __enter__(self) -> SubprocessGateway:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


class SeatTransport:
    """A per-seat Transport view over a shared SubprocessGateway."""

    def __init__(self, gateway: SubprocessGateway, seat: int) -> None:
        self._gateway = gateway
        self._seat = seat

    def recv(self) -> dict:
        return self._gateway.recv(self._seat)

    def send(self, frame: dict) -> None:
        self._gateway.send(self._seat, frame)


def run_local_match(
    harness: str,
    seats: list[int],
    policies: dict[int, Policy],
    *,
    seed: int = 0,
    match_id: str = "00000000-0000-4000-8000-000000000000",
    agent_ids: dict[int, str] | None = None,
    timeout: float = 30.0,
) -> dict[int, MatchResult]:
    """Run one match against the harnessed reference core: connect an ArenaClient
    per seat over a shared transport, then pump observe→act round-robin until every
    seat reaches End. Single-threaded — the demux buffers whichever seat's frame
    arrives first — so it is deterministic and deadlock-free. Returns each seat's
    MatchResult (all seats receive the same canonical result)."""
    argv = [harness, "--match-id", match_id, "--seed", str(seed), "--seats", str(len(seats))]
    ids = agent_ids or {s: f"agent-{s}" for s in seats}
    with SubprocessGateway(argv, timeout=timeout) as gateway:
        # The loopback harness blocks for one frame per seat per tick and enforces no
        # wall-clock, so the client must always answer — never drop a frame on a
        # deadline (that is a real-transport behaviour and would deadlock here).
        clients = {
            s: ArenaClient(SeatTransport(gateway, s), agent_id=ids[s], enforce_deadline=False)
            for s in seats
        }
        for client in clients.values():
            client.connect()
        results: dict[int, MatchResult] = {}
        while len(results) < len(seats):
            for seat, client in clients.items():
                if client.done:
                    continue
                outcome = client.poll(policies[seat])
                if outcome is not None:
                    results[seat] = outcome
        return results
