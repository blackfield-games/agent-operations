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
    `on_match_start` carries no live state, only the layout a human sees on the map)."""

    def on_match_start(self, start: MatchStart) -> None: ...
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
        marks the client done), else None. Connects lazily on first call."""
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
    behavioral shield replay is a coupled follow-up."""
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
