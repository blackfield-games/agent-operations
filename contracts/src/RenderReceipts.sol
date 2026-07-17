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

    /// @notice Hard upper bound on an `issueReceipts` batch — caps the O(n) fence/attest/
    ///         persist/fee-route loop (and its single N-element `multiAttest`) so one batch
    ///         can never approach the block gas limit, even from a buggy or compromised
    ///         coordinator. 64 is well above the coordinator's default relay drain chunk
    ///         (`relay_batch_size`, 32) and any realistic settle wave, so it never rejects a
    ///         legitimate batch; it exists purely to make the batch's gas envelope a contract
    ///         guarantee rather than a trust assumption — exactly as `MatchSettlement.MAX_FIELD`
    ///         does for a field roster. The coordinator mirrors this cap as a boot-time guard,
    ///         so a mis-set `relay_batch_size` fails fast rather than reverting every drain.
    uint256 public constant MAX_BATCH = 64;

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
    error ZeroEas();
    error ZeroRegionAuthority();
    error ZeroFeeRate();
    error ZeroFeeToken();
    error EmptyBatch();
    error BatchTooLarge();

    constructor(address eas_, address owner_, address regionAuthority_, uint256 renderFeeRate_)
        Ownable(owner_)
    {
        if (eas_ == address(0)) revert ZeroEas();
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
        //
        // The reentrancy and skip-not-revert guarantees above rest on feeToken being the
        // trusted, callback-free $BLCKFLD (read from RegionAuthority.TOKEN()): a token that
        // ran code on transfer and was *also* an authorized coordinator could revoke
        // mid-issue, and one that unstaked the region between regionExists and depositFees
        // could turn the skip into a revert — but $BLCKFLD is a plain ERC-20 and is never a
        // coordinator, so a malicious fee token could at worst DoS issuance, never exploit
        // it (the same trust boundary RegionAuthority/ComputeMeter/ArtifactTemplate assume).
        //
        // renderFeeRate * renderSeconds is checked arithmetic: it reverts only on the
        // astronomical overflow of an absurd owner-set rate times a uint64 render-seconds,
        // never for plausible values (mirrors ArtifactTemplate._scaledFee).
        uint256 fee = renderFeeRate * renderSeconds;
        uint256 regionTokenId = uint256(regionId);
        if (fee != 0 && regionAuthority.regionExists(regionTokenId)) {
            feeToken.safeTransferFrom(msg.sender, address(this), fee);
            feeToken.forceApprove(address(regionAuthority), fee);
            regionAuthority.depositFees(regionTokenId, fee);
            emit RenderFeeRouted(jobId, regionTokenId, fee);
        }
    }

    /// @notice One element of an `issueReceipts` batch — the same six fields `issueReceipt`
    ///         takes positionally. A struct array (not six parallel vectors) so a batch can
    ///         never be built with mismatched-length columns, the per-element fields stay
    ///         grouped, and the call mirrors EAS's own `MultiAttestationRequest` shape.
    struct ReceiptRequest {
        address earner;
        bytes32 jobId;
        uint64 renderSeconds;
        uint16 jobKind;
        bytes32 outputHash;
        bytes32 regionId;
    }

    /// @notice Issue render receipts for a batch of validated jobs in ONE transaction and
    ///         ONE `EAS.multiAttest`, instead of N separate `issueReceipt` calls. Every
    ///         per-job effect of `issueReceipt` applies PER ELEMENT — the `receiptIssued`
    ///         fence, the live counters, the uid persisted against each job, and the region
    ///         fee-share route — but amortized: a coordinator settling the N jobs that
    ///         landed in one block pays one base tx cost and one attestation call.
    ///
    ///         An empty batch reverts (`EmptyBatch`) and a batch above `MAX_BATCH` reverts
    ///         (`BatchTooLarge`, the gas-envelope bound) rather than silently attesting
    ///         nothing or running unbounded — a zero-length or oversized call is a caller
    ///         bug, not a no-op. Authorization and the schema gate are identical to
    ///         `issueReceipt`.
    ///
    ///         Atomicity: every effect runs in this one tx, so ANY element reverting — a
    ///         jobId already issued OR repeated within this batch (it hits the fence an
    ///         earlier element set), or a claimed region whose fee the coordinator can't
    ///         cover — rolls the WHOLE batch back. All-or-nothing: never some jobs
    ///         attested-but-unpaid.
    ///
    ///         CEI mirrors `issueReceipt` and `MatchSettlement.settleFieldWager`: fence +
    ///         credit every element and build the single `MultiAttestationRequest` BEFORE
    ///         the external `multiAttest`; then persist each uid against its OWN jobId and
    ///         emit — fully materializing every receipt — BEFORE routing any fee. So the
    ///         same reentrancy reasoning as `issueReceipt` holds: the fences reject a
    ///         reentrant re-issue, the `uid == 0` guard rejects a reentrant revoke, and
    ///         `feeToken` is the trusted callback-free $BLCKFLD, never a coordinator.
    function issueReceipts(ReceiptRequest[] calldata items) external returns (bytes32[] memory uids) {
        if (!authorizedCoordinators[msg.sender]) revert NotAuthorized();
        if (schemaUid == bytes32(0)) revert SchemaNotSet();

        uint256 n = items.length;
        if (n == 0) revert EmptyBatch();
        if (n > MAX_BATCH) revert BatchTooLarge();

        // Phase 1 (checks + effects): fence each job, credit the counters, and build the one
        // multiAttest payload. The fence is per ELEMENT, so a jobId repeated within the batch
        // hits the receiptIssued flag the earlier element set and reverts — an intra-batch
        // duplicate can never double-issue.
        IEAS.AttestationRequestData[] memory data = new IEAS.AttestationRequestData[](n);
        for (uint256 i = 0; i < n; ++i) {
            ReceiptRequest calldata it = items[i];
            if (receiptIssued[it.jobId]) revert DuplicateReceipt(it.jobId);
            receiptIssued[it.jobId] = true;
            _receiptEarner[it.jobId] = it.earner;
            ++receiptCount;
            ++receiptsByEarner[it.earner];
            data[i] = IEAS.AttestationRequestData({
                recipient: it.earner,
                expirationTime: 0,
                revocable: true,
                refUid: bytes32(0),
                data: abi.encode(
                    it.earner, it.jobId, it.renderSeconds, it.jobKind, it.outputHash, it.regionId
                ),
                value: 0
            });
        }

        // Phase 2: one multiAttest for the whole batch. EAS returns the uids flat across the
        // single group — exactly `n`, in submission order — so uids[i] is element i's. An
        // indexed read of a short return reverts (atomic) rather than mis-mapping a job to a
        // stale uid; the same trust placed in EAS.attest's single return by issueReceipt.
        IEAS.MultiAttestationRequest[] memory requests = new IEAS.MultiAttestationRequest[](1);
        requests[0] = IEAS.MultiAttestationRequest({schema: schemaUid, data: data});
        uids = EAS.multiAttest(requests);

        // Phase 3: persist each uid against its OWN jobId and emit, fully materializing every
        // receipt before any fee token gets control (the batch twin of issueReceipt's
        // set-uid-then-emit, run for the whole batch before phase 4).
        for (uint256 i = 0; i < n; ++i) {
            ReceiptRequest calldata it = items[i];
            receiptUid[it.jobId] = uids[i];
            emit ReceiptIssued(uids[i], it.earner, it.jobId, it.jobKind, it.renderSeconds);
        }

        // Phase 4: route each element's region fee-share LAST. Identical per-element policy
        // to issueReceipt — fee = renderFeeRate * renderSeconds (checked mul), a zero fee or
        // an unclaimed region skips the route (the attestation still stands), the fee is
        // pulled from the issuing coordinator (msg.sender), and the allowance is set to
        // exactly `fee` and fully consumed by depositFees. A claimed region the coordinator
        // can't cover reverts the whole batch (nothing attested-but-unpaid). Per-job
        // RenderFeeRouted events preserve the same audit trail as N single issues.
        for (uint256 i = 0; i < n; ++i) {
            ReceiptRequest calldata it = items[i];
            uint256 fee = renderFeeRate * it.renderSeconds;
            uint256 regionTokenId = uint256(it.regionId);
            if (fee != 0 && regionAuthority.regionExists(regionTokenId)) {
                feeToken.safeTransferFrom(msg.sender, address(this), fee);
                feeToken.forceApprove(address(regionAuthority), fee);
                regionAuthority.depositFees(regionTokenId, fee);
                emit RenderFeeRouted(it.jobId, regionTokenId, fee);
            }
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

    /// @notice Revoke a batch of previously-issued render receipts in ONE transaction, instead
    ///         of N separate `revokeReceipt` calls — the retraction twin of `issueReceipts`.
    ///         Used to invalidate a whole bad wave at once (a compromised or deauthorized earner
    ///         whose entire output is fraudulent) without paying N base tx costs or racing N
    ///         transactions. Every per-job effect of `revokeReceipt` applies PER ELEMENT: the
    ///         issued-and-not-revoked fence, the live-counter decrements against the STORED
    ///         earner, the `revokedCount` bump, and the EAS revoke of the persisted uid.
    ///
    ///         Authorization is identical to `revokeReceipt` — gated to the `authorizedCoordinators`
    ///         set, checked ONCE before any state, so a deauthorized coordinator retracts nothing
    ///         and leaks no gas past the auth revert. An empty batch reverts (`EmptyBatch`) and a
    ///         batch above the SAME `MAX_BATCH` reverts (`BatchTooLarge`), so the O(n) fence/
    ///         decrement/revoke loop can never approach the block gas limit.
    ///
    ///         Atomicity: ANY bad element — a jobId never issued, already revoked, or repeated
    ///         within this batch (it hits the fence an earlier element set) — reverts the WHOLE
    ///         call. Never a partial revoke that desyncs `receiptCount`/`revokedCount` or leaves
    ///         some attestations live and some retracted with no atomic record of which.
    ///
    ///         CEI mirrors `revokeReceipt`: fence + decrement + emit EVERY element BEFORE any
    ///         external `EAS.revoke`, so a hostile EAS reentering `revokeReceipt`/`revokeReceipts`
    ///         for a batched job finds it already flagged and is rejected by `AlreadyRevoked` — it
    ///         can neither double-decrement nor double-revoke. The single `EAS.revoke` is looped
    ///         per element (no `IEAS` change), and the counters net EXACTLY the sum of N single
    ///         revokes. Retracts the attestations only, never the region fee-share the matching
    ///         issue routed — the same attestation-only posture `revokeReceipt` has.
    function revokeReceipts(bytes32[] calldata jobIds) external {
        if (!authorizedCoordinators[msg.sender]) revert NotAuthorized();

        uint256 n = jobIds.length;
        if (n == 0) revert EmptyBatch();
        if (n > MAX_BATCH) revert BatchTooLarge();

        // Phase 1 (checks + effects): fence each job, decrement the live counters against the
        // stored earner, bump revokedCount, and cache the uid to revoke. The fence is per ELEMENT,
        // so a jobId repeated within the batch hits the receiptRevoked flag the earlier element set
        // and reverts (AlreadyRevoked) — an intra-batch duplicate can never double-decrement. The
        // uid==0 guard mirrors revokeReceipt: unreachable outside a reentrant issue window, kept so
        // the guarantee is self-contained. Each element's guards prove one un-revoked issue exists,
        // so both decrements are >= 1 (checked arithmetic backstops any underflow regardless).
        bytes32[] memory uids = new bytes32[](n);
        for (uint256 i = 0; i < n; ++i) {
            bytes32 jobId = jobIds[i];
            if (!receiptIssued[jobId]) revert NotIssued(jobId);
            if (receiptRevoked[jobId]) revert AlreadyRevoked(jobId);
            bytes32 uid = receiptUid[jobId];
            if (uid == bytes32(0)) revert NotIssued(jobId);

            receiptRevoked[jobId] = true;
            address earner = _receiptEarner[jobId];
            --receiptCount;
            --receiptsByEarner[earner];
            ++revokedCount;
            uids[i] = uid;
            emit ReceiptRevoked(uid, earner, jobId);
        }

        // Phase 2: revoke each stored uid LAST, after the whole batch is flagged revoked and the
        // counters are fully decremented (checks-effects-interactions). By the time any EAS gets
        // control every batched receipt is already retracted, so a reentrant revoke of ANY batched
        // job hits AlreadyRevoked — the batch twin of revokeReceipt's single-call CEI.
        for (uint256 i = 0; i < n; ++i) {
            EAS.revoke(
                IEAS.RevocationRequest({
                    schema: schemaUid, data: IEAS.RevocationRequestData({uid: uids[i], value: 0})
                })
            );
        }
    }
}
