// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {RenderReceipts, IEAS, ISchemaRegistry} from "../src/RenderReceipts.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @dev Records the last attest request and returns a deterministic uid.
contract MockEAS is IEAS {
    IEAS.AttestationRequest public lastRequest;
    uint256 public attestCalls;

    // Flattened mirror of the last request so tests can read nested fields easily.
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

    function multiAttest(IEAS.MultiAttestationRequest[] calldata) external payable returns (bytes32[] memory) {
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

contract RenderReceiptsTest is Test {
    RenderReceipts receipts;
    MockEAS eas;
    MockSchemaRegistry registry;

    address owner = address(0xA11CE);
    address coordinator = address(0xC0DE);
    address earner = address(0xEA12);
    address stranger = address(0xBEEF);

    event ReceiptIssued(
        bytes32 indexed uid, address indexed earner, bytes32 indexed jobId, uint16 jobKind, uint64 renderSeconds
    );
    event CoordinatorSet(address indexed coordinator, bool authorized);
    event SchemaRegistered(bytes32 indexed uid);

    function setUp() public {
        eas = new MockEAS();
        registry = new MockSchemaRegistry();
        receipts = new RenderReceipts(address(eas), owner);
    }

    // --- construction ---

    function test_constructor_setsEasAndOwner() public view {
        assertEq(address(receipts.EAS()), address(eas));
        assertEq(receipts.owner(), owner);
        assertEq(receipts.schemaUid(), bytes32(0));
        assertEq(receipts.receiptCount(), 0);
    }

    // --- registerSchema ---

    function test_registerSchema_setsUidAndEmits() public {
        vm.expectEmit(true, false, false, true);
        emit SchemaRegistered(registry.FIXED_UID());
        vm.prank(owner);
        bytes32 returned = receipts.registerSchema(address(registry));

        assertEq(returned, registry.FIXED_UID());
        assertEq(receipts.schemaUid(), registry.FIXED_UID());
    }

    function test_registerSchema_forwardsCanonicalSchemaArgs() public {
        vm.prank(owner);
        receipts.registerSchema(address(registry));

        assertEq(
            registry.lastSchemaStr(),
            "address earner, bytes32 jobId, uint64 renderSeconds, uint16 jobKind, bytes32 outputHash, bytes32 regionId"
        );
        assertEq(registry.lastResolver(), address(0));
        assertTrue(registry.lastRevocable());
    }

    function test_registerSchema_onlyOwner() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, stranger));
        vm.prank(stranger);
        receipts.registerSchema(address(registry));
    }

    // --- setCoordinator ---

    function test_setCoordinator_gatesAndEmits() public {
        vm.expectEmit(true, false, false, true);
        emit CoordinatorSet(coordinator, true);
        vm.prank(owner);
        receipts.setCoordinator(coordinator, true);
        assertTrue(receipts.authorizedCoordinators(coordinator));

        vm.expectEmit(true, false, false, true);
        emit CoordinatorSet(coordinator, false);
        vm.prank(owner);
        receipts.setCoordinator(coordinator, false);
        assertFalse(receipts.authorizedCoordinators(coordinator));
    }

    function test_setCoordinator_onlyOwner() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, stranger));
        vm.prank(stranger);
        receipts.setCoordinator(coordinator, true);
    }

    // --- issueReceipt revert paths ---

    function test_issueReceipt_revertsNotAuthorized() public {
        // Schema set first to prove the auth check fires before SchemaNotSet.
        vm.startPrank(owner);
        receipts.registerSchema(address(registry));
        vm.stopPrank();

        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(stranger);
        receipts.issueReceipt(earner, keccak256("j"), 10, 0, bytes32(0), bytes32(0));
    }

    function test_issueReceipt_revertsSchemaNotSet() public {
        vm.prank(owner);
        receipts.setCoordinator(coordinator, true);

        vm.expectRevert(RenderReceipts.SchemaNotSet.selector);
        vm.prank(coordinator);
        receipts.issueReceipt(earner, keccak256("j"), 10, 0, bytes32(0), bytes32(0));
    }

    // --- issueReceipt happy path ---

    function _arm() internal {
        vm.startPrank(owner);
        receipts.registerSchema(address(registry));
        receipts.setCoordinator(coordinator, true);
        vm.stopPrank();
    }

    function test_issueReceipt_happyPath_emitsAndForwards() public {
        _arm();

        bytes32 jobId = keccak256("render-job-42");
        uint64 renderSeconds = 1234;
        uint16 jobKind = 3; // diffusion_tile
        bytes32 outputHash = keccak256("output");
        bytes32 regionId = keccak256("region-7");

        bytes memory expectedData = abi.encode(earner, jobId, renderSeconds, jobKind, outputHash, regionId);
        bytes32 expectedUid = keccak256(abi.encode(registry.FIXED_UID(), earner, expectedData));

        vm.expectEmit(true, true, true, true);
        emit ReceiptIssued(expectedUid, earner, jobId, jobKind, renderSeconds);

        vm.prank(coordinator);
        bytes32 uid = receipts.issueReceipt(earner, jobId, renderSeconds, jobKind, outputHash, regionId);

        assertEq(uid, expectedUid);

        // EAS received exactly one well-formed request.
        assertEq(eas.attestCalls(), 1);
        assertEq(eas.lastSchema(), registry.FIXED_UID());
        assertEq(eas.lastRecipient(), earner);
        assertEq(eas.lastExpirationTime(), 0);
        assertTrue(eas.lastRevocable());
        assertEq(eas.lastRefUid(), bytes32(0));
        assertEq(eas.lastValue(), 0);
        assertEq(eas.lastData(), expectedData);

        // Decoded payload round-trips to the original args.
        (
            address dEarner,
            bytes32 dJobId,
            uint64 dRenderSeconds,
            uint16 dJobKind,
            bytes32 dOutputHash,
            bytes32 dRegionId
        ) = abi.decode(eas.lastData(), (address, bytes32, uint64, uint16, bytes32, bytes32));
        assertEq(dEarner, earner);
        assertEq(dJobId, jobId);
        assertEq(dRenderSeconds, renderSeconds);
        assertEq(dJobKind, jobKind);
        assertEq(dOutputHash, outputHash);
        assertEq(dRegionId, regionId);
    }

    function test_issueReceipt_incrementsReceiptCount() public {
        _arm();

        vm.prank(coordinator);
        receipts.issueReceipt(earner, keccak256("j1"), 10, 0, bytes32(0), bytes32(0));
        assertEq(receipts.receiptCount(), 1);

        vm.prank(coordinator);
        receipts.issueReceipt(earner, keccak256("j2"), 10, 0, bytes32(0), bytes32(0));
        assertEq(receipts.receiptCount(), 2);

        vm.prank(coordinator);
        receipts.issueReceipt(earner, keccak256("j3"), 10, 0, bytes32(0), bytes32(0));
        assertEq(receipts.receiptCount(), 3);
    }

    function test_issueReceipt_deauthorizedCoordinatorReverts() public {
        _arm();
        vm.prank(owner);
        receipts.setCoordinator(coordinator, false);

        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(coordinator);
        receipts.issueReceipt(earner, keccak256("j"), 1, 0, bytes32(0), bytes32(0));
    }

    // --- Ownable2Step ---

    function test_ownership_twoStepTransfer() public {
        address newOwner = address(0xDEAD);

        vm.prank(owner);
        receipts.transferOwnership(newOwner);

        // Pending; owner unchanged until accepted.
        assertEq(receipts.owner(), owner);
        assertEq(receipts.pendingOwner(), newOwner);

        // A non-pending account cannot accept.
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, stranger));
        vm.prank(stranger);
        receipts.acceptOwnership();

        vm.prank(newOwner);
        receipts.acceptOwnership();
        assertEq(receipts.owner(), newOwner);
        assertEq(receipts.pendingOwner(), address(0));

        // Old owner is now powerless.
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, owner));
        vm.prank(owner);
        receipts.setCoordinator(coordinator, true);
    }
}
