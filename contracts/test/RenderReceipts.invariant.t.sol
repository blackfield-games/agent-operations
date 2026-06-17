// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {RenderReceipts, IEAS, ISchemaRegistry} from "../src/RenderReceipts.sol";

// ---------------------------------------------------------------------------
// Local mocks — same shape as RenderReceipts.t.sol
// ---------------------------------------------------------------------------

/// @dev Records the last attest request and returns a deterministic uid.
contract MockEAS is IEAS {
    IEAS.AttestationRequest public lastRequest;
    uint256 public attestCalls;

    bytes32 public lastSchema;
    address public lastRecipient;
    uint64 public lastExpirationTime;
    bool public lastRevocable;
    bytes32 public lastRefUid;
    bytes public lastData;
    uint256 public lastValue;

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
        return keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
    }

    function multiAttest(IEAS.MultiAttestationRequest[] calldata)
        external
        payable
        returns (bytes32[] memory)
    {
        revert("not implemented");
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
    mapping(address => bool) public isAuthorized;
    mapping(bytes32 => uint256) public ghost_successesByJob;
    uint256 public ghost_duplicateRejections;

    uint256 public ghost_issued;
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
    RenderReceiptsHandler handler;

    address owner = address(0xA11CE);

    function setUp() public {
        eas = new MockEAS();
        registry = new MockSchemaRegistry();
        receipts = new RenderReceipts(address(eas), owner);

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

    /// @dev Every issued receipt causes exactly one EAS attest call — no spurious or
    ///      missing forwards.
    function invariant_oneAttestationPerIssuedReceipt() public view {
        assertEq(eas.attestCalls(), handler.ghost_issued());
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

    /// @dev Guards against `invariant_noJobIssuedTwice` passing vacuously: by the end
    ///      of the campaign the fuzzer must have actually replayed a jobId and been
    ///      rejected, so the dedup path was genuinely exercised (≈189 in practice).
    function afterInvariant() external view {
        assertGt(handler.ghost_duplicateRejections(), 0);
    }

    /// @dev No jobId is ever issued more than once: the dedup guard holds under
    ///      fuzzing even though the handler keeps replaying the same small job pool.
    function invariant_noJobIssuedTwice() public view {
        assertLe(handler.maxSuccessesPerJob(), 1);
    }

    /// @dev Every successful issue increments receiptCount exactly once — rejected
    ///      duplicates leave it untouched.
    function invariant_receiptCountEqualsIssued() public view {
        assertEq(receipts.receiptCount(), handler.ghost_issued());
    }
}

// ---------------------------------------------------------------------------
// Fuzz tests
// ---------------------------------------------------------------------------

contract RenderReceiptsFuzzTest is Test {
    RenderReceipts receipts;
    MockEAS eas;
    MockSchemaRegistry registry;

    address owner = address(0xA11CE);
    address coordinator = address(0xC0DE);

    function setUp() public {
        eas = new MockEAS();
        registry = new MockSchemaRegistry();
        receipts = new RenderReceipts(address(eas), owner);

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
}
