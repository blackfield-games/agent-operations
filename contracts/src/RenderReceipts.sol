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
    /// @notice Running count of issued receipts (EAS render attestations).
    ///         The HUD reads this for the total number of validated render jobs.
    uint256 public receiptCount;
    mapping(address coordinator => bool authorized) public authorizedCoordinators;

    event ReceiptIssued(
        bytes32 indexed uid, address indexed earner, bytes32 indexed jobId, uint16 jobKind, uint64 renderSeconds
    );
    event CoordinatorSet(address indexed coordinator, bool authorized);
    event SchemaRegistered(bytes32 indexed uid);

    error NotAuthorized();
    error SchemaNotSet();

    constructor(address eas_, address owner_) Ownable(owner_) {
        EAS = IEAS(eas_);
    }

    function registerSchema(address registry_) external onlyOwner returns (bytes32) {
        bytes32 uid = ISchemaRegistry(registry_).register(
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

        ++receiptCount;
        emit ReceiptIssued(uid, earner, jobId, jobKind, renderSeconds);
    }
}
