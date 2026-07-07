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
from dataclasses import dataclass
from pathlib import Path
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


class StatefulPolicy(Protocol):
    """A `Policy` that also wants the match's STATIC map. `ArenaClient` calls
    `on_match_start` exactly ONCE, after the handshake and before the first
    `__call__`, with the cover + pickup layout — so a policy can plan around fixed
    geometry the per-tick parity-bounded `Observation` deliberately omits. The hook is
    OPTIONAL: a plain `Policy` callable without it runs unchanged, and the per-tick
    decision still depends only on the `Observation` (the parity boundary is untouched —
    `on_match_start` carries no live state, only the layout a human sees on the map).

    `on_starting` is the second optional hook: it fires for each pre-live
    (phase==Starting) countdown observation, which draws no reply — a policy uses it to
    read `starting_remaining` and prepare for GO, never to act.

    `on_match_end` is the third optional hook: it fires EXACTLY ONCE with the terminal
    `MatchResult` when the match ends — the canonical outcome (final standings, own
    score) every seat receives. It closes the lifecycle (start → starting → live → end)
    for a stateful/learning policy that must observe its own result in-band to adapt,
    which the SDK already receives but otherwise only hands back to the caller of
    `run`/`poll`. All three hooks are getattr-guarded, so a plain callable with none of
    them runs unchanged and the parity boundary is untouched (each hook carries only
    map layout, the pre-live counter, or the public result — never a live private
    field)."""

    def on_match_start(self, start: MatchStart) -> None: ...
    def on_starting(self, obs: Observation) -> None: ...
    def on_match_end(self, result: MatchResult) -> None: ...
    def __call__(self, obs: Observation) -> ActionIntent: ...


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
        # Guards the one-shot on_match_start delivery: set on the first poll after
        # connect so a map-aware policy is handed the static layout exactly once.
        self._started = False
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
        marks the client done, surfacing the result to a policy's `on_match_end`),
        else None. Connects lazily on first call. A pre-live (Starting-countdown)
        observation is informational and draws no reply — the client answers only
        once the match is Live."""
        if not self.connected:
            self.connect()
        if not self._started:
            # First frame after the handshake: hand a map-aware policy the static
            # layout once, before any decision. A plain callable has no hook (degrade
            # cleanly, never AttributeError) and is unaffected.
            self._started = True
            on_start = getattr(policy, "on_match_start", None)
            if callable(on_start):
                on_start(MatchStart(blockers=list(self.blockers), pickup_points=list(self.pickup_points)))
        msg = decode_gateway(self.transport.recv())
        if isinstance(msg, MatchResult):
            # Terminal frame: record it, then surface the canonical outcome to a policy
            # that wants it (a stateful/learning policy closing its own loop) via the
            # same optional getattr-guarded hook as on_match_start/on_starting. Fires
            # exactly once — poll sets `done`, so run() and the multiplexed driver stop
            # polling this seat after the result lands.
            self.result = msg
            self.done = True
            on_end = getattr(policy, "on_match_end", None)
            if callable(on_end):
                on_end(msg)
            return msg
        if isinstance(msg, Reject):
            self.rejections.append(msg.reason)
            return None
        if isinstance(msg, Observation):
            self._respond(msg, policy)
            return None
        raise ProtocolError(f"unexpected mid-match frame {type(msg).__name__}")

    def _respond(self, obs: Observation, policy: Policy) -> None:
        if obs.phase != "live":
            # Pre-live (the Starting countdown): the server broadcasts the observation
            # without reading a reply (the arena harness pump_starting), so an Act here
            # would sit in the pipe and be consumed as the stale first Live action,
            # desyncing the match. A pre-live observation is informational — surface it
            # to a countdown-aware policy (which reads `starting_remaining` to time GO)
            # via the same optional hook as on_match_start, but send nothing until Live.
            on_starting = getattr(policy, "on_starting", None)
            if callable(on_starting):
                on_starting(obs)
            return
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


@dataclass(frozen=True, slots=True)
class LadderStanding:
    """A ranked seat's post-match ladder position: its `rating` after the settle and
    the signed `delta` that move applied. An unranked seat — a casual / human / Mixed
    match, or any run that moved no ladder — has no standing and reads None, never a
    zeroed one: 'no rating' is distinct from 'a rating of zero'."""

    rating: int
    delta: int


@dataclass(frozen=True, slots=True)
class MatchStart:
    """The static map a seat received at Start: the cover `blockers` and the pickup
    spawn `pickup_points`. Read it back from `run_local_match` (via `starts=`) to see
    what arena the match actually played under — both lists are empty for the default
    (no `arena=`) empty arena."""

    blockers: list[Blocker]
    pickup_points: list[Vec2]


_LADDER_PREFIX = "[ladder] "


class SubprocessGateway:
    """Spawns a gateway harness and multiplexes the seat-tagged transport frames it
    speaks (`{"seat": u8, "frame": <arena-01>}`) into per-seat queues, so several
    ArenaClients can share one stdio pipe. A real networked gateway is one
    connection per seat; this is the local loopback twin used to run a match
    against the reference core. A watchdog kills a silent harness so a hang fails
    the caller loudly instead of blocking forever.

    With `capture_stderr`, the harness's stderr is drained on a background thread so a
    post-match `[ladder]` line can be read back via `ladder()` without ever blocking the
    stdout frame pump; without it, stderr inherits the parent's (today's behaviour, so a
    panic backtrace still surfaces to the console)."""

    def __init__(self, argv: list[str], timeout: float = 30.0, *, capture_stderr: bool = False) -> None:
        self._proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE if capture_stderr else None,
            text=True,
            bufsize=1,
        )
        self._queues: dict[int, deque[dict]] = defaultdict(deque)
        self._timeout = timeout
        # Drain stderr on its own thread so a full stderr pipe can never deadlock the
        # stdout read the match loop blocks on (e.g. a mid-match panic backtrace).
        self._stderr_lines: list[str] = []
        self._stderr_thread: threading.Thread | None = None
        if capture_stderr:
            self._stderr_thread = threading.Thread(target=self._drain_stderr, daemon=True)
            self._stderr_thread.start()

    def _drain_stderr(self) -> None:
        assert self._proc.stderr is not None
        for line in self._proc.stderr:
            self._stderr_lines.append(line)

    def ladder(self, match_id: str) -> dict[int, LadderStanding]:
        """The per-seat ladder standing the harness settled for `match_id`, parsed from
        the structured `[ladder]` stderr line (captured only under `capture_stderr`).
        Returns seat -> LadderStanding for a ranked (Agent-mode) match; an EMPTY map when
        nothing settled (casual / human / Mixed / direct), so each seat reads None. A
        `[ladder]` line that is present but not the exact
        `{match_id, seats:[{seat,rating,delta}]}` shape is a hard error, never a silent
        drop — an emission drift must fail loud, not masquerade as 'unranked'."""
        out: dict[int, LadderStanding] = {}
        for line in self._stderr_lines:
            if not line.startswith(_LADDER_PREFIX):
                continue
            payload = line[len(_LADDER_PREFIX) :].strip()
            try:
                frame = json.loads(payload)
                if frame["match_id"] != match_id:
                    continue
                for seat in frame["seats"]:
                    out[int(seat["seat"])] = LadderStanding(
                        rating=int(seat["rating"]), delta=int(seat["delta"])
                    )
            except (json.JSONDecodeError, KeyError, TypeError, ValueError) as e:
                raise ProtocolError(f"malformed [ladder] emission: {payload!r}") from e
        return out

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
        # The process is down, so stderr has hit EOF; join the drain so every captured
        # line (the [ladder] readout included) is visible before close() returns.
        if self._stderr_thread is not None:
            self._stderr_thread.join(timeout=5)

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
    ladder_file: str | Path | None = None,
    ratings: dict[int, LadderStanding | None] | None = None,
    arena: str | None = None,
    starts: dict[int, MatchStart] | None = None,
    perception_memory: int = 0,
    fov: int = 4,
    aim_mode: str = "octant",
    friendly_fire: bool = False,
    gravity: int = 0,
    starting_ticks: int = 0,
    weapon_mode: str = "hitscan",
    vertical_hit_tolerance: int = 0,
    fall_damage: int = 0,
    knockback_velocity: int = 0,
    wall_slide: bool = False,
    fall_damage_threshold: int = 0,
    knockback_horizontal: int = 0,
    dash_cooldown: int = 0,
    pawn_radius: int = 0,
    pawn_height: int = 0,
    max_shield: int = 0,
    start_health: int | None = None,
    damage: int | None = None,
    fire_cooldown: int | None = None,
    mag_size: int | None = None,
    max_speed: int | None = None,
    perception_range: int | None = None,
    weapon_range: int | None = None,
    hit_radius: int | None = None,
    melee_cooldown: int | None = None,
    melee_damage: int | None = None,
    melee_range: int | None = None,
    projectile_speed: int | None = None,
    action_deadline_micros: int | None = None,
    pickup_radius: int | None = None,
    pickup_respawn_cooldown: int | None = None,
    spawn_jitter: int | None = None,
    spawn_radius: int | None = None,
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
    all-one-kind Mixed match, so a human-less Mixed call is rejected up front.

    Pass a `ratings` dict to read back each seat's post-match ladder standing: it is
    filled in place with `seat -> LadderStanding(rating, delta)` for a ranked (Agent-mode)
    seat and `seat -> None` for an unranked one (casual / human / Mixed / direct), so an
    A2A author can see how the match moved their rating. Omitting it is byte-identical to
    before — the MatchResult return is unchanged and stderr is not even captured.

    Pass a `ladder_file` to persist the matchmaker's ranked ladder across calls: the
    harness seeds the ladder from it at startup and writes the post-match ladder back, so
    two sequential ranked matches sharing one path accumulate a seat's rating (read it via
    `ratings`). The ladder only MOVES on the `--mode` path, so `ladder_file` with `mode=None`
    is rejected up front (the direct path would silently ignore it); a human/Mixed run loads
    and rewrites the file with no movement. Omitting it adds no flag — byte-identical to before.

    Pass an `arena` (a builtin map key, e.g. `"reference"`) to play the match under that
    arena's static geometry — its vision blockers and pickup spawns, forwarded to the
    harness as `--map`. It applies to BOTH the direct (`mode=None`) and `--mode` paths, so
    an agent receives the cover + pickups in its Start frame however the roster is formed.
    Omitting it (`arena=None`) adds no flag — the empty arena, byte-identical to before; an
    unknown key aborts the harness loudly (surfacing as a `GatewayClosed`/timeout here, not
    a silent empty arena). Pass a `starts` dict to read back each seat's received geometry
    as a `MatchStart` (filled in place at connect), so a caller can confirm what arena the
    match played under.

    Pass `perception_memory` (ticks > 0) to turn on perception memory: a seat then
    remembers a lost entity's last-known position for that many ticks, surfaced as a
    `VisibleEntity` with `in_line_of_sight=False`. The harness carries it through BOTH the
    direct and the matchmaker (`--mode`) paths — a matchmade/ranked match forms under it via
    `MatchParams.rules` — so it applies under any `mode` and to every seat, including a human
    seat in a Mixed match. Default `0` disables it, byte-identical to before.

    Pass `fov` (an octant spread in `0..=4`) to narrow each seat's forward perception cone: a
    seat then perceives an in-range enemy only when its bearing is within `fov` octants of its
    facing — `4` (the default) is the full circle (omnidirectional), `0` the facing octant
    alone (~45°). Like `perception_memory` it carries through BOTH paths via `MatchParams.rules`,
    so it applies under any `mode` and to every seat. An out-of-range value raises before any
    spawn (the harness would otherwise saturate a spread >4 to the full circle). Default `4`
    adds no flag — byte-identical to before.

    Pass `aim_mode` (`"octant"` or `"fine"`) to pick how a seat's facing resolves: `"octant"`
    (the default) snaps aim to the 8 compass directions, `"fine"` selects the 64-way half-step
    table, changing which shots land at a sub-octant lead. Validated SDK-side against that literal
    set (an unknown value raises before any spawn, like `mode`/`fov`) rather than forwarded blindly
    to abort the harness. It carries through BOTH paths via `--aim-mode` (the harness threads it via
    `MatchParams.rules`), so it applies under any `mode`. Default `"octant"` adds no flag —
    byte-identical to before.

    Set `friendly_fire=True` to allow allied damage (`Rules.friendly_fire`): a fire crossing a
    same-team body damages it but never scores a kill for the shooter. Unlike `aim_mode`/`fov` this
    is a BARE presence flag (`--friendly-fire`, no value) carried through BOTH paths via
    `MatchParams.rules`. Default `False` adds no token — byte-identical argv. It is wired but DARK
    in this roster: `run_local_match` seats every agent on its own team (free-for-all), and
    `friendly_fire` only diverges hit resolution for same-team bodies, so it changes nothing here
    until a teamed roster exists — the flag's reachability is the contract, its behavioural effect
    is a separate teamed slice.

    Pass `gravity` (a downward magnitude, `0`..=`2**31-1`) to turn on vertical physics: core gates
    jumps/`z` trajectory on `gravity > 0`, so `0` (the default) is the 2D match. Forwarded as
    `--gravity <n>` (value-flag, like `fov`) on BOTH paths via `MatchParams.rules`, only when
    nonzero. A negative or `> i32::MAX` value raises before any spawn (both would read as off in
    core — a silent footgun — so they are rejected loudly, mirroring the harness fence). Default `0`
    adds no token — byte-identical argv. Gravity alone changes only the observable `z` arc, not a
    hit outcome, until the separate `vertical_hit_tolerance` knob couples `z` into combat.

    Pass `starting_ticks` (a pre-live countdown length) to open the match in the `Starting`
    phase for that many ticks before it goes `Live`: the harness broadcasts a Starting
    observation each tick (carrying `starting_remaining`, no reply read) via `pump_starting`,
    so a countdown-aware policy reads the countdown through `on_starting` while the client
    sends nothing until `Live`. Forwarded as `--starting-ticks <n>` (value-flag, like
    `gravity`) on BOTH paths via `MatchParams.rules`, only when nonzero. A negative or
    `> u32::MAX` value raises before any spawn (the harness parses `--starting-ticks` as a
    `u32` and panics on a negative, so a bad value would abort the match late — reject it
    loudly here instead, mirroring the sibling fences). `starting_ticks` is
    outside `canonical_encoding`, so a countdown match's `replay_hash` is byte-identical to
    the no-countdown one — the pre-live delay is digest-inert. Default `0` opens `Live` at
    tick 0 and adds no token — byte-identical argv.

    Pass `weapon_mode` (`"hitscan"`, `"projectile"`, or `"melee"`) to pick how a fire resolves:
    `"hitscan"` (the default) is the instant beam, `"projectile"` spawns a travelling shot that
    hits when its swept path crosses a body on a later/point-blank tick, `"melee"` strikes every
    enemy in `melee_range` + the frontal arc. Validated SDK-side against that literal set (an
    unknown value raises before any spawn, like `aim_mode`) and forwarded as `--weapon-mode <value>`
    on BOTH paths via `MatchParams.rules`, only when non-default. Unlike the other three this has an
    IMMEDIATE combat effect even in the free-for-all roster (it changes core fire resolution and the
    replay digest the moment it is set). Default `"hitscan"` adds no token — byte-identical argv.

    Pass `vertical_hit_tolerance` (an elevation band, `0`..=`2**31-1`) to couple `z` into combat: a
    fire connects only when `|shooter_z - target_z| <= vertical_hit_tolerance`, so `0` (the default)
    is the planar match where `z` is ignored in hit resolution. Forwarded as `--vertical-hit-tolerance
    <n>` (value-flag, like `gravity`) on BOTH paths via `MatchParams.rules`, only when nonzero. A
    negative or `> i32::MAX` value raises before any spawn (both read as off/inverted in core — a
    silent footgun — so they are rejected loudly, mirroring the harness fence). Default `0` adds no
    token — byte-identical argv. It is the combat companion to `gravity`: tolerance only changes a hit
    once `gravity > 0` has moved pawns in `z`, so on the default flat field it is observable at the
    argv layer alone (a gravity+tolerance replay divergence is a coupled follow-up).

    Pass `fall_damage` (a `u16` HP magnitude, `0`..=`65535`) to punish a hard landing: a falling pawn
    whose downward impact speed exceeds the core `fall_damage_threshold` takes this much damage on
    landing, so `0` (the default) keeps every landing safe. Forwarded as `--fall-damage <hp>`
    (value-flag, like `gravity`) on BOTH paths via `MatchParams.rules`, only when nonzero. A negative
    or `> 65535` value raises before any spawn (core takes it as a `u16`, so an out-of-range forward
    would wrap into a magnitude the caller never asked for — rejected loudly, mirroring the harness's
    `u16` parse). Default `0` adds no token — byte-identical argv. Like `vertical_hit_tolerance` it is
    a `gravity` companion: damage only fires once `gravity > 0` drops pawns and a landing clears the
    threshold, so on the default flat field it is observable at the argv layer alone (a behavioral
    landing-damage replay is a coupled follow-up).

    Pass `knockback_velocity` (a non-negative `i32` upward impulse, `0`..=`2**31-1`) to pop a hit
    survivor upward: a landed damaging hit adds this much `z` velocity to the target, which then arcs
    back under gravity (the variable-fall-height source), so `0` (the default) imparts no impulse.
    Forwarded as `--knockback-velocity <n>` (value-flag, like `gravity`) on BOTH paths via
    `MatchParams.rules`, only when nonzero. A negative or `> i32::MAX` value raises before any spawn
    (a negative would launch the target DOWNWARD into the floor, an overflow would wrap — both
    rejected loudly, mirroring the harness's `u32`-then-`i32` parse). Default `0` adds no token —
    byte-identical argv. Like `fall_damage` it is a `gravity` companion: the impulse is suppressed
    while `gravity == 0`, so on the default flat field it is observable at the argv layer alone (a
    behavioral knockback replay is a coupled follow-up).

    Set `wall_slide=True` to slide a grazing move along a blocker instead of dead-stopping
    (`Rules.wall_slide`): a diagonal step whose full path a blocker refuses retries each axis and keeps
    the component that clears, so the pawn slides along the surface. Like `friendly_fire` this is a BARE
    presence flag (`--wall-slide`, no value) carried through BOTH paths via `MatchParams.rules`. Default
    `False` adds no token — byte-identical argv. The effect needs a blocker to graze: the default empty
    arena has none, so pass an `arena` with cover (e.g. `"reference"`) for `wall_slide` to diverge
    movement — its reachability is the contract here, a behavioral slide replay is a coupled follow-up.

    Pass `fall_damage_threshold` (a non-negative `i32` impact-speed gate, `0`..=`2**31-1`) to raise the
    bar a landing must clear to take `fall_damage`: core wounds a landing only when its downward impact
    speed EXCEEDS this gate, so `0` (the default) gates nothing — every impact>0 landing takes the full
    `fall_damage`. The impact-speed companion to the `fall_damage` magnitude. Forwarded as
    `--fall-damage-threshold <n>` (value-flag, like `gravity`) on BOTH paths via `MatchParams.rules`,
    only when nonzero. A negative or `> i32::MAX` value raises before any spawn (a negative makes core's
    `impact > threshold` true for EVERY landing — the inverse of raising the bar — and an overflow wraps,
    both rejected loudly, mirroring the harness's `u32`-then-`i32` parse). Default `0` adds no token —
    byte-identical argv. It is fully inert while `fall_damage == 0` (it gates a magnitude that is off);
    on the default flat field it is observable at the argv layer alone (a behavioral soft-landing-spared
    replay needs a coupled `gravity` + `fall_damage` + threshold, a follow-up).

    Pass `knockback_horizontal` (a non-negative `i32` planar shove, `0`..=`2**31-1`) to displace a hit
    survivor away from the shooter: a landed damaging hit shoves the target this many position units along
    the shooter→target octant (through the same `slide()` a walk uses, so it stops at a wall), so `0` (the
    default) imparts no shove. The horizontal sibling of `knockback_velocity` — but UNLIKE the vertical
    impulse it needs NO gravity (core gates the shove on `> 0` alone, so it is meaningful in a 2D match).
    Forwarded as `--knockback-horizontal <n>` (value-flag, like `gravity`) on BOTH paths via
    `MatchParams.rules`, only when nonzero. A negative or `> i32::MAX` value raises before any spawn (a
    negative is inert in core's `> 0` gate — a silent no-shove footgun — and an overflow wraps, both
    rejected loudly, mirroring the harness's `u32`-then-`i32` parse). Default `0` adds no token —
    byte-identical argv. Because it needs no gravity the shove is observable as soon as a hit lands, but
    the default free-for-all roster + seed may not produce a clean displacement, so the pin is argv
    reachability; a behavioral pos-shove replay is a coupled follow-up.

    Pass `dash_cooldown` (a `u16` tick cadence, `0`..=`65535`) to turn on the `ability`-button dash: a
    grounded ability press with a move direction bursts the pawn the fixed core `DASH_DISTANCE` along
    `move_dir`, then this many ticks must elapse before the next dash, so `0` (the default) disables the
    dash entirely (an ability press is inert). Forwarded as `--dash-cooldown <ticks>` (value-flag, like
    `gravity`) on BOTH paths via `MatchParams.rules`, only when nonzero. A negative or `> 65535` value
    raises before any spawn (core takes it as a `u16`, so an out-of-range forward would wrap into a
    cadence the caller never asked for — and `65536 -> 0` reads as "dash off", doubly wrong — rejected
    loudly, mirroring the harness's `u16` parse). Default `0` adds no token — byte-identical argv. The
    burst DISTANCE is a fixed core constant, not a knob; this cadence is the only dash tuning the digest
    binds, so the pin is argv reachability — a behavioral dash replay is a coupled follow-up.

    Pass `pawn_radius` (a non-negative `i32` occupancy radius, `0`..=`2**31-1`) to make pawns physical
    obstacles to one another: a step whose swept path would bring the mover's centre within this distance
    of another alive pawn is refused (the mover holds at the step origin), gating the walk, the dash
    burst, AND a `knockback_horizontal` shove alike, so `0` (the default) disables occupancy (pawns may
    overlap). Forwarded as `--pawn-radius <n>` (value-flag, like `gravity`) on BOTH paths via
    `MatchParams.rules`, only when nonzero. A negative or `> i32::MAX` value raises before any spawn (a
    negative is inert in core's `> 0` gate — a silent no-collision footgun — and an overflow wraps, both
    rejected loudly, mirroring the harness's `u32`-then-`i32` parse). Default `0` adds no token —
    byte-identical argv. The z-companion `pawn_height` (the vertical band a jump vaults a body over) is a
    separate knob; the pin here is argv reachability — a behavioral occupancy replay is a coupled
    follow-up.

    Pass `pawn_height` (a non-negative `i32` body band, `0`..=`2**31-1`) to z-couple `pawn_radius`: with
    it set, another pawn's body blocks a step only when their XY discs overlap AND their feet are within
    `pawn_height` (each pawn a cylinder of this height), so a pawn that jumps higher than the band vaults
    the body. `0` (the default) keeps occupancy PLANAR (elevation ignored). Forwarded as `--pawn-height
    <n>` (value-flag, like `gravity`) on BOTH paths via `MatchParams.rules`, only when nonzero. A
    negative or `> i32::MAX` value raises before any spawn (a negative is inert in core's `> 0` gate —
    silently planar — and an overflow wraps, both rejected loudly, mirroring the harness's
    `u32`-then-`i32` parse). Default `0` adds no token — byte-identical argv. Only meaningful with
    `pawn_radius > 0` (no occupancy otherwise) AND `gravity > 0` (the sole source of any non-zero `z`),
    so the pin is argv reachability — a behavioral vault replay is a coupled follow-up.

    Pass `max_shield` (a `u16` shield-pool cap, `0`..=`65535`) to turn on shield: a pawn starts at `0`
    shield and earns it (capped here) by collecting a Shield pickup, and incoming damage drains shield
    before health, so `0` (the default) disables shield entirely (a Shield pickup is inert). Forwarded as
    `--max-shield <cap>` (value-flag, like `gravity`) on BOTH paths via `MatchParams.rules`, only when
    nonzero. A negative or `> 65535` value raises before any spawn (core takes it as a `u16`, so an
    out-of-range forward would wrap into a cap the caller never asked for — and `65536 -> 0` reads as
    "shield off", doubly wrong — rejected loudly, mirroring the harness's `u16` parse). Default `0` adds
    no token — byte-identical argv. The effect needs a Shield pickup to collect: the default arenas spawn
    none, so pass an `arena` with shield pickups for it to matter — the pin here is argv reachability, a
    behavioral shield replay is a coupled follow-up.

    Pass `start_health` (a `u16` HP pool, `0`..=`65535`) to set the health every pawn spawns with and
    reloads/heals cap toward — lower HP is a faster time-to-kill. UNLIKE the feature-toggle knobs above,
    this is a base-balance value with a NON-ZERO core default, so it uses a `None` sentinel: omit it (the
    default `None`) to add no token and let the harness apply its own default — byte-identical argv. A
    value (even one equal to the core default) forwards `--start-health <hp>` (value-flag) on BOTH paths
    via `MatchParams.rules`. The sentinel avoids hardcoding the Rust default in Python (the harness owns
    it). A non-None negative or `> 65535` value raises before any spawn (core takes it as a `u16`, so an
    out-of-range forward would wrap into a pool the caller never asked for — e.g. `65536 -> 0`, an
    already-downed spawn — rejected loudly, mirroring the harness's `u16` parse).

    Pass `damage` (a `u16` per-shot HP, `0`..=`65535`) to set the health one landed shot subtracts — lower
    damage is a slower time-to-kill (more shots to down a pawn). The companion to `start_health` (the two
    set a match's time-to-kill: this the HP a shot lands, that the HP pool it lands into). Like
    `start_health` it is a base-balance value with a NON-ZERO core default, so it uses a `None` sentinel:
    omit it (the default `None`) to add no token and let the harness apply its own default — byte-identical
    argv. A value (even one equal to the core default, AND an explicit `0`) forwards `--damage <hp>`
    (value-flag) on BOTH paths via `MatchParams.rules`. A non-None negative or `> 65535` value raises before
    any spawn (core takes it as a `u16`, so an out-of-range forward would wrap into a per-shot HP the caller
    never asked for — e.g. `65536 -> 0`, a shot that can never down a pawn — rejected loudly, mirroring the
    harness's `u16` parse).

    Pass `fire_cooldown` (a `u16` tick count, `0`..=`65535`) to set the ticks BETWEEN ranged shots — the
    rate of fire (lower is a faster cadence, more shots/sec), orthogonal to `damage`/`start_health` (it sets
    how FAST a pawn shoots, not how hard or how long it survives). Like `damage` it is a base-balance value
    with a NON-ZERO core default (`6` — five shots/sec at 30 Hz), so it uses a `None` sentinel: omit it (the
    default `None`) to add no token and let the harness apply its own default — byte-identical argv. A value
    (even one equal to the core default, AND an explicit `0`) forwards `--fire-cooldown <ticks>` (value-flag)
    on BOTH paths via `MatchParams.rules`. The `0` case is sharper than for `damage`: a `0`-cooldown pawn can
    fire EVERY tick (the unbounded-projectile-spawn degenerate, backstopped only by the core's live-projectile
    cap), not the default cadence — so an explicit `0` must forward, never be swallowed. A non-None negative
    or `> 65535` value raises before any spawn (core takes it as a `u16`, so an out-of-range forward would
    wrap into a cadence the caller never asked for — rejected loudly, mirroring the harness's `u16` parse).

    Pass `mag_size` (a `u16` round count, `0`..=`65535`) to set the rounds a full magazine holds before a
    reload is forced — smaller is more reload pressure (a pawn empties and must reload sooner), the ammo
    sibling of `fire_cooldown` (that the ticks between shots, this the rounds per magazine — together the
    sustained-fire envelope). Like `fire_cooldown` it is a base-balance value with a NON-ZERO core default
    (`30`), so it uses a `None` sentinel: omit it (the default `None`) to add no token and let the harness
    apply its own default — byte-identical argv. A value (even one equal to the core default, AND an explicit
    `0`) forwards `--mag-size <rounds>` (value-flag) on BOTH paths via `MatchParams.rules`. The `0` case is
    degenerate: a `0`-capacity magazine spawns a pawn with `0` ammo and every reload refills to `0`, so it can
    NEVER fire a ranged shot — so an explicit `0` must forward, never be swallowed. A non-None negative or
    `> 65535` value raises before any spawn (core takes it as a `u16`, so an out-of-range forward would wrap
    into a capacity the caller never asked for — rejected loudly, mirroring the harness's `u16` parse).

    Pass `max_speed` (a non-negative `i32` per-tick pace, `0`..=`2**31-1`) to set the max planar displacement
    a pawn slides per tick at full move intent, in position units — higher is a faster movement pace. Like
    `mag_size` it is a base-balance value with a NON-ZERO core default (`200` — 0.2 m/tick, ~6 m/s at 30 Hz),
    so it uses a `None` sentinel: omit it (the default `None`) to add no token and let the harness apply its
    own default — byte-identical argv. A value (even one equal to the core default, AND an explicit `0`)
    forwards `--max-speed <units>` (value-flag) on BOTH paths via `MatchParams.rules`. The `0` case is
    degenerate: a `0`-speed pawn is FROZEN in place (unable to walk, dodge, or chase), not the default pace —
    so an explicit `0` must forward, never be swallowed. UNLIKE the `u16` knobs above
    (`damage`/`fire_cooldown`/`mag_size`, `0`..=`65535`) this is a non-negative `i32`, so the range is
    `0`..=`2**31-1`: a non-None negative (no movement meaning — core never walks a pawn backward by the cap)
    or `> 2**31-1` value raises before any spawn, mirroring the harness's `u32`-then-`i32` `parse_max_speed`
    fence.

    Pass `perception_range` (a non-negative `i32` sightline radius, `0`..=`2**31-1`) to set how far a seat
    perceives another entity, in position units — an entity is observed only if it lies within this radius of
    the eye (then further gated by the forward FOV cone), so a shorter range is less battlefield awareness.
    Like `max_speed` it is a non-negative `i32` base-balance value with a NON-ZERO core default
    (`40 * POSITION_SCALE`, 40 m), so it uses a `None` sentinel: omit it (the default `None`) to add no token
    and let the harness apply its own default — byte-identical argv. A value (even one equal to the core
    default, AND an explicit `0`) forwards `--perception-range <units>` (value-flag) on BOTH paths via
    `MatchParams.rules`. The `0` case is degenerate: a `0`-range seat is BLIND — it perceives no entity at any
    distance — not the default sightline, so an explicit `0` must forward, never be swallowed. The range is
    `0`..=`2**31-1` (the `i32` shape, NOT `u16`): a non-None negative (meaningless for a radius) or
    `> 2**31-1` value raises before any spawn, mirroring the harness's `u32`-then-`i32` `parse_perception_range`
    fence.

    Pass `weapon_range` (a non-negative `i32` beam reach, `0`..=`2**31-1`) to set how far a beam-hitscan shot
    reaches, in position units — the sim resolves a hitscan hit only within this reach and expires a traveling
    projectile once it has flown past it, so a shorter range is closer-quarters engagements. The
    engagement-distance companion to `perception_range` (`perception_range` sets how far a seat SEES,
    `weapon_range` how far it can HIT). Like `perception_range` it is a non-negative `i32` base-balance value
    with a NON-ZERO core default (`30 * POSITION_SCALE`, 30 m), so it uses a `None` sentinel: omit it (the
    default `None`) to add no token and let the harness apply its own default — byte-identical argv. A value
    (even one equal to the core default, AND an explicit `0`) forwards `--weapon-range <units>` (value-flag)
    on BOTH paths via `MatchParams.rules`. The `0` case is degenerate: a `0`-range weapon reaches nothing — it
    lands no ranged hit at any distance and expires every projectile at the muzzle — not the default reach, so
    an explicit `0` must forward, never be swallowed. The range is `0`..=`2**31-1` (the `i32` shape, NOT
    `u16`): a non-None negative (meaningless for a reach) or `> 2**31-1` value raises before any spawn,
    mirroring the harness's `u32`-then-`i32` `parse_weapon_range` fence.

    Pass `hit_radius` (a non-negative `i32` lateral hit tolerance, `0`..=`2**31-1`) to set the beam half-width,
    in position units — the sim counts a hitscan hit only when the squared perpendicular distance from the
    shot axis is `<= hit_radius**2`, so a wider radius forgives more aim error and a narrower one demands a more
    dead-centre target (in `weapon_mode="projectile"` it doubles as the pawn-body half-width a projectile's swept
    path must reach). The accuracy lever, orthogonal to `weapon_range` (that sets how FAR a shot reaches, this how
    WIDE its tolerance). Like `weapon_range` it is a non-negative `i32` base-balance value with a NON-ZERO core
    default (`1500`, 1.5 m), so it uses a `None` sentinel: omit it (the default `None`) to add no token and let the
    harness apply its own default — byte-identical argv. A value (even one equal to the core default, AND an
    explicit `0`) forwards `--hit-radius <units>` (value-flag) on BOTH paths via `MatchParams.rules`. The `0` case
    is degenerate: a `0`-radius beam is pin-precise — only a perfectly on-axis shot connects (squared perpendicular
    distance `<= 0`) — not the default tolerance, so an explicit `0` must forward, never be swallowed. The range is
    `0`..=`2**31-1` (the `i32` shape, NOT `u16`): a non-None negative (a squared perpendicular distance is never
    `< 0`, so a negative is meaningless) or `> 2**31-1` value raises before any spawn, mirroring the harness's
    `u32`-then-`i32` `parse_hit_radius` fence.

    Pass `melee_cooldown` (a `u16` tick count, `0`..=`65535`) to set the ticks BETWEEN melee swings — the swing
    cadence in `weapon_mode="melee"` (a pawn may swing only when its melee cooldown is `0`, so a lower value is a
    faster cleave). The melee twin of the ranged `fire_cooldown`, read only in `weapon_mode="melee"` (a hitscan or
    projectile match ignores it at runtime) but still bound into the `Rules` replay digest. Like `damage` it is a
    base-balance value with a NON-ZERO core default (`15` ticks), so it uses a `None` sentinel: omit it (the
    default `None`) to add no token and let the harness apply its own default — byte-identical argv. A value (even
    one equal to the core default, AND an explicit `0`) forwards `--melee-cooldown <ticks>` (value-flag) on BOTH
    paths via `MatchParams.rules`. The `0` case is the sharp degenerate: melee is gated only on `melee_cooldown`
    (it needs no ammo), so a `0` cooldown swings on EVERY tick — a continuous cleave that downs every enemy in arc
    the instant they enter it — not the default cadence, so an explicit `0` must forward, never be swallowed. The
    range is `0`..=`65535` (the `u16` shape, NOT the `i32` `0`..=`2**31-1` of the radius/reach twins): a non-None
    negative or `> 65535` value raises before any spawn, mirroring the harness's `u16` `parse()` fence.

    Pass `melee_damage` (a `u16` per-swing HP, `0`..=`65535`) to set the health one `weapon_mode="melee"` swing
    subtracts from each enemy it cleaves (clamped to the target's health, so a huge value one-shots and cannot
    overkill into a wrap) — lower damage is more swings to down a pawn. The melee twin of the ranged `damage`,
    read only in `weapon_mode="melee"` (a hitscan or projectile match ignores it at runtime) but still bound into
    the `Rules` replay digest. Like `melee_cooldown` it is a `u16` base-balance value with a NON-ZERO core default
    (`50`, two swings to down a full pawn), so it uses a `None` sentinel: omit it (the default `None`) to add no
    token and let the harness apply its own default — byte-identical argv. A value (even one equal to the core
    default, AND an explicit `0`) forwards `--melee-damage <hp>` (value-flag) on BOTH paths via `MatchParams.rules`.
    The `0` case is degenerate: a `0`-damage swing harms nobody — a harmless melee pawn, every swing whiffs even on
    a perfect arc hit — not the default damage, so an explicit `0` must forward, never be swallowed. The range is
    `0`..=`65535` (the `u16` shape, NOT the `i32` of the radius/reach twins): a non-None negative or `> 65535`
    value raises before any spawn, mirroring the harness's `u16` `parse()` fence.

    Pass `melee_range` (a non-negative `i32` cleave reach, `0`..=`2**31-1`) to set how far a `weapon_mode="melee"`
    swing reaches, in position units — the sim cleaves EVERY enemy whose centre is within this distance (squared:
    `(melee_range)**2`) AND inside the frontal arc, so a longer reach cleaves from farther and a shorter one
    demands closing the distance (read only in `weapon_mode="melee"` but still bound into the `Rules` replay
    digest). The melee twin of the ranged `weapon_range`. Like `hit_radius` it is a non-negative `i32` base-balance
    value with a NON-ZERO core default (`2 * POSITION_SCALE`, 2 m), so it uses a `None` sentinel: omit it (the
    default `None`) to add no token and let the harness apply its own default — byte-identical argv. A value (even
    one equal to the core default, AND an explicit `0`) forwards `--melee-range <units>` (value-flag) on BOTH paths
    via `MatchParams.rules`. The `0` case is degenerate: a `0`-reach swing cleaves nothing — its squared reach is
    `0`, so no enemy centre is ever within it (a harmless melee pawn) — not the default reach, so an explicit `0`
    must forward, never be swallowed. The range is `0`..=`2**31-1` (the `i32` shape, NOT `u16`): a non-None
    negative (core squares the reach, so a negative would cleave as far as its magnitude — a misleading sign) or
    `> 2**31-1` value raises before any spawn, mirroring the harness's `u32`-then-`i32` `parse_melee_range`
    fence.

    Pass `projectile_speed` (a non-negative `i32` per-tick travel speed, `0`..=`2**31-1`) to set how fast a
    `weapon_mode="projectile"` shot flies, in position units per tick — a fired projectile spawns and travels this
    far each tick along the firing octant, hitting only when its swept path crosses a body, so a faster shot
    closes the gap to a strafing target sooner and a slower one is easier to dodge (read only in
    `weapon_mode="projectile"`, where core snaps it to the octant and clamps it to a per-tick bound at spawn, but
    still bound into the `Rules` replay digest). The travel-speed lever for the projectile weapon mode. Like
    `melee_range` it is a non-negative `i32` base-balance value with a NON-ZERO core default (`2 * POSITION_SCALE`,
    2 m/tick), so it uses a `None` sentinel: omit it (the default `None`) to add no token and let the harness apply
    its own default — byte-identical argv. A value (even one equal to the core default, AND an explicit `0`)
    forwards `--projectile-speed <units>` (value-flag) on BOTH paths via `MatchParams.rules`. The `0` case is
    degenerate: a `0`-speed shot never leaves the muzzle and is force-expired by core's termination backstop
    landing no hit — not the default speed, so an explicit `0` must forward, never be swallowed. The range is
    `0`..=`2**31-1` (the `i32` shape, NOT `u16`): a non-None negative (core flies a projectile FORWARD along the
    octant, never backward) or `> 2**31-1` value raises before any spawn, mirroring the harness's `u32`-then-`i32`
    `parse_projectile_speed` fence.

    Pass `action_deadline_micros` (a per-action wall-clock budget in microseconds, `0`..=`2**32-1`) to set how long
    a seat has to answer an observation before the match-runner forfeits that tick on its behalf — the timing knob
    the transport enforces, not a combat balance value (a tighter budget pressures a slow agent; a slow seat that
    misses it skips its action that tick). Like `projectile_speed` it is a base-balance value with a NON-ZERO core
    default (`50_000`, 50 ms), so it uses a `None` sentinel: omit it (the default `None`) to add no token and let the
    harness apply its own default — byte-identical argv. A value (even one equal to the core default, AND an explicit
    `0`) forwards `--action-deadline-micros <micros>` (value-flag) on BOTH paths via `MatchParams.rules`. The `0` case
    is degenerate: a `0`-microsecond budget forfeits every seat's tick the instant it is offered (no seat can ever
    act) — not the default budget, so an explicit `0` must forward, never be swallowed. The range is `0`..=`2**32-1`
    (the `u32` shape, wider than the `i32` knobs and with no negative): a non-None negative or `> 2**32-1` value raises
    before any spawn, mirroring the harness's `u32` parse.

    Pass `pickup_radius` (a collection reach in core distance units, `0`..=`2**31-1`) to set how close a pawn's centre
    must get to a world pickup to collect it — a wider radius grabs from farther, a tighter one demands closer contact.
    Like the other base-balance knobs it has a NON-ZERO core default (`1000`, a 1 m contact disc), so it uses a `None`
    sentinel: omit it (the default `None`) to add no token and let the harness apply its own default — byte-identical
    argv. A value (even one equal to the core default, AND an explicit `0`) forwards `--pickup-radius <units>`
    (value-flag) on BOTH paths via `MatchParams.rules`. The `0` case is degenerate: a `0` radius is collectible only by
    a pawn exactly on the pickup (effectively uncollectable) — not the default reach, so an explicit `0` must forward,
    never be swallowed. The range is `0`..=`2**31-1` (the `i32` shape like `melee_range`, NOT `u16`): a non-None negative
    (core compares a squared distance, never `< 0`) or `> 2**31-1` value raises before any spawn, mirroring the
    harness's `u32`-then-`i32` `parse_pickup_radius` fence.

    Pass `pickup_respawn_cooldown` (a tick count, `0`..=`65535`) to set how long a collected pickup stays dormant
    before it respawns at its spawn point — a deterministic per-tick countdown (no wall-clock) the sim arms when a
    pawn collects a pickup, so a longer cooldown keeps a contested heal/ammo spot empty longer and a shorter one
    refreshes it sooner (a pickup-free match never consults it). Like the other base-balance knobs it has a NON-ZERO
    core default (`300` ticks, ~10 s at 30 Hz), so it uses a `None` sentinel: omit it (the default `None`) to add no
    token and let the harness apply its own default — byte-identical argv. A value (even one equal to the core default,
    AND an explicit `0`) forwards `--pickup-respawn-cooldown <ticks>` (value-flag) on BOTH paths via
    `MatchParams.rules`. The `0` case is degenerate: a `0` cooldown respawns a collected pickup the very next tick
    (effectively always present) — not the default dormancy, so an explicit `0` must forward, never be swallowed. The
    range is `0`..=`65535` (the `u16` shape like `melee_cooldown`/`melee_damage`, NOT the `i32` reach knobs or the
    `u32` deadline): a non-None negative or `> 65535` value raises before any spawn, mirroring the harness's `u16`
    parse.

    Pass `spawn_jitter` (a per-axis jitter in position units, `0`..=`2**31-1`) to set how far the PRNG perturbs each
    seat's opening position: when the sim seats pawns around the opening line it draws a per-axis offset in
    `[-spawn_jitter, +spawn_jitter]`, so a wider jitter scatters the opening more (the seed perturbs it) and a `0`
    jitter is a fully deterministic opening (every seed seats pawns at the exact same spots). Like the other
    base-balance knobs it has a NON-ZERO core default (`2000`, a 2 m per-axis jitter), so it uses a `None` sentinel:
    omit it (the default `None`) to add no token and let the harness apply its own default — byte-identical argv. A
    value (even one equal to the core default, AND an explicit `0`) forwards `--spawn-jitter <units>` (value-flag) on
    BOTH paths via `MatchParams.rules`. The `0` case is degenerate: a `0` jitter is a fully deterministic opening with
    no per-seed perturbation — not the default scatter, so an explicit `0` must forward, never be swallowed. The range
    is `0`..=`2**31-1` (the `i32` shape like `pickup_radius`, NOT `u16`): a non-None negative (core would invert the
    `[-jitter, +jitter]` draw span, and independently rejects `spawn_jitter < 0` as an invalid `Rules`) or `> 2**31-1`
    value raises before any spawn, mirroring the harness's `u32`-then-`i32` `parse_spawn_jitter` fence.

    Pass `spawn_radius` (the spawn-line half-width in position units, `0`..=`2**31-1`) to set how far apart the seats
    open: the sim spreads the seats evenly across `[-spawn_radius, +spawn_radius]` on the X axis at match start, then
    `spawn_jitter` perturbs each, so a wider radius opens the seats farther apart and a `0` radius stacks every seat
    on the X origin (only the jitter then separates them). Like the other base-balance knobs it has a NON-ZERO core
    default (`20000`, a 20 m half-width), so it uses a `None` sentinel: omit it (the default `None`) to add no token
    and let the harness apply its own default — byte-identical argv. A value (even one equal to the core default, AND
    an explicit `0`) forwards `--spawn-radius <units>` (value-flag) on BOTH paths via `MatchParams.rules`. The `0`
    case is degenerate: a `0` radius stacks every seat on the X origin — not the default spread, so an explicit `0`
    must forward, never be swallowed. The range is `0`..=`2**31-1` (the `i32` shape like `spawn_jitter`, NOT `u16`): a
    non-None negative (core would invert the `[-radius, +radius]` spread span, and independently rejects
    `spawn_radius < 0` as an invalid `Rules`) or `> 2**31-1` value raises before any spawn, mirroring the harness's
    `u32`-then-`i32` `parse_spawn_radius` fence."""
    ids = agent_ids or {s: f"agent-{s}" for s in seats}
    keys = signing_keys or {}
    humans = human_seats or []
    if ladder_file is not None and mode is None:
        raise ValueError(
            "ladder_file persists the matchmaker's ranked ladder, which only moves on the "
            "--mode path; the direct (mode=None) path ignores it — pass mode='agent' (or 'mixed')"
        )
    if not 0 <= fov <= 4:
        raise ValueError(f"fov is an octant spread in 0..=4 (4 = full circle); got {fov}")
    if aim_mode not in ("octant", "fine"):
        raise ValueError(f"aim_mode is 'octant' or 'fine'; got {aim_mode!r}")
    if not 0 <= gravity <= 2**31 - 1:
        # Core gates vertical physics on gravity > 0, so a negative would silently run a 2D match
        # (off) and a value past i32::MAX would wrap the fall integration negative (also off) —
        # reject both loudly here, mirroring the harness's u32-then-i32 parse_gravity fence.
        raise ValueError(f"gravity is a downward magnitude in 0..=2**31-1 (0 = off); got {gravity}")
    if not 0 <= starting_ticks <= 2**32 - 1:
        # Core opens the Starting countdown on starting_ticks > 0 and streams starting_remaining (a
        # u32) down from it, so a negative reads as "no countdown" (off) and a value past u32::MAX
        # would wrap — reject both loudly here, mirroring the harness's u32 --starting-ticks parse
        # (which panics on a negative), rather than forwarding a value the harness aborts on.
        raise ValueError(
            f"starting_ticks is a pre-live countdown in 0..=2**32-1 (0 = open Live directly); got {starting_ticks}"
        )
    if not 0 <= vertical_hit_tolerance <= 2**31 - 1:
        # Core gates z-coupled hits on vertical_hit_tolerance > 0, so a negative reads as off (planar)
        # and a value past i32::MAX wraps the band negative (also off) — reject both loudly here,
        # mirroring the harness's u32-then-i32 parse_vertical_hit_tolerance fence.
        raise ValueError(
            f"vertical_hit_tolerance is an elevation band in 0..=2**31-1 (0 = off); got {vertical_hit_tolerance}"
        )
    if not 0 <= fall_damage <= 65535:
        # Core takes fall_damage as a u16, so a negative is meaningless and a value past 65535 would
        # wrap into a magnitude the caller never asked for (e.g. 65536 -> 0, every landing safe) —
        # reject both loudly here, mirroring the harness's u16 --fall-damage parse.
        raise ValueError(f"fall_damage is a landing magnitude in 0..=65535 (0 = safe); got {fall_damage}")
    if not 0 <= knockback_velocity <= 2**31 - 1:
        # Core adds knockback_velocity as an upward z impulse, so a negative would launch the hit
        # target DOWNWARD into the floor and a value past i32::MAX would wrap the impulse (also off) —
        # reject both loudly here, mirroring the harness's u32-then-i32 parse_knockback_velocity fence.
        raise ValueError(
            f"knockback_velocity is an upward impulse in 0..=2**31-1 (0 = off); got {knockback_velocity}"
        )
    if not 0 <= fall_damage_threshold <= 2**31 - 1:
        # Core compares `impact > threshold`, so a negative gate makes EVERY landing (impact > 0) wound
        # — the inverse of raising the bar — and a value past i32::MAX wraps the gate negative (same
        # footgun); reject both loudly here, mirroring the harness's u32-then-i32 parse_fall_damage_threshold.
        raise ValueError(
            f"fall_damage_threshold is an impact-speed gate in 0..=2**31-1 (0 = gate open); got {fall_damage_threshold}"
        )
    if not 0 <= knockback_horizontal <= 2**31 - 1:
        # Core gates the planar shove on knockback_horizontal > 0, so a negative is INERT (silently no
        # shove — the operator dialed a pull and got nothing) and a value past i32::MAX wraps; reject
        # both loudly here, mirroring the harness's u32-then-i32 parse_knockback_horizontal fence.
        raise ValueError(
            f"knockback_horizontal is a planar shove in 0..=2**31-1 (0 = off); got {knockback_horizontal}"
        )
    if not 0 <= dash_cooldown <= 65535:
        # Core takes dash_cooldown as a u16, so a negative is meaningless and a value past 65535 would
        # wrap into a cadence the caller never asked for (e.g. 65536 -> 0, which ALSO reads as "dash off",
        # so the wrap is doubly wrong) — reject both loudly here, mirroring the harness's u16 parse.
        raise ValueError(f"dash_cooldown is a tick cadence in 0..=65535 (0 = off); got {dash_cooldown}")
    if not 0 <= pawn_radius <= 2**31 - 1:
        # Core gates pawn-vs-pawn occupancy on pawn_radius > 0, so a negative is INERT (silently no
        # collision — the operator dialed a body and got a ghost) and a value past i32::MAX wraps; reject
        # both loudly here, mirroring the harness's u32-then-i32 parse_pawn_radius fence.
        raise ValueError(f"pawn_radius is an occupancy radius in 0..=2**31-1 (0 = off); got {pawn_radius}")
    if not 0 <= pawn_height <= 2**31 - 1:
        # Core gates the z-band on pawn_height > 0, so a negative reads as off (planar occupancy) and a
        # value past i32::MAX wraps the band negative (also off) — reject both loudly here, mirroring the
        # harness's u32-then-i32 parse_pawn_height fence.
        raise ValueError(f"pawn_height is an occupancy band in 0..=2**31-1 (0 = planar); got {pawn_height}")
    if not 0 <= max_shield <= 65535:
        # Core takes max_shield as a u16, so a negative is meaningless and a value past 65535 would wrap
        # into a cap the caller never asked for (e.g. 65536 -> 0, which ALSO reads as "shield off", so the
        # wrap is doubly wrong) — reject both loudly here, mirroring the harness's u16 parse.
        raise ValueError(f"max_shield is a shield-pool cap in 0..=65535 (0 = off); got {max_shield}")
    if not 0 <= perception_memory <= 65535:
        # Core takes perception_memory as a u16 tick window (0 = off), so a negative is meaningless and a
        # value past 65535 would wrap into a window the caller never asked for (65536 -> 0, which ALSO reads
        # as "memory off", so the wrap is doubly wrong) — reject both loudly here, mirroring the harness u16.
        raise ValueError(f"perception_memory is a memory window in ticks 0..=65535 (0 = off); got {perception_memory}")
    if start_health is not None and not 0 <= start_health <= 65535:
        # Core takes start_health as a u16. None means "let the harness apply its default" (no token); a
        # given value must be in range, else an out-of-range forward would wrap into a pool the caller
        # never asked for (65536 -> 0, an already-downed spawn) — reject loudly, mirroring the harness u16.
        raise ValueError(f"start_health is an HP pool in 0..=65535 (None = harness default); got {start_health}")
    if damage is not None and not 0 <= damage <= 65535:
        # Core takes damage as a u16. None means "let the harness apply its default" (no token); a given
        # value must be in range, else an out-of-range forward would wrap into a per-shot HP the caller never
        # asked for (65536 -> 0, a shot that can never down a pawn) — reject loudly, mirroring the harness u16.
        raise ValueError(f"damage is per-shot HP in 0..=65535 (None = harness default); got {damage}")
    if fire_cooldown is not None and not 0 <= fire_cooldown <= 65535:
        # Core takes fire_cooldown as a u16. None means "let the harness apply its default" (no token); a
        # given value must be in range, else an out-of-range forward would wrap into a cadence the caller
        # never asked for (65536 -> 0, a fire-every-tick pawn) — reject loudly, mirroring the harness u16.
        raise ValueError(f"fire_cooldown is a tick count in 0..=65535 (None = harness default); got {fire_cooldown}")
    if mag_size is not None and not 0 <= mag_size <= 65535:
        # Core takes mag_size as a u16. None means "let the harness apply its default" (no token); a given
        # value must be in range, else an out-of-range forward would wrap into a capacity the caller never
        # asked for (65536 -> 0, an unfireable magazine) — reject loudly, mirroring the harness u16.
        raise ValueError(f"mag_size is a round count in 0..=65535 (None = harness default); got {mag_size}")
    if max_speed is not None and not 0 <= max_speed <= 2**31 - 1:
        # Core takes max_speed as a non-negative i32. None means "let the harness apply its default" (no
        # token); a given value must be in range, else a negative (no movement meaning — core never walks a
        # pawn backward by the cap) or a value past i32::MAX (which wraps negative) would forward a footgun —
        # reject loudly, mirroring the harness's u32-then-i32 parse_max_speed fence.
        raise ValueError(f"max_speed is a per-tick pace in 0..=2**31-1 (None = harness default); got {max_speed}")
    if perception_range is not None and not 0 <= perception_range <= 2**31 - 1:
        # Core takes perception_range as a non-negative i32. None means "let the harness apply its default"
        # (no token); a given value must be in range, else a negative (meaningless for a radius — perceives
        # nothing) or a value past i32::MAX (which wraps negative) would forward a footgun — reject loudly,
        # mirroring the harness's u32-then-i32 parse_perception_range fence.
        raise ValueError(
            f"perception_range is a sightline radius in 0..=2**31-1 (None = harness default); got {perception_range}"
        )
    if weapon_range is not None and not 0 <= weapon_range <= 2**31 - 1:
        # Core takes weapon_range as a non-negative i32. None means "let the harness apply its default" (no
        # token); a given value must be in range, else a negative (meaningless for a reach — and core squares
        # it, so a negative would reach as far as its magnitude) or a value past i32::MAX (which wraps
        # negative) would forward a footgun — reject loudly, mirroring the harness u32-then-i32 fence.
        raise ValueError(
            f"weapon_range is a beam reach in 0..=2**31-1 (None = harness default); got {weapon_range}"
        )
    if hit_radius is not None and not 0 <= hit_radius <= 2**31 - 1:
        # Core takes hit_radius as a non-negative i32. None means "let the harness apply its default" (no token);
        # a given value must be in range, else a negative (a squared perpendicular distance is never < 0, so a
        # negative tolerance is meaningless) or a value past i32::MAX (which wraps negative) would forward a
        # footgun — reject loudly, mirroring the harness's u32-then-i32 parse_hit_radius fence.
        raise ValueError(
            f"hit_radius is a lateral tolerance in 0..=2**31-1 (None = harness default); got {hit_radius}"
        )
    if melee_cooldown is not None and not 0 <= melee_cooldown <= 65535:
        # Core takes melee_cooldown as a u16. None means "let the harness apply its default" (no token); a given
        # value must be in range, else an out-of-range forward would wrap into a cadence the caller never asked
        # for (65536 -> 0, a swing-every-tick continuous cleave) — reject loudly, mirroring the harness u16.
        raise ValueError(
            f"melee_cooldown is a tick count in 0..=65535 (None = harness default); got {melee_cooldown}"
        )
    if melee_damage is not None and not 0 <= melee_damage <= 65535:
        # Core takes melee_damage as a u16. None means "let the harness apply its default" (no token); a given
        # value must be in range, else an out-of-range forward would wrap into a per-swing HP the caller never
        # asked for (65536 -> 0, a harmless swing) — reject loudly, mirroring the harness u16.
        raise ValueError(
            f"melee_damage is per-swing HP in 0..=65535 (None = harness default); got {melee_damage}"
        )
    if melee_range is not None and not 0 <= melee_range <= 2**31 - 1:
        # Core takes melee_range as a non-negative i32. None means "let the harness apply its default" (no token);
        # a given value must be in range, else a negative (core squares the reach, so a negative would cleave as
        # far as its magnitude) or a value past i32::MAX (which wraps negative) would forward a footgun — reject
        # loudly, mirroring the harness's u32-then-i32 parse_melee_range fence.
        raise ValueError(
            f"melee_range is a cleave reach in 0..=2**31-1 (None = harness default); got {melee_range}"
        )
    if projectile_speed is not None and not 0 <= projectile_speed <= 2**31 - 1:
        # Core takes projectile_speed as a non-negative i32. None means "let the harness apply its default" (no
        # token); a given value must be in range, else a negative (core flies a projectile forward along the
        # octant, never backward) or a value past i32::MAX (which wraps negative) would forward a footgun — reject
        # loudly, mirroring the harness's u32-then-i32 parse_projectile_speed fence.
        raise ValueError(
            f"projectile_speed is a per-tick travel speed in 0..=2**31-1 (None = harness default); got {projectile_speed}"
        )
    if action_deadline_micros is not None and not 0 <= action_deadline_micros <= 2**32 - 1:
        # Core takes action_deadline_micros as a u32 (wider than the i32 knobs, no negative). None means "let the
        # harness apply its own default" (no token); a given value must be in range, else a negative or a value past
        # u32::MAX (which wraps) would forward a footgun the harness aborts on — reject loudly, mirroring the
        # harness's u32 parse.
        raise ValueError(
            f"action_deadline_micros is a per-action microsecond budget in 0..=2**32-1 (None = harness default); got {action_deadline_micros}"
        )
    if pickup_radius is not None and not 0 <= pickup_radius <= 2**31 - 1:
        # Core takes pickup_radius as a non-negative i32. None means "let the harness apply its default" (no token);
        # a given value must be in range, else a negative (core compares a squared distance, never < 0) or a value
        # past i32::MAX (which wraps negative) would forward a footgun — reject loudly, mirroring the harness's
        # u32-then-i32 parse_pickup_radius fence.
        raise ValueError(
            f"pickup_radius is a collection reach in 0..=2**31-1 (None = harness default); got {pickup_radius}"
        )
    if pickup_respawn_cooldown is not None and not 0 <= pickup_respawn_cooldown <= 65535:
        # Core takes pickup_respawn_cooldown as a u16. None means "let the harness apply its default" (no token); a
        # given value must be in range, else an out-of-range forward would wrap into a dormancy the caller never
        # asked for (65536 -> 0, an always-present pickup) — reject loudly, mirroring the harness u16.
        raise ValueError(
            f"pickup_respawn_cooldown is a tick count in 0..=65535 (None = harness default); got {pickup_respawn_cooldown}"
        )
    if spawn_jitter is not None and not 0 <= spawn_jitter <= 2**31 - 1:
        # Core takes spawn_jitter as a non-negative i32. None means "let the harness apply its default" (no token);
        # a given value must be in range, else a negative (core would invert the [-jitter, +jitter] draw span, and
        # rejects spawn_jitter < 0 as an invalid Rules) or a value past i32::MAX (which wraps negative) would forward
        # a footgun — reject loudly, mirroring the harness's u32-then-i32 parse_spawn_jitter fence.
        raise ValueError(
            f"spawn_jitter is a per-axis opening jitter in 0..=2**31-1 (None = harness default); got {spawn_jitter}"
        )
    if spawn_radius is not None and not 0 <= spawn_radius <= 2**31 - 1:
        # Core takes spawn_radius as a non-negative i32. None means "let the harness apply its default" (no token);
        # a given value must be in range, else a negative (core would invert the [-radius, +radius] spread span, and
        # rejects spawn_radius < 0 as an invalid Rules) or a value past i32::MAX (which wraps negative) would forward
        # a footgun — reject loudly, mirroring the harness's u32-then-i32 parse_spawn_radius fence.
        raise ValueError(
            f"spawn_radius is a spawn-line half-width in 0..=2**31-1 (None = harness default); got {spawn_radius}"
        )
    if weapon_mode not in ("hitscan", "projectile", "melee"):
        raise ValueError(f"weapon_mode is 'hitscan', 'projectile', or 'melee'; got {weapon_mode!r}")
    argv = [harness, "--match-id", match_id, "--seed", str(seed), "--seats", str(len(seats))]
    if arena is not None:
        # Unconditional, before the mode block: --map drives both the direct and --mode
        # paths in the harness, so the arena loads however the roster is formed.
        argv += ["--map", arena]
    if perception_memory:
        argv += ["--perception-memory", str(perception_memory)]
    if fov != 4:
        # Unconditional, before the mode block: --fov narrows the cone on both the direct and
        # --mode paths (the matchmaker carries it via MatchParams.rules). Default 4 (full
        # circle) adds no token — byte-identical argv to the pre-flag SDK.
        argv += ["--fov", str(fov)]
    if aim_mode != "octant":
        # Same shape as --fov: select fine aim on both paths (the matchmaker carries it via
        # MatchParams.rules). Default "octant" adds no token — byte-identical argv.
        argv += ["--aim-mode", aim_mode]
    if friendly_fire:
        # A BARE presence flag (no value, unlike --fov/--aim-mode): enable allied damage on both
        # paths (the matchmaker carries it via MatchParams.rules). Default False adds no token —
        # byte-identical argv.
        argv += ["--friendly-fire"]
    if gravity:
        # Same shape as --fov: a value-flag magnitude on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (physics off) adds no token — byte-identical argv.
        argv += ["--gravity", str(gravity)]
    if starting_ticks:
        # Same shape as --gravity: a value-flag count on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (open Live directly) adds no token — byte-identical argv.
        argv += ["--starting-ticks", str(starting_ticks)]
    if weapon_mode != "hitscan":
        # Same shape as --aim-mode: a named-enum value-flag on both paths (the matchmaker carries
        # it via MatchParams.rules). Default "hitscan" adds no token — byte-identical argv.
        argv += ["--weapon-mode", weapon_mode]
    if vertical_hit_tolerance:
        # Same shape as --gravity: a value-flag band on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (combat planar) adds no token — byte-identical argv.
        argv += ["--vertical-hit-tolerance", str(vertical_hit_tolerance)]
    if fall_damage:
        # Same shape as --gravity: a value-flag magnitude on both paths (the matchmaker carries it
        # via MatchParams.rules). Default 0 (safe landings) adds no token — byte-identical argv.
        argv += ["--fall-damage", str(fall_damage)]
    if knockback_velocity:
        # Same shape as --gravity: a value-flag impulse on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (no impulse) adds no token — byte-identical argv.
        argv += ["--knockback-velocity", str(knockback_velocity)]
    if wall_slide:
        # A BARE presence flag (no value, like --friendly-fire): slide a grazing move along a blocker
        # on both paths (the matchmaker carries it via MatchParams.rules). Default False adds no token —
        # byte-identical argv. No preflight: a bool cannot be out of range.
        argv += ["--wall-slide"]
    if fall_damage_threshold:
        # Same shape as --gravity: a value-flag gate on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (gate open) adds no token — byte-identical argv.
        argv += ["--fall-damage-threshold", str(fall_damage_threshold)]
    if knockback_horizontal:
        # Same shape as --gravity: a value-flag shove on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (no shove) adds no token — byte-identical argv.
        argv += ["--knockback-horizontal", str(knockback_horizontal)]
    if dash_cooldown:
        # Same shape as --gravity: a value-flag cadence on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (dash off) adds no token — byte-identical argv.
        argv += ["--dash-cooldown", str(dash_cooldown)]
    if pawn_radius:
        # Same shape as --gravity: a value-flag radius on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (occupancy off) adds no token — byte-identical argv.
        argv += ["--pawn-radius", str(pawn_radius)]
    if pawn_height:
        # Same shape as --gravity: a value-flag band on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (planar occupancy) adds no token — byte-identical argv.
        argv += ["--pawn-height", str(pawn_height)]
    if max_shield:
        # Same shape as --gravity: a value-flag cap on both paths (the matchmaker carries it via
        # MatchParams.rules). Default 0 (shield off) adds no token — byte-identical argv.
        argv += ["--max-shield", str(max_shield)]
    if start_health is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--start-health", str(start_health)]
    if damage is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--damage", str(damage)]
    if fire_cooldown is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--fire-cooldown", str(fire_cooldown)]
    if mag_size is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--mag-size", str(mag_size)]
    if max_speed is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--max-speed", str(max_speed)]
    if perception_range is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--perception-range", str(perception_range)]
    if weapon_range is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--weapon-range", str(weapon_range)]
    if hit_radius is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--hit-radius", str(hit_radius)]
    if melee_cooldown is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--melee-cooldown", str(melee_cooldown)]
    if melee_damage is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--melee-damage", str(melee_damage)]
    if melee_range is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--melee-range", str(melee_range)]
    if projectile_speed is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--projectile-speed", str(projectile_speed)]
    if action_deadline_micros is not None:
        # Per-action deadline value-flag on both paths (the matchmaker carries it via MatchParams.rules), forwarded
        # before the mode block so it reaches the direct and --mode paths alike. None (omitted) adds no token so the
        # harness applies its own default — byte-identical argv.
        argv += ["--action-deadline-micros", str(action_deadline_micros)]
    if pickup_radius is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--pickup-radius", str(pickup_radius)]
    if pickup_respawn_cooldown is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--pickup-respawn-cooldown", str(pickup_respawn_cooldown)]
    if spawn_jitter is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--spawn-jitter", str(spawn_jitter)]
    if spawn_radius is not None:
        # Base-balance value-flag on both paths (the matchmaker carries it via MatchParams.rules). None
        # (omitted) adds no token so the harness applies its own default — byte-identical argv.
        argv += ["--spawn-radius", str(spawn_radius)]
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
        if ladder_file is not None:
            argv += ["--ladder-file", str(ladder_file)]
    with SubprocessGateway(argv, timeout=timeout, capture_stderr=ratings is not None) as gateway:
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
        if starts is not None:
            # Every client has its Start now (connected above), so record the static map
            # each seat received — the cover + pickups the chosen arena loaded.
            for s, client in clients.items():
                starts[s] = MatchStart(
                    blockers=list(client.blockers), pickup_points=list(client.pickup_points)
                )
        results: dict[int, MatchResult] = {}
        while len(results) < len(seats):
            for seat, client in clients.items():
                if client.done:
                    continue
                outcome = client.poll(policies[seat])
                if outcome is not None:
                    results[seat] = outcome
    if ratings is not None:
        # The gateway has closed (stderr fully drained), so the [ladder] line — keyed by
        # the match's own id (the minted one in matchmade mode, the argv id direct) — is
        # readable now. A seat the harness never settled (unranked) maps to None.
        standings = gateway.ladder(next(iter(results.values())).match_id)
        for s in seats:
            ratings[s] = standings.get(s)
    return results
