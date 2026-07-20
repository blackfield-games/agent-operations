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
    Broadcast,
    BroadcastEntity,
    MatchResult,
    Observation,
    Start,
    Vec2,
    act_frame,
    decode_gateway,
    decode_spectator,
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


def _spectator_golden() -> dict:
    """The SAME committed SPECTATOR wire-frame parity golden the Rust arena-proto drift-gate
    pins (arena/proto/tests/spectator_parity.json), resolved by repo-relative path from THIS
    file (never the CWD), so it holds under validate.sh's agents-dir pytest run. A missing
    golden fails LOUD — it is the single source of truth for the Rust<->Python spectator
    contract. Regenerate after an intentional wire change with `cargo test -p arena-proto
    --test spectator_parity regenerate_spectator_parity_golden -- --ignored`."""
    path = Path(__file__).resolve().parents[1] / "arena" / "proto" / "tests" / "spectator_parity.json"
    assert path.is_file(), f"shared spectator-parity golden not found at {path}"
    return json.loads(path.read_text())


def test_spectator_golden_header_matches_proto():
    golden = _spectator_golden()
    assert golden["domain"] == "blackfield/arena/spectator-parity/v1"
    assert golden["protocol_version"] == proto.PROTOCOL_VERSION
    assert golden["match_id"] == FIXED_MATCH  # the fixed, byte-stable parity match id


def test_spectator_frames_decode_and_reencode_against_rust_golden():
    # Every frame in the SHARED spectator golden (emitted by Rust spectator_parity_vectors,
    # diffed by the Rust drift-gate) decodes via decode_spectator into the typed model and
    # re-encodes to the SAME wire body — the machine-checked cross-implementer pin a Python
    # caster/grader relies on. A Rust wire change (which regenerates the golden) or a Python
    # model drift breaks this against the one source of truth, including the frame/end newtype
    # flattening next to "type" (a nested {"broadcast": ...} would fail to decode).
    expected_type = {"frame": Broadcast, "end": MatchResult}
    frames = _spectator_golden()["frames"]
    seen = set()
    for case in frames:
        frame = case["frame"]
        msg = decode_spectator(frame)
        seen.add(frame["type"])
        # decode_spectator must route each tag to the RIGHT concrete model — a mis-wired
        # tag->type mapping is caught here, not only by field shape.
        assert isinstance(msg, expected_type[frame["type"]]), f"{case['label']} decoded to {type(msg).__name__}"
        body = {k: v for k, v in frame.items() if k != "type"}
        if case["exact_reencode"]:
            assert msg.model_dump(mode="json") == body, (
                f"{case['label']} did not decode/re-encode to the golden body"
            )
    assert seen == {"frame", "end"}, "a SpectatorMsg variant is missing from the golden"

    # decode_spectator REFUSES an untagged or unknown-tag frame (fail loud on drift, not a
    # silent skip that would mask a real envelope change).
    with pytest.raises(proto.ProtocolError):
        decode_spectator({"tick": 0})
    with pytest.raises(proto.ProtocolError):
        decode_spectator({"type": "observe"})  # a gateway tag is not a spectator frame


def test_spectator_golden_backcompat_frame_fills_default():
    # The omit frame pins the serde(default) back-compat: a Live broadcast that OMITS
    # starting_remaining decodes to 0 (not an error under extra=forbid). It omits the key
    # entirely, so a re-encode can't reproduce it (the default is filled back) — the
    # wire-shape pin is the decode-to-default here. The spectator twin of the Start omit frame.
    by_label = {c["label"]: c["frame"] for c in _spectator_golden()["frames"]}
    legacy = decode_spectator(by_label["frame_legacy_omits_starting_remaining"])
    assert isinstance(legacy, Broadcast)
    assert legacy.starting_remaining == 0


def test_broadcast_entity_carries_scoreboard_but_no_private_hud():
    # FM (wrong shape): BroadcastEntity is genuinely distinct from VisibleEntity — it ADDS the
    # health bar + scoreboard a broadcast renders and DROPS in_line_of_sight (a spectator sees
    # the whole map, not one seat's cone). The model REQUIRES those fields (the Rust wire always
    # carries them, so a drift fails loud instead of silently defaulting).
    entity = {
        "entity_id": 7, "kind": "player", "team": 2, "position": {"x": 0, "y": 0}, "z": 0,
        "facing": 0, "health": 80, "max_health": 100, "score": 12, "alive": True,
    }
    e = BroadcastEntity.model_validate(entity)
    assert (e.health, e.max_health, e.score, e.alive) == (80, 100, 12, True)
    for required in ("health", "max_health", "score", "alive"):
        with pytest.raises(pydantic.ValidationError):
            BroadcastEntity.model_validate({k: v for k, v in entity.items() if k != required})

    # Security line: a spectator learns no more than a stream viewer, so the broadcast entity
    # carries NO private HUD state — extra=forbid rejects ammo/cooldown (a tactical x-ray) and
    # the perception-bounded in_line_of_sight (which is a per-seat concept, not a broadcast one).
    for hidden in ("ammo", "cooldown", "in_line_of_sight"):
        with pytest.raises(pydantic.ValidationError):
            BroadcastEntity.model_validate({**entity, hidden: 1})


def test_self_state_carries_cooldown_but_visible_entity_does_not():
    # FM3 parity: the Rust SelfState emits the seat's own fire cooldown, so the
    # model must accept it — and require it, since the wire always carries it, so a
    # Rust/Python drift fails loud instead of silently defaulting.
    own = {
        "seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "z_vel": 0, "facing": 0,
        "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
        "ammo": 30, "cooldown": 3, "dash_cooldown": 0, "score": 0, "alive": True,
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
        "seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "z_vel": 0, "facing": 0,
        "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 40,
        "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "score": 0, "alive": True,
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
        "seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "z_vel": 0, "facing": 0,
        "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
        "ammo": 30, "cooldown": 0, "dash_cooldown": 7, "score": 0, "alive": True,
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


def test_self_state_carries_score_but_visible_entity_does_not():
    # Own cumulative match score (damage dealt). UNLIKE cooldown/shield it is not secret
    # — the same i32 is public on the broadcast scoreboard — but it stays own-state only:
    # SelfState REQUIRES it (the Rust wire always carries it, so a drift fails loud
    # instead of silently defaulting), and VisibleEntity must REJECT it (extra=forbid
    # mirrors the Rust wire-pin exclusion), so an enemy's exact score (who is ahead) is
    # never on the per-seat perception slice.
    own = {
        "seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "z_vel": 0, "facing": 0,
        "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
        "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "score": 42, "alive": True,
    }
    assert proto.SelfState.model_validate(own).score == 42
    with pytest.raises(pydantic.ValidationError):
        proto.SelfState.model_validate({k: v for k, v in own.items() if k != "score"})
    with pytest.raises(pydantic.ValidationError):
        proto.VisibleEntity.model_validate({
            "entity_id": 1, "kind": "player", "team": 2,
            "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
            "in_line_of_sight": True, "score": 42,
        })


def test_self_state_carries_z_vel_but_visible_entity_does_not():
    # Own vertical velocity (the rate z changes) — the vertical twin of `velocity`. Own
    # state only: SelfState REQUIRES it (the Rust wire always carries it, so a drift fails
    # loud instead of silently defaulting), and VisibleEntity must REJECT it
    # (extra=forbid), so an enemy's vertical velocity (when it lands, whether it is
    # committed to a jump) is never on the per-seat perception slice.
    own = {
        "seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "z_vel": 700, "facing": 0,
        "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
        "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "score": 0, "alive": True,
    }
    assert proto.SelfState.model_validate(own).z_vel == 700
    # A falling pawn reads negative — the signed i32 is load-bearing (a u16 would misread).
    assert proto.SelfState.model_validate({**own, "z_vel": -700}).z_vel == -700
    with pytest.raises(pydantic.ValidationError):
        proto.SelfState.model_validate({k: v for k, v in own.items() if k != "z_vel"})
    with pytest.raises(pydantic.ValidationError):
        proto.VisibleEntity.model_validate({
            "entity_id": 1, "kind": "player", "team": 2,
            "position": {"x": 0, "y": 0}, "z": 0, "facing": 0,
            "in_line_of_sight": True, "z_vel": 700,
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
            "seat": -1, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "z_vel": 0, "facing": 0,
            "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0, "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "score": 0, "alive": True,
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


def _observe_frame(tick: int = 0, seat: int = 0, alive: bool = True, deadline: int = 50_000,
                   phase: str = "live", starting_remaining: int = 0) -> dict:
    return {
        "type": "observe", "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH,
        "seat": seat, "tick": tick, "phase": phase, "starting_remaining": starting_remaining,
        "deadline_micros": deadline,
        "own": {"seat": seat, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "z_vel": 0, "facing": 0,
                "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0, "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "score": 0, "alive": alive},
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


def test_policy_receives_the_static_map_once_before_the_first_decision():
    # A map-aware policy is handed the static layout via on_match_start exactly ONCE,
    # after connect and BEFORE the first decision — so it can plan around fixed cover the
    # per-tick Observation omits. Two ticks are scripted, so a per-tick (wrong) firing
    # would surface as more than one start. Driven with a MockTransport, no harness.
    from arena_client.sdk import ArenaClient, MatchStart

    start = {
        "type": "start", "match_id": FIXED_MATCH,
        "config": {"tick_hz": 30, "max_ticks": 3600, "bounds": {"x": 50_000, "y": 50_000}, "seats": 2},
        "blockers": [{"min": {"x": 0, "y": 0}, "max": {"x": 500, "y": 500}}],
        "pickup_points": [{"x": 1000, "y": 0}, {"x": -1000, "y": 0}],
    }

    class _Recorder:
        def __init__(self):
            self.order: list[str] = []
            self.received: list[MatchStart] = []

        def on_match_start(self, ms):
            self.order.append("start")
            self.received.append(ms)

        def __call__(self, _obs):
            self.order.append("decide")
            return _intent(0, 0)

    pol = _Recorder()
    inbound = [_challenge_frame(), _welcome_frame(), start,
               _observe_frame(tick=0), _observe_frame(tick=1), _end_frame()]
    ArenaClient(MockTransport(inbound), agent_id="a", clock=FakeClock([0.0] * 6)).run(pol)

    assert len(pol.received) == 1, "on_match_start fires exactly once, not per tick"
    assert pol.order[0] == "start", "before any decision"
    assert pol.order.index("start") < pol.order.index("decide")
    got = pol.received[0]
    assert got.blockers == [Blocker(min=Vec2(x=0, y=0), max=Vec2(x=500, y=500))]
    assert got.pickup_points == [Vec2(x=1000, y=0), Vec2(x=-1000, y=0)]


def test_hookless_policy_runs_unchanged():
    # A plain callable with no on_match_start runs a full match with no AttributeError —
    # the hook is getattr-guarded, so the per-tick Policy contract is unchanged.
    from arena_client.sdk import ArenaClient

    t = MockTransport([_challenge_frame(), _welcome_frame(), _start_frame(), _observe_frame(), _end_frame()])
    result = ArenaClient(t, agent_id="a", clock=FakeClock([0.0, 0.0])).run(_fixed_policy)
    assert result is not None
    assert any(f["type"] == "act" for f in t.sent)  # it still made its decision


def test_sends_no_reply_during_the_starting_countdown():
    # The harness pump_starting broadcasts the pre-live countdown WITHOUT reading a
    # reply, so the SDK must answer nothing until Live — else the pre-live Act sits in
    # the pipe and is consumed as the stale first Live action, desyncing the match. Two
    # Starting frames then a Live one draw EXACTLY ONE act (the Live tick); dropping the
    # phase guard yields three (the mutation proof).
    from arena_client.sdk import ArenaClient

    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(phase="starting", starting_remaining=2),
               _observe_frame(phase="starting", starting_remaining=1),
               _observe_frame(phase="live"),
               _end_frame()]
    t = MockTransport(inbound)
    result = ArenaClient(t, agent_id="a", clock=FakeClock([0.0] * 4)).run(_fixed_policy)

    assert result is not None
    acts = [f for f in t.sent if f["type"] == "act"]
    assert len(acts) == 1, "exactly one reply — the Live tick, not the two Starting frames"
    assert acts[0]["tick"] == 0  # the Live tick-0 action, with no pre-live leak ahead of it


def test_starting_countdown_surfaces_to_a_countdown_aware_policy():
    # A countdown-aware policy reads each pre-live observation via on_starting (mirroring
    # on_match_start) so it can time GO off starting_remaining. The hook fires once per
    # Starting frame with the LIVE countdown and NEVER for the Live tick (else seen would
    # end in 0), and the policy still draws exactly one Live act.
    from arena_client.sdk import ArenaClient

    class _Countdown:
        def __init__(self):
            self.seen: list[int] = []

        def on_starting(self, obs):
            self.seen.append(obs.starting_remaining)

        def __call__(self, _obs):
            return _intent(0, 0)

    pol = _Countdown()
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(phase="starting", starting_remaining=2),
               _observe_frame(phase="starting", starting_remaining=1),
               _observe_frame(phase="live"),
               _end_frame()]
    t = MockTransport(inbound)
    ArenaClient(t, agent_id="a", clock=FakeClock([0.0] * 4)).run(pol)

    assert pol.seen == [2, 1], "on_starting fires per Starting frame with the live counter, not for Live"
    acts = [f for f in t.sent if f["type"] == "act"]
    assert len(acts) == 1, "still exactly one Live reply"


def test_match_result_surfaces_to_a_result_aware_policy_exactly_once():
    # The terminal MatchResult is surfaced to a result-aware policy via on_match_end
    # (mirroring on_match_start/on_starting) so a stateful policy can close its own loop.
    # It fires EXACTLY ONCE — not per tick — and receives the SAME canonical object run()
    # returns. Two ticks are scripted, so a per-tick (wrong) firing would surface as more
    # than one recorded result. Dropping the dispatch empties `ended` (the mutation proof).
    from arena_client.sdk import ArenaClient

    class _ResultAware:
        def __init__(self):
            self.ended: list[MatchResult] = []

        def on_match_end(self, result):
            self.ended.append(result)

        def __call__(self, _obs):
            return _intent(0, 0)

    pol = _ResultAware()
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(tick=0), _observe_frame(tick=1), _end_frame()]
    result = ArenaClient(MockTransport(inbound), agent_id="a", clock=FakeClock([0.0] * 6)).run(pol)

    assert len(pol.ended) == 1, "on_match_end fires exactly once at match end, not per tick"
    assert pol.ended[0] is result, "the hook receives the SAME canonical result run() returns"


def test_on_match_end_never_fires_on_a_mid_match_reject():
    # on_match_end fires ONLY for the terminal MatchResult, never for a mid-match Reject
    # (an action rejection — sdk.py appends it to `rejections` and reads on). A stream with
    # a Reject BEFORE the end still fires the hook exactly once (the true end), and the
    # Reject is still processed — proof the dispatch keys off the MatchResult frame alone.
    from arena_client.sdk import ArenaClient

    class _ResultAware:
        def __init__(self):
            self.ended: list[MatchResult] = []

        def on_match_end(self, result):
            self.ended.append(result)

        def __call__(self, _obs):
            return _intent(0, 0)

    pol = _ResultAware()
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(tick=0), {"type": "reject", "reason": "stale tick"},
               _observe_frame(tick=1), _end_frame()]
    c = ArenaClient(MockTransport(inbound), agent_id="a", clock=FakeClock([0.0] * 8))
    c.run(pol)

    assert len(pol.ended) == 1, "the Reject did not fire on_match_end; only the terminal result did"
    assert c.rejections == ["stale tick"], "the mid-match Reject was still processed"


def test_partial_hook_policy_without_on_match_end_runs_unchanged():
    # Each hook is INDEPENDENTLY getattr-guarded: a policy that defines on_match_start but
    # NOT on_match_end runs a full match to End with no AttributeError, and run() still
    # returns the result. Proves the new hook's guard is per-attribute, not all-or-nothing.
    from arena_client.sdk import ArenaClient

    class _StartOnly:
        def __init__(self):
            self.started = False

        def on_match_start(self, _ms):
            self.started = True

        def __call__(self, _obs):
            return _intent(0, 0)

    pol = _StartOnly()
    t = MockTransport([_challenge_frame(), _welcome_frame(), _start_frame(),
                       _observe_frame(), _end_frame()])
    result = ArenaClient(t, agent_id="a", clock=FakeClock([0.0, 0.0])).run(pol)

    assert result is not None
    assert pol.started, "on_match_start still fired"
    assert any(f["type"] == "act" for f in t.sent), "the per-tick decision is untouched"


def test_lifecycle_hooks_fire_in_order_start_then_decisions_then_end():
    # The full lifecycle fires in order: on_match_start (once, before any decision) →
    # per-tick decisions → on_match_end (once, after the last decision). A recorder logs
    # each moment so an out-of-order or interleaved end (e.g. dispatched before the final
    # decision) would surface as a wrong sequence.
    from arena_client.sdk import ArenaClient

    class _Recorder:
        def __init__(self):
            self.order: list[str] = []

        def on_match_start(self, _ms):
            self.order.append("start")

        def on_match_end(self, _result):
            self.order.append("end")

        def __call__(self, _obs):
            self.order.append("decide")
            return _intent(0, 0)

    pol = _Recorder()
    inbound = [_challenge_frame(), _welcome_frame(), _start_frame(),
               _observe_frame(tick=0), _observe_frame(tick=1), _end_frame()]
    ArenaClient(MockTransport(inbound), agent_id="a", clock=FakeClock([0.0] * 6)).run(pol)

    assert pol.order == ["start", "decide", "decide", "end"], "start once first, end once last"


def test_wants_leave_concedes_the_match_and_sends_no_action():
    # A policy that concedes via wants_leave (a reason on a Live tick) makes the client send
    # AgentMsg::Leave INSTEAD of an act — the harness forfeits the seat — then read the
    # server's terminal End (broadcast to every seat, the leaver included) so run() still
    # returns the canonical result. __call__ raises: a conceding tick must never reach the
    # action path, so dropping the wants_leave check would fire it (the mutation proof).
    from arena_client.sdk import ArenaClient

    class _Conceder:
        def wants_leave(self, _obs):
            return "gg"

        def __call__(self, _obs):
            raise AssertionError("a conceding tick never reaches the action decision")

    t = MockTransport([_challenge_frame(), _welcome_frame(), _start_frame(),
                       _observe_frame(phase="live"), _end_frame()])
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0] * 2))
    result = c.run(_Conceder())

    assert [f for f in t.sent if f["type"] == "leave"] == [leave_frame("gg")], "exactly one leave, with the reason"
    assert [f for f in t.sent if f["type"] == "act"] == [], "a conceding policy sends no act"
    assert c.left_reason == "gg"
    # done + result land from the End the leaver still reads — its outcome reflects the forfeit.
    assert c.done and c.result is result


def test_a_left_client_draws_no_action_from_a_later_observation():
    # FM3 (act-after-leave): once the Leave is sent the client draws NO further act, even if
    # the server races another Live observation before the End. The policy concedes on tick 0
    # only (tick 1 wants nothing), so WITHOUT the post-leave guard the second observation would
    # fall through to the action path and send an act — exactly one leave, zero acts is the proof.
    from arena_client.sdk import ArenaClient

    class _ConcedeOnce:
        def __init__(self):
            self.ticks = 0

        def wants_leave(self, _obs):
            self.ticks += 1
            return "done" if self.ticks == 1 else None

        def __call__(self, _obs):
            return _intent(100, 0)

    t = MockTransport([_challenge_frame(), _welcome_frame(), _start_frame(),
                       _observe_frame(tick=0, phase="live"), _observe_frame(tick=1, phase="live"),
                       _end_frame()])
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0] * 4))
    c.run(_ConcedeOnce())

    assert [f for f in t.sent if f["type"] == "leave"] == [leave_frame("done")], "left exactly once"
    assert [f for f in t.sent if f["type"] == "act"] == [], "no act before OR after the leave"


def test_a_policy_without_wants_leave_never_leaves():
    # The hook is optional (getattr-guarded): a plain Callable policy with no wants_leave
    # attribute runs a full match to End sending only acts, never a Leave — so the
    # parity-bounded Policy contract stays byte-identical (an unconditional leave reddens this).
    from arena_client.sdk import ArenaClient
    t = MockTransport([_challenge_frame(), _welcome_frame(), _start_frame(),
                       _observe_frame(tick=0), _observe_frame(tick=1), _end_frame()])
    ArenaClient(t, agent_id="a", clock=FakeClock([0.0] * 6)).run(_fixed_policy)
    assert [f for f in t.sent if f["type"] == "leave"] == [], "a hookless policy never leaves"
    assert len([f for f in t.sent if f["type"] == "act"]) == 2, "both live ticks still acted"


def test_wants_leave_never_fires_during_the_starting_countdown():
    # wants_leave is consulted only on a Live tick: a Leave during the pre-live countdown
    # would sit in the pipe and be read as the stale first Live action, desyncing the match
    # (the same reason an Act is withheld during Starting). A policy that always wants to leave
    # draws NO leave on the Starting frames, exactly one on the Live frame — checking
    # wants_leave BEFORE the phase guard would leave during the countdown (the mutation proof).
    from arena_client.sdk import ArenaClient

    class _AlwaysLeave:
        def wants_leave(self, _obs):
            return "bye"

        def __call__(self, _obs):
            return _intent(0, 0)

    t = MockTransport([_challenge_frame(), _welcome_frame(), _start_frame(),
                       _observe_frame(phase="starting", starting_remaining=2),
                       _observe_frame(phase="starting", starting_remaining=1),
                       _observe_frame(phase="live"), _end_frame()])
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0] * 4))
    c.run(_AlwaysLeave())

    assert [f for f in t.sent if f["type"] == "leave"] == [leave_frame("bye")], "leaves once, on the Live tick only"


def test_leave_before_connect_sends_nothing():
    # leave() is safe to call before the handshake: no transport frame, no recorded reason —
    # the guard needs a live, connected match, else the Leave races or the server rejects it.
    from arena_client.sdk import ArenaClient
    t = MockTransport([])
    c = ArenaClient(t, agent_id="a")
    c.leave("early")
    assert t.sent == [] and c.left_reason is None


def test_a_direct_leave_during_the_starting_countdown_is_a_noop():
    # A direct client.leave() (an external-signal abandon) during the countdown sends nothing:
    # the client has processed only a Starting observation, so _phase != "live" and the guard
    # withholds the frame — the direct-call twin of the hook's Live gate.
    from arena_client.sdk import ArenaClient
    t = MockTransport([_challenge_frame(), _welcome_frame(), _start_frame(),
                       _observe_frame(phase="starting", starting_remaining=1)])
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0] * 2)).connect()
    c.poll(_fixed_policy)  # process the Starting frame — _phase becomes "starting", no reply
    c.leave("abort")
    assert [f for f in t.sent if f["type"] == "leave"] == [] and c.left_reason is None


def test_a_second_leave_is_a_noop():
    # FM3 (double-send): once conceded, a second leave() — a retry, or a policy that also
    # calls it — sends no second Leave frame (the guard latches on left_reason), so the
    # server never sees two forfeits for one seat and the first reason stands.
    from arena_client.sdk import ArenaClient
    t = MockTransport([_challenge_frame(), _welcome_frame(), _start_frame(), _observe_frame(phase="live")])
    c = ArenaClient(t, agent_id="a", clock=FakeClock([0.0] * 4)).connect()
    c.poll(_fixed_policy)  # a Live observation sets _phase == "live"
    c.leave("first")
    c.leave("second")
    assert [f for f in t.sent if f["type"] == "leave"] == [leave_frame("first")], "the second leave is a no-op"
    assert c.left_reason == "first"


def test_static_geometry_never_rides_the_parity_bounded_observation():
    # FM1 (parity): the static map is surfaced ONCE at Start, never on the per-tick
    # Observation — the security boundary. A blockers field on an Observation is
    # rejected (extra=forbid), so no static-geometry channel can be added to the
    # parity-bounded snapshot by drift on either side.
    with pytest.raises(pydantic.ValidationError):
        Observation.model_validate({
            "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH, "seat": 0, "tick": 1,
            "phase": "live", "deadline_micros": 50_000,
            "own": {"seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "z_vel": 0, "facing": 0,
                    "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
                    "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "score": 0, "alive": True},
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
            "own": {"seat": 0, "team": 0, "position": {"x": 0, "y": 0}, "z": 0, "z_vel": 0, "facing": 0,
                    "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0,
                    "ammo": 30, "cooldown": 0, "dash_cooldown": 0, "score": 0, "alive": True},
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
    # A visible entry is (eid, team, x, y) — in line of sight — or (eid, team, x, y, los)
    # to pin a perception-memory echo (los=False), the last-known position of a lost enemy.
    def _entry(item):
        eid, t, ex, ey, *rest = item
        los = rest[0] if rest else True
        return {"entity_id": eid, "kind": "player", "team": t, "position": {"x": ex, "y": ey},
                "z": 0, "facing": 0, "in_line_of_sight": los}

    return Observation.model_validate({
        "protocol_version": proto.PROTOCOL_VERSION, "match_id": FIXED_MATCH, "seat": 0, "tick": 0,
        "phase": "live", "deadline_micros": 50_000,
        "own": {"seat": 0, "team": team, "position": {"x": x, "y": y}, "z": 0, "z_vel": 0, "facing": facing,
                "velocity": {"x": 0, "y": 0}, "health": 100, "max_health": 100, "shield": 0, "ammo": ammo, "cooldown": 0, "dash_cooldown": 0, "score": 0, "alive": alive},
        "visible": [_entry(v) for v in visible],
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


def test_baseline_does_not_fire_at_a_remembered_ghost():
    # FM2: the only enemy is a perception-memory echo (in_line_of_sight=False) due
    # east — the last-known position of a lost enemy. The baseline moves to re-acquire
    # it but never fires at the stale position (a shot there can't land, only burns ammo).
    obs = _make_obs(0, 0, team=0, visible=[(1, 1, 5000, 0, False)])
    intent = BaselinePolicy()(obs)
    assert intent.buttons.fire is False  # no blind fire at a ghost
    assert intent.aim == 0  # but advance to re-acquire (east, toward the last-known spot)
    assert intent.move_dir == Vec2(x=1000, y=0)  # not a passive hold — close the distance


def test_baseline_prefers_a_live_target_over_a_nearer_ghost():
    # FM3: a closer ghost (north, out of sight) and a farther LIVE enemy (east, in
    # sight). The baseline fires at the LIVE one even though the ghost is nearer —
    # firing at the nearer echo would waste the shot.
    obs = _make_obs(0, 0, team=0, visible=[(1, 1, 0, 1000, False), (2, 1, 5000, 0, True)])
    intent = BaselinePolicy()(obs)
    assert intent.buttons.fire is True
    assert intent.aim == 0  # east, at the live enemy — not 16384 (north, the nearer ghost)
    assert intent.move_dir.x > 0 and intent.move_dir.y == 0


def test_baseline_default_play_is_byte_identical_when_all_in_sight():
    # FM1: with perception memory off every enemy is in_line_of_sight=True, so the
    # live branch alone decides and the decision matches the range-only baseline — a
    # 4-tuple visible entry (los defaulting True) fires exactly as before.
    obs = _make_obs(0, 0, team=0, visible=[(1, 1, 5000, 0), (2, 1, 0, 3000)])
    intent = BaselinePolicy()(obs)
    assert intent.aim == 16384  # north, the nearer enemy — unchanged from before
    assert intent.buttons.fire is True
    assert intent.move_dir.y > 0 and intent.move_dir.x == 0


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


def test_run_local_match_forfeits_a_conceding_seat():
    # The concede path end to end against the real core: seat 0's policy leaves on its first
    # Live tick, so the SDK sends AgentMsg::Leave, the harness downs the seat (active.remove +
    # m.forfeit), and the 1v1 ends before the tick cap with seat 0 a forfeit DQ (ranked
    # strictly last, not alive) and seat 1 the lone survivor. Proves leave() drives a real
    # durable forfeit — not merely a tick idle — through run_local_match with no bespoke wiring.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    class _Conceder:
        def wants_leave(self, _obs):
            return "conceding"

        def __call__(self, obs):
            return BaselinePolicy()(obs)  # a valid fallback; unreached — it concedes first tick

    match_id = "11111111-2222-4333-8444-555555555555"
    results = run_local_match(harness, [0, 1], {0: _Conceder(), 1: BaselinePolicy()},
                              seed=12345, match_id=match_id)

    assert set(results) == {0, 1}
    r0 = results[0]
    assert r0 == results[1], "every seat sees the same canonical result"
    assert r0.match_id == match_id
    # The concede ends the duel before the tick cap (a losing seat idling to max_ticks is the
    # very state leave() removes), not at 0 (the forfeit lands on the next tick's step).
    assert 0 < r0.final_tick < 3600

    o0 = next(o for o in r0.outcomes if o.seat == 0)
    o1 = next(o for o in r0.outcomes if o.seat == 1)
    assert o0.forfeited and not o0.alive_at_end, "the leaver is a forfeit DQ, downed not surviving"
    assert not o1.forfeited and o1.alive_at_end, "the opponent never left and survives"
    assert o1.placement < o0.placement, "the forfeiter is ranked strictly below the survivor"


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


def _stdout_gateway(*lines: str, collect_spectator: bool = False):
    # A portable stand-in for the harness: write the given stdout lines and exit, so recv
    # reads them line by line — no cargo, no real match. The stderr `emit` helper's twin
    # for the stdout frame stream. `collect_spectator` opts the gateway into buffering the
    # spectator channel for `recv_spectator` (default off = today's drop-it behaviour).
    from arena_client.sdk import SubprocessGateway

    text = "".join(lines)
    return SubprocessGateway(
        [sys.executable, "-c", "import sys; sys.stdout.write(sys.argv[1])", text],
        collect_spectator=collect_spectator,
    )


def test_gateway_recv_tolerates_the_interleaved_spectator_channel():
    # FM1 (crash) + FM4 (seat order): a --spectate/--replay harness interleaves spectator
    # envelopes { "channel": "spectator", "frame": ... } — NO "seat" key — on the SAME
    # stdout as the per-seat frames. recv dispatches on env["seat"], so a spectator line
    # used to raise KeyError and kill the transport mid-match. A participant transport must
    # DROP the spectator channel (it belongs to the spectator client, not a seat) and still
    # deliver every seat frame in order, even with a broadcast wedged between two of them.
    seat0_a = json.dumps({"seat": 0, "frame": {"type": "observe", "tick": 0}}) + "\n"
    spectator = json.dumps({"channel": "spectator", "frame": {"type": "frame", "tick": 0}}) + "\n"
    seat0_b = json.dumps({"seat": 0, "frame": {"type": "observe", "tick": 1}}) + "\n"

    gw = _stdout_gateway(seat0_a, spectator, seat0_b)
    try:
        assert gw.recv(0) == {"type": "observe", "tick": 0}
        assert gw.recv(0) == {"type": "observe", "tick": 1}
        # FM2 (no unbounded buffer): the dropped spectator line routed to no queue — only
        # seat 0 was ever created, so a spectated match never accumulates a sink this side
        # never drains.
        assert set(gw._queues) == {0}, "the spectator channel is dropped, never buffered"
    finally:
        gw.close()


def test_gateway_recv_rejects_an_envelope_with_neither_seat_nor_channel():
    # FM3 (malformed swallowed): a line that is neither a per-seat { "seat" } frame nor the
    # known spectator channel is drift — a typed ProtocolError, never a raw KeyError (the
    # old crash) and never a silent skip (which would mask a real envelope-format change as
    # a benign spectator line).
    unknown = json.dumps({"frame": {"type": "observe"}}) + "\n"  # no seat, no channel
    gw = _stdout_gateway(unknown)
    try:
        with pytest.raises(proto.ProtocolError, match="neither a seat"):
            gw.recv(0)
    finally:
        gw.close()


def _spectator_envelope(frame: dict) -> str:
    # The { "channel": "spectator", "frame": <SpectatorMsg> } line emit_spectator writes — a
    # stdout stand-in for one cast frame, so the consumer is exercised without cargo.
    return json.dumps({"channel": "spectator", "frame": frame}) + "\n"


class _RecordingSpectator:
    # A SpectatorPolicy that records the dispatched stream, so a test can assert the
    # frames-then-terminal-End shape and the public-only field set.
    def __init__(self) -> None:
        self.frames: list = []
        self.ended: list = []

    def on_frame(self, broadcast) -> None:
        self.frames.append(broadcast)

    def on_end(self, result) -> None:
        self.ended.append(result)


# The public on-stage BroadcastEntity key set a caster sees — the scoreboard, never a seat's
# private HUD (no ammo/cooldown/intent/velocity/shield). The consumer-side twin of the Rust
# live_cast_frames_expose_no_private_hud pin.
_PUBLIC_ENTITY_KEYS = {
    "entity_id", "kind", "team", "position", "z", "facing",
    "health", "max_health", "score", "alive",
}


def test_recv_spectator_requires_a_collecting_gateway():
    # recv_spectator on a participant transport (collect_spectator=False, the default) has no
    # buffer — a spectator frame would be dropped by the shared router — so calling it is a loud
    # misconfiguration, never a silent read of a channel that is being discarded.
    gw = _stdout_gateway(_spectator_envelope({"type": "frame", "tick": 0}))
    try:
        with pytest.raises(proto.ProtocolError, match="collect_spectator"):
            gw.recv_spectator()
    finally:
        gw.close()


def test_recv_spectator_drains_only_the_spectator_channel_routing_seat_frames():
    # FM4 (channel isolation) at the gateway seam: on ONE stdout carrying both channels,
    # recv_spectator returns only { "channel": "spectator" } frames in order, and a seat frame it
    # steps over is ROUTED to its queue for recv(seat) — never leaked into the spectator stream
    # nor lost. The mirror of recv(seat) stepping over the spectator channel.
    seat0 = json.dumps({"seat": 0, "frame": {"type": "observe", "tick": 0}}) + "\n"
    gw = _stdout_gateway(
        seat0,
        _spectator_envelope({"type": "frame", "tick": 0}),
        _spectator_envelope({"type": "frame", "tick": 1}),
        collect_spectator=True,
    )
    try:
        assert gw.recv_spectator() == {"type": "frame", "tick": 0}
        assert gw.recv_spectator() == {"type": "frame", "tick": 1}
        # The seat frame the drain stepped over was routed, not dropped.
        assert gw.recv(0) == {"type": "observe", "tick": 0}
    finally:
        gw.close()


def test_arena_spectator_dispatches_golden_frames_then_end_and_returns_result():
    # The decode+dispatch contract a --replay grader relies on, exercised without cargo against
    # the SHARED golden's real wire shapes: two Broadcast frames to on_frame, the terminal End to
    # on_end EXACTLY once, and the MatchResult returned. FM1 (public-only) + FM2 (frames-then-End).
    from arena_client.sdk import ArenaSpectator

    by_label = {c["label"]: c["frame"] for c in _spectator_golden()["frames"]}
    gw = _stdout_gateway(
        _spectator_envelope(by_label["frame_populated"]),
        _spectator_envelope(by_label["frame_legacy_omits_starting_remaining"]),
        _spectator_envelope(by_label["end"]),
        collect_spectator=True,
    )
    policy = _RecordingSpectator()
    try:
        result = ArenaSpectator(gw).run(policy)
    finally:
        gw.close()

    assert [type(f) for f in policy.frames] == [proto.Broadcast, proto.Broadcast]
    assert len(policy.ended) == 1 and policy.ended[0] is result
    assert isinstance(result, MatchResult)
    # FM1: the decoded broadcast entities carry the public scoreboard and no seat-private HUD.
    assert set(policy.frames[0].entities[0].model_dump()) == _PUBLIC_ENTITY_KEYS
    # The legacy frame that omitted starting_remaining decoded to the back-compat default 0.
    assert policy.frames[1].starting_remaining == 0


def test_arena_spectator_truncated_stream_before_end_is_loud():
    # FM2: a cast that ends before its terminal End is a truncated/killed cast — fail LOUD, never
    # return a partial or mis-grade a match that never actually ended.
    from arena_client.sdk import ArenaSpectator

    by_label = {c["label"]: c["frame"] for c in _spectator_golden()["frames"]}
    gw = _stdout_gateway(_spectator_envelope(by_label["frame_populated"]), collect_spectator=True)
    try:
        with pytest.raises(proto.ProtocolError, match="before its terminal End"):
            ArenaSpectator(gw).run(_RecordingSpectator())
    finally:
        gw.close()


def _drive_live_spectate(harness: str, record: Path, *, seed: int, match_id: str):
    # Drive a real 2-seat --spectate match to completion, emitting its replay record AND
    # collecting the live broadcast stream off the SAME gateway. recv(seat) buffers each cast
    # frame while the seats play (so the OS pipe never backs up), then an ArenaSpectator drains
    # the buffered frames plus the trailing terminal End. Returns (live_msgs, seat_results).
    from arena_client.sdk import ArenaClient, ArenaSpectator, SeatTransport, SubprocessGateway

    argv = [
        harness, "--match-id", match_id, "--seed", str(seed), "--seats", "2",
        "--spectate", "--emit-replay", str(record),
    ]
    live: list = []

    class _Collect:
        def on_frame(self, b) -> None:
            live.append(b)

        def on_end(self, r) -> None:
            live.append(r)

    with SubprocessGateway(argv, collect_spectator=True) as gw:
        clients = {
            s: ArenaClient(SeatTransport(gw, s), agent_id=f"agent-{s}", enforce_deadline=False)
            for s in (0, 1)
        }
        for client in clients.values():
            client.connect()
        results: dict[int, MatchResult] = {}
        while len(results) < 2:
            for seat, client in clients.items():
                if client.done:
                    continue
                outcome = client.poll(BaselinePolicy())
                if outcome is not None:
                    results[seat] = outcome
        # The seats hold their Ends; the buffered cast frames + the trailing terminal End remain
        # on the spectator channel — drain them from the SAME gateway.
        ArenaSpectator(gw).run(_Collect())
    return live, results


def test_spectator_replay_casts_public_frames_then_one_terminal_end(tmp_path):
    # FM1 + FM2 + FM4 on REAL cast data: a --replay of a finished match's record casts a stream of
    # public Broadcast frames then EXACTLY ONE terminal End (the stream EOFs right after it), every
    # broadcast entity carries only the public scoreboard, and the cast rides ONLY the spectator
    # channel — no per-seat frame ever arrives. The offline-grading path, end to end.
    harness = _require_or_skip_harness()
    from arena_client.sdk import ArenaSpectator, GatewayClosed

    record = tmp_path / "match.json"
    _drive_live_spectate(harness, record, seed=777, match_id="aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee")

    policy = _RecordingSpectator()
    with ArenaSpectator.replay(harness, record) as spec:
        result = spec.run(policy)
        # FM2: the End was terminal — the cast stream is exhausted right after it.
        with pytest.raises(GatewayClosed):
            spec._gateway.recv_spectator()
        # FM4: a --replay cast emits no per-seat frames (its own channel alone).
        assert dict(spec._gateway._queues) == {}, "a replay cast routes no seat frames"

    assert isinstance(result, MatchResult)
    assert len(policy.ended) == 1 and policy.ended[0] is result
    assert policy.frames, "a real match casts at least one broadcast frame"
    assert all(isinstance(f, proto.Broadcast) for f in policy.frames)
    for frame in policy.frames:
        for entity in frame.entities:
            assert set(entity.model_dump()) == _PUBLIC_ENTITY_KEYS


def test_spectator_live_cast_equals_replay_of_the_same_match(tmp_path):
    # FM3 (live == replay): a live --spectate cast and a --replay of that same seeded match's
    # record decode to the SAME Broadcast sequence, frame for frame, and the same terminal result
    # — the Python mirror of the Rust a_no_countdown_live_cast_matches_the_replay_of_the_same_match.
    harness = _require_or_skip_harness()
    from arena_client.sdk import ArenaSpectator

    record = tmp_path / "match.json"
    live, results = _drive_live_spectate(
        harness, record, seed=4242, match_id="12121212-3434-4545-8656-767676767676"
    )

    replay = _RecordingSpectator()
    with ArenaSpectator.replay(harness, record) as spec:
        replay_result = spec.run(replay)

    live_frames = [m for m in live if isinstance(m, proto.Broadcast)]
    live_end = [m for m in live if isinstance(m, MatchResult)]
    # Guard the equality against a vacuous [] == [] pass: a real match casts frames on both paths.
    assert live_frames, "the live cast produced at least one broadcast frame"
    assert live_frames == replay.frames, "the live cast equals the record's replay, frame for frame"
    assert live_end == [replay_result], "and the same terminal result"
    assert replay_result.match_id == results[0].match_id


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


def test_run_local_match_forwards_map_as_one_argv_token_and_omits_it_by_default(monkeypatch):
    # arena= forwards --map <key> as a single argv token; omitted, no --map appears, so an
    # existing caller's argv is byte-identical. Captured without a harness by stubbing the
    # gateway before it spawns.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--map" not in captured["argv"]  # omitted: byte-identical argv

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, arena="reference")
    argv = captured["argv"]
    assert argv[argv.index("--map") + 1] == "reference"  # set: the key is the one next token


def test_run_local_match_forwards_map_file_as_one_argv_token_and_omits_it_by_default(monkeypatch):
    # FM1 (default omit) + FM3 (path-type): map_file forwards --map-file <path> as ONE argv
    # token, a str and a Path producing the SAME token (str(map_file)); omitted, no
    # --map-file appears so an existing caller's argv is byte-identical. Both directions
    # mutation-prove the `if map_file is not None:` gate — dropping the block reddens the
    # set-forwards asserts, making it always-forward reddens the omit assert. Captured
    # without a harness by stubbing the gateway before it spawns.
    from pathlib import Path

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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--map-file" not in captured["argv"]  # omitted: byte-identical argv

    # Set as a str: the exact path is the one token after the flag (spaces intact, one token).
    spaced = "/tmp/a b/authored arena.json"
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, map_file=spaced)
    argv = captured["argv"]
    assert argv[argv.index("--map-file") + 1] == spaced

    # Set as a Path: forwards as the SAME stringified token, never a repr / PosixPath(...).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, map_file=Path(spaced))
    argv = captured["argv"]
    assert argv[argv.index("--map-file") + 1] == spaced


def test_run_local_match_forwards_starting_ticks_and_omits_it_by_default(monkeypatch):
    # starting_ticks forwards --starting-ticks <n> as a value-flag when nonzero; omitted (0),
    # no --starting-ticks appears, so an existing caller's argv is byte-identical (the match
    # opens Live at tick 0). Captured without a harness by stubbing the gateway before it spawns.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--starting-ticks" not in captured["argv"]  # omitted: byte-identical argv

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, starting_ticks=3)
    argv = captured["argv"]
    assert argv[argv.index("--starting-ticks") + 1] == "3"  # set: the count is the one next token


def test_run_local_match_rejects_an_out_of_range_starting_ticks():
    # starting_ticks is a u32 in core (0..=2**32-1), the pre-live countdown length. Unlike the None-sentinel
    # deadline it defaults to 0 (off) and forwards on truthiness, so a negative is TRUTHY and would be forwarded
    # as --starting-ticks -1 rather than swallowed as off — and the harness parses --starting-ticks as a u32 and
    # panics on a negative / past-max value, so it raises before any spawn, mirroring the sibling fences. 0 stays
    # the valid off case (tested above). A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -3, 2**32, 2**40):
        with pytest.raises(ValueError, match="starting_ticks"):
            run_local_match("/no/such/harness", [0, 1], policies, starting_ticks=bad)


def test_run_local_match_rejects_an_out_of_range_seed():
    # seed forwards raw as --seed <n>, which core parses as a u64 (main.rs `let mut seed: u64`) and
    # panics at startup on a negative / past-u64::MAX value. It is the base determinism-arg family's
    # unfenced sibling, so it raises before any spawn like the starting_ticks fence — width u64 (wider
    # than the u32 twins) with BOTH bounds enforced, so a negative (the seed=hash(...) footgun) and an
    # overflow both reject while the default 0 stays valid. The bogus harness path proves the guard
    # precedes spawn: a real spawn attempt would raise a non-matching OSError, not this ValueError.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -3, 2**64, 2**80):
        with pytest.raises(ValueError, match="seed"):
            run_local_match("/no/such/harness", [0, 1], policies, seed=bad)


def test_run_local_match_forwards_max_ticks_and_omits_it_by_default(monkeypatch):
    # max_ticks forwards --max-ticks <n> as a value-flag when set (the MatchConfig cap the SDK
    # previously could not reach); omitted (None), no --max-ticks token appears so the argv is
    # byte-identical to the base and the harness applies its 3600-tick default. Captured without a
    # harness by stubbing the gateway before it spawns.
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
    mid = "00000000-0000-4000-8000-000000000000"

    # Omitted (None): the argv is EXACTLY the base — no --max-ticks token anywhere.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, match_id=mid)
    assert captured["argv"] == ["h", "--match-id", mid, "--seed", "0", "--seats", "2"]

    # Set: --max-ticks lands in the base block (before any mode-gated flag) with 4 as its one next token.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, match_id=mid, max_ticks=4)
    argv = captured["argv"]
    assert argv[argv.index("--max-ticks") + 1] == "4"
    assert argv == ["h", "--match-id", mid, "--seed", "0", "--seats", "2", "--max-ticks", "4"]


def test_run_local_match_rejects_an_out_of_range_max_ticks():
    # max_ticks forwards raw as --max-ticks <n>, which the harness parses as a u64 and panics on a
    # negative / past-u64::MAX cap. Unlike seed it defaults to None (omit → harness 3600 default), so
    # the fence guards only a set value: a negative or overflow raises before any spawn, while 0 stays a
    # valid (caller-owned) cap. The bogus harness path proves the guard precedes spawn — a real spawn
    # would raise a non-matching OSError, not this ValueError.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -3, 2**64, 2**80):
        with pytest.raises(ValueError, match="max_ticks"):
            run_local_match("/no/such/harness", [0, 1], policies, max_ticks=bad)


def test_run_local_match_caps_match_length_at_max_ticks():
    # The bound the loose 0 < final_tick < 3600 e2es structurally cannot pin: with max_ticks=4 the match
    # is force-ended at the cap, so final_tick <= 4 (a baseline duel is not decisive by tick 4, so the cap,
    # not a KO, ends it — the pre-forward SDK ran to ~decisive or the 3600 default). Required (fails, not
    # skips) under CI/validate.sh; skips only on a plain local pytest without the toolchain.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    results = run_local_match(harness, [0, 1], policies, seed=7, max_ticks=4)

    assert set(results) == {0, 1}
    assert results[0] == results[1], "every seat sees the same canonical result"
    assert 0 < results[0].final_tick <= 4


def test_run_local_match_selects_a_named_arena_and_surfaces_its_geometry():
    # arena="reference" plays the match under the reference arena: each seat's Start frame
    # carries the central occluder + the two health pickups, read back via starts=. The
    # default (no arena=) surfaces the empty arena — proof the flag, not a harness default,
    # drives the geometry.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    mid = "33333333-2222-4333-8444-555555555555"
    named: dict = {}
    run_local_match(harness, [0, 1], policies, seed=3, match_id=mid, arena="reference", starts=named)
    assert set(named) == {0, 1}
    for seat in (0, 1):
        assert named[seat].blockers, f"seat {seat} received the reference cover"
        assert len(named[seat].pickup_points) == 2, f"seat {seat} received the two pickups"

    empty: dict = {}
    run_local_match(harness, [0, 1], policies, seed=3, match_id=mid, starts=empty)
    assert empty[0].blockers == [] and empty[0].pickup_points == []


def test_run_local_match_streams_the_starting_countdown_to_a_policy():
    # The end-to-end proof of the Starting-phase agent path against the REAL harness (not
    # MockTransport): starting_ticks=3 opens the match in Starting for 3 ticks. The harness
    # pump_starting broadcasts each pre-live observation with NO reply read, so a
    # countdown-aware policy reads the decrementing starting_remaining via on_starting while
    # the SDK sends nothing — the phase!=live guard and the hook, proven against pump_starting.
    # The countdown is digest-inert (outside canonical_encoding), so the match resolves to the
    # SAME replay_hash as the no-countdown run driven by the identical Live-phase policy.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    class _Countdown(BaselinePolicy):
        def __init__(self):
            super().__init__()
            self.countdown: list[int] = []

        def on_starting(self, obs):
            self.countdown.append(obs.starting_remaining)

    seed = 24680
    match_id = "33333333-4444-4555-8666-777777777777"
    pols = {0: _Countdown(), 1: _Countdown()}
    counted = run_local_match(harness, [0, 1], pols, seed=seed, match_id=match_id, starting_ticks=3)

    # Each seat saw the full countdown 3,2,1 broadcast per Starting tick, never 0 (the flip is Live).
    assert pols[0].countdown == [3, 2, 1], "seat 0 read the live pre-live countdown via on_starting"
    assert pols[1].countdown == [3, 2, 1], "seat 1 read the same broadcast countdown"

    # The countdown is a digest-inert pre-live delay: the no-countdown run under the same seat
    # policies (identical Live actions) carries the byte-identical replay_hash.
    plain = run_local_match(harness, [0, 1], {0: BaselinePolicy(), 1: BaselinePolicy()},
                            seed=seed, match_id=match_id)
    assert counted[0].replay_hash == plain[0].replay_hash, "the countdown moves no replay_hash"
    assert 0 < counted[0].final_tick < 3600 and counted[0] == counted[1]


def test_run_matchmade_named_arena_surfaces_geometry_on_the_agent_path():
    # --map reaches the --mode (matchmaker) path too: an Agent-mode match under the
    # reference arena surfaces the same cover + pickups to each ranked seat.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    starts: dict = {}
    run_local_match(
        harness, [0, 1], policies, seed=5, match_id="44444444-2222-4333-8444-555555555555",
        mode="agent", signing_keys=keys, arena="reference", starts=starts,
    )
    for seat in (0, 1):
        assert starts[seat].blockers and len(starts[seat].pickup_points) == 2


def test_run_local_match_map_file_overrides_the_arena_key_geometry(tmp_path):
    # FM2 (override precedence forwarded): with BOTH arena="reference" (two health pickups)
    # and map_file=<file> (one shield pickup), the SDK forwards both flags and the harness
    # resolves the FILE over the key — each seat's Start carries the file's single pickup, not
    # reference's two. The SDK never drops --map to force precedence; the harness owns it.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    authored = tmp_path / "authored.json"
    authored.write_text(
        '{"blockers":[{"min":{"x":-400,"y":-400},"max":{"x":400,"y":400}}],'
        '"pickups":[{"kind":"shield","position":{"x":800,"y":0},"amount":40}]}'
    )
    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    starts: dict = {}
    run_local_match(
        harness, [0, 1], policies, seed=3, match_id="66666666-2222-4333-8444-555555555555",
        arena="reference", map_file=authored, starts=starts,
    )
    assert set(starts) == {0, 1}
    for seat in (0, 1):
        assert starts[seat].blockers, f"seat {seat} received the file's cover"
        assert len(starts[seat].pickup_points) == 1, (
            f"seat {seat} played the FILE's single pickup, not reference's two — the file won over the key"
        )


def test_run_matchmade_map_file_surfaces_authored_geometry_on_the_agent_path(tmp_path):
    # FM4 (matchmade path): --map-file reaches the --mode (matchmaker) path too — an Agent-mode
    # match forms under the authored file (arena-match-map-override made --map-file valid with
    # --mode), surfacing the file's single pickup to each ranked seat, not the empty default.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    authored = tmp_path / "authored.json"
    authored.write_text(
        '{"blockers":[{"min":{"x":-400,"y":-400},"max":{"x":400,"y":400}}],'
        '"pickups":[{"kind":"shield","position":{"x":800,"y":0},"amount":40}]}'
    )
    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    starts: dict = {}
    run_local_match(
        harness, [0, 1], policies, seed=5, match_id="77777777-2222-4333-8444-555555555555",
        mode="agent", signing_keys=keys, map_file=authored, starts=starts,
    )
    for seat in (0, 1):
        assert starts[seat].blockers and len(starts[seat].pickup_points) == 1


def test_run_local_match_bad_map_file_fails_loud_not_silent_empty():
    # FM3 (bad path is loud): an unreadable --map-file aborts the harness at load (like an
    # unknown --map key), so the stream closes — never a silent fall-through to an empty arena
    # that would mask a mis-pathed authored map.
    harness = _require_or_skip_harness()
    from arena_client.sdk import GatewayClosed, run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    with pytest.raises(GatewayClosed):
        run_local_match(harness, [0, 1], policies, seed=3, map_file="/no/such/authored/map.json")


def test_run_local_match_unknown_arena_fails_loud_not_silent_empty():
    # An unknown --map key aborts the harness at parse (mirroring --mode), so the stream
    # closes (GatewayClosed) — never a silent fall-through to an empty arena.
    harness = _require_or_skip_harness()
    from arena_client.sdk import GatewayClosed, run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    with pytest.raises(GatewayClosed):
        run_local_match(
            harness, [0, 1], policies, seed=1,
            match_id="55555555-2222-4333-8444-555555555555", arena="does-not-exist",
        )


def test_run_local_match_forwards_perception_memory_and_omits_it_by_default(monkeypatch):
    # perception_memory>0 forwards --perception-memory <ticks> as one argv token; omitted
    # (0) adds no flag, byte-identical argv. Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--perception-memory" not in captured["argv"]

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, perception_memory=45)
    argv = captured["argv"]
    assert argv[argv.index("--perception-memory") + 1] == "45"


def test_run_local_match_forwards_perception_memory_under_mode(monkeypatch):
    # arena-matchparams-rules-knob threaded --perception-memory through the matchmaker too
    # (MatchParams.rules), so a ranked/matchmade run now carries the window — the old
    # "direct-path only" rejection is gone. The forward sits before the mode block, so --mode
    # and --perception-memory both reach argv. Spawn is stubbed, so this needs no toolchain.
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
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, perception_memory=30)
    argv = captured["argv"]
    assert argv[argv.index("--perception-memory") + 1] == "30"
    assert argv[argv.index("--mode") + 1] == "agent"

    # FM3: omitted (0) adds no flag under --mode either — byte-identical to the pre-knob
    # matchmade argv. The forward is mode-independent (before the mode block), so it stays off.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys)
    assert "--perception-memory" not in captured["argv"]


def test_run_local_match_forwards_fov_and_omits_it_at_the_default(monkeypatch):
    # A non-default fov forwards --fov <spread> as one argv token, mode-independently (before
    # the mode block, like --map/--perception-memory); the default 4 (full circle) adds no
    # flag — byte-identical argv. fov=0 is non-default, so it MUST still forward. Captured
    # without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--fov" not in captured["argv"], "the default full circle adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, fov=2)
    argv = captured["argv"]
    assert argv[argv.index("--fov") + 1] == "2"

    # fov=0 (facing octant alone) is non-default — it must forward, not be mistaken for "off".
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, fov=0)
    argv = captured["argv"]
    assert argv[argv.index("--fov") + 1] == "0"

    # Threads under --mode too (mode-independent forward): --fov and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, fov=1)
    argv = captured["argv"]
    assert argv[argv.index("--fov") + 1] == "1"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_perception_memory():
    # perception_memory is a u16 tick window (0..=65535, 0 = off); a negative is inert in core's
    # `> 0` gate and an over-u16 value wraps (65536 -> 0, also off), so it raises before any spawn
    # — the u16 fence every sibling numeric knob has, mirroring the harness u16 rather than
    # forwarding a value the harness aborts on. A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -45, 65536, 70000):
        with pytest.raises(ValueError, match="perception_memory"):
            run_local_match("/no/such/harness", [0, 1], policies, perception_memory=bad)


def test_run_local_match_rejects_an_out_of_range_fov():
    # The cone is an octant spread in 0..=4; an out-of-range value raises before any spawn
    # (mirroring the ladder_file/mode preflights) rather than letting the harness saturate a
    # spread >4 to a full circle. A bogus harness path proves the guard precedes the spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, 5, 100):
        with pytest.raises(ValueError, match="0..=4"):
            run_local_match("/no/such/harness", [0, 1], policies, fov=bad)


def test_run_local_match_forwards_aim_mode_and_omits_it_at_the_default(monkeypatch):
    # A non-default aim_mode forwards --aim-mode <value> as one argv token, mode-independently
    # (before the mode block, like --fov); the default "octant" adds no flag — byte-identical
    # argv. Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--aim-mode" not in captured["argv"], "the default octant adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, aim_mode="fine")
    argv = captured["argv"]
    assert argv[argv.index("--aim-mode") + 1] == "fine"

    # Explicit "octant" is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, aim_mode="octant")
    assert "--aim-mode" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): --aim-mode and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, aim_mode="fine")
    argv = captured["argv"]
    assert argv[argv.index("--aim-mode") + 1] == "fine"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_unknown_aim_mode():
    # aim_mode is the literal set {octant, fine}; an unknown value raises before any spawn
    # (mirroring the fov/mode preflights) rather than forwarding a bad token that aborts the
    # harness into an opaque GatewayClosed. A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in ("Fine", "fancy", "", "8"):
        with pytest.raises(ValueError, match="octant.*fine"):
            run_local_match("/no/such/harness", [0, 1], policies, aim_mode=bad)


def test_run_local_match_forwards_friendly_fire_as_one_bare_flag_and_omits_it_by_default(monkeypatch):
    # friendly_fire=True forwards EXACTLY one BARE --friendly-fire token (no value, unlike
    # --fov/--aim-mode), before the mode block so both paths get it; the default False adds no
    # flag — byte-identical argv. Captured without a harness via the spy gateway. No behavioral
    # e2e: run_local_match's free-for-all roster (each seat its own team) gives friendly_fire no
    # allied body to hit, so the contract here is reachability, not an outcome divergence.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    base = captured["argv"]
    assert "--friendly-fire" not in base, "the default adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, friendly_fire=True)
    on = captured["argv"]
    assert on.count("--friendly-fire") == 1
    # It is a PRESENCE flag: dropping the single inserted token yields the EXACT default argv —
    # so no stray value rides along (a ['--friendly-fire', value] forward would add two tokens,
    # the second of which the harness reads as an unknown argument and panics on).
    assert [t for t in on if t != "--friendly-fire"] == base

    # Explicit False is the default — no flag (byte-identical to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, friendly_fire=False)
    assert "--friendly-fire" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): --friendly-fire and --mode both reach
    # argv, and --friendly-fire stays bare — the token after it is --mode (a flag), not a value.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, friendly_fire=True)
    argv = captured["argv"]
    assert argv.count("--friendly-fire") == 1
    assert argv[argv.index("--friendly-fire") + 1].startswith("--")
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_forwards_gravity_and_omits_it_at_the_default(monkeypatch):
    # gravity>0 forwards --gravity <n> as one value token, mode-independently (before the mode
    # block, like --fov); the default 0 (physics off) adds no flag — byte-identical argv. Captured
    # without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--gravity" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, gravity=500)
    argv = captured["argv"]
    assert argv[argv.index("--gravity") + 1] == "500"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, gravity=0)
    assert "--gravity" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): --gravity and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, gravity=500)
    argv = captured["argv"]
    assert argv[argv.index("--gravity") + 1] == "500"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_gravity():
    # gravity is a non-negative magnitude in 0..=i32::MAX; a negative (silently OFF in core, which
    # gates physics on gravity > 0) or an overflow (wraps the fall integration negative = also off)
    # raises before any spawn, mirroring the harness's u32-then-i32 parse_gravity fence rather than
    # forwarding a value that runs a 2D match the caller did not ask for. A bogus harness path
    # proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -500, 2**31, 2**40):
        with pytest.raises(ValueError, match="gravity"):
            run_local_match("/no/such/harness", [0, 1], policies, gravity=bad)


def test_run_local_match_forwards_weapon_mode_and_omits_it_at_the_default(monkeypatch):
    # A non-default weapon_mode forwards --weapon-mode <value> as one token, mode-independently
    # (before the mode block, like --aim-mode); the default "hitscan" adds no flag — byte-identical
    # argv. Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--weapon-mode" not in captured["argv"], "the default hitscan adds no flag"

    for weapon in ("projectile", "melee"):
        with pytest.raises(_Stop):
            sdk.run_local_match("h", [0, 1], policies, weapon_mode=weapon)
        argv = captured["argv"]
        assert argv[argv.index("--weapon-mode") + 1] == weapon

    # Explicit "hitscan" is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, weapon_mode="hitscan")
    assert "--weapon-mode" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): --weapon-mode and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, weapon_mode="melee")
    argv = captured["argv"]
    assert argv[argv.index("--weapon-mode") + 1] == "melee"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_unknown_weapon_mode():
    # weapon_mode is the literal set {hitscan, projectile, melee}; an unknown value raises before
    # any spawn (mirroring the aim_mode/fov preflights) rather than forwarding a bad token that
    # aborts the harness into an opaque GatewayClosed. Case-sensitive: "Hitscan"/"HITSCAN" are not
    # the canonical name. A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in ("Hitscan", "bow", "", "HITSCAN", " melee"):
        with pytest.raises(ValueError, match="hitscan.*projectile.*melee"):
            run_local_match("/no/such/harness", [0, 1], policies, weapon_mode=bad)


def test_run_local_match_forwards_vertical_hit_tolerance_and_omits_it_at_the_default(monkeypatch):
    # vertical_hit_tolerance>0 forwards --vertical-hit-tolerance <n> as one value token,
    # mode-independently (before the mode block, like --gravity); the default 0 (combat planar) adds
    # no flag — byte-identical argv. Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--vertical-hit-tolerance" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, vertical_hit_tolerance=500)
    argv = captured["argv"]
    assert argv[argv.index("--vertical-hit-tolerance") + 1] == "500"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, vertical_hit_tolerance=0)
    assert "--vertical-hit-tolerance" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match(
            "h", [0, 1], policies, mode="agent", signing_keys=keys, vertical_hit_tolerance=500
        )
    argv = captured["argv"]
    assert argv[argv.index("--vertical-hit-tolerance") + 1] == "500"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_vertical_hit_tolerance():
    # vertical_hit_tolerance is a non-negative band in 0..=i32::MAX; a negative (silently OFF in core,
    # which gates z-coupled hits on tolerance > 0) or an overflow (wraps the band negative = also off)
    # raises before any spawn, mirroring the harness's u32-then-i32 parse_vertical_hit_tolerance fence
    # rather than forwarding a value that runs a planar match the caller did not ask for. A bogus
    # harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -500, 2**31, 2**40):
        with pytest.raises(ValueError, match="vertical_hit_tolerance"):
            run_local_match("/no/such/harness", [0, 1], policies, vertical_hit_tolerance=bad)


def test_run_local_match_forwards_fall_damage_and_omits_it_at_the_default(monkeypatch):
    # fall_damage>0 forwards --fall-damage <hp> as one value token, mode-independently (before the
    # mode block, like --gravity); the default 0 (safe landings) adds no flag — byte-identical argv.
    # Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--fall-damage" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, fall_damage=250)
    argv = captured["argv"]
    assert argv[argv.index("--fall-damage") + 1] == "250"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, fall_damage=0)
    assert "--fall-damage" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, fall_damage=250)
    argv = captured["argv"]
    assert argv[argv.index("--fall-damage") + 1] == "250"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_fall_damage():
    # fall_damage is a u16 in core (0..=65535); a negative or an overflow forwarded blindly would
    # wrap into a magnitude the caller never asked for (65536 -> 0, every landing safe) and abort the
    # harness, so it raises before any spawn, mirroring the harness's u16 --fall-damage parse. A bogus
    # harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -500, 65536, 2**20):
        with pytest.raises(ValueError, match="fall_damage"):
            run_local_match("/no/such/harness", [0, 1], policies, fall_damage=bad)


def test_run_local_match_forwards_knockback_velocity_and_omits_it_at_the_default(monkeypatch):
    # knockback_velocity>0 forwards --knockback-velocity <n> as one value token, mode-independently
    # (before the mode block, like --gravity); the default 0 (no impulse) adds no flag — byte-identical
    # argv. Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--knockback-velocity" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, knockback_velocity=800)
    argv = captured["argv"]
    assert argv[argv.index("--knockback-velocity") + 1] == "800"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, knockback_velocity=0)
    assert "--knockback-velocity" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match(
            "h", [0, 1], policies, mode="agent", signing_keys=keys, knockback_velocity=800
        )
    argv = captured["argv"]
    assert argv[argv.index("--knockback-velocity") + 1] == "800"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_knockback_velocity():
    # knockback_velocity is a non-negative i32 (0..=2**31-1); a negative would launch the hit target
    # DOWNWARD into the floor and an overflow would wrap the impulse, so it raises before any spawn,
    # mirroring the harness's u32-then-i32 parse_knockback_velocity fence rather than forwarding a
    # value the harness aborts on. A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -800, 2**31, 2**40):
        with pytest.raises(ValueError, match="knockback_velocity"):
            run_local_match("/no/such/harness", [0, 1], policies, knockback_velocity=bad)


def test_run_local_match_forwards_fall_damage_threshold_and_omits_it_at_the_default(monkeypatch):
    # fall_damage_threshold>0 forwards --fall-damage-threshold <n> as one value token, mode-independently
    # (before the mode block, like --gravity); the default 0 (gate open) adds no flag — byte-identical
    # argv. Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--fall-damage-threshold" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, fall_damage_threshold=3000)
    argv = captured["argv"]
    assert argv[argv.index("--fall-damage-threshold") + 1] == "3000"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, fall_damage_threshold=0)
    assert "--fall-damage-threshold" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match(
            "h", [0, 1], policies, mode="agent", signing_keys=keys, fall_damage_threshold=3000
        )
    argv = captured["argv"]
    assert argv[argv.index("--fall-damage-threshold") + 1] == "3000"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_fall_damage_threshold():
    # fall_damage_threshold is a non-negative i32 (0..=2**31-1); a negative makes core's `impact >
    # threshold` true for EVERY landing (the inverse of raising the bar) and an overflow wraps, so it
    # raises before any spawn, mirroring the harness's u32-then-i32 parse_fall_damage_threshold fence
    # rather than forwarding a value the harness aborts on. A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -3000, 2**31, 2**40):
        with pytest.raises(ValueError, match="fall_damage_threshold"):
            run_local_match("/no/such/harness", [0, 1], policies, fall_damage_threshold=bad)


def test_run_local_match_forwards_knockback_horizontal_and_omits_it_at_the_default(monkeypatch):
    # knockback_horizontal>0 forwards --knockback-horizontal <n> as one value token, mode-independently
    # (before the mode block, like --gravity); the default 0 (no shove) adds no flag — byte-identical
    # argv. Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--knockback-horizontal" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, knockback_horizontal=200)
    argv = captured["argv"]
    assert argv[argv.index("--knockback-horizontal") + 1] == "200"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, knockback_horizontal=0)
    assert "--knockback-horizontal" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match(
            "h", [0, 1], policies, mode="agent", signing_keys=keys, knockback_horizontal=200
        )
    argv = captured["argv"]
    assert argv[argv.index("--knockback-horizontal") + 1] == "200"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_knockback_horizontal():
    # knockback_horizontal is a non-negative i32 (0..=2**31-1); a negative is inert in core's `> 0` gate
    # (silently no shove) and an overflow wraps, so it raises before any spawn, mirroring the harness's
    # u32-then-i32 parse_knockback_horizontal fence rather than forwarding a value the harness aborts on.
    # A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -200, 2**31, 2**40):
        with pytest.raises(ValueError, match="knockback_horizontal"):
            run_local_match("/no/such/harness", [0, 1], policies, knockback_horizontal=bad)


def test_run_local_match_forwards_wall_slide_as_one_bare_flag_and_omits_it_by_default(monkeypatch):
    # wall_slide=True forwards EXACTLY one BARE --wall-slide token (no value, like --friendly-fire),
    # before the mode block so both paths get it; the default False adds no flag — byte-identical argv.
    # Captured without a harness via the spy gateway. No behavioral e2e: the default empty arena has no
    # blocker to graze, so the contract here is reachability, not a movement divergence.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    base = captured["argv"]
    assert "--wall-slide" not in base, "the default adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, wall_slide=True)
    on = captured["argv"]
    assert on.count("--wall-slide") == 1
    # It is a PRESENCE flag: dropping the single inserted token yields the EXACT default argv — so no
    # stray value rides along (a ['--wall-slide', value] forward would add two tokens, the second of
    # which the harness reads as an unknown argument and panics on).
    assert [t for t in on if t != "--wall-slide"] == base

    # Explicit False is the default — no flag (byte-identical to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, wall_slide=False)
    assert "--wall-slide" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): --wall-slide and --mode both reach argv, and
    # --wall-slide stays bare — the token after it is --mode (a flag), not a value.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, wall_slide=True)
    argv = captured["argv"]
    assert argv.count("--wall-slide") == 1
    assert argv[argv.index("--wall-slide") + 1].startswith("--")
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_forwards_dash_cooldown_and_omits_it_at_the_default(monkeypatch):
    # dash_cooldown>0 forwards --dash-cooldown <ticks> as one value token, mode-independently (before the
    # mode block, like --gravity); the default 0 (dash off) adds no flag — byte-identical argv. Captured
    # without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--dash-cooldown" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, dash_cooldown=20)
    argv = captured["argv"]
    assert argv[argv.index("--dash-cooldown") + 1] == "20"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, dash_cooldown=0)
    assert "--dash-cooldown" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, dash_cooldown=20)
    argv = captured["argv"]
    assert argv[argv.index("--dash-cooldown") + 1] == "20"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_dash_cooldown():
    # dash_cooldown is a u16 in core (0..=65535); a negative or an overflow forwarded blindly would wrap
    # into a cadence the caller never asked for (65536 -> 0, which ALSO reads as "dash off") and abort the
    # harness, so it raises before any spawn, mirroring the harness's u16 --dash-cooldown parse. A bogus
    # harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -20, 65536, 2**20):
        with pytest.raises(ValueError, match="dash_cooldown"):
            run_local_match("/no/such/harness", [0, 1], policies, dash_cooldown=bad)


def test_run_local_match_forwards_pawn_radius_and_omits_it_at_the_default(monkeypatch):
    # pawn_radius>0 forwards --pawn-radius <n> as one value token, mode-independently (before the mode
    # block, like --gravity); the default 0 (occupancy off) adds no flag — byte-identical argv. Captured
    # without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--pawn-radius" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pawn_radius=750)
    argv = captured["argv"]
    assert argv[argv.index("--pawn-radius") + 1] == "750"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pawn_radius=0)
    assert "--pawn-radius" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, pawn_radius=750)
    argv = captured["argv"]
    assert argv[argv.index("--pawn-radius") + 1] == "750"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_pawn_radius():
    # pawn_radius is a non-negative i32 (0..=2**31-1); a negative is inert in core's `> 0` gate (silently
    # no collision) and an overflow wraps, so it raises before any spawn, mirroring the harness's
    # u32-then-i32 parse_pawn_radius fence rather than forwarding a value the harness aborts on. A bogus
    # harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -750, 2**31, 2**40):
        with pytest.raises(ValueError, match="pawn_radius"):
            run_local_match("/no/such/harness", [0, 1], policies, pawn_radius=bad)


def test_run_local_match_forwards_pawn_height_and_omits_it_at_the_default(monkeypatch):
    # pawn_height>0 forwards --pawn-height <n> as one value token, mode-independently (before the mode
    # block, like --gravity); the default 0 (planar occupancy) adds no flag — byte-identical argv.
    # Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--pawn-height" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pawn_height=1800)
    argv = captured["argv"]
    assert argv[argv.index("--pawn-height") + 1] == "1800"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pawn_height=0)
    assert "--pawn-height" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, pawn_height=1800)
    argv = captured["argv"]
    assert argv[argv.index("--pawn-height") + 1] == "1800"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_pawn_height():
    # pawn_height is a non-negative i32 (0..=2**31-1); a negative is inert in core's `> 0` gate (silently
    # planar) and an overflow wraps, so it raises before any spawn, mirroring the harness's u32-then-i32
    # parse_pawn_height fence rather than forwarding a value the harness aborts on. A bogus harness path
    # proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -1800, 2**31, 2**40):
        with pytest.raises(ValueError, match="pawn_height"):
            run_local_match("/no/such/harness", [0, 1], policies, pawn_height=bad)


def test_run_local_match_forwards_max_shield_and_omits_it_at_the_default(monkeypatch):
    # max_shield>0 forwards --max-shield <cap> as one value token, mode-independently (before the mode
    # block, like --gravity); the default 0 (shield off) adds no flag — byte-identical argv. Captured
    # without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--max-shield" not in captured["argv"], "the default 0 adds no flag"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, max_shield=50)
    argv = captured["argv"]
    assert argv[argv.index("--max-shield") + 1] == "50"

    # Explicit 0 is the default — it must NOT forward (byte-identical argv to omitting it).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, max_shield=0)
    assert "--max-shield" not in captured["argv"]

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, max_shield=50)
    argv = captured["argv"]
    assert argv[argv.index("--max-shield") + 1] == "50"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_max_shield():
    # max_shield is a u16 in core (0..=65535); a negative or an overflow forwarded blindly would wrap
    # into a cap the caller never asked for (65536 -> 0, which ALSO reads as "shield off") and abort the
    # harness, so it raises before any spawn, mirroring the harness's u16 --max-shield parse. A bogus
    # harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -50, 65536, 2**20):
        with pytest.raises(ValueError, match="max_shield"):
            run_local_match("/no/such/harness", [0, 1], policies, max_shield=bad)


def test_run_local_match_forwards_start_health_and_omits_it_when_none(monkeypatch):
    # start_health is a base-balance knob with a non-zero core default, so it uses a None sentinel: the
    # default None adds no token (the harness applies its own default — byte-identical argv); a value
    # forwards --start-health <hp> as one value token, mode-independently (before the mode block).
    # Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--start-health" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, start_health=50)
    argv = captured["argv"]
    assert argv[argv.index("--start-health") + 1] == "50"

    # An explicit 0 is NOT the sentinel — it must forward (a 0-HP/already-downed match the caller asked
    # for), UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, start_health=0)
    argv = captured["argv"]
    assert argv[argv.index("--start-health") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, start_health=50)
    argv = captured["argv"]
    assert argv[argv.index("--start-health") + 1] == "50"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_start_health():
    # start_health is a u16 in core (0..=65535); a non-None negative or overflow forwarded blindly would
    # wrap into a pool the caller never asked for (65536 -> 0, an already-downed spawn) and abort the
    # harness, so it raises before any spawn, mirroring the harness's u16 --start-health parse. None is
    # exempt (the sentinel). A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -50, 65536, 2**20):
        with pytest.raises(ValueError, match="start_health"):
            run_local_match("/no/such/harness", [0, 1], policies, start_health=bad)


def test_run_local_match_forwards_damage_and_omits_it_when_none(monkeypatch):
    # damage is a base-balance knob with a non-zero core default, so it uses a None sentinel: the default
    # None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --damage <hp> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--damage" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, damage=40)
    argv = captured["argv"]
    assert argv[argv.index("--damage") + 1] == "40"

    # An explicit 0 is NOT the sentinel — it must forward (a 0-damage/unkillable match the caller asked for),
    # UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, damage=0)
    argv = captured["argv"]
    assert argv[argv.index("--damage") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, damage=40)
    argv = captured["argv"]
    assert argv[argv.index("--damage") + 1] == "40"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_damage():
    # damage is a u16 in core (0..=65535); a non-None negative or overflow forwarded blindly would wrap into
    # a per-shot HP the caller never asked for (65536 -> 0, a shot that can never down a pawn) and abort the
    # harness, so it raises before any spawn, mirroring the harness's u16 --damage parse. None is exempt (the
    # sentinel). A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -40, 65536, 2**20):
        with pytest.raises(ValueError, match="damage"):
            run_local_match("/no/such/harness", [0, 1], policies, damage=bad)


def test_run_local_match_forwards_fire_cooldown_and_omits_it_when_none(monkeypatch):
    # fire_cooldown is a base-balance knob with a non-zero core default, so it uses a None sentinel: the
    # default None adds no token (the harness applies its own default — byte-identical argv); a value
    # forwards --fire-cooldown <ticks> as one value token, mode-independently (before the mode block).
    # Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--fire-cooldown" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, fire_cooldown=3)
    argv = captured["argv"]
    assert argv[argv.index("--fire-cooldown") + 1] == "3"

    # An explicit 0 is NOT the sentinel — it must forward (a fire-every-tick match the caller asked for),
    # UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, fire_cooldown=0)
    argv = captured["argv"]
    assert argv[argv.index("--fire-cooldown") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, fire_cooldown=3)
    argv = captured["argv"]
    assert argv[argv.index("--fire-cooldown") + 1] == "3"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_fire_cooldown():
    # fire_cooldown is a u16 in core (0..=65535); a non-None negative or overflow forwarded blindly would
    # wrap into a cadence the caller never asked for (65536 -> 0, a fire-every-tick pawn) and abort the
    # harness, so it raises before any spawn, mirroring the harness's u16 --fire-cooldown parse. None is
    # exempt (the sentinel). A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -3, 65536, 2**20):
        with pytest.raises(ValueError, match="fire_cooldown"):
            run_local_match("/no/such/harness", [0, 1], policies, fire_cooldown=bad)


def test_run_local_match_forwards_mag_size_and_omits_it_when_none(monkeypatch):
    # mag_size is a base-balance knob with a non-zero core default, so it uses a None sentinel: the default
    # None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --mag-size <rounds> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--mag-size" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mag_size=10)
    argv = captured["argv"]
    assert argv[argv.index("--mag-size") + 1] == "10"

    # An explicit 0 is NOT the sentinel — it must forward (an unfireable-magazine match the caller asked for),
    # UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mag_size=0)
    argv = captured["argv"]
    assert argv[argv.index("--mag-size") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, mag_size=10)
    argv = captured["argv"]
    assert argv[argv.index("--mag-size") + 1] == "10"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_mag_size():
    # mag_size is a u16 in core (0..=65535); a non-None negative or overflow forwarded blindly would wrap into
    # a capacity the caller never asked for (65536 -> 0, an unfireable magazine) and abort the harness, so it
    # raises before any spawn, mirroring the harness's u16 --mag-size parse. None is exempt (the sentinel). A
    # bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -10, 65536, 2**20):
        with pytest.raises(ValueError, match="mag_size"):
            run_local_match("/no/such/harness", [0, 1], policies, mag_size=bad)


def test_run_local_match_forwards_max_speed_and_omits_it_when_none(monkeypatch):
    # max_speed is a base-balance knob with a non-zero core default, so it uses a None sentinel: the default
    # None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --max-speed <units> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--max-speed" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, max_speed=400)
    argv = captured["argv"]
    assert argv[argv.index("--max-speed") + 1] == "400"

    # An explicit 0 is NOT the sentinel — it must forward (a frozen-pawn match the caller asked for), UNLIKE
    # the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, max_speed=0)
    argv = captured["argv"]
    assert argv[argv.index("--max-speed") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, max_speed=400)
    argv = captured["argv"]
    assert argv[argv.index("--max-speed") + 1] == "400"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_max_speed():
    # max_speed is a non-negative i32 in core (0..=2**31-1), NOT a u16 — a non-None negative (no movement
    # meaning) or a value past i32::MAX (which wraps negative) forwarded blindly would be a footgun and abort
    # the harness, so it raises before any spawn, mirroring the harness's u32-then-i32 parse_max_speed fence.
    # None is exempt (the sentinel). A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -200, 2**31, 2**40):
        with pytest.raises(ValueError, match="max_speed"):
            run_local_match("/no/such/harness", [0, 1], policies, max_speed=bad)


def test_run_local_match_forwards_perception_range_and_omits_it_when_none(monkeypatch):
    # perception_range is a base-balance knob with a non-zero core default, so it uses a None sentinel: the
    # default None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --perception-range <units> as one value token, mode-independently (before the mode block). Captured
    # without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--perception-range" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, perception_range=20000)
    argv = captured["argv"]
    assert argv[argv.index("--perception-range") + 1] == "20000"

    # An explicit 0 is NOT the sentinel — it must forward (a blind-seat match the caller asked for), UNLIKE
    # the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, perception_range=0)
    argv = captured["argv"]
    assert argv[argv.index("--perception-range") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, perception_range=20000)
    argv = captured["argv"]
    assert argv[argv.index("--perception-range") + 1] == "20000"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_perception_range():
    # perception_range is a non-negative i32 in core (0..=2**31-1), NOT a u16 — a non-None negative
    # (meaningless for a radius) or a value past i32::MAX (which wraps negative) forwarded blindly would be a
    # footgun and abort the harness, so it raises before any spawn, mirroring the harness's u32-then-i32
    # parse_perception_range fence. None is exempt (the sentinel). A bogus harness path proves the guard
    # precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -20000, 2**31, 2**40):
        with pytest.raises(ValueError, match="perception_range"):
            run_local_match("/no/such/harness", [0, 1], policies, perception_range=bad)


def test_run_local_match_forwards_weapon_range_and_omits_it_when_none(monkeypatch):
    # weapon_range is a base-balance knob with a non-zero core default, so it uses a None sentinel: the
    # default None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --weapon-range <units> as one value token, mode-independently (before the mode block). Captured without
    # a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--weapon-range" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, weapon_range=15000)
    argv = captured["argv"]
    assert argv[argv.index("--weapon-range") + 1] == "15000"

    # An explicit 0 is NOT the sentinel — it must forward (a reaches-nothing match the caller asked for),
    # UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, weapon_range=0)
    argv = captured["argv"]
    assert argv[argv.index("--weapon-range") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, weapon_range=15000)
    argv = captured["argv"]
    assert argv[argv.index("--weapon-range") + 1] == "15000"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_weapon_range():
    # weapon_range is a non-negative i32 in core (0..=2**31-1), NOT a u16 — a non-None negative (meaningless
    # for a reach; core squares it, so a negative would reach as far as its magnitude) or a value past
    # i32::MAX (which wraps negative) forwarded blindly would be a footgun and abort the harness, so it raises
    # before any spawn, mirroring the harness's u32-then-i32 parse_weapon_range fence. None is exempt (the
    # sentinel). A bogus harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -15000, 2**31, 2**40):
        with pytest.raises(ValueError, match="weapon_range"):
            run_local_match("/no/such/harness", [0, 1], policies, weapon_range=bad)


def test_run_local_match_forwards_hit_radius_and_omits_it_when_none(monkeypatch):
    # hit_radius is a base-balance knob with a non-zero core default, so it uses a None sentinel: the default
    # None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --hit-radius <units> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--hit-radius" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, hit_radius=3000)
    argv = captured["argv"]
    assert argv[argv.index("--hit-radius") + 1] == "3000"

    # An explicit 0 is NOT the sentinel — it must forward (a pin-precise beam the caller asked for), UNLIKE the
    # feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, hit_radius=0)
    argv = captured["argv"]
    assert argv[argv.index("--hit-radius") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, hit_radius=3000)
    argv = captured["argv"]
    assert argv[argv.index("--hit-radius") + 1] == "3000"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_hit_radius():
    # hit_radius is a non-negative i32 in core (0..=2**31-1), NOT a u16 — a non-None negative (a squared
    # perpendicular distance is never < 0, so a negative tolerance is meaningless) or a value past i32::MAX
    # (which wraps negative) forwarded blindly would be a footgun and abort the harness, so it raises before any
    # spawn, mirroring the harness's u32-then-i32 parse_hit_radius fence. None is exempt (the sentinel). A bogus
    # harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -3000, 2**31, 2**40):
        with pytest.raises(ValueError, match="hit_radius"):
            run_local_match("/no/such/harness", [0, 1], policies, hit_radius=bad)


def test_run_local_match_forwards_melee_cooldown_and_omits_it_when_none(monkeypatch):
    # melee_cooldown is a base-balance knob with a non-zero core default, so it uses a None sentinel: the default
    # None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --melee-cooldown <ticks> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--melee-cooldown" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, melee_cooldown=30)
    argv = captured["argv"]
    assert argv[argv.index("--melee-cooldown") + 1] == "30"

    # An explicit 0 is NOT the sentinel — it must forward (a continuous swing-every-tick cleave the caller asked
    # for), UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, melee_cooldown=0)
    argv = captured["argv"]
    assert argv[argv.index("--melee-cooldown") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, melee_cooldown=30)
    argv = captured["argv"]
    assert argv[argv.index("--melee-cooldown") + 1] == "30"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_melee_cooldown():
    # melee_cooldown is a u16 in core (0..=65535), NOT an i32 — a non-None negative or a value past 65535 (which
    # would wrap into a cadence the caller never asked for) forwarded blindly would abort the harness, so it
    # raises before any spawn, mirroring the harness's u16 parse() fence. None is exempt (the sentinel). A bogus
    # harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -30, 65536, 2**20):
        with pytest.raises(ValueError, match="melee_cooldown"):
            run_local_match("/no/such/harness", [0, 1], policies, melee_cooldown=bad)


def test_run_local_match_forwards_melee_damage_and_omits_it_when_none(monkeypatch):
    # melee_damage is a base-balance knob with a non-zero core default, so it uses a None sentinel: the default
    # None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --melee-damage <hp> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--melee-damage" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, melee_damage=70)
    argv = captured["argv"]
    assert argv[argv.index("--melee-damage") + 1] == "70"

    # An explicit 0 is NOT the sentinel — it must forward (a harmless 0-damage swing the caller asked for),
    # UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, melee_damage=0)
    argv = captured["argv"]
    assert argv[argv.index("--melee-damage") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, melee_damage=70)
    argv = captured["argv"]
    assert argv[argv.index("--melee-damage") + 1] == "70"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_melee_damage():
    # melee_damage is a u16 in core (0..=65535), NOT an i32 — a non-None negative or a value past 65535 (which
    # would wrap into a per-swing HP the caller never asked for) forwarded blindly would abort the harness, so it
    # raises before any spawn, mirroring the harness's u16 parse() fence. None is exempt (the sentinel). A bogus
    # harness path proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -70, 65536, 2**20):
        with pytest.raises(ValueError, match="melee_damage"):
            run_local_match("/no/such/harness", [0, 1], policies, melee_damage=bad)


def test_run_local_match_forwards_melee_range_and_omits_it_when_none(monkeypatch):
    # melee_range is a base-balance knob with a non-zero core default, so it uses a None sentinel: the default
    # None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --melee-range <units> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--melee-range" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, melee_range=8000)
    argv = captured["argv"]
    assert argv[argv.index("--melee-range") + 1] == "8000"

    # An explicit 0 is NOT the sentinel — it must forward (a cleaves-nothing swing the caller asked for), UNLIKE
    # the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, melee_range=0)
    argv = captured["argv"]
    assert argv[argv.index("--melee-range") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, melee_range=8000)
    argv = captured["argv"]
    assert argv[argv.index("--melee-range") + 1] == "8000"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_melee_range():
    # melee_range is a non-negative i32 in core (0..=2**31-1), NOT a u16 — a non-None negative (core squares the
    # reach, so a negative would cleave as far as its magnitude) or a value past i32::MAX (which wraps negative)
    # forwarded blindly would be a footgun and abort the harness, so it raises before any spawn, mirroring the
    # harness's u32-then-i32 parse_melee_range fence. None is exempt (the sentinel). A bogus harness path proves
    # the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -8000, 2**31, 2**40):
        with pytest.raises(ValueError, match="melee_range"):
            run_local_match("/no/such/harness", [0, 1], policies, melee_range=bad)


def test_run_local_match_forwards_projectile_speed_and_omits_it_when_none(monkeypatch):
    # projectile_speed is a base-balance knob with a non-zero core default, so it uses a None sentinel: the
    # default None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --projectile-speed <units> as one value token, mode-independently (before the mode block). Captured without
    # a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--projectile-speed" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, projectile_speed=8000)
    argv = captured["argv"]
    assert argv[argv.index("--projectile-speed") + 1] == "8000"

    # An explicit 0 is NOT the sentinel — it must forward (a never-leaves-the-muzzle shot the caller asked for),
    # UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, projectile_speed=0)
    argv = captured["argv"]
    assert argv[argv.index("--projectile-speed") + 1] == "0"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, projectile_speed=8000)
    argv = captured["argv"]
    assert argv[argv.index("--projectile-speed") + 1] == "8000"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_projectile_speed():
    # projectile_speed is a non-negative i32 in core (0..=2**31-1), NOT a u16 — a non-None negative (core flies a
    # projectile forward along the octant, never backward) or a value past i32::MAX (which wraps negative)
    # forwarded blindly would be a footgun and abort the harness, so it raises before any spawn, mirroring the
    # harness's u32-then-i32 parse_projectile_speed fence. None is exempt (the sentinel). A bogus harness path
    # proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -8000, 2**31, 2**40):
        with pytest.raises(ValueError, match="projectile_speed"):
            run_local_match("/no/such/harness", [0, 1], policies, projectile_speed=bad)


def test_run_local_match_forwards_action_deadline_micros_and_omits_it_when_none(monkeypatch):
    # action_deadline_micros is a timing knob with a non-zero core default (50_000 us, 50 ms), so it uses a None
    # sentinel: the default None adds no token (the harness applies its own default — byte-identical argv); a value
    # forwards --action-deadline-micros <micros> as one value token, mode-independently (before the mode block).
    # Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--action-deadline-micros" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, action_deadline_micros=25000)
    argv = captured["argv"]
    assert argv[argv.index("--action-deadline-micros") + 1] == "25000"

    # An explicit 0 is NOT the sentinel — it must forward (a forfeit-every-tick match the caller asked for),
    # UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, action_deadline_micros=0)
    argv = captured["argv"]
    assert argv[argv.index("--action-deadline-micros") + 1] == "0"

    # The u32 ceiling (2**32-1) forwards — a value past the i32 knobs' 2**31-1 max, proving this knob's range is the
    # wider u32, not the i32 the combat knobs use (a discriminating boundary the i32 twins would reject).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, action_deadline_micros=2**32 - 1)
    argv = captured["argv"]
    assert argv[argv.index("--action-deadline-micros") + 1] == str(2**32 - 1)

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, action_deadline_micros=25000)
    argv = captured["argv"]
    assert argv[argv.index("--action-deadline-micros") + 1] == "25000"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_action_deadline_micros():
    # action_deadline_micros is a u32 in core (0..=2**32-1), wider than the i32 knobs and with no negative — a
    # non-None negative or a value past u32::MAX (which wraps) forwarded blindly would be a footgun and abort the
    # harness, so it raises before any spawn, mirroring the harness's u32 parse. None is exempt (the sentinel).
    # 2**31 is NOT rejected here (a valid u32, unlike the i32 knobs) — the ceiling is 2**32. A bogus harness path
    # proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -25000, 2**32, 2**40):
        with pytest.raises(ValueError, match="action_deadline_micros"):
            run_local_match("/no/such/harness", [0, 1], policies, action_deadline_micros=bad)


def test_run_local_match_forwards_pickup_radius_and_omits_it_when_none(monkeypatch):
    # pickup_radius is a base-balance knob with a non-zero core default (1000), so it uses a None sentinel: the
    # default None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --pickup-radius <units> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--pickup-radius" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pickup_radius=1500)
    argv = captured["argv"]
    assert argv[argv.index("--pickup-radius") + 1] == "1500"

    # An explicit 0 is NOT the sentinel — it must forward (a collectible-only-when-exactly-on-the-pickup match the
    # caller asked for), UNLIKE the feature-toggle knobs where 0 is the omit-default. This is the None-vs-0 distinction.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pickup_radius=0)
    argv = captured["argv"]
    assert argv[argv.index("--pickup-radius") + 1] == "0"

    # The i32 ceiling (2**31-1) forwards — a value past the u16 twins' 65535 max, proving this knob's range is the
    # wider i32, not the u16 the cadence/damage knobs use (a discriminating boundary the u16 twins would reject).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pickup_radius=2**31 - 1)
    argv = captured["argv"]
    assert argv[argv.index("--pickup-radius") + 1] == str(2**31 - 1)

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, pickup_radius=1500)
    argv = captured["argv"]
    assert argv[argv.index("--pickup-radius") + 1] == "1500"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_pickup_radius():
    # pickup_radius is a non-negative i32 in core (0..=2**31-1), NOT a u16 — a non-None negative (core compares a
    # squared distance, never < 0) or a value past i32::MAX (which wraps negative) forwarded blindly would be a
    # footgun and abort the harness, so it raises before any spawn, mirroring the harness's u32-then-i32
    # parse_pickup_radius fence. None is exempt (the sentinel). 2**31 IS rejected here (past i32::MAX, unlike the
    # u32 action_deadline_micros) — the ceiling is 2**31, not 2**32. A bogus harness path proves the guard precedes
    # spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -1500, 2**31, 2**40):
        with pytest.raises(ValueError, match="pickup_radius"):
            run_local_match("/no/such/harness", [0, 1], policies, pickup_radius=bad)


def test_run_local_match_forwards_pickup_respawn_cooldown_and_omits_it_when_none(monkeypatch):
    # pickup_respawn_cooldown is a base-balance knob with a non-zero core default (300 ticks), so it uses a None
    # sentinel: the default None adds no token (the harness applies its own default — byte-identical argv); a value
    # forwards --pickup-respawn-cooldown <ticks> as one value token, mode-independently (before the mode block).
    # Captured without a harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--pickup-respawn-cooldown" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pickup_respawn_cooldown=450)
    argv = captured["argv"]
    assert argv[argv.index("--pickup-respawn-cooldown") + 1] == "450"

    # An explicit 0 is NOT the sentinel — it must forward (an always-present pickup that respawns the tick after
    # collection, the caller asked for), UNLIKE the feature-toggle knobs where 0 is the omit-default. None-vs-0.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pickup_respawn_cooldown=0)
    argv = captured["argv"]
    assert argv[argv.index("--pickup-respawn-cooldown") + 1] == "0"

    # The u16 ceiling (65535) forwards — pins the INCLUSIVE upper bound (a `< 65535` off-by-one would wrongly reject
    # it), the largest dormancy core accepts before the u16 wraps.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, pickup_respawn_cooldown=65535)
    argv = captured["argv"]
    assert argv[argv.index("--pickup-respawn-cooldown") + 1] == "65535"

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, pickup_respawn_cooldown=450)
    argv = captured["argv"]
    assert argv[argv.index("--pickup-respawn-cooldown") + 1] == "450"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_pickup_respawn_cooldown():
    # pickup_respawn_cooldown is a u16 in core (0..=65535), NOT an i32/u32 — a non-None negative or a value past
    # 65535 (which would wrap into a dormancy the caller never asked for) forwarded blindly would abort the harness,
    # so it raises before any spawn, mirroring the harness's u16 parse() fence. None is exempt (the sentinel).
    # 65536 IS rejected here (the discriminating boundary the wider i32/u32 twins forward). A bogus harness path
    # proves the guard precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -450, 65536, 2**20):
        with pytest.raises(ValueError, match="pickup_respawn_cooldown"):
            run_local_match("/no/such/harness", [0, 1], policies, pickup_respawn_cooldown=bad)


def test_run_local_match_forwards_spawn_jitter_and_omits_it_when_none(monkeypatch):
    # spawn_jitter is a base-balance knob with a non-zero core default (2000), so it uses a None sentinel: the
    # default None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --spawn-jitter <units> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--spawn-jitter" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, spawn_jitter=3000)
    argv = captured["argv"]
    assert argv[argv.index("--spawn-jitter") + 1] == "3000"

    # An explicit 0 is NOT the sentinel — it must forward (a fully deterministic opening with no per-seed
    # perturbation, the caller asked for), UNLIKE the feature-toggle knobs where 0 is the omit-default. None-vs-0.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, spawn_jitter=0)
    argv = captured["argv"]
    assert argv[argv.index("--spawn-jitter") + 1] == "0"

    # The i32 ceiling (2**31-1) forwards — a value past the u16 twins' 65535 max, proving this knob's range is the
    # wider i32, not the u16 the cadence/damage knobs use (a discriminating boundary the u16 twins would reject).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, spawn_jitter=2**31 - 1)
    argv = captured["argv"]
    assert argv[argv.index("--spawn-jitter") + 1] == str(2**31 - 1)

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, spawn_jitter=3000)
    argv = captured["argv"]
    assert argv[argv.index("--spawn-jitter") + 1] == "3000"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_spawn_jitter():
    # spawn_jitter is a non-negative i32 in core (0..=2**31-1), NOT a u16 — a non-None negative (core would invert
    # the [-jitter, +jitter] draw span, and rejects spawn_jitter < 0 as an invalid Rules) or a value past i32::MAX
    # (which wraps negative) forwarded blindly would be a footgun and abort the harness, so it raises before any
    # spawn, mirroring the harness's u32-then-i32 parse_spawn_jitter fence. None is exempt (the sentinel). 2**31 IS
    # rejected here (past i32::MAX, unlike the u32 action_deadline_micros). A bogus harness path proves the guard
    # precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -3000, 2**31, 2**40):
        with pytest.raises(ValueError, match="spawn_jitter"):
            run_local_match("/no/such/harness", [0, 1], policies, spawn_jitter=bad)


def test_run_local_match_forwards_spawn_radius_and_omits_it_when_none(monkeypatch):
    # spawn_radius is a base-balance knob with a non-zero core default (20000), so it uses a None sentinel: the
    # default None adds no token (the harness applies its own default — byte-identical argv); a value forwards
    # --spawn-radius <units> as one value token, mode-independently (before the mode block). Captured without a
    # harness via the spy gateway.
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

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies)
    assert "--spawn-radius" not in captured["argv"], "the default None adds no flag (harness default)"

    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, spawn_radius=30000)
    argv = captured["argv"]
    assert argv[argv.index("--spawn-radius") + 1] == "30000"

    # An explicit 0 is NOT the sentinel — it must forward (every seat stacked on the X origin, only spawn_jitter then
    # separating them, the caller asked for), UNLIKE the feature-toggle knobs where 0 is the omit-default. None-vs-0.
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, spawn_radius=0)
    argv = captured["argv"]
    assert argv[argv.index("--spawn-radius") + 1] == "0"

    # The i32 ceiling (2**31-1) forwards — a value past the u16 twins' 65535 max, proving this knob's range is the
    # wider i32, not the u16 the cadence/damage knobs use (a discriminating boundary the u16 twins would reject).
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, spawn_radius=2**31 - 1)
    argv = captured["argv"]
    assert argv[argv.index("--spawn-radius") + 1] == str(2**31 - 1)

    # Threads under --mode too (mode-independent forward): the flag and --mode both reach argv.
    keys = {0: _DEV_KEY, 1: _DEV_KEY2}
    with pytest.raises(_Stop):
        sdk.run_local_match("h", [0, 1], policies, mode="agent", signing_keys=keys, spawn_radius=30000)
    argv = captured["argv"]
    assert argv[argv.index("--spawn-radius") + 1] == "30000"
    assert argv[argv.index("--mode") + 1] == "agent"


def test_run_local_match_rejects_an_out_of_range_spawn_radius():
    # spawn_radius is a non-negative i32 in core (0..=2**31-1), NOT a u16 — a non-None negative (core would invert
    # the [-radius, +radius] spread span, and rejects spawn_radius < 0 as an invalid Rules) or a value past i32::MAX
    # (which wraps negative) forwarded blindly would be a footgun and abort the harness, so it raises before any
    # spawn, mirroring the harness's u32-then-i32 parse_spawn_radius fence. None is exempt (the sentinel). 2**31 IS
    # rejected here (past i32::MAX, unlike the u32 action_deadline_micros). A bogus harness path proves the guard
    # precedes spawn.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    for bad in (-1, -30000, 2**31, 2**40):
        with pytest.raises(ValueError, match="spawn_radius"):
            run_local_match("/no/such/harness", [0, 1], policies, spawn_radius=bad)


def test_run_local_match_surfaces_a_perception_memory_echo_to_a_policy():
    # The end-to-end payoff: --map reference (a central occluder) + a memory window means a
    # seat that loses sight of its enemy still receives its last-known position as a
    # VisibleEntity with in_line_of_sight=False. Deterministic at this seed — the recording
    # policy observes at least one echo, proving the knob turns memory on AND the echo
    # reaches the agent (the channel arena-baseline-fires-only-in-sight consumes).
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    class _EchoWatch:
        def __init__(self):
            self.echoes = 0
            self._b = BaselinePolicy()

        def __call__(self, obs):
            self.echoes += sum(1 for e in obs.visible if not e.in_line_of_sight)
            return self._b(obs)

    watch = {0: _EchoWatch(), 1: _EchoWatch()}
    run_local_match(
        harness, [0, 1], watch, seed=2, match_id="66666666-2222-4333-8444-555555555555",
        arena="reference", perception_memory=30,
    )
    assert watch[0].echoes + watch[1].echoes > 0, "a lost enemy surfaced as an out-of-sight echo"


def test_run_local_match_fov_cone_narrows_what_a_policy_perceives():
    # The end-to-end payoff: a narrow forward cone (fov=0, the facing octant alone) gates
    # perception by ANGLE, so a seat perceives an in-range enemy only while facing it. Proven
    # by running the SAME deterministic match (direct path, fixed seed) under the full circle
    # vs the narrow cone and counting in-sight observations: the full circle perceives the
    # in-range enemy nearly every tick (both seats), the narrow cone a tiny fraction — if --fov
    # were dropped the two runs would be byte-identical, so the gap is the flag reaching the sim.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    class _SightCount:
        def __init__(self):
            self.seen = 0
            self._b = BaselinePolicy()

        def __call__(self, obs):
            self.seen += sum(1 for e in obs.visible if e.in_line_of_sight)
            return self._b(obs)

    def sightings(fov):
        w = {0: _SightCount(), 1: _SightCount()}
        run_local_match(
            harness, [0, 1], w, seed=2, match_id="77777777-3333-4444-8555-666666666666",
            arena="reference", fov=fov,
        )
        return w[0].seen, w[1].seen

    full = sightings(4)
    narrow = sightings(0)
    assert full[0] > 0 and full[1] > 0, "the full circle perceives the in-range enemy (else the gap proves nothing)"
    assert sum(narrow) * 10 < sum(full), (
        f"the facing-octant cone perceives the enemy a tiny fraction of the full circle "
        f"(full={full}, narrow={narrow}) — the cone gates perception by angle end to end"
    )


def test_run_local_match_fine_aim_diverges_from_octant_at_a_discriminating_seed():
    # The end-to-end payoff: aim_mode="fine" selects the 64-way half-step table over the 8-way
    # octant snap, so a sub-octant lead resolves a shot differently. Proven by running the SAME
    # deterministic match (direct path, fixed seed) under octant vs fine and asserting the replay
    # hash — the canonical record of every shot's outcome — diverges. Seed 2 is discriminating
    # (FM3: many seeds are aim-invariant); if --aim-mode were dropped the two runs would be
    # byte-identical, so the gap is the flag reaching hit resolution.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    seed = 2
    match_id = "22222222-3333-4444-8555-666666666666"

    def run(**kw):
        return run_local_match(
            harness, [0, 1], {0: BaselinePolicy(), 1: BaselinePolicy()},
            seed=seed, match_id=match_id, **kw,
        )

    octant = run()
    fine = run(aim_mode="fine")
    assert fine[0].replay_hash != octant[0].replay_hash, (
        "fine aim resolves a shot differently than the octant snap at this seed — the replay "
        "hash, the canonical record of every shot, must diverge"
    )
    # Each mode re-runs byte-for-byte, so the divergence is the aim table, not nondeterminism;
    # explicit "octant" matches the default (no flag).
    assert run()[0].replay_hash == octant[0].replay_hash
    assert run(aim_mode="fine")[0].replay_hash == fine[0].replay_hash
    assert run(aim_mode="octant")[0].replay_hash == octant[0].replay_hash


def test_perception_memory_does_not_bypass_the_mode_preflights():
    # FM2: lifting the perception_memory mode guard must leave its neighbours intact —
    # perception_memory must not smuggle an unsigned seat into ranked, nor a ladder_file past
    # its --mode requirement. Both raise before any spawn, so a bogus harness path proves the
    # guard precedes it.
    from arena_client.sdk import run_local_match

    policies = {0: BaselinePolicy(), 1: BaselinePolicy()}
    with pytest.raises(ValueError, match="signing key"):
        run_local_match("/no/such/harness", [0, 1], policies, mode="agent", perception_memory=30)
    with pytest.raises(ValueError, match="ladder"):
        run_local_match("/no/such/harness", [0, 1], policies, ladder_file="x.json", perception_memory=30)


def test_run_matchmade_surfaces_a_perception_memory_echo_to_a_policy():
    # The matchmade-path payoff: a --mode agent (ranked) match under --map reference + a
    # memory window surfaces a lost enemy as an in_line_of_sight=False echo, exactly as the
    # direct path does — proving --perception-memory reaches the sim through the matchmaker
    # (MatchParams.rules), not just that the SDK no longer raises.
    #
    # Unlike the direct-path echo test we CANNOT fix a seed: the matchmaker mints a fresh
    # server-authoritative match_id per formation and derives the spawn seed from it, so the
    # match is non-deterministic by design (the harness's --seed/--match-id drive only the
    # direct path). A reference match surfaces an echo more often than not (~60% at authoring
    # — a no-echo match needs both agents to engage without ever breaking the central
    # occluder's sightline), so we retry formations until one echoes; the cap makes a
    # spurious failure negligible (<1e-6) while the early break keeps the typical run to ~2
    # matches.
    harness = _require_or_skip_harness()
    from arena_client.sdk import run_local_match

    class _EchoWatch:
        def __init__(self):
            self.echoes = 0
            self._b = BaselinePolicy()

        def __call__(self, obs):
            self.echoes += sum(1 for e in obs.visible if not e.in_line_of_sight)
            return self._b(obs)

    formations = 24
    for _ in range(formations):
        watch = {0: _EchoWatch(), 1: _EchoWatch()}
        run_local_match(
            harness, [0, 1], watch, arena="reference", perception_memory=30, mode="agent",
            signing_keys={0: _DEV_KEY, 1: _DEV_KEY2},
        )
        if watch[0].echoes + watch[1].echoes > 0:
            break
    else:
        pytest.fail(f"no out-of-sight echo across {formations} matchmade reference matches")


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
