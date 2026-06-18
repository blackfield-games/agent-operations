// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

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

/// @notice The slice of RegionAuthority this contract depends on: the fee-share intake
///         (`depositFees`), the staked $BLCKFLD token it pulls (`TOKEN`, read once at
///         construction so the approve target can never desync from the deposit target),
///         and a non-reverting existence check (`regionExists`) so issuance can route
///         only into a claimed region and otherwise skip — never reverting the
///         attestation for a region with no staked holder.
interface IRegionAuthority {
    function depositFees(uint256 tokenId, uint256 amount) external;
    function TOKEN() external view returns (address);
    function regionExists(uint256 tokenId) external view returns (bool);
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
    using SafeERC20 for IERC20;

    IEAS public immutable EAS;

    /// @notice RegionAuthority that receives the per-receipt region fee-share via
    ///         `depositFees`. Wired at deploy and immutable. A render fee is routed here
    ///         only when the receipt's region is claimed; an unclaimed/unknown region
    ///         skips the route (the attestation still issues), so a region with no staked
    ///         holder never bricks issuance.
    IRegionAuthority public immutable regionAuthority;

    /// @notice The $BLCKFLD token the region fee-share is paid in — real, transferable
    ///         tokens, NOT the burned ComputeMeter credit a render's compute fee debits
    ///         elsewhere. Read from `regionAuthority.TOKEN()` at construction so the
    ///         approve target can never desync from the token `depositFees` pulls.
    IERC20 public immutable feeToken;

    /// @notice Region fee-share per render-second, in real $BLCKFLD. The fee charged on a
    ///         validated receipt is `renderFeeRate * renderSeconds` — additive to and
    ///         independent of the burned compute fee, and pulled from the receipt-issuing
    ///         coordinator/treasury (`msg.sender`), not the rewarded earner. Owner-set and
    ///         kept strictly positive (mirrors RegionAuthority's non-zero stake floor and
    ///         ArtifactTemplate's fee/royalty rates) so the fee gate can't be globally
    ///         opened; a `renderSeconds == 0` receipt still yields a zero fee and skips the
    ///         route.
    uint256 public renderFeeRate;

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
    event RenderFeeRateSet(uint256 rate);
    /// @notice A validated receipt routed `amount` $BLCKFLD into `regionId`'s fee pool.
    ///         Absent for a receipt whose region was unclaimed (route skipped) or whose
    ///         fee was zero (`renderSeconds == 0`).
    event RenderFeeRouted(bytes32 indexed jobId, uint256 indexed regionId, uint256 amount);

    error NotAuthorized();
    error SchemaNotSet();
    error DuplicateReceipt(bytes32 jobId);
    error NotIssued(bytes32 jobId);
    error AlreadyRevoked(bytes32 jobId);
    error ZeroRegionAuthority();
    error ZeroFeeRate();
    error ZeroFeeToken();

    constructor(address eas_, address owner_, address regionAuthority_, uint256 renderFeeRate_)
        Ownable(owner_)
    {
        if (regionAuthority_ == address(0)) revert ZeroRegionAuthority();
        if (renderFeeRate_ == 0) revert ZeroFeeRate();
        EAS = IEAS(eas_);
        regionAuthority = IRegionAuthority(regionAuthority_);
        // Bind the fee token to exactly what RegionAuthority pulls in depositFees, so the
        // approve target can never desync from the deposit target. Reject a zero token (a
        // misconfigured RegionAuthority) here rather than letting every fee-routing
        // receipt revert opaquely at the SafeERC20 call (mirrors ArtifactTemplate).
        IERC20 token = IERC20(IRegionAuthority(regionAuthority_).TOKEN());
        if (address(token) == address(0)) revert ZeroFeeToken();
        feeToken = token;
        renderFeeRate = renderFeeRate_;
    }

    /// @notice Owner sets the per-render-second region fee-share. Zero is rejected so the
    ///         fee gate can never be globally disabled (mirrors RegionAuthority's stake
    ///         floor and ArtifactTemplate's rate setters).
    function setRenderFeeRate(uint256 rate) external onlyOwner {
        if (rate == 0) revert ZeroFeeRate();
        renderFeeRate = rate;
        emit RenderFeeRateSet(rate);
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
        // so a reentrant relay is rejected by the fence. Only `receiptUid` is set after —
        // EAS derives the uid, so it is unknowable until attest returns; a reentrant
        // revoke that races the pending uid reads zero and is rejected by revokeReceipt's
        // own uid==0 guard, so the issue still completes or reverts atomically.
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

        // Route the region fee-share LAST — after the fence, counters, uid, and event are
        // all committed (checks-effects-interactions). The receipt is fully materialized
        // before any fee token gets control, so a hostile token reentering issueReceipt
        // hits the DuplicateReceipt fence and a reentering revokeReceipt hits NotAuthorized
        // (the token is not a coordinator); the attestation can neither be double-issued
        // nor revoked mid-issue. fee = renderFeeRate * renderSeconds (exact, render-seconds
        // is the compute unit); a zero fee (renderSeconds == 0) skips the route.
        //
        // The fee pays in real $BLCKFLD pulled from the receipt-issuing coordinator/
        // treasury (msg.sender) — distinct from the burned compute credit. An unclaimed/
        // unknown region skips the route so validated work is still attested for regions
        // with no staked holder (depositFees would revert UnknownRegion); a CLAIMED region
        // with the coordinator unable to cover the fee reverts the whole issue (issues
        // nothing) so an underfunded coordinator never attests without paying the region.
        // Allowance is set to exactly `fee` and fully consumed by depositFees (no standing
        // allowance, no transient balance — feeToken nets zero across the call).
        uint256 fee = renderFeeRate * renderSeconds;
        uint256 regionTokenId = uint256(regionId);
        if (fee != 0 && regionAuthority.regionExists(regionTokenId)) {
            feeToken.safeTransferFrom(msg.sender, address(this), fee);
            feeToken.forceApprove(address(regionAuthority), fee);
            regionAuthority.depositFees(regionTokenId, fee);
            emit RenderFeeRouted(jobId, regionTokenId, fee);
        }
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

        bytes32 uid = receiptUid[jobId];
        // A receipt is only fully materialized once its uid is persisted (set after the
        // issue-time attest). The single state where receiptIssued is set but receiptUid
        // is still zero is a reentrant revoke during issueReceipt's attest — reject it
        // here so the guarantee is self-contained, not dependent on EAS reverting a zero
        // uid. Outside that reentrant window this branch is unreachable.
        if (uid == bytes32(0)) revert NotIssued(jobId);

        // Effects before interaction (CEI): mark revoked and decrement before the
        // external EAS.revoke, so a reentrant or replayed revoke is rejected by the
        // AlreadyRevoked guard and cannot double-decrement. The guards above prove one
        // un-revoked issue exists for this job, so both decrements are >= 1 (checked
        // arithmetic backstops any underflow regardless).
        receiptRevoked[jobId] = true;
        address earner = _receiptEarner[jobId];
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
