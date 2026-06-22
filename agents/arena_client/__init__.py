"""arena_client — a Python SDK for playing the Blackfield arena as an agent.

An agent is a player: it speaks the arena-01 Gateway (handshake with a protocol
version check, a parity-bounded `Observation` in, a validated `Action` out within
the tick deadline) against the arena-02 reference core. The wire types live in
`proto`, the client state machine in `sdk`, and a deterministic scripted agent in
`baseline`.
"""

from .proto import (
    MOVE_INTENT_SCALE,
    POSITION_SCALE,
    PROTOCOL_VERSION,
    VELOCITY_SCALE,
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
    SeatInfo,
    SeatOutcome,
    SelfState,
    Start,
    Vec2,
    VisibleEntity,
    Welcome,
    decode_gateway,
)
from .baseline import BaselinePolicy, aim_at
from .sdk import (
    ArenaClient,
    GatewayClosed,
    HandshakeRejected,
    Policy,
    SubprocessGateway,
    Transport,
    VersionMismatch,
    check_version,
    run_local_match,
)

__all__ = [
    "ArenaClient",
    "BaselinePolicy",
    "GatewayClosed",
    "SubprocessGateway",
    "aim_at",
    "run_local_match",
    "HandshakeRejected",
    "Policy",
    "Transport",
    "VersionMismatch",
    "check_version",
    "MOVE_INTENT_SCALE",
    "POSITION_SCALE",
    "PROTOCOL_VERSION",
    "VELOCITY_SCALE",
    "Action",
    "ActionButtons",
    "ActionIntent",
    "Blocker",
    "Challenge",
    "MatchConfig",
    "MatchResult",
    "Observation",
    "ProtocolError",
    "Reject",
    "SeatInfo",
    "SeatOutcome",
    "SelfState",
    "Start",
    "Vec2",
    "VisibleEntity",
    "Welcome",
    "decode_gateway",
]
