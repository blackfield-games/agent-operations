// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {RenderReceipts, IEAS, ISchemaRegistry} from "../src/RenderReceipts.sol";
import {RegionAuthority} from "../src/RegionAuthority.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

// ---------------------------------------------------------------------------
// Local mocks — same shape as RenderReceipts.t.sol
// ---------------------------------------------------------------------------

/// @dev Real $BLCKFLD stand-in for the region fee-share. The deployer holds the supply.
contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

/// @dev Records the last attest request and returns a deterministic uid.
contract MockEAS is IEAS {
    IEAS.AttestationRequest public lastRequest;
    uint256 public attestCalls;
    // Sum of elements attested through multiAttest (the batch path), tracked apart from
    // attestCalls so the one-attestation-per-receipt invariant can span both forwards:
    // a single issue bumps attestCalls, a batch of N bumps this by N.
    uint256 public multiAttestedReceipts;

    bytes32 public lastSchema;
    address public lastRecipient;
    uint64 public lastExpirationTime;
    bool public lastRevocable;
    bytes32 public lastRefUid;
    bytes public lastData;
    uint256 public lastValue;

    mapping(bytes32 => bool) public isAttested;
    mapping(bytes32 => bool) public isRevoked;
    uint256 public revokeCalls;

    function attest(IEAS.AttestationRequest calldata request) external payable returns (bytes32) {
        attestCalls++;
        lastRequest = request;
        lastSchema = request.schema;
        lastRecipient = request.data.recipient;
        lastExpirationTime = request.data.expirationTime;
        lastRevocable = request.data.revocable;
        lastRefUid = request.data.refUid;
        lastData = request.data.data;
        lastValue = request.data.value;
        bytes32 uid = keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
        isAttested[uid] = true;
        return uid;
    }

    function revoke(IEAS.RevocationRequest calldata request) external payable {
        require(isAttested[request.data.uid], "unknown uid");
        require(!isRevoked[request.data.uid], "already revoked");
        isRevoked[request.data.uid] = true;
        revokeCalls++;
    }

    /// @dev Mirrors `attest`'s per-element uid derivation so a batch-issued receipt is
    ///      revocable (revoke checks isAttested[uid]) and returns the flat uid[] across all
    ///      groups in submission order — exactly the real EAS. RenderReceipts sends one group,
    ///      so the returned length equals that group's element count.
    function multiAttest(IEAS.MultiAttestationRequest[] calldata multiRequests)
        external
        payable
        returns (bytes32[] memory)
    {
        uint256 total;
        for (uint256 g = 0; g < multiRequests.length; g++) {
            total += multiRequests[g].data.length;
        }
        multiAttestedReceipts += total;

        bytes32[] memory uids = new bytes32[](total);
        uint256 k;
        for (uint256 g = 0; g < multiRequests.length; g++) {
            bytes32 schema = multiRequests[g].schema;
            IEAS.AttestationRequestData[] calldata items = multiRequests[g].data;
            for (uint256 i = 0; i < items.length; i++) {
                bytes32 uid = keccak256(abi.encode(schema, items[i].recipient, items[i].data));
                isAttested[uid] = true;
                uids[k++] = uid;
                lastSchema = schema;
                lastRecipient = items[i].recipient;
                lastExpirationTime = items[i].expirationTime;
                lastRevocable = items[i].revocable;
                lastRefUid = items[i].refUid;
                lastData = items[i].data;
                lastValue = items[i].value;
            }
        }
        return uids;
    }
}

contract MockSchemaRegistry is ISchemaRegistry {
    bytes32 public constant FIXED_UID = keccak256("blackfield.render.schema.v1");

    string public lastSchemaStr;
    address public lastResolver;
    bool public lastRevocable;

    function register(string calldata schema, address resolver, bool revocable) external returns (bytes32) {
        lastSchemaStr = schema;
        lastResolver = resolver;
        lastRevocable = revocable;
        return FIXED_UID;
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// @notice Stateful handler driving bounded issueReceipt calls. Authorized actors
///         succeed (incrementing ghost_issued); unauthorized actors hit NotAuthorized
///         which is caught so the run stays revert-free.
contract RenderReceiptsHandler is Test {
    RenderReceipts public receipts;
    MockEAS public eas;

    address[] public actors;
    address[] public earners; // fixed earner-recipient pool; bounds receiptsByEarner's domain
    bytes32[] public jobs; // fixed jobId pool; collisions exercise the dedup guard
    bytes32[] public batchJobs; // batch-issued jobIds; revokeReceipt draws these so batch receipts hit the revoke lifecycle
    mapping(address => bool) public isAuthorized;
    mapping(bytes32 => uint256) public ghost_successesByJob;
    uint256 public ghost_duplicateRejections;

    // Fresh unique jobIds for every batch element, so the happy path issues rather than
    // colliding on an already-fenced job (the batched twin of the fixed-pool single path).
    uint256 public issueReceiptsNonce;

    uint256 public ghost_issued;
    uint256 public ghost_revoked;
    bool public ghost_unauthorizedSuccess;
    bool public ghost_forwardMismatch;

    constructor(RenderReceipts receipts_, MockEAS eas_, address[] memory actors_, bool[] memory authorized_) {
        receipts = receipts_;
        eas = eas_;
        actors = actors_;
        for (uint256 i = 0; i < actors_.length; i++) {
            isAuthorized[actors_[i]] = authorized_[i];
        }

        // Fixed earner-recipient pool (see issueReceipt / sumReceipts).
        earners.push(address(0xEA0));
        earners.push(address(0xEA1));
        earners.push(address(0xEA2));

        // Fixed jobId pool. Small enough that the fuzzer keeps re-drawing the same
        // jobId across a run, so the dedup guard's revert path is actually hit
        // (a freshly-fuzzed bytes32 would collide ~never) — letting the invariants
        // prove duplicates corrupt none of the accounting.
        for (uint256 i = 0; i < 64; i++) {
            jobs.push(keccak256(abi.encode("job", i)));
        }
    }

    function issueReceipt(
        uint256 actorSeed,
        uint256 earnerSeed,
        uint256 jobSeed,
        uint64 renderSeconds,
        uint256 jobKindSeed,
        bytes32 outputHash,
        bytes32 regionId
    ) external {
        // Draw the earner from a small fixed pool rather than fuzzing a fresh
        // address each call. Bounding the earner domain keeps receiptsByEarner
        // enumerable so sumReceipts() can partition-check it in O(pool) — the same
        // reason the ComputeMeter handler routes every buy through a fixed actor
        // set. Arbitrary earner/jobId forwarding stays covered by the fuzz test;
        // pool members are all nonzero so no zero-address guard is needed here.
        address earner = earners[earnerSeed % earners.length];
        bytes32 jobId = jobs[jobSeed % jobs.length];

        address actor = actors[actorSeed % actors.length];
        uint16 jobKind = uint16(jobKindSeed % 5);

        vm.prank(actor);
        try receipts.issueReceipt(earner, jobId, renderSeconds, jobKind, outputHash, regionId) returns (
            bytes32
        ) {
            ghost_issued++;
            ghost_successesByJob[jobId]++;
            // If an unauthorized actor somehow succeeded, flag it.
            if (!isAuthorized[actor]) ghost_unauthorizedSuccess = true;
            // Every successful forward must use the canonical schema and correct recipient.
            if (eas.lastSchema() != receipts.schemaUid() || eas.lastRecipient() != earner) {
                ghost_forwardMismatch = true;
            }
        } catch (bytes memory err) {
            if (bytes4(err) == RenderReceipts.DuplicateReceipt.selector) ghost_duplicateRejections++;
        }
    }

    /// @notice Drives bounded issueReceipts (batch) calls — the batched twin of issueReceipt
    ///         and the relayer's per-block attestation hot path. Each element carries a FRESH
    ///         unique jobId (so the all-or-nothing batch issues rather than reverting on a
    ///         re-drawn job) recipient'd to the fixed earner pool; on success the jobIds land
    ///         in batchJobs so revokeReceipt exercises the revoke lifecycle over batch
    ///         receipts. The whole batch routes ONE multiAttest, so its N receipts must move
    ///         the same global counters (receiptCount, receiptsByEarner, ghost_issued) as N
    ///         single issues — the invariants prove the batch path keeps the books straight.
    function issueReceipts(
        uint256 actorSeed,
        uint256 earnerSeed,
        uint256 sizeSeed,
        uint64 renderSeconds,
        uint256 jobKindSeed,
        bytes32 outputHash
    ) external {
        uint256 size = bound(sizeSeed, 1, 8);
        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](size);
        for (uint256 i = 0; i < size; i++) {
            address earner = earners[uint256(keccak256(abi.encode(earnerSeed, i))) % earners.length];
            items[i] = RenderReceipts.ReceiptRequest({
                earner: earner,
                jobId: keccak256(abi.encode("issueReceipts", issueReceiptsNonce++)),
                renderSeconds: renderSeconds,
                jobKind: uint16(jobKindSeed % 5),
                outputHash: outputHash,
                // No region is minted in this test, so regionExists is false and the phase-4
                // fee route skips — the coordinator needs no fee tokens (mirrors the single
                // handler, which relies on the same no-region-minted skip).
                regionId: bytes32(0)
            });
        }

        address actor = actors[actorSeed % actors.length];
        vm.prank(actor);
        try receipts.issueReceipts(items) returns (bytes32[] memory) {
            for (uint256 i = 0; i < size; i++) {
                ghost_issued++;
                batchJobs.push(items[i].jobId);
            }
            // An unauthorized actor must never reach here (issueReceipts reverts NotAuthorized
            // before any effect); if one does, the no-unauthorized-issue invariant catches it.
            if (!isAuthorized[actor]) ghost_unauthorizedSuccess = true;
            // Every batch element forwards the canonical schema (multiAttest records the last).
            if (eas.lastSchema() != receipts.schemaUid()) ghost_forwardMismatch = true;
        } catch {
            // Only expected revert is NotAuthorized (unauthorized actor); fresh jobIds never
            // collide, so the all-or-nothing DuplicateReceipt path is a unit-test concern.
        }
    }

    /// @notice Drives bounded revokeReceipt calls over the same job/actor pools. Most
    ///         draws hit NotIssued/AlreadyRevoked/NotAuthorized (caught); the ones that
    ///         land on an issued, not-yet-revoked job exercise the decrement path so the
    ///         live==issued-revoked invariants are genuinely tested.
    function revokeReceipt(uint256 actorSeed, uint256 jobSeed, uint256 poolSeed) external {
        address actor = actors[actorSeed % actors.length];
        // Draw from the fixed single-issue pool or, when it has entries, the batch-issued
        // pool — so the revoke lifecycle (live==issued-revoked, revoked-reconciles) is
        // exercised over BOTH single and batch receipts.
        bytes32 jobId = (poolSeed & 1 == 1 && batchJobs.length > 0)
            ? batchJobs[jobSeed % batchJobs.length]
            : jobs[jobSeed % jobs.length];

        vm.prank(actor);
        try receipts.revokeReceipt(jobId) {
            ghost_revoked++;
            if (!isAuthorized[actor]) ghost_unauthorizedSuccess = true;
        } catch {
            // NotAuthorized / NotIssued / AlreadyRevoked — all expected.
        }
    }

    /// @notice Largest number of successful issues observed for any single job in
    ///         the pool. The dedup guard caps this at 1.
    function maxSuccessesPerJob() external view returns (uint256 mx) {
        for (uint256 i = 0; i < jobs.length; i++) {
            uint256 s = ghost_successesByJob[jobs[i]];
            if (s > mx) mx = s;
        }
    }

    /// @notice Sum of `receiptsByEarner` over every earner in the fixed pool. The
    ///         handler only ever issues to pool members, so this is the full
    ///         domain of the mapping — its sum must equal the global receiptCount.
    function sumReceipts() external view returns (uint256 total) {
        for (uint256 i = 0; i < earners.length; i++) {
            total += receipts.receiptsByEarner(earners[i]);
        }
    }
}

// ---------------------------------------------------------------------------
// Invariant test
// ---------------------------------------------------------------------------

contract RenderReceiptsInvariantTest is Test {
    RenderReceipts receipts;
    MockEAS eas;
    MockSchemaRegistry registry;
    RegionAuthority region;
    MockToken token;
    RenderReceiptsHandler handler;

    address owner = address(0xA11CE);

    function setUp() public {
        eas = new MockEAS();
        registry = new MockSchemaRegistry();
        token = new MockToken();
        region = new RegionAuthority(address(token), 100 ether, owner);
        receipts = new RenderReceipts(address(eas), owner, address(region), 1e12);

        // Build actor list: first two authorized, last two not.
        address[] memory actors = new address[](4);
        actors[0] = address(0xA1);
        actors[1] = address(0xB0B);
        actors[2] = address(0xCA7);
        actors[3] = address(0xDEAD);

        bool[] memory authorized = new bool[](4);
        authorized[0] = true;
        authorized[1] = true;
        authorized[2] = false;
        authorized[3] = false;

        // Owner registers schema and authorizes only the first two actors.
        vm.startPrank(owner);
        receipts.registerSchema(address(registry));
        receipts.setCoordinator(actors[0], true);
        receipts.setCoordinator(actors[1], true);
        vm.stopPrank();

        handler = new RenderReceiptsHandler(receipts, eas, actors, authorized);

        targetContract(address(handler));
    }

    /// @dev Every issued receipt causes exactly one EAS attestation — no spurious or missing
    ///      forwards — whether via a single `attest` (issueReceipt) or a batched `multiAttest`
    ///      (issueReceipts, which mints N receipts in one call). The two paths partition:
    ///      single issues bump attestCalls, batch elements bump multiAttestedReceipts, and
    ///      their sum must equal every receipt the handler observed issued.
    function invariant_oneAttestationPerIssuedReceipt() public view {
        assertEq(eas.attestCalls() + eas.multiAttestedReceipts(), handler.ghost_issued());
    }

    /// @dev An unauthorized sender can never successfully issue a receipt.
    function invariant_noUnauthorizedIssue() public view {
        assertFalse(handler.ghost_unauthorizedSuccess());
    }

    /// @dev Every forward used the registered schema uid and the correct recipient.
    function invariant_forwardsCanonicalSchemaAndRecipient() public view {
        assertFalse(handler.ghost_forwardMismatch());
    }

    /// @dev Per-earner `receiptsByEarner` partitions the global `receiptCount`:
    ///      summed over every earner it equals the contract's receiptCount.
    function invariant_receiptsByEarnerSumsToCount() public view {
        assertEq(handler.sumReceipts(), receipts.receiptCount());
    }

    /// @dev Guards against the invariants passing vacuously: by the end of the campaign the
    ///      fuzzer must have actually replayed a jobId (dedup rejection), landed a successful
    ///      revoke, AND driven a batch through multiAttest. The batch guard matters most: the
    ///      issueReceipts handler swallows every revert, so without it a regression that made
    ///      authorized batches revert would leave the batch path silently dead while every
    ///      invariant still passed — the exact vacuous-pass this task's batch coverage exists
    ///      to prevent. multiAttestedReceipts only moves when a batch reaches the EAS forward.
    function afterInvariant() external view {
        assertGt(handler.ghost_duplicateRejections(), 0);
        assertGt(handler.ghost_revoked(), 0);
        assertGt(eas.multiAttestedReceipts(), 0);
    }

    /// @dev No jobId is ever issued more than once: the dedup guard holds under
    ///      fuzzing even though the handler keeps replaying the same small job pool.
    ///      (A revoke never re-opens a job — the fence stays set.)
    function invariant_noJobIssuedTwice() public view {
        assertLe(handler.maxSuccessesPerJob(), 1);
    }

    /// @dev The core count invariant: live receipts == issued - revoked, with issued
    ///      and revoked tracked independently by the handler. Each issue +1s
    ///      receiptCount, each revoke -1s it.
    function invariant_liveReceiptsEqualIssuedMinusRevoked() public view {
        assertEq(receipts.receiptCount(), handler.ghost_issued() - handler.ghost_revoked());
    }

    /// @dev The revoked tally equals the number of successful revokes, and with the live
    ///      count reconstructs cumulative issued — the books never drift.
    function invariant_revokedCountReconcilesIssued() public view {
        assertEq(receipts.revokedCount(), handler.ghost_revoked());
        assertEq(receipts.receiptCount() + receipts.revokedCount(), handler.ghost_issued());
    }

    /// @dev Each successful revoke causes exactly one EAS.revoke call — no spurious or
    ///      missing forwards on the revoke side.
    function invariant_oneRevokeCallPerRevokedReceipt() public view {
        assertEq(eas.revokeCalls(), handler.ghost_revoked());
    }
}

// ---------------------------------------------------------------------------
// Fuzz tests
// ---------------------------------------------------------------------------

contract RenderReceiptsFuzzTest is Test {
    RenderReceipts receipts;
    MockEAS eas;
    MockSchemaRegistry registry;
    RegionAuthority region;
    MockToken token;

    address owner = address(0xA11CE);
    address coordinator = address(0xC0DE);

    function setUp() public {
        eas = new MockEAS();
        registry = new MockSchemaRegistry();
        token = new MockToken();
        region = new RegionAuthority(address(token), 100 ether, owner);
        receipts = new RenderReceipts(address(eas), owner, address(region), 1e12);

        vm.startPrank(owner);
        receipts.registerSchema(address(registry));
        receipts.setCoordinator(coordinator, true);
        vm.stopPrank();
    }

    function testFuzz_issueReceipt_forwardsEncodedArgs(
        address earner,
        bytes32 jobId,
        uint64 renderSeconds,
        uint16 jobKind,
        bytes32 outputHash,
        bytes32 regionId
    ) public {
        vm.assume(earner != address(0));

        bytes32 schemaUid = receipts.schemaUid();

        vm.prank(coordinator);
        bytes32 uid = receipts.issueReceipt(earner, jobId, renderSeconds, jobKind, outputHash, regionId);

        assertEq(eas.attestCalls(), 1);
        assertEq(eas.lastSchema(), schemaUid);
        assertEq(eas.lastRecipient(), earner);
        assertEq(eas.lastData(), abi.encode(earner, jobId, renderSeconds, jobKind, outputHash, regionId));

        // Returned uid must match the mock's deterministic formula.
        bytes32 expectedUid = keccak256(
            abi.encode(
                schemaUid, earner, abi.encode(earner, jobId, renderSeconds, jobKind, outputHash, regionId)
            )
        );
        assertEq(uid, expectedUid);
    }

    function testFuzz_issueReceipt_unauthorizedReverts(address sender) public {
        vm.assume(sender != coordinator);

        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(sender);
        receipts.issueReceipt(address(0xEA12), keccak256("job"), 10, 0, bytes32(0), bytes32(0));
    }

    function testFuzz_revokeReceipt_unauthorizedReverts(address sender) public {
        vm.assume(sender != coordinator);
        bytes32 jobId = keccak256("job");
        vm.prank(coordinator);
        receipts.issueReceipt(address(0xEA12), jobId, 10, 0, bytes32(0), bytes32(0));

        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(sender);
        receipts.revokeReceipt(jobId);
    }

    function testFuzz_revokeReceipt_revokesStoredUidAndDecrements(
        address earner,
        bytes32 jobId,
        uint64 renderSeconds,
        uint16 jobKind,
        bytes32 outputHash,
        bytes32 regionId
    ) public {
        vm.assume(earner != address(0));

        vm.startPrank(coordinator);
        bytes32 uid = receipts.issueReceipt(earner, jobId, renderSeconds, jobKind, outputHash, regionId);
        receipts.revokeReceipt(jobId);
        vm.stopPrank();

        // The stored uid (not a caller argument) was the one revoked at EAS.
        assertEq(eas.revokeCalls(), 1);
        assertTrue(eas.isRevoked(uid));
        assertTrue(receipts.receiptRevoked(jobId));
        assertEq(receipts.receiptCount(), 0);
        assertEq(receipts.receiptsByEarner(earner), 0);
        assertEq(receipts.revokedCount(), 1);
    }
}
