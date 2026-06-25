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

import json
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
    Blocker,
    MatchResult,
    Observation,
    Start,
    Vec2,
    act_frame,
    decode_gateway,
    join_frame,
    leave_frame,
)

FIXED_MATCH = "550e8400-e29b-41d4-a716-446655440000"


def _frame_golden() -> dict:
    """The SAME committed Gateway wire-frame parity golden the Rust arena-proto
    drift-gate pins (arena/proto/tests/frame_parity.json), resolved by repo-relative
    path from THIS file (never the CWD), so it holds under validate.sh's agents-dir
    pytest run. A missing golden fails LOUD — it is the single source of truth for the
    Rust<->Python frame contract and must never be silently skipped (which would let a
    wire drift through). Regenerate after an intentional wire change with
    `cargo test -p arena-proto --test frame_parity regenerate_frame_parity_golden -- --ignored`."""
    path = Path(__file__).resolve().parents[1] / "arena" / "proto" / "tests" / "frame_parity.json"
    assert path.is_file(), f"shared frame-parity golden not found at {path}"
    return json.loads(path.read_text())


def _golden_frames(direction: str) -> list[dict]:
    return [c for c in _frame_golden()["frames"] if c["direction"] == direction]


def test_frame_golden_header_matches_proto():
    golden = _frame_golden()
    assert golden["domain"] == "blackfield/arena/frame-parity/v1"
    assert golden["protocol_version"] == proto.PROTOCOL_VERSION
    assert golden["match_id"] == FIXED_MATCH  # the fixed, byte-stable parity match id


def test_gateway_frames_decode_and_reencode_against_rust_golden():
    # Every server->agent frame in the SHARED golden (emitted by Rust
    # frame_parity_vectors, diffed by the Rust drift-gate) decodes via decode_gateway
    # into the typed model and re-encodes to the SAME wire body — the machine-checked
    # cross-implementer pin the hand-copied frames could not give. A Rust wire change
    # (which regenerates the golden) or a Python model drift breaks this against the one
    # source of truth, including the observe/end newtype flattening next to "type".
    expected_type = {
        "challenge": proto.Challenge, "welcome": proto.Welcome, "reject": proto.Reject,
        "start": Start, "observe": Observation, "end": MatchResult,
    }
    server = _golden_frames("server_to_agent")
    assert server, "golden carries no server->agent frames"
    seen = set()
    for case in server:
        frame = case["frame"]
        msg = decode_gateway(frame)
        seen.add(frame["type"])
        # decode_gateway must route each tag to the RIGHT concrete model — a mis-wired
        # tag->type mapping is caught here, not only by field shape.
        assert isinstance(msg, expected_type[frame["type"]]), (
            f"{case['label']} decoded to {type(msg).__name__}"
        )
        body = {k: v for k, v in frame.items() if k != "type"}
        if case["exact_reencode"]:
            assert msg.model_dump(mode="json") == body, (
                f"{case['label']} did not decode/re-encode to the golden body"
            )
    assert {"challenge", "welcome", "reject", "start", "observe", "end"} <= seen, (
        "a server->agent GatewayMsg variant is missing from the golden"
    )


def test_gateway_golden_backcompat_frames_fill_defaults():
    # The omit frames pin the serde(default) back-compat both implementers must keep:
    # an older server's Start (or a no-occluder / no-pickup match) decodes unchanged.
    # These frames OMIT the optional key entirely, so a re-encode can't reproduce them
    # (the default is filled back) — the wire-shape pin is the decode-to-default here.
    by_label = {c["label"]: c["frame"] for c in _frame_golden()["frames"]}
    legacy = decode_gateway(by_label["start_legacy_omits_optional_fields"])
    assert isinstance(legacy, Start)
    assert legacy.blockers == [] and legacy.pickup_points == []

    no_height = decode_gateway(by_label["start_blocker_omits_height"])
    assert isinstance(no_height, Start)
    assert no_height.blockers and no_height.blockers[0].height == 0


def test_match_config_wire_shape_matches_rust_golden():
    # MatchConfig rides the Start frame; pin it from the SAME golden (the hand-copy is
    # gone) by validating the populated Start's config and re-dumping it unchanged.
    start = {c["label"]: c["frame"] for c in _frame_golden()["frames"]}["start_populated"]
    cfg = proto.MatchConfig.model_validate(start["config"])
    assert cfg.model_dump(mode="json") == start["config"]


def test_agent_frames_encode_to_rust_golden():
    # Agent->server frames pin the Python ENCODERS against the golden: join_frame /
    # act_frame / leave_frame must produce the exact bytes the Rust serde emits. And
    # decode_gateway REFUSES an agent->server frame — the one-directional boundary (a
    # join/act/leave is not a valid server->agent message).
    agent = {c["frame"]["type"]: c["frame"] for c in _golden_frames("agent_to_server")}
    assert set(agent) == {"join", "act", "leave"}, "an agent->server AgentMsg variant is missing"

    join = agent["join"]
    assert join_frame(join["agent_id"], join["signature_hex"]) == join

    act = agent["act"]
    action = Action.model_validate({k: v for k, v in act.items() if k != "type"})
    assert act_frame(action) == act

    leave = agent["leave"]
    assert leave_frame(leave["reason"]) == leave

    for frame in agent.values():
        with pytest.raises(proto.ProtocolError):
            decode_gateway(frame)


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


def test_unknown_gateway_tag_raises():
    with pytest.raises(proto.ProtocolError):
        decode_gateway({"type": "teleport"})
    with pytest.raises(proto.ProtocolError):
        decode_gateway({"no_type": 1})


def test_unranked_join_frame_carries_an_empty_signature():
    # The golden's join frame is a ranked-ish placeholder ("0xcafe"); the SDK's own
    # default path is an UNRANKED seat — an empty signature_hex — so pin that too.
    assert join_frame("0xabc") == {
        "type": "join", "protocol_version": proto.PROTOCOL_VERSION,
        "agent_id": "0xabc", "signature_hex": "",
    }


# Cross-language known-answer vectors: arena_proto's OWN join_digest / sign_join over
# the canonical secp256k1 dev key + challenge from the Rust suite. Pinning the EXACT
# bytes proves the Python digest construction and the RFC6979-deterministic signature
# are byte-identical to the Gateway's — so a signature this SDK emits is the one
# verify_join_signature recovers and admits. Regenerate from arena_proto if the digest
# domain/layout or PROTOCOL_VERSION ever changes (both sides bump in lock-step).
_DEV_KEY = bytes.fromhex("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318")
_DEV_ADDR = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23"
# A second distinct ranked key, so a 2-seat ranked e2e gives each seat its own
# key-derived identity (ranked() recovers each address independently).
_DEV_KEY2 = bytes.fromhex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
_DEV_NONCE = b"arena-challenge-nonce"
_GOLDEN_DIGEST = "099de3d1b29be2ae5bf35f85b55f43711757cf609229be790d0acebe35f178dd"
_GOLDEN_SIG = (
    "3916c5207f17a13677b955c5179113ffbf054b56ad9953f47c187d5f58e11673"
    "634419a9a57b018ff14c85bda46716b1706d318fa1411b84f2c33989eb55493101"
)
_SECP256K1_N = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141


def test_address_from_private_key_matches_the_rust_derivation():
    # The agent_id a ranked seat claims is the lowercase 0x address the key recovers
    # to — the same derivation arena_proto::address_from_verifying_key uses, so one
    # session key spans arena + mesh.
    assert proto.address_from_private_key(_DEV_KEY) == _DEV_ADDR


def test_join_digest_is_byte_identical_to_arena_proto():
    # KAT: the keccak256 commitment must equal the Rust join_digest byte-for-byte, or
    # the Gateway hashes different bytes and never recovers the signer.
    got = proto.join_digest(proto.PROTOCOL_VERSION, _DEV_ADDR, _DEV_NONCE)
    assert got.hex() == _GOLDEN_DIGEST


def test_sign_join_reproduces_the_rust_signature():
    # KAT (the strongest wire-compat proof): RFC6979 is deterministic, so the agent's
    # signature must equal the one the Rust signer (k256 sign_prehash_recoverable)
    # produces — same r, s, v — the exact 65 bytes the Gateway admits.
    assert proto.sign_join(_DEV_KEY, proto.PROTOCOL_VERSION, _DEV_ADDR, _DEV_NONCE) == _GOLDEN_SIG


def test_sign_join_is_canonical_low_s_65_bytes_raw_recid():
    # The Gateway rejects high-S (NonCanonicalSignature) and anything but 65 [r||s||v]
    # bytes with v the raw recovery id. Pin the structural invariants its EIP-2 gate
    # enforces — a backend that emitted high-S or the 27/28 offset would be admitted
    # here but refused by verify_join_signature.
    raw = bytes.fromhex(proto.sign_join(_DEV_KEY, proto.PROTOCOL_VERSION, _DEV_ADDR, _DEV_NONCE))
    assert len(raw) == 65
    assert int.from_bytes(raw[32:64], "big") <= _SECP256K1_N // 2, "must be low-S"
    assert raw[64] in (0, 1), "v is the raw recovery id, not the 27/28 offset"


def test_signed_join_recovers_to_the_claimed_agent_id():
    # The round-trip the Gateway runs: recover the signer from the signature over the
    # digest and assert it equals the claimed agent_id — the agent's self-check before
    # it sends the Join.
    sig = proto.sign_join(_DEV_KEY, proto.PROTOCOL_VERSION, _DEV_ADDR, _DEV_NONCE)
    assert proto.recover_join_signer(proto.PROTOCOL_VERSION, _DEV_ADDR, _DEV_NONCE, sig) == _DEV_ADDR


def test_signature_does_not_recover_to_claim_under_a_different_nonce():
    # FM (replay): the nonce is folded into the digest, so a signature captured for one
    # challenge recovers a DIFFERENT address against a fresh challenge — exactly the
    # AddressMismatch that makes a captured Join un-replayable on another connection.
    sig = proto.sign_join(_DEV_KEY, proto.PROTOCOL_VERSION, _DEV_ADDR, _DEV_NONCE)
    under_other = proto.recover_join_signer(
        proto.PROTOCOL_VERSION, _DEV_ADDR, b"a-different-challenge", sig
    )
    assert under_other != _DEV_ADDR


def test_recover_join_signer_rejects_malformed_signatures():
    # The BadSignatureEncoding / Unrecoverable arms: non-hex, wrong length, and a
    # 65-byte blob with an out-of-range v all degrade to None — never raise.
    d = proto.PROTOCOL_VERSION
    assert proto.recover_join_signer(d, _DEV_ADDR, _DEV_NONCE, "nothexatall") is None
    assert proto.recover_join_signer(d, _DEV_ADDR, _DEV_NONCE, "00") is None
    assert proto.recover_join_signer(d, _DEV_ADDR, _DEV_NONCE, "ff" * 65) is None


def _intent(x: int, y: int) -> ActionIntent:
    return ActionIntent(
        move_dir=Vec2(x=x, y=y), aim=0,
        buttons=ActionButtons(fire=False, jump=False, ability=False, reload=False),
    )


def _clamp_golden() -> dict:
    """The SAME committed clamp-parity golden the Rust arena-proto drift-gate pins
    (arena/proto/tests/clamp_parity.json), resolved by repo-relative path from THIS
    file (never the CWD), so it holds under validate.sh's agents-dir pytest run. A
    missing golden fails LOUD — it is the single source of truth for the Rust⇄Python
    move-clamp contract and must never be silently skipped (which would let drift
    through). Regenerate after an intentional clamp change with
    `cargo test -p arena-proto --test clamp_parity regenerate_clamp_parity_golden -- --ignored`."""
    path = Path(__file__).resolve().parents[1] / "arena" / "proto" / "tests" / "clamp_parity.json"
    assert path.is_file(), f"shared clamp-parity golden not found at {path}"
    return json.loads(path.read_text())


def test_move_clamp_matches_rust_golden():
    # Python ActionIntent.clamped() must reproduce EVERY case in the Rust golden
    # bit-for-bit — the machine-checked cross-implementer pin the hand-copied rows
    # could not give. A Rust clamp change (which regenerates the golden) or a Python
    # clamp regression breaks this against the one source of truth, including the
    # i32::MIN overflow corner, the trunc-toward-zero discriminator, and the 64
    # fixed-seed fuzz_NN cases that sweep the full i32 range (every input here is a
    # valid I32 the field must accept, every output bit-identical to Rust's clamp).
    golden = _clamp_golden()
    assert golden["domain"] == "blackfield/arena/clamp-parity/v2"
    assert golden["move_intent_scale"] == proto.MOVE_INTENT_SCALE
    assert golden["cases"], "golden carries no cases"
    for case in golden["cases"]:
        inp, exp = case["input"], case["output"]
        got = _intent(inp["x"], inp["y"]).clamped().move_dir
        assert got == Vec2(x=exp["x"], y=exp["y"]), (
            f"clamp parity drift on {case['label']!r}: input {inp} -> "
            f"Python ({got.x}, {got.y}) vs Rust golden ({exp['x']}, {exp['y']})"
        )


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


def test_ranked_client_derives_agent_id_from_the_key():
    from arena_client.sdk import ArenaClient
    c = ArenaClient.ranked(MockTransport([]), _DEV_KEY)
    # The claimed identity IS the address the key recovers to, by construction — so a
    # ranked() client can never present a claim the Gateway recovers a different key for.
    assert c.agent_id == _DEV_ADDR
    assert c.signing_key == _DEV_KEY


def test_ranked_join_signs_over_the_connection_challenge_nonce():
    from arena_client.sdk import ArenaClient
    nonce = "9f3a1c00ff"  # the per-connection challenge token issued in the Challenge
    t = MockTransport([_challenge_frame(nonce), _welcome_frame(), _start_frame()])
    c = ArenaClient.ranked(t, _DEV_KEY).connect()
    join = t.sent[0]
    assert join["type"] == "join" and join["agent_id"] == _DEV_ADDR
    # The signature is sign_join over the nonce just received (its utf-8 bytes), and it
    # recovers to the claimed agent_id — exactly the round-trip the Gateway runs.
    expected = proto.sign_join(_DEV_KEY, proto.PROTOCOL_VERSION, _DEV_ADDR, nonce.encode())
    assert join["signature_hex"] == expected
    recovered = proto.recover_join_signer(
        proto.PROTOCOL_VERSION, _DEV_ADDR, nonce.encode(), join["signature_hex"]
    )
    assert recovered == _DEV_ADDR
    assert c.connected


def test_ranked_signature_binds_the_nonce_so_two_connections_differ():
    # FM (cross-connection replay): a fresh challenge per connection yields a DIFFERENT
    # signature, so a Join sniffed off one connection is worthless on another — its sig
    # recovers the wrong address against that connection's nonce.
    from arena_client.sdk import ArenaClient
    sigs = []
    for nonce in ("aaaa1111", "bbbb2222"):
        t = MockTransport([_challenge_frame(nonce), _welcome_frame(), _start_frame()])
        ArenaClient.ranked(t, _DEV_KEY).connect()
        sigs.append(t.sent[0]["signature_hex"])
    assert sigs[0] != sigs[1]


def test_unranked_client_sends_empty_signature_unchanged():
    # The no-key path is untouched: an unranked seat still sends an empty signature_hex
    # and never invokes the crypto stack.
    from arena_client.sdk import ArenaClient
    t = MockTransport([_challenge_frame("abcd"), _welcome_frame(), _start_frame()])
    ArenaClient(t, agent_id="agent-x").connect()
    assert t.sent[0]["signature_hex"] == ""


def test_explicit_mismatched_claim_recovers_a_different_address():
    # The constructor still allows an explicit agent_id alongside a key. If the claim
    # isn't the key's address, the signature recovers a DIFFERENT address — the
    # Gateway's AddressMismatch. ranked() is the safe default precisely because it
    # forecloses this; this pins that the seam is honest when bypassed.
    from arena_client.sdk import ArenaClient
    t = MockTransport([_challenge_frame("abcd"), _welcome_frame(), _start_frame()])
    ArenaClient(t, agent_id="0xnottheaddress", signing_key=_DEV_KEY).connect()
    join = t.sent[0]
    assert join["agent_id"] == "0xnottheaddress"
    recovered = proto.recover_join_signer(
        proto.PROTOCOL_VERSION, "0xnottheaddress", b"abcd", join["signature_hex"]
    )
    assert recovered == _DEV_ADDR and recovered != "0xnottheaddress"


def test_repr_never_leaks_the_ranked_signing_key():
    # FM2: the secp256k1 private key is the whole security of a ranked seat, so it must
    # never reach a log or repr — logging the client, or an error that interpolates one,
    # renders it through __repr__/__str__. Pin that no rendering carries the key in any
    # form (hex or raw bytes), and that the repr stays useful: the PUBLIC identity and
    # connection state, never the secret.
    from arena_client.sdk import ArenaClient
    c = ArenaClient.ranked(MockTransport([]), _DEV_KEY)
    for rendered in (repr(c), str(c), f"{c}", f"{c!r}"):
        assert _DEV_KEY.hex() not in rendered
        assert str(_DEV_KEY) not in rendered  # the raw b'...' bytes repr, too
    assert _DEV_ADDR in repr(c) and "ranked=True" in repr(c)


def test_start_carries_static_cover_blockers_and_defaults_empty():
    # Decode side: a Start with the static cover layout decodes into typed Blockers
    # an agent can path around; a Start WITHOUT the field (an older server, or a
    # no-occluder match) decodes to an empty list — the back-compat default.
    with_blockers = decode_gateway({
        "type": "start", "match_id": FIXED_MATCH,
        "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 2},
        "blockers": [{"min": {"x": -1000, "y": -2000}, "max": {"x": 1000, "y": 2000}}],
    })
    assert isinstance(with_blockers, Start)
    assert with_blockers.blockers == [Blocker(min=Vec2(x=-1000, y=-2000), max=Vec2(x=1000, y=2000))]

    without = decode_gateway(_start_frame())  # no "blockers" key
    assert isinstance(without, Start) and without.blockers == []


def test_blocker_carries_a_height_and_defaults_to_infinitely_tall():
    # A blocker may carry a top `height` (a pawn high enough sees/shoots over it); a
    # blocker WITHOUT the field — an older server, or an infinitely-tall wall —
    # decodes to height 0, the historical occlude-any-crossing behavior.
    tall = decode_gateway({
        "type": "start", "match_id": FIXED_MATCH,
        "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 2},
        "blockers": [{"min": {"x": 0, "y": 0}, "max": {"x": 500, "y": 500}, "height": 2000}],
    })
    assert isinstance(tall, Start)
    assert tall.blockers == [Blocker(min=Vec2(x=0, y=0), max=Vec2(x=500, y=500), height=2000)]

    # The field is optional and defaults to 0 (infinitely tall) for back-compat.
    short = decode_gateway({
        "type": "start", "match_id": FIXED_MATCH,
        "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 2},
        "blockers": [{"min": {"x": 0, "y": 0}, "max": {"x": 500, "y": 500}}],
    })
    assert isinstance(short, Start) and short.blockers[0].height == 0


def test_connect_exposes_the_static_blockers():
    # The client surfaces the cover layout learned at Start (the twin of c.config),
    # so a policy can read c.blockers to path around physical cover.
    from arena_client.sdk import ArenaClient
    start = {
        "type": "start", "match_id": FIXED_MATCH,
        "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 2},
        "blockers": [{"min": {"x": 0, "y": 0}, "max": {"x": 500, "y": 500}}],
    }
    c = ArenaClient(MockTransport([_challenge_frame(), _welcome_frame(), start]), agent_id="a").connect()
    assert c.blockers == [Blocker(min=Vec2(x=0, y=0), max=Vec2(x=500, y=500))]

    # A match with no occluders (the default Start) exposes an empty layout.
    c2 = ArenaClient(
        MockTransport([_challenge_frame(), _welcome_frame(), _start_frame()]), agent_id="a"
    ).connect()
    assert c2.blockers == []


def test_static_geometry_never_rides_the_parity_bounded_observation():
    # FM1 (parity): the static map is surfaced ONCE at Start, never on the per-tick
    # Observation — the security boundary. A blockers field on an Observation is
    # rejected (extra=forbid), so no static-geometry channel can be added to the
    # parity-bounded snapshot by drift on either side.
    with pytest.raises(pydantic.ValidationError):
        Observation.model_validate({
            "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH, "seat": 0, "tick": 1,
            "phase": "live", "deadline_micros": 50_000,
            "own": {"seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
                    "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
                    "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "alive": True},
            "visible": [], "blockers": [],
        })


def test_start_carries_static_pickup_points_and_defaults_empty():
    # Decode side: a Start with the static pickup layout decodes into typed Vec2
    # spawn points an agent can path toward; a Start WITHOUT the field (an older
    # server, or a no-pickup match) decodes to an empty list — the back-compat default.
    with_points = decode_gateway({
        "type": "start", "match_id": FIXED_MATCH,
        "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 2},
        "pickup_points": [{"x": 100, "y": 200}, {"x": -100, "y": -200}],
    })
    assert isinstance(with_points, Start)
    assert with_points.pickup_points == [Vec2(x=100, y=200), Vec2(x=-100, y=-200)]

    without = decode_gateway(_start_frame())  # no "pickup_points" key
    assert isinstance(without, Start) and without.pickup_points == []


def test_connect_exposes_the_static_pickup_points():
    # The client surfaces the pickup layout learned at Start (the twin of c.blockers),
    # so a policy can read c.pickup_points to path toward where items spawn.
    from arena_client.sdk import ArenaClient
    start = {
        "type": "start", "match_id": FIXED_MATCH,
        "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 2},
        "pickup_points": [{"x": 500, "y": 0}],
    }
    c = ArenaClient(MockTransport([_challenge_frame(), _welcome_frame(), start]), agent_id="a").connect()
    assert c.pickup_points == [Vec2(x=500, y=0)]

    # A match with no pickups (the default Start) exposes an empty layout.
    c2 = ArenaClient(
        MockTransport([_challenge_frame(), _welcome_frame(), _start_frame()]), agent_id="a"
    ).connect()
    assert c2.pickup_points == []


def test_pickup_points_are_position_only_never_kind_or_amount():
    # FM1 (no full-reveal): the surfaced layout is position-only by construction — a
    # bare Vec2 list. The pickup kind/amount stays empirical (learned by collecting),
    # so a spawn point carrying a kind/amount is REJECTED (extra=forbid on Vec2): the
    # channel structurally cannot leak the effect, on either side of a drift.
    with pytest.raises(pydantic.ValidationError):
        decode_gateway({
            "type": "start", "match_id": FIXED_MATCH,
            "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 2},
            "pickup_points": [{"x": 100, "y": 200, "kind": "health", "amount": 25}],
        })


def test_static_pickup_layout_never_rides_the_parity_bounded_observation():
    # FM2 (dynamic-availability): only the STATIC layout is surfaced (once, at Start).
    # Whether a pickup is currently collectible or on respawn-cooldown is dynamic state
    # that stays parity-bounded — a pickup_points field on an Observation is rejected
    # (extra=forbid), so no static-layout channel can be added to the per-tick snapshot.
    with pytest.raises(pydantic.ValidationError):
        Observation.model_validate({
            "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH, "seat": 0, "tick": 1,
            "phase": "live", "deadline_micros": 50_000,
            "own": {"seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
                    "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
                    "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "alive": True},
            "visible": [], "pickup_points": [],
        })


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
    it, so this just locates it.

    Prefers the DEBUG profile because that is the one CI (`cargo build -p
    arena-harness`) and validate.sh build right before the e2e run — so a stale
    `target/release/` binary left over from a dev build never shadows the freshly
    compiled debug one (which would silently run the e2e match against old code)."""
    arena = Path(__file__).resolve().parent.parent / "arena"
    for profile in ("debug", "release"):
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


def test_run_local_match_forms_a_ranked_match():
    # The agent-signs (arena-agent-join-signing) → harness-verifies
    # (arena-harness-verifies-ranked-join) loop, end to end: two seats join with
    # distinct signing keys, the harness recovers each signer and admits it ranked,
    # and a real decisive match forms. If either ranked join were rejected, connect()
    # would raise HandshakeRejected before the first tick — so "forms + settles" is
    # itself the proof the harness admitted the signed joins.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    seed = 12345
    match_id = "11111111-2222-4333-8444-555555555555"
    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    results = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id, signing_keys=keys)

    assert set(results) == {0, 1}
    r0 = results[0]
    assert r0 == results[1], "every seat sees the same canonical result"
    assert r0.match_id == match_id
    assert 0 < r0.final_tick < 3600
    assert len([o for o in r0.outcomes if o.alive_at_end]) == 1
    assert len(r0.replay_hash) == 64

    # RFC6979 join signatures are deterministic, so the ranked match re-runs
    # byte-for-byte on the same seed + keys.
    again = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id, signing_keys=keys)
    assert again[0] == r0
    assert again[0].replay_hash == r0.replay_hash


def test_run_local_match_mixes_a_ranked_and_unranked_seat():
    # A partly-keyed seat map must still complete the round-robin handshake: seat 0
    # joins ranked (signed), seat 1 unranked (empty signature). The harness admits
    # both and the match forms — neither seat hangs waiting on the other's Welcome.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    results = run_local_match(
        harness,
        [0, 1],
        policies,
        seed=7,
        match_id="22222222-2222-4333-8444-555555555555",
        signing_keys={0: _DEV_KEY},
    )
    assert set(results) == {0, 1}
    assert results[0] == results[1]
    assert 0 < results[0].final_tick < 3600
    assert len([o for o in results[0].outcomes if o.alive_at_end]) == 1


def test_run_local_match_keyed_seat_overrides_a_conflicting_agent_id():
    # FM1: a keyed seat derives its claimed id from the key (ranked()), IGNORING a
    # conflicting agent_ids entry. Were run_local_match to claim the agent_ids id while
    # signing with the key, the harness would recover a different address
    # (AddressMismatch), reject the join, and the match would never form — so a clean
    # formation despite agent_ids[0] proves the key wins.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    results = run_local_match(
        harness,
        [0, 1],
        policies,
        seed=3,
        match_id="33333333-2222-4333-8444-555555555555",
        agent_ids={0: "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"},
        signing_keys={0: _DEV_KEY, 1: _DEV_KEY2},
    )
    assert set(results) == {0, 1}
    assert 0 < results[0].final_tick < 3600


def test_run_local_match_unranked_default_is_unchanged():
    # FM3: threading signing_keys must not perturb the no-key path. An absent map and
    # an empty map both yield today's deterministic unranked result, replay hash incl.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    seed = 999
    match_id = "44444444-2222-4333-8444-555555555555"
    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    base = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id)
    empty = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id, signing_keys={})
    assert empty[0] == base[0]
    assert empty[0].replay_hash == base[0].replay_hash


def test_run_matchmade_agent_mode_forms_through_the_matchmaker():
    # The headline: an Agent-mode match formed through the harness's arena-match
    # Matchmaker, driven from the SDK. Both seats sign, the matchmaker admits each
    # ranked identity, and a decisive match forms + settles. Forming is itself the
    # proof — a rejected ranked join would raise HandshakeRejected before tick 0, and a
    # sequential connect would DEADLOCK (the matchmaker withholds every Welcome until
    # the last join), so a clean run is the proof the batched handshake works.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    salt = "00000000-0000-4000-8000-000000000000"
    a = run_local_match(harness, [0, 1], policies, seed=5, match_id=salt, mode="agent", signing_keys=keys)
    assert set(a) == {0, 1}
    assert a[0] == a[1], "every seat sees the same canonical result"
    assert 0 < a[0].final_tick < 3600
    assert len([o for o in a[0].outcomes if o.alive_at_end]) == 1
    assert len(a[0].replay_hash) == 64

    # The matchmaker mints its OWN match id (Uuid::new_v4), not the argv salt the
    # challenge nonce is derived from — so the formed match carries a fresh id, and a
    # second run mints a DIFFERENT one (the matchmade path is non-deterministic by
    # design, unlike the direct path). Both inequalities would fail were the SDK
    # silently on the direct-seating path (which pins the id to the argv salt).
    assert a[0].match_id != salt
    b = run_local_match(harness, [0, 1], policies, seed=5, match_id=salt, mode="agent", signing_keys=keys)
    assert b[0].match_id != a[0].match_id


def test_run_matchmade_mode_none_is_byte_identical_to_the_direct_path():
    # FM2: adding the mode param must not perturb the no-mode path. mode=None yields
    # today's deterministic direct-seating result, replay hash incl., identical to the
    # call with no mode argument at all.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    seed = 999
    match_id = "44444444-2222-4333-8444-555555555555"
    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    base = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id)
    none = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id, mode=None)
    assert none[0] == base[0]
    assert none[0].replay_hash == base[0].replay_hash
    assert none[0].match_id == match_id, "the direct path keeps the argv id, unlike the matchmade path"


def test_run_matchmade_mixed_admits_a_casual_agent_alongside_a_human():
    # FM3 (corrected): the Matchmaker never forms an all-one-kind Mixed match — it
    # needs a human AND an agent (select_mixed/composition_ok). So the honest cross-play
    # test is a token-less HUMAN seat (declared via human_seats) plus a token-less CASUAL
    # agent: the casual seat is admitted (not forced to sign, not rejected) and the match
    # forms. A ranked agent composes with a human the same way.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    casual = run_local_match(
        harness, [0, 1], policies, seed=8, mode="mixed", human_seats=[0]
    )
    assert set(casual) == {0, 1}
    assert 0 < casual[0].final_tick < 3600
    assert len([o for o in casual[0].outcomes if o.alive_at_end]) == 1

    ranked = run_local_match(
        harness, [0, 1], policies, seed=8, mode="mixed", human_seats=[0], signing_keys={1: _DEV_KEY2}
    )
    assert set(ranked) == {0, 1}
    assert 0 < ranked[0].final_tick < 3600


def test_run_matchmade_agent_mode_reads_each_seat_moved_ladder_rating():
    # The readout headline: after a ranked Agent-mode match, each seat reads its own
    # post-match ladder standing through the structured [ladder] emission — the rating
    # movement MatchResult alone never carries. Both seats are ranked, so neither is None.
    harness = _require_or_skip_harness()
    from arena_client.sdk import LadderStanding, run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    salt = "00000000-0000-4000-8000-000000000000"
    ratings: dict[int, LadderStanding | None] = {}
    results = run_local_match(
        harness, [0, 1], policies, seed=5, match_id=salt, mode="agent", signing_keys=keys, ratings=ratings
    )

    assert set(ratings) == {0, 1}
    assert all(isinstance(ratings[s], LadderStanding) for s in (0, 1))
    s0, s1 = ratings[0], ratings[1]

    # Zero-sum + decisive: equal pre-match ratings, so the deltas mirror and one seat
    # gains exactly what the other loses (the ladder mints no rating). A draw (delta 0)
    # would fail this, and the seat-order tie-break makes a symmetric duel decisive.
    assert s0.delta == -s1.delta != 0
    # Both started from the same default rating (rating - delta is the pre-match value),
    # so a swapped or mis-keyed readout would break this equality.
    assert s0.rating - s0.delta == s1.rating - s1.delta
    # The standing tracks the match outcome: the surviving (placement-1) seat is the one
    # whose rating rose, the other fell — a winner reading a negative delta is a mis-pair.
    winner = next(o.seat for o in results[0].outcomes if o.placement == 1)
    loser = next(o.seat for o in results[0].outcomes if o.placement != 1)
    assert ratings[winner].delta > 0 and ratings[loser].delta < 0
    assert ratings[winner].rating > ratings[loser].rating


def test_run_local_match_unranked_seats_read_none_and_result_is_byte_identical():
    # FM2 + FM3: a direct (unranked) match moves no ladder, so EVERY seat reads None —
    # never a zeroed standing. And requesting the readout must not perturb the match: the
    # MatchResult (replay hash included) is byte-identical to the call without ratings.
    harness = _require_or_skip_harness()
    from arena_client.sdk import LadderStanding, run_local_match

    seed, match_id = 999, "44444444-2222-4333-8444-555555555555"
    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    base = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id)

    ratings: dict[int, LadderStanding | None] = {}
    rated = run_local_match(harness, [0, 1], policies, seed=seed, match_id=match_id, ratings=ratings)
    assert rated[0] == base[0]
    assert rated[0].replay_hash == base[0].replay_hash
    assert ratings == {0: None, 1: None}


def test_run_matchmade_mixed_reads_none_even_for_the_ranked_seat():
    # FM2 (cross-play): the Matchmaker registers ONLY Agent-mode matches in the ladder, so
    # a Mixed match has no rating movement — even its signed agent seat reads None, not a
    # standing. A non-None here would mean a casual/human/Mixed match leaked into the ladder.
    harness = _require_or_skip_harness()
    from arena_client.sdk import LadderStanding, run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    ratings: dict[int, LadderStanding | None] = {}
    run_local_match(
        harness, [0, 1], policies, seed=8, mode="mixed", human_seats=[0],
        signing_keys={1: _DEV_KEY2}, ratings=ratings,
    )
    assert ratings == {0: None, 1: None}


def test_gateway_ladder_fails_loud_on_a_drifted_emission_but_skips_a_foreign_match():
    # FM1: the [ladder] readout is a structured contract, so a line that drifts from the
    # exact {match_id, seats:[{seat,rating,delta}]} shape must fail LOUD — never be read
    # as 'unranked' (None), which would mask an emission-format regression as a casual
    # match. A well-formed line for a DIFFERENT match_id is a legitimate skip, not drift.
    from arena_client.sdk import SubprocessGateway

    def emit(line: str) -> SubprocessGateway:
        # Portable stand-in for the harness: write one stderr line and exit, so close()
        # joins the drain and the line is captured. No stdout frames, no cargo needed.
        gw = SubprocessGateway(
            [sys.executable, "-c", "import sys; sys.stderr.write(sys.argv[1])", line],
            capture_stderr=True,
        )
        gw.close()
        return gw

    # Garbage after the prefix is not JSON: a hard error, never a silent None.
    with pytest.raises(proto.ProtocolError, match="malformed"):
        emit("[ladder] {not json}\n").ladder("m")
    # Well-formed JSON for this match but a seat missing rating/delta: shape drift.
    with pytest.raises(proto.ProtocolError, match="malformed"):
        emit('[ladder] {"match_id": "m", "seats": [{"seat": 0}]}\n').ladder("m")
    # Same shape but a foreign match_id is silently skipped, so the seat reads None — a
    # concurrent match's line is never misread as this one's, nor raised on.
    foreign = '[ladder] {"match_id": "other", "seats": [{"seat": 0, "rating": 9, "delta": 1}]}\n'
    assert emit(foreign).ladder("m") == {}


def test_run_matchmade_ladder_file_accumulates_a_seat_rating_across_two_runs(tmp_path):
    # The headline: a --ladder-file makes the ranked ladder DURABLE across SDK calls. Run a
    # ranked match twice sharing one file; run 2 seeds from run 1's written ladder, so each
    # seat's pre-match rating in run 2 is EXACTLY its post-match rating from run 1 — the
    # standing accumulates instead of resetting to the default every call. The spaced
    # filename also proves the path reaches the harness as one argv token (not shell-split).
    harness = _require_or_skip_harness()
    from arena_client.sdk import LadderStanding, run_local_match

    ladder = tmp_path / "rated ladder.json"
    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    salt = "00000000-0000-4000-8000-000000000000"

    r1: dict[int, LadderStanding | None] = {}
    run_local_match(
        harness, [0, 1], policies, seed=5, match_id=salt, mode="agent",
        signing_keys=keys, ladder_file=ladder, ratings=r1,
    )
    assert ladder.exists(), "run 1 persisted the ladder to the exact (spaced) path"
    assert r1[0] is not None and r1[1] is not None, "both ranked seats have a run-1 standing"

    r2: dict[int, LadderStanding | None] = {}
    run_local_match(
        harness, [0, 1], policies, seed=5, match_id=salt, mode="agent",
        signing_keys=keys, ladder_file=ladder, ratings=r2,
    )
    assert r2[0] is not None and r2[1] is not None

    # Both seats start run 1 at the same default (equal pre-ratings, zero-sum), so the
    # default is recoverable without hardcoding it; run 1 actually moved the ladder.
    default = r1[0].rating - r1[0].delta
    assert r1[1].rating - r1[1].delta == default, "run 1 starts both seats at the default"
    assert any(r1[s].rating != default for s in (0, 1)), "run 1 actually moved the ladder"

    # Run 2 resumed each seat's run-1 POST-match standing from the file (per seat, robust to
    # which seat won either non-deterministic run): a fresh start would make run 2's pre ==
    # default, not run 1's moved value.
    for s in (0, 1):
        assert r2[s].rating - r2[s].delta == r1[s].rating, f"seat {s} resumed its run-1 rating in run 2"
    assert any(r2[s].rating - r2[s].delta != default for s in (0, 1)), "run 2 did NOT silently start fresh"


def test_ladder_file_without_a_ranked_mode_is_rejected_before_spawning():
    # FM1: the harness only persists the ladder on the --mode path, so a ladder_file with
    # mode=None (the direct path) is a silent no-op. Reject it up front — fail loud, never
    # quietly drop the persistence the caller asked for. Raises before any spawn, so the
    # nonexistent harness path is never reached.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    with pytest.raises(ValueError, match="ladder_file"):
        run_local_match("/no/such/harness", [0, 1], policies, ladder_file="/tmp/ladder.json")


def test_run_local_match_forwards_ladder_file_as_one_argv_token_and_omits_it_by_default(monkeypatch):
    # FM2 (quoting) + FM3 (additive): when set, the EXACT path is forwarded as a SINGLE argv
    # token (run_local_match builds an argv list, not a shell string, so spaces survive
    # verbatim); when omitted, no --ladder-file appears at all, so an existing caller's argv
    # is byte-identical. Captured without a harness by stubbing the gateway before it spawns.
    from arena_client import sdk

    captured: dict[str, list[str]] = {}

    class _Stop(Exception):
        pass

    class _SpyGateway:
        def __init__(self, argv, **_kw):
            captured["argv"] = argv
            raise _Stop

    monkeypatch.setattr(sdk, "SubprocessGateway", _SpyGateway)
    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}

    # Omitted: not a single --ladder-file token (additive — byte-identical argv).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys)
    assert "--ladder-file" not in captured["argv"]

    # Set: the exact path is the one argv token after the flag — spaces intact, never split.
    spaced = "/tmp/a b/rated ladder.json"
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, ladder_file=spaced)
    argv = captured["argv"]
    assert argv[argv.index("--ladder-file") + 1] == spaced


def test_run_matchmade_rejects_a_degenerate_mode_before_spawning():
    # FM1: a mode/composition the Matchmaker can never form must fail LOUD up front, not
    # hang on a Welcome that never comes. These raise before the harness is spawned, so
    # they need no toolchain and run everywhere. (A nonexistent harness path would error
    # on spawn — that none does proves the guard precedes it.)
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    # Agent mode is ranked-only: an unkeyed seat would be Unauthenticated forever.
    with pytest.raises(ValueError, match="ranked-only"):
        run_local_match("/no/such/harness", [0, 1], policies, mode="agent")
    with pytest.raises(ValueError, match="ranked-only"):
        run_local_match("/no/such/harness", [0, 1], policies, mode="agent", signing_keys={0: _DEV_KEY})
    # Mixed needs a human AND an agent: all-agent and all-human both refused.
    with pytest.raises(ValueError, match="human"):
        run_local_match("/no/such/harness", [0, 1], policies, mode="mixed")
    with pytest.raises(ValueError, match="human"):
        run_local_match("/no/such/harness", [0, 1], policies, mode="mixed", human_seats=[0, 1])
    # An unknown mode is caught here, not as an opaque harness crash downstream.
    with pytest.raises(ValueError, match="human, agent, or mixed"):
        run_local_match("/no/such/harness", [0, 1], policies, mode="ranked", signing_keys={0: _DEV_KEY})


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
