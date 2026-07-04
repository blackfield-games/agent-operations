"""Arena Gateway wire types — the Python mirror of `arena/proto` (arena-01).

These are the exact serde-JSON shapes the Rust Gateway emits and accepts, so a
Python agent built on this SDK is wire-compatible with the headless reference
arena (and, later, the UE5 server) without a second protocol definition. The
field names, the `snake_case` enums, and the internally-tagged `type` envelopes
match `arena/proto/src/lib.rs` byte-for-byte. The exact canonical frames are no
longer hand-copied on both sides: `arena_proto::frame_parity_vectors()` emits the
shared golden `arena/proto/tests/frame_parity.json` (one representative of every
GatewayMsg + AgentMsg frame, diffed by a Rust drift-gate), and `test_arena_client.py`
decodes/re-encodes each frame in that SAME file — so a wire-shape drift on either
side breaks loud against one source of truth, the sibling of the clamp-parity golden.

Like the Rust side, every spatial quantity is integer fixed-point — no floats on
the wire — so the same match hashes identically on every platform. Integer fields
mirror the Rust integer WIDTH (`u8`/`u16`/`u32`/`u64`/`i32`) and `match_id` is
UUID-shaped, so an out-of-domain value an agent might emit fails here with a clean
parity error rather than panicking the Rust server on deserialize. `extra=forbid`
makes an unexpected field a decode error rather than a silent drop, so any drift
between the two implementers fails loud. `PROTOCOL_VERSION` is exact-matched at the
handshake and bound into the replay/join settlement digests, so a change to what
those digests commit — the seed, the per-tick action stream, the handshake — bumps
it. The parity-bounded observation is *not* a settlement determinant (no digest
hashes it), so while the wire is pre-release its own-state shape is completed in
lockstep across both sides rather than minting a version that would invalidate
every prior settlement commitment for a HUD-only field.
"""

from __future__ import annotations

import uuid
from math import isqrt
from typing import Annotated, Literal

from pydantic import AfterValidator, BaseModel, ConfigDict, Field

PROTOCOL_VERSION = 1
POSITION_SCALE = 1000
VELOCITY_SCALE = 1000
MOVE_INTENT_SCALE = 1000

Phase = Literal["lobby", "starting", "live", "ended"]
EntityKindName = Literal["player", "projectile", "pickup"]

# Integer fields carry the Rust wire width, so a value the Rust type cannot hold
# is rejected on the Python side instead of panicking the server.
U8 = Annotated[int, Field(ge=0, le=2**8 - 1)]
U16 = Annotated[int, Field(ge=0, le=2**16 - 1)]
U32 = Annotated[int, Field(ge=0, le=2**32 - 1)]
U64 = Annotated[int, Field(ge=0, le=2**64 - 1)]
I32 = Annotated[int, Field(ge=-(2**31), le=2**31 - 1)]


def _uuid_str(v: str) -> str:
    uuid.UUID(v)  # arena-01 match_id is a Uuid; reject a non-UUID before the wire
    return v


MatchIdStr = Annotated[str, AfterValidator(_uuid_str)]


class _Wire(BaseModel):
    model_config = ConfigDict(extra="forbid")


class Vec2(_Wire):
    x: I32
    y: I32


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
    aim: U16
    buttons: ActionButtons

    def clamped(self) -> ActionIntent:
        """The anti-god-mode move clamp, mirroring `ActionIntent::clamped` in
        arena-proto exactly (integer `isqrt`, truncate toward zero) so a baseline
        can emit an already-legal move that the server passes through unchanged.
        An in-range request is returned untouched; an overlong one is scaled down
        to magnitude `MOVE_INTENT_SCALE`, never up.

        The bit-for-bit equivalence with the Rust reference clamp is machine-pinned
        by the shared `arena/proto/tests/clamp_parity.json` golden — emitted by
        `arena_proto::clamp_parity_vectors()`, diffed by the Rust drift-gate, and
        replayed through this method by `test_arena_client.py` — so a clamp drift on
        either side breaks loud against one source of truth."""
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
    protocol_version: U32
    match_id: MatchIdStr
    seat: U8
    tick: U64
    intent: ActionIntent


class SelfState(_Wire):
    seat: U8
    team: U16
    position: Vec2
    z: I32
    # Per-tick vertical velocity — the rate z changes, the vertical twin of velocity:
    # positive rising, negative falling, 0 at rest / when gravity is off. Own state only,
    # never on VisibleEntity (an enemy's vertical velocity is the same x-ray as its planar).
    z_vel: I32
    facing: U16
    velocity: Vec2
    health: U16
    max_health: U16
    # Remaining damage-absorbing shield, drained before health on a hit. Own state
    # only — like ammo/cooldown, never on VisibleEntity (an enemy's armor is x-ray).
    shield: U16
    ammo: U16
    # Ticks until this pawn may fire again as the next action sees it: 0 means a
    # fire submitted for this observation's tick is honored (subject to ammo). The
    # server already subtracts the start-of-tick decrement, so the fire-ready
    # predicate is simply `cooldown == 0`. Own state only — never on VisibleEntity.
    cooldown: U16
    # Ticks until this pawn may dash again as the next action sees it: 0 means an
    # ability dash submitted for this tick is honored (subject to a non-zero
    # move_dir). Same start-of-tick adjustment as cooldown, so the dash-ready
    # predicate is simply `dash_cooldown == 0`. Own state only — never on VisibleEntity.
    dash_cooldown: U16
    # The seat's own cumulative match score (damage dealt) — the same i32 the broadcast
    # scoreboard carries for this pawn. Own state only: never on VisibleEntity, since an
    # enemy's exact score is a tactical read the per-seat perception slice excludes.
    score: I32
    alive: bool


class VisibleEntity(_Wire):
    entity_id: U32
    kind: EntityKindName
    team: U16
    position: Vec2
    z: I32
    facing: U16
    in_line_of_sight: bool


class Observation(_Wire):
    protocol_version: U32
    match_id: MatchIdStr
    seat: U8
    tick: U64
    phase: Phase
    # Ticks left in the Starting countdown before the match goes Live (the live counter
    # during Starting, 0 once Live/Ended). serde(default)=0 on the Rust side, so a frame
    # written before the field existed decodes to a done-counting-down observation.
    starting_remaining: U32 = 0
    deadline_micros: U32
    own: SelfState
    visible: list[VisibleEntity]


class SeatOutcome(_Wire):
    seat: U8
    team: U16
    placement: U16
    score: I32
    alive_at_end: bool
    # True if this seat forfeited (leave/disconnect/persistent-silence) rather than
    # being killed in play; a forfeit is a DQ, so `placement` already reflects the
    # demotion below every non-forfeiter. Defaults to False for a result encoded before
    # the field existed (the Rust side is serde(default) — the same back-compat).
    forfeited: bool = False


class MatchResult(_Wire):
    protocol_version: U32
    match_id: MatchIdStr
    final_tick: U64
    outcomes: list[SeatOutcome]
    replay_hash: str


class MatchConfig(_Wire):
    tick_hz: U16
    max_ticks: U64
    bounds: Vec2
    seats: U8


class SeatInfo(_Wire):
    seat: U8
    team: U16
    controller: str


class Challenge(_Wire):
    nonce: str


class Welcome(_Wire):
    protocol_version: U32
    match_id: MatchIdStr
    seat: U8


class Reject(_Wire):
    reason: str


class Blocker(_Wire):
    """A static axis-aligned cover box on the ground plane, mirroring
    `arena_proto::Blocker` (`min`/`max` corners, `min <= max` per axis). Physical
    cover: it stops movement, fire, and sightlines in the reference core. Carried in
    `Start.blockers` so an agent learns the arena's static cover layout.

    `height` is the wall's top in position units: `0` (the default) is an
    infinitely-tall wall (the historical behavior — occludes any sightline crossing
    its footprint), a positive value lets a pawn high enough see/shoot OVER it. The
    field defaults to `0`, so a `Start` from a server that predates it parses to the
    infinitely-tall behavior."""

    min: Vec2
    max: Vec2
    height: I32 = 0


class Start(_Wire):
    match_id: MatchIdStr
    config: MatchConfig
    # The static vision/cover blockers the match runs under — the arena's cover
    # layout, the same map knowledge a human player has. Surfaced ONCE at match
    # start (never on the per-tick parity-bounded Observation, whose security
    # boundary is unchanged) so an AgentController can path around physical cover.
    # Mirrors the Rust GatewayMsg::Start field; defaults to empty so a Start without
    # it — an older server, or a match with no occluders — decodes unchanged.
    blockers: list[Blocker] = Field(default_factory=list)
    # The static pickup spawn POINTS — where world items spawn, the same map layout a
    # human knows. Deliberately position-only: the pickup kind/amount stays empirical
    # (an agent learns a pickup's effect by collecting it — the PickupKind posture), so
    # this is a bare Vec2 list that structurally cannot carry kind/amount. The live
    # collectible/dormant STATE stays on the per-tick Observation; this is only the
    # static layout. Mirrors the Rust GatewayMsg::Start field; defaults to empty.
    pickup_points: list[Vec2] = Field(default_factory=list)


GatewayMsg = Challenge | Welcome | Reject | Start | Observation | MatchResult


class ProtocolError(Exception):
    """An incoming frame is malformed or not a frame the agent expects here."""


def decode_gateway(frame: dict) -> GatewayMsg:
    """Decode one server→agent frame. Envelopes are internally tagged on `type`
    (`snake_case`), and the two newtype variants — `observe` / `end` — carry the
    `Observation` / `MatchResult` fields flattened next to `type`, exactly as the
    Rust `GatewayMsg` serializes them. This reconstruction is machine-pinned against
    the shared `arena/proto/tests/frame_parity.json` golden (one representative of
    every GatewayMsg + AgentMsg frame, emitted by `arena_proto::frame_parity_vectors()`
    and diffed by the Rust drift-gate) — see `test_arena_client.py`."""
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


_JOIN_DOMAIN = b"blackfield/arena/join/v1"


def _crypto():
    """secp256k1 + keccak256, imported lazily so unranked play (no signature) carries
    no crypto dependency. Ranked seats need `eth-keys` + `eth-hash[pycryptodome]` —
    pure Python, so unlike `coincurve` they build on 3.14; a missing extra fails here
    with a clear pointer rather than an opaque ImportError at module load."""
    try:
        from eth_hash.auto import keccak
        from eth_keys import keys
    except ImportError as e:  # pragma: no cover - both are declared dependencies
        raise RuntimeError(
            "ranked arena play needs eth-keys + eth-hash[pycryptodome] (declared "
            "dependencies) — reinstall the agents package"
        ) from e
    return keccak, keys


def join_digest(protocol_version: int, agent_id: str, nonce: bytes) -> bytes:
    """The 32-byte keccak256 commitment an agent signs to prove control of `agent_id`,
    byte-identical to `arena_proto::join_digest` so the Rust Gateway recovers the
    signer. Bytes = `keccak256( DOMAIN || protocol_version_be || len(agent_id) ||
    agent_id || len(nonce) || nonce )`, every `len` a big-endian u32. The DOMAIN tag
    separates it from the replay digest and mesh's hello digest (no signature crosses
    protocols); the length prefixes make the field boundaries unambiguous; the
    server-issued challenge `nonce` is folded IN (not merely sent alongside) so a
    captured Join can't be replayed against a fresh challenge. `nonce` is the raw bytes
    of the `Challenge.nonce` token (`Challenge.nonce.encode()`) — the Gateway verifies
    over the same `nonce.as_bytes()`, so the token stays opaque to both sides. Both
    MUST build it identically."""
    keccak, _ = _crypto()
    aid = agent_id.encode("utf-8")
    return keccak(
        _JOIN_DOMAIN
        + protocol_version.to_bytes(4, "big")
        + len(aid).to_bytes(4, "big")
        + aid
        + len(nonce).to_bytes(4, "big")
        + nonce
    )


def address_from_private_key(private_key: bytes) -> str:
    """The lowercase 0x agent address a secp256k1 key recovers to: `0x` +
    keccak256(uncompressed_pubkey[1:])[12:] — the same derivation
    `arena_proto::address_from_verifying_key` and mesh use, so one session key spans
    both subsystems. This is the `agent_id` a ranked seat must claim."""
    _, keys = _crypto()
    return keys.PrivateKey(private_key).public_key.to_address()


def sign_join(private_key: bytes, protocol_version: int, agent_id: str, nonce: bytes) -> str:
    """Sign `join_digest` with `private_key`; return the recoverable secp256k1
    `[r||s||v]` as hex (65 bytes) — the `Join.signature_hex` the Gateway verifies. `v`
    is the raw recovery id (0/1, NOT the 27/28 Ethereum offset) and eth-keys emits
    canonical low-S, both matching `verify_join_signature`'s gate exactly. `agent_id`
    MUST be `address_from_private_key(private_key)` (it is folded into the digest AND
    is the recovery target), else the Gateway recovers a different address and refuses
    the ranked seat."""
    _, keys = _crypto()
    digest = join_digest(protocol_version, agent_id, nonce)
    sig = keys.PrivateKey(private_key).sign_msg_hash(digest)
    return (sig.r.to_bytes(32, "big") + sig.s.to_bytes(32, "big") + bytes([sig.v])).hex()


def recover_join_signer(
    protocol_version: int, agent_id: str, nonce: bytes, signature_hex: str
) -> str | None:
    """The lowercase 0x address that signed `join_digest` under `signature_hex`, or
    `None` if it is not 65 valid `[r||s||v]` bytes / nothing recovers. The agent's
    self-check that its signature recovers to the address it claims before sending
    (the Gateway is authoritative — `arena_proto::verify_join_signature` also rejects
    high-S, which this recovery step does not)."""
    _, keys = _crypto()
    from eth_keys.exceptions import BadSignature, ValidationError

    try:
        raw = bytes.fromhex(signature_hex.removeprefix("0x"))
    except ValueError:
        return None
    if len(raw) != 65:
        return None
    digest = join_digest(protocol_version, agent_id, nonce)
    try:
        sig = keys.Signature(
            vrs=(raw[64], int.from_bytes(raw[:32], "big"), int.from_bytes(raw[32:64], "big"))
        )
        return sig.recover_public_key_from_msg_hash(digest).to_address()
    except (BadSignature, ValidationError):  # bad v/r/s, or no point recovers
        return None


def join_frame(agent_id: str, signature_hex: str = "") -> dict:
    """An `AgentMsg::Join` frame. An empty `signature_hex` is an unranked seat —
    matchmaking decides whether that is allowed; a ranked seat carries a `sign_join`
    over this connection's challenge nonce, which the Gateway recovers to `agent_id`."""
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
