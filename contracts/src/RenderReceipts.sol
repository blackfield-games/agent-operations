// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Minimal EAS interfaces — we depend on the canonical Base EAS deployment.
///         Address resolved at deploy time from .env (EAS_ADDRESS / EAS_SCHEMA_REGISTRY).
interface IEAS {
    struct AttestationRequestData {
        address recipient;
        uint64 expirationTime;
        bool revocable;
        bytes32 refUid;
        bytes data;
        uint256 value;
    }

    struct AttestationRequest {
        bytes32 schema;
        AttestationRequestData data;
    }

    function attest(AttestationRequest calldata request) external payable returns (bytes32);

    struct MultiAttestationRequest {
        bytes32 schema;
        AttestationRequestData[] data;
    }

    function multiAttest(MultiAttestationRequest[] calldata multiRequests)
        external
        payable
        returns (bytes32[] memory);

    struct RevocationRequestData {
        bytes32 uid;
        uint256 value;
    }

    struct RevocationRequest {
        bytes32 schema;
        RevocationRequestData data;
    }

    function revoke(RevocationRequest calldata request) external payable;
}

interface ISchemaRegistry {
    function register(string calldata schema, address resolver, bool revocable) external returns (bytes32);
}

/// @notice Records validated render-jobs as EAS attestations. The attestation is
///         the canonical proof a render-second / NPC-tick / dream-tile was completed
///         and accepted by the validator gate.
///
///         Schema (registered at deploy):
///             address earner, bytes32 jobId, uint64 renderSeconds,
///             uint16 jobKind, bytes32 outputHash, bytes32 regionId
///
///         jobKind: 0=terrain, 1=foliage, 2=npc_tick, 3=diffusion_tile, 4=optimization
contract RenderReceipts is Ownable2Step {
    IEAS public immutable EAS;
    bytes32 public schemaUid;
    /// @notice Running count of LIVE receipts — issued and not since revoked. The HUD
    ///         reads this for the number of currently-valid render attestations; a
    ///         revoke decrements it. Cumulative issued is `receiptCount + revokedCount`.
    uint256 public receiptCount;
    /// @notice Per-earner running count of LIVE receipts, for the HUD earner-leaderboard
    ///         read path. Across all earners these sum to `receiptCount`; a revoke
    ///         decrements the credited earner's slot. `earner` is also indexed in
    ///         `ReceiptIssued`, so clients can cross-reference the event stream.
    mapping(address earner => uint256 count) public receiptsByEarner;
    /// @notice Whether a receipt has ever been issued for a job. `jobId` is the render
    ///         job's UUID — globally unique — so this fences a replayed relay to exactly
    ///         one attestation per validated job: the on-chain twin of the coordinator's
    ///         settle-exactly-once guard. Stays set after a revoke, so a revoked job can
    ///         never be re-issued.
    mapping(bytes32 jobId => bool issued) public receiptIssued;
    /// @notice Whether an issued receipt has since been revoked. A revoked receipt is
    ///         not live (excluded from `receiptCount`) and cannot be revoked again.
    mapping(bytes32 jobId => bool revoked) public receiptRevoked;
    /// @notice The EAS attestation uid minted for a job, persisted at issue so revoke
    ///         can pass it to `EAS.revoke`. Zero until the job is issued.
    mapping(bytes32 jobId => bytes32 uid) public receiptUid;
    /// @notice The earner credited at issue, kept so revoke decrements the correct
    ///         `receiptsByEarner` slot from stored state rather than a caller argument.
    mapping(bytes32 jobId => address earner) internal _receiptEarner;
    /// @notice Running count of revoked receipts. With the live `receiptCount` this
    ///         reconstructs cumulative issued: `receiptCount + revokedCount`.
    uint256 public revokedCount;
    mapping(address coordinator => bool authorized) public authorizedCoordinators;

    event ReceiptIssued(
        bytes32 indexed uid,
        address indexed earner,
        bytes32 indexed jobId,
        uint16 jobKind,
        uint64 renderSeconds
    );
    event ReceiptRevoked(bytes32 indexed uid, address indexed earner, bytes32 indexed jobId);
    event CoordinatorSet(address indexed coordinator, bool authorized);
    event SchemaRegistered(bytes32 indexed uid);

    error NotAuthorized();
    error SchemaNotSet();
    error DuplicateReceipt(bytes32 jobId);
    error NotIssued(bytes32 jobId);
    error AlreadyRevoked(bytes32 jobId);

    constructor(address eas_, address owner_) Ownable(owner_) {
        EAS = IEAS(eas_);
    }

    function registerSchema(address registry_) external onlyOwner returns (bytes32) {
        bytes32 uid = ISchemaRegistry(registry_)
            .register(
                "address earner, bytes32 jobId, uint64 renderSeconds, uint16 jobKind, bytes32 outputHash, bytes32 regionId",
                address(0),
                true
            );
        schemaUid = uid;
        emit SchemaRegistered(uid);
        return uid;
    }

    function setCoordinator(address coordinator, bool authorized) external onlyOwner {
        authorizedCoordinators[coordinator] = authorized;
        emit CoordinatorSet(coordinator, authorized);
    }

    function issueReceipt(
        address earner,
        bytes32 jobId,
        uint64 renderSeconds,
        uint16 jobKind,
        bytes32 outputHash,
        bytes32 regionId
    ) external returns (bytes32 uid) {
        if (!authorizedCoordinators[msg.sender]) revert NotAuthorized();
        if (schemaUid == bytes32(0)) revert SchemaNotSet();
        if (receiptIssued[jobId]) revert DuplicateReceipt(jobId);

        // Effects before the external attest (checks-effects-interactions): the fence,
        // the credited earner, and both live counters are consistent before the call,
        // so a reentrant relay is rejected by the fence and a cross-function reentrant
        // revoke during attest sees coherent counters. Only `receiptUid` is set after —
        // EAS derives the uid, so it is unknowable until attest returns; a revoke that
        // raced the pending uid would read zero and EAS rejects a zero uid.
        receiptIssued[jobId] = true;
        _receiptEarner[jobId] = earner;
        ++receiptCount;
        ++receiptsByEarner[earner];

        bytes memory data = abi.encode(earner, jobId, renderSeconds, jobKind, outputHash, regionId);
        uid = EAS.attest(
            IEAS.AttestationRequest({
                schema: schemaUid,
                data: IEAS.AttestationRequestData({
                    recipient: earner,
                    expirationTime: 0,
                    revocable: true,
                    refUid: bytes32(0),
                    data: data,
                    value: 0
                })
            })
        );

        receiptUid[jobId] = uid;
        emit ReceiptIssued(uid, earner, jobId, jobKind, renderSeconds);
    }

    /// @notice Revoke a previously-issued render receipt — used when a settled job is
    ///         later found invalid (failed re-validation, disputed output). Gated to the
    ///         same `authorizedCoordinators` set as issuance (not issuer-only, so a
    ///         rotated/deauthorized coordinator can never strand a receipt). Flips the
    ///         per-job fence to revoked — neither re-issuable nor revocable again —
    ///         decrements the live counters, then revokes the stored EAS attestation.
    function revokeReceipt(bytes32 jobId) external {
        if (!authorizedCoordinators[msg.sender]) revert NotAuthorized();
        if (!receiptIssued[jobId]) revert NotIssued(jobId);
        if (receiptRevoked[jobId]) revert AlreadyRevoked(jobId);

        // Effects before interaction (CEI): mark revoked and decrement before the
        // external EAS.revoke, so a reentrant or replayed revoke is rejected by the
        // AlreadyRevoked guard and cannot double-decrement. The guards above prove one
        // un-revoked issue exists for this job, so both decrements are >= 1 (checked
        // arithmetic backstops any underflow regardless).
        receiptRevoked[jobId] = true;
        address earner = _receiptEarner[jobId];
        bytes32 uid = receiptUid[jobId];
        --receiptCount;
        --receiptsByEarner[earner];
        ++revokedCount;

        emit ReceiptRevoked(uid, earner, jobId);

        EAS.revoke(
            IEAS.RevocationRequest({
                schema: schemaUid, data: IEAS.RevocationRequestData({uid: uid, value: 0})
            })
        );
    }
}
