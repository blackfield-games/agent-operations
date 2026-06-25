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
    Blocker,
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
    address_from_private_key,
    decode_gateway,
    join_frame,
    sign_join,
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
        *,
        signing_key: bytes | None = None,
    ) -> None:
        self.transport = transport
        self.agent_id = agent_id
        # A ranked seat: sign join_digest over THIS connection's challenge nonce (set
        # in connect(), once it is known). A static signature_hex can't bind the
        # per-connection challenge, so a key takes precedence over it when present.
        # agent_id must be address_from_private_key(signing_key) or the Gateway
        # recovers a different address and refuses the seat — `ranked()` guarantees it.
        self.signing_key = signing_key
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
        # The arena's static cover layout, learned at Start — empty until connect()
        # and on a match with no occluders. A policy reads it to path around cover.
        self.blockers: list[Blocker] = []
        # The static pickup spawn points (position-only — kind/amount stays empirical),
        # learned at Start. Empty until connect() and on a match with no pickups. A
        # policy reads it to path toward where items spawn.
        self.pickup_points: list[Vec2] = []
        self.nonce: str | None = None
        self.result: MatchResult | None = None
        self.done = False
        self.connected = False
        self.rejections: list[str] = []
        self.forfeits = 0

    def __repr__(self) -> str:
        # A ranked client holds a secp256k1 private key whose secrecy IS the seat's
        # security — it must never reach a log or repr. Render the PUBLIC identity and
        # connection state only; `ranked` is a bool, never the key bytes. Logging a
        # client (or an error that interpolates it) goes through here, so the key has
        # no path to the wire, a log line, or a crash report.
        return (
            f"ArenaClient(agent_id={self.agent_id!r}, ranked={self.signing_key is not None}, "
            f"seat={self.seat}, connected={self.connected}, done={self.done})"
        )

    @classmethod
    def ranked(
        cls,
        transport: Transport,
        signing_key: bytes,
        clock: Callable[[], float] = time.monotonic,
        enforce_deadline: bool = True,
    ) -> ArenaClient:
        """A ranked-seat client whose `agent_id` is DERIVED from `signing_key`, so the
        claimed identity is always the address the Gateway recovers — the
        AddressMismatch footgun (claiming an id the key doesn't control) can't happen.
        The signature itself is computed per-connection in `connect()` from the
        challenge nonce."""
        return cls(
            transport,
            agent_id=address_from_private_key(signing_key),
            clock=clock,
            enforce_deadline=enforce_deadline,
            signing_key=signing_key,
        )

    def _join_signature(self) -> str:
        """The Join's `signature_hex`. With a `signing_key`, sign `join_digest` over
        THIS connection's challenge nonce (a ranked seat) — the nonce is folded in
        here, after the Challenge, so the proof binds the freshly-issued challenge and
        a captured Join can't be replayed on another connection. Without a key, the
        static `signature_hex` passed at construction (empty = unranked)."""
        if self.signing_key is None:
            return self.signature_hex
        assert self.nonce is not None
        return sign_join(self.signing_key, PROTOCOL_VERSION, self.agent_id, self.nonce.encode())

    def connect(self) -> ArenaClient:
        """Run the handshake: receive the connection challenge, send Join, and on a
        Welcome check the version and record the assigned seat + rules. Raises
        `HandshakeRejected`/`VersionMismatch` on refusal — before any match loop."""
        self.send_join()
        self.recv_welcome()
        return self

    def send_join(self) -> None:
        """Handshake phase one: read the challenge and send the Join (a ranked seat
        signs over the just-received nonce). Split from `recv_welcome` so a matchmade
        driver can send EVERY seat's Join before any one blocks on its Welcome — the
        Matchmaker forms only on the last join, so no Welcome is issued until all seats
        are in, and a per-seat `connect()` would deadlock seat 0 against seat 1's Join."""
        first = decode_gateway(self.transport.recv())
        if not isinstance(first, Challenge):
            raise ProtocolError(f"expected challenge first, got {type(first).__name__}")
        self.nonce = first.nonce
        self.transport.send(join_frame(self.agent_id, self._join_signature()))

    def recv_welcome(self) -> None:
        """Handshake phase two: read the Welcome (or Reject) and the Start, recording
        the assigned seat, version, rules, and geometry. Raises `HandshakeRejected` on
        a Reject, `VersionMismatch` on a version skew."""
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
        self.blockers = start.blockers
        self.pickup_points = start.pickup_points
        self.connected = True

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
    signing_keys: dict[int, bytes] | None = None,
    mode: str | None = None,
    human_seats: list[int] | None = None,
    timeout: float = 30.0,
) -> dict[int, MatchResult]:
    """Run one match against the harnessed reference core: connect an ArenaClient
    per seat over a shared transport, then pump observe→act round-robin until every
    seat reaches End. Single-threaded — the demux buffers whichever seat's frame
    arrives first — so it is deterministic and deadlock-free. Returns each seat's
    MatchResult (all seats receive the same canonical result).

    A seat in `signing_keys` joins RANKED: `ArenaClient.ranked` derives its agent_id
    from the key and signs the challenge nonce, so the harness recovers the signer and
    admits the seat (the key overrides any `agent_ids` entry for that seat — the claim
    is always the address the key controls). Every other seat joins unranked under
    `agent_ids` (default `agent-{s}`), so the no-key call is byte-identical to before.

    `mode` ({"human","agent","mixed"}) routes formation through the harness's
    arena-match Matchmaker (mode-gated, identity-verified) instead of seating the
    roster directly; `mode=None` is the byte-identical direct path. Because the
    Matchmaker forms only once the last seat joins, the harness withholds every Welcome
    until all joins are in, so the matchmade path sends all joins first, then reads each
    Welcome (a per-seat connect would deadlock). Agent mode is ranked-only — every seat
    must be in `signing_keys` or its token-less join is Unauthenticated and no match
    ever forms, so it is rejected up front. Mixed mode needs at least one `human_seats`
    entry (a token-less human) AND at least one agent — the Matchmaker never forms an
    all-one-kind Mixed match, so a human-less Mixed call is rejected up front."""
    ids = agent_ids or {s: f"agent-{s}" for s in seats}
    keys = signing_keys or {}
    humans = human_seats or []
    argv = [harness, "--match-id", match_id, "--seed", str(seed), "--seats", str(len(seats))]
    if mode is not None:
        if mode not in ("human", "agent", "mixed"):
            raise ValueError(f"mode is human, agent, or mixed (or None); got {mode!r}")
        argv += ["--mode", mode]
        if mode == "agent":
            unkeyed = [s for s in seats if s not in keys]
            if unkeyed:
                raise ValueError(f"agent mode is ranked-only; seats {unkeyed} need a signing key")
        if mode == "mixed" and not (
            any(s in humans for s in seats) and any(s not in humans for s in seats)
        ):
            raise ValueError("mixed mode forms a human+agent match; declare some (not all) seats human")
        if humans:
            argv += ["--human-seats", ",".join(str(s) for s in humans)]
    with SubprocessGateway(argv, timeout=timeout) as gateway:
        # The loopback harness blocks for one frame per seat per tick and enforces no
        # wall-clock, so the client must always answer — never drop a frame on a
        # deadline (that is a real-transport behaviour and would deadlock here). A seat
        # declared human (Mixed) is token-less, so it joins unranked even if a stray key
        # is present — the harness gates kind on --human-seats, not the signature.
        clients = {
            s: ArenaClient.ranked(SeatTransport(gateway, s), keys[s], enforce_deadline=False)
            if s in keys and s not in humans
            else ArenaClient(SeatTransport(gateway, s), agent_id=ids[s], enforce_deadline=False)
            for s in seats
        }
        if mode is None:
            for client in clients.values():
                client.connect()
        else:
            for client in clients.values():
                client.send_join()
            for client in clients.values():
                client.recv_welcome()
        results: dict[int, MatchResult] = {}
        while len(results) < len(seats):
            for seat, client in clients.items():
                if client.done:
                    continue
                outcome = client.poll(policies[seat])
                if outcome is not None:
                    results[seat] = outcome
        return results
