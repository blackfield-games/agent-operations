// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {RenderReceipts, IEAS, ISchemaRegistry} from "../src/RenderReceipts.sol";
import {RegionAuthority} from "../src/RegionAuthority.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20Errors} from "@openzeppelin/contracts/interfaces/draft-IERC6093.sol";

/// @dev Real $BLCKFLD stand-in: the coordinator/treasury pays the region fee-share from
///      this, and RegionAuthority stakes/pulls it. The test contract holds the supply.
contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

/// @dev A RegionAuthority-shaped contract whose TOKEN() is the zero address — a misconfig
///      the RenderReceipts constructor must reject (ZeroFeeToken) rather than binding a
///      zero fee token. Only TOKEN() is reached during construction.
contract ZeroTokenRegion {
    function TOKEN() external pure returns (address) {
        return address(0);
    }
}

/// @dev Hostile fee token that reenters issueReceipt with the same jobId on the fee pull
///      (transferFrom), to prove the receipt fence — set before the fee route — closes the
///      double-issue window even when the fee token itself is malicious. The one-shot
///      `reentered` latch also lets the outer route's second hop (RegionAuthority pulling
///      from RenderReceipts) complete without recursing. Mirrors ReentrantEAS.
contract ReentrantFeeToken is ERC20 {
    RenderReceipts public target;
    address public reEarner;
    bytes32 public reJobId;
    bool public armed;
    bool public reentered;
    bool public reentryReverted;
    bytes4 public reentryRevertSelector;

    constructor() ERC20("Reentrant", "RE") {
        _mint(msg.sender, 1_000_000 ether);
    }

    function arm(RenderReceipts target_, address earner_, bytes32 jobId_) external {
        target = target_;
        reEarner = earner_;
        reJobId = jobId_;
        armed = true;
    }

    function transferFrom(address from, address to, uint256 amount) public override returns (bool) {
        if (armed && !reentered) {
            reentered = true;
            try target.issueReceipt(reEarner, reJobId, 1, 0, bytes32(0), bytes32(0)) returns (
                bytes32
            ) {
            // reentry unexpectedly succeeded — reentryReverted stays false
            }
            catch (bytes memory err) {
                reentryReverted = true;
                reentryRevertSelector = bytes4(err);
            }
        }
        return super.transferFrom(from, to, amount);
    }
}

/// @dev Records the last attest request and returns a deterministic uid.
contract MockEAS is IEAS {
    IEAS.AttestationRequest public lastRequest;
    uint256 public attestCalls;
    // Separate from attestCalls so a batch test can prove issueReceipts took the multiAttest
    // path (one call) and NOT N single attests.
    uint256 public multiAttestCalls;
    uint256 public lastBatchSize;

    // Flattened mirror of the last request so tests can read nested fields easily.
    bytes32 public lastSchema;
    address public lastRecipient;
    uint64 public lastExpirationTime;
    bool public lastRevocable;
    bytes32 public lastRefUid;
    bytes public lastData;
    uint256 public lastValue;

    // Revoke modelling: only an attestation this mock actually minted, and not yet
    // revoked, can be revoked — same precondition the real EAS enforces.
    mapping(bytes32 => bool) public isAttested;
    mapping(bytes32 => bool) public isRevoked;
    uint256 public revokeCalls;
    bytes32 public lastRevokedUid;
    bytes32 public lastRevokeSchema;

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
        lastRevokedUid = request.data.uid;
        lastRevokeSchema = request.schema;
    }

    /// @dev Mirrors `attest`'s per-element uid derivation so a batch-issued receipt is
    ///      revocable (revoke checks `isAttested[uid]`) and predictable, and returns the
    ///      flat uid[] across all groups in submission order — exactly the real EAS
    ///      contract. RenderReceipts sends a single group, so the returned length equals
    ///      that group's element count.
    function multiAttest(IEAS.MultiAttestationRequest[] calldata multiRequests)
        external
        payable
        returns (bytes32[] memory)
    {
        multiAttestCalls++;
        uint256 total;
        for (uint256 g = 0; g < multiRequests.length; g++) {
            total += multiRequests[g].data.length;
        }
        lastBatchSize = total;

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

/// @dev Hostile EAS that reenters issueReceipt with the same jobId mid-attest,
///      to prove receiptIssued is set before the external call (checks-effects-
///      interactions) and a reentrant relay cannot mint a second attestation.
contract ReentrantEAS is IEAS {
    RenderReceipts public target;
    address public reEarner;
    bytes32 public reJobId;
    uint256 public attestCalls;
    bool public reentered;
    bool public reentryReverted;
    bytes4 public reentryRevertSelector;

    function arm(RenderReceipts target_, address earner_, bytes32 jobId_) external {
        target = target_;
        reEarner = earner_;
        reJobId = jobId_;
    }

    function attest(IEAS.AttestationRequest calldata request) external payable returns (bytes32) {
        attestCalls++;
        if (!reentered) {
            reentered = true;
            try target.issueReceipt(reEarner, reJobId, 1, 0, bytes32(0), bytes32(0)) returns (
                bytes32
            ) {
            // reentry unexpectedly succeeded — reentryReverted stays false
            }
            catch (bytes memory err) {
                reentryReverted = true;
                reentryRevertSelector = bytes4(err);
            }
        }
        return keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
    }

    function revoke(IEAS.RevocationRequest calldata) external payable {
        revert("not implemented");
    }

    function multiAttest(IEAS.MultiAttestationRequest[] calldata)
        external
        payable
        returns (bytes32[] memory)
    {
        revert("not implemented");
    }
}

/// @dev Hostile EAS that reenters revokeReceipt with the same jobId mid-revoke, to
///      prove receiptRevoked is flipped and the counters decremented BEFORE the external
///      EAS.revoke (checks-effects-interactions): the reentrant revoke is rejected by
///      AlreadyRevoked and cannot double-decrement or double-revoke.
contract ReentrantRevokeEAS is IEAS {
    RenderReceipts public target;
    bytes32 public reJobId;
    uint256 public attestCalls;
    uint256 public revokeCalls;
    bool public reentered;
    bool public reentryReverted;
    bytes4 public reentryRevertSelector;

    function arm(RenderReceipts target_, bytes32 jobId_) external {
        target = target_;
        reJobId = jobId_;
    }

    function attest(IEAS.AttestationRequest calldata request) external payable returns (bytes32) {
        attestCalls++;
        return keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
    }

    function revoke(IEAS.RevocationRequest calldata) external payable {
        revokeCalls++;
        if (!reentered) {
            reentered = true;
            try target.revokeReceipt(reJobId) {
            // reentry unexpectedly succeeded — reentryReverted stays false
            }
            catch (bytes memory err) {
                reentryReverted = true;
                reentryRevertSelector = bytes4(err);
            }
        }
    }

    function multiAttest(IEAS.MultiAttestationRequest[] calldata)
        external
        payable
        returns (bytes32[] memory)
    {
        revert("not implemented");
    }
}

/// @dev Hostile EAS that reenters revokeReceipt with the same jobId mid-ATTEST (during
///      issueReceipt), when receiptIssued is already set but receiptUid is still zero.
///      Proves revokeReceipt's uid==0 guard rejects it (NotIssued) without relying on EAS
///      to reject a zero uid, and the outer issue still completes cleanly.
contract ReentrantIssueRevokeEAS is IEAS {
    RenderReceipts public target;
    bytes32 public reJobId;
    uint256 public attestCalls;
    bool public reentered;
    bool public reentryReverted;
    bytes4 public reentryRevertSelector;

    function arm(RenderReceipts target_, bytes32 jobId_) external {
        target = target_;
        reJobId = jobId_;
    }

    function attest(IEAS.AttestationRequest calldata request) external payable returns (bytes32) {
        attestCalls++;
        if (!reentered) {
            reentered = true;
            try target.revokeReceipt(reJobId) {
            // reentry unexpectedly succeeded — reentryReverted stays false
            }
            catch (bytes memory err) {
                reentryReverted = true;
                reentryRevertSelector = bytes4(err);
            }
        }
        return keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
    }

    function revoke(IEAS.RevocationRequest calldata) external payable {
        revert("not implemented");
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

contract RenderReceiptsTest is Test {
    RenderReceipts receipts;
    MockEAS eas;
    MockSchemaRegistry registry;
    RegionAuthority region;
    MockToken token;

    address owner = address(0xA11CE);
    address coordinator = address(0xC0DE);
    address earner = address(0xEA12);
    address stranger = address(0xBEEF);
    // Region holder (an EOA so RegionAuthority's _safeMint receiver check passes) that
    // earns the fee-share routed into `claimedRegion`.
    address regionHolder = address(0x5E6104);

    uint256 constant RENDER_FEE_RATE = 1e12; // per render-second, real $BLCKFLD
    uint256 constant STAKE = 100 ether;
    // A claimed region the fee-share routes into; its bytes32 form is the receipt field.
    uint256 claimedRegion = uint256(keccak256("region-claimed"));
    bytes32 claimedRegionId = keccak256("region-claimed");

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
    event RenderFeeRouted(bytes32 indexed jobId, uint256 indexed regionId, uint256 amount);
    // RegionAuthority's intake event, re-declared for expectEmit against the region.
    event FeesDeposited(uint256 indexed tokenId, address indexed from, uint256 amount);

    function setUp() public {
        eas = new MockEAS();
        registry = new MockSchemaRegistry();
        token = new MockToken();
        region = new RegionAuthority(address(token), STAKE, owner);
        receipts = new RenderReceipts(address(eas), owner, address(region), RENDER_FEE_RATE);

        // Claim a region (held by a dedicated EOA) so the fee-share has a live recipient.
        // Existing tests pass arbitrary unclaimed regionIds, so their route is skipped and
        // they need no fee funding; the fee-routing tests target `claimedRegion`.
        token.transfer(regionHolder, STAKE);
        vm.startPrank(regionHolder);
        token.approve(address(region), STAKE);
        region.claim(claimedRegion, STAKE);
        vm.stopPrank();
    }

    /// @dev Fund the coordinator (the fee payer) with real $BLCKFLD and approve receipts
    ///      to pull it, so a fee-routing receipt can transfer the region fee-share.
    function _fundCoordinator(uint256 amount) internal {
        token.transfer(coordinator, amount);
        vm.prank(coordinator);
        token.approve(address(receipts), type(uint256).max);
    }

    // --- construction ---

    function test_constructor_setsEasAndOwner() public view {
        assertEq(address(receipts.EAS()), address(eas));
        assertEq(receipts.owner(), owner);
        assertEq(receipts.schemaUid(), bytes32(0));
        assertEq(receipts.receiptCount(), 0);
    }

    function test_constructor_wiresRegionAuthorityAndFeeToken() public view {
        assertEq(address(receipts.regionAuthority()), address(region));
        assertEq(receipts.renderFeeRate(), RENDER_FEE_RATE);
        // The fee token is derived from RegionAuthority.TOKEN(), never desyncing from what
        // depositFees pulls (mirrors ArtifactTemplate.royaltyToken).
        assertEq(address(receipts.feeToken()), address(token));
    }

    function test_constructor_revertsZeroRegionAuthority() public {
        vm.expectRevert(RenderReceipts.ZeroRegionAuthority.selector);
        new RenderReceipts(address(eas), owner, address(0), RENDER_FEE_RATE);
    }

    function test_constructor_revertsZeroFeeRate() public {
        // A zero rate would silently disable the region fee gate — reject at construction
        // (mirrors RegionAuthority's ZeroStake and ArtifactTemplate's ZeroRoyaltyRate).
        vm.expectRevert(RenderReceipts.ZeroFeeRate.selector);
        new RenderReceipts(address(eas), owner, address(region), 0);
    }

    function test_constructor_revertsZeroFeeToken() public {
        // A RegionAuthority whose TOKEN() is zero is a misconfig; fail loudly at deploy
        // rather than letting every fee-routing receipt revert opaquely at SafeERC20.
        ZeroTokenRegion bad = new ZeroTokenRegion();
        vm.expectRevert(RenderReceipts.ZeroFeeToken.selector);
        new RenderReceipts(address(eas), owner, address(bad), RENDER_FEE_RATE);
    }

    // --- setRenderFeeRate ---

    function test_setRenderFeeRate_updatesAndEmits() public {
        vm.expectEmit(false, false, false, true);
        emit RenderFeeRateSet(5e12);
        vm.prank(owner);
        receipts.setRenderFeeRate(5e12);
        assertEq(receipts.renderFeeRate(), 5e12);
    }

    function test_setRenderFeeRate_revertsZero() public {
        vm.expectRevert(RenderReceipts.ZeroFeeRate.selector);
        vm.prank(owner);
        receipts.setRenderFeeRate(0);
    }

    function test_setRenderFeeRate_onlyOwner() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, stranger));
        vm.prank(stranger);
        receipts.setRenderFeeRate(5e12);
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

    /// @dev The forwarded `data` must match the registered schema's field layout
    ///      exactly: `(address, bytes32, uint64, uint16, bytes32, bytes32)` in that
    ///      order. EAS readers decode against the registered schema string, so a
    ///      reordered or retyped field here makes every attestation unreadable. This
    ///      pins the on-chain `abi.encode` to the schema declared in `registerSchema`.
    function test_issueReceipt_encodingMatchesRegisteredSchemaLayout() public {
        _arm();

        address e = address(0x1234);
        bytes32 jobId = keccak256("layout-job");
        uint64 renderSeconds = 7;
        uint16 jobKind = 2;
        bytes32 outputHash = keccak256("out");
        bytes32 regionId = keccak256("reg");

        vm.prank(coordinator);
        receipts.issueReceipt(e, jobId, renderSeconds, jobKind, outputHash, regionId);

        // Decode in the schema's declared order; every field round-trips.
        (
            address dEarner,
            bytes32 dJobId,
            uint64 dRenderSeconds,
            uint16 dJobKind,
            bytes32 dOutputHash,
            bytes32 dRegionId
        ) = abi.decode(eas.lastData(), (address, bytes32, uint64, uint16, bytes32, bytes32));
        assertEq(dEarner, e);
        assertEq(dJobId, jobId);
        assertEq(dRenderSeconds, renderSeconds);
        assertEq(dJobKind, jobKind);
        assertEq(dOutputHash, outputHash);
        assertEq(dRegionId, regionId);

        // Byte-identical to a from-scratch encode in the declared field order.
        assertEq(eas.lastData(), abi.encode(e, jobId, renderSeconds, jobKind, outputHash, regionId));
    }

    /// @dev Boundary values for the two narrow numeric fields encode without
    ///      truncation: the schema declares `uint64 renderSeconds` and `uint16
    ///      jobKind`, so max-width inputs (and a jobKind beyond the documented 0..4
    ///      set — range is an off-chain concern) must round-trip intact.
    function test_issueReceipt_encodesBoundaryValuesWithoutTruncation() public {
        _arm();

        uint64 renderSeconds = type(uint64).max;
        uint16 jobKind = type(uint16).max;
        bytes32 jobId = bytes32(type(uint256).max);
        bytes32 outputHash = bytes32(type(uint256).max);
        bytes32 regionId = bytes32(type(uint256).max);

        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, renderSeconds, jobKind, outputHash, regionId);

        (, bytes32 dJobId, uint64 dRenderSeconds, uint16 dJobKind,,) =
            abi.decode(eas.lastData(), (address, bytes32, uint64, uint16, bytes32, bytes32));
        assertEq(dRenderSeconds, type(uint64).max);
        assertEq(dJobKind, type(uint16).max);
        assertEq(dJobId, jobId);
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

    function test_issueReceipt_tracksPerEarnerCountSummingToTotal() public {
        _arm();
        address earnerB = address(0xEA34);

        // Two receipts for `earner`, one for `earnerB`.
        vm.startPrank(coordinator);
        receipts.issueReceipt(earner, keccak256("a1"), 10, 0, bytes32(0), bytes32(0));
        receipts.issueReceipt(earnerB, keccak256("b1"), 10, 0, bytes32(0), bytes32(0));
        receipts.issueReceipt(earner, keccak256("a2"), 10, 0, bytes32(0), bytes32(0));
        vm.stopPrank();

        // Per-earner getter reflects each earner's own count.
        assertEq(receipts.receiptsByEarner(earner), 2);
        assertEq(receipts.receiptsByEarner(earnerB), 1);
        // An earner who never earned reads zero (mapping default).
        assertEq(receipts.receiptsByEarner(stranger), 0);
        // The per-earner counts sum to the global receiptCount.
        assertEq(
            receipts.receiptsByEarner(earner) + receipts.receiptsByEarner(earnerB), receipts.receiptCount()
        );
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

    // --- issueReceipt idempotency (one attestation per job) ---

    function test_issueReceipt_duplicateJobIdReverts() public {
        _arm();
        bytes32 jobId = keccak256("once");

        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));
        assertTrue(receipts.receiptIssued(jobId));

        vm.expectRevert(abi.encodeWithSelector(RenderReceipts.DuplicateReceipt.selector, jobId));
        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        // The rejected replay left no trace: no second attest, no double count.
        assertEq(eas.attestCalls(), 1);
        assertEq(receipts.receiptCount(), 1);
        assertEq(receipts.receiptsByEarner(earner), 1);
    }

    /// @dev The guard is keyed on jobId alone, so the same job cannot be re-attested
    ///      to a *different* earner — closing the obvious double-credit escape hatch.
    function test_issueReceipt_duplicateRevertsAcrossDifferentEarners() public {
        _arm();
        bytes32 jobId = keccak256("shared");
        address earnerB = address(0xEA34);

        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        vm.expectRevert(abi.encodeWithSelector(RenderReceipts.DuplicateReceipt.selector, jobId));
        vm.prank(coordinator);
        receipts.issueReceipt(earnerB, jobId, 10, 0, bytes32(0), bytes32(0));

        assertEq(receipts.receiptsByEarner(earnerB), 0);
        assertEq(receipts.receiptCount(), 1);
    }

    /// @dev The guard fences per job, not globally: distinct jobIds each issue.
    function test_issueReceipt_distinctJobIdsBothSucceed() public {
        _arm();

        vm.startPrank(coordinator);
        receipts.issueReceipt(earner, keccak256("j1"), 10, 0, bytes32(0), bytes32(0));
        receipts.issueReceipt(earner, keccak256("j2"), 10, 0, bytes32(0), bytes32(0));
        vm.stopPrank();

        assertEq(receipts.receiptCount(), 2);
        assertFalse(receipts.receiptIssued(keccak256("never")));
    }

    /// @dev Auth is checked before the duplicate guard, so an unauthorized caller
    ///      gets NotAuthorized even for an already-issued job — the dedup state is
    ///      not an oracle that leaks which jobs have been attested.
    function test_issueReceipt_unauthorizedRevertsEvenForIssuedJob() public {
        _arm();
        bytes32 jobId = keccak256("issued");

        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(stranger);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));
    }

    /// @dev The receiptIssued flag is set before the external attest, so a hostile
    ///      EAS reentering issueReceipt with the same jobId mid-attest is rejected:
    ///      exactly one attestation and one receipt result, counters consistent.
    ///      (Would fail if the flag were written after the attest call.)
    function test_issueReceipt_reentrantAttestCannotDoubleIssue() public {
        ReentrantEAS reentrantEas = new ReentrantEAS();
        RenderReceipts r = new RenderReceipts(address(reentrantEas), owner, address(region), RENDER_FEE_RATE);

        bytes32 jobId = keccak256("reentrant-job");
        reentrantEas.arm(r, earner, jobId);

        vm.startPrank(owner);
        r.registerSchema(address(registry));
        r.setCoordinator(coordinator, true);
        r.setCoordinator(address(reentrantEas), true); // reentry's msg.sender is the EAS
        vm.stopPrank();

        vm.prank(coordinator);
        r.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        assertTrue(reentrantEas.reentered());
        assertTrue(reentrantEas.reentryReverted());
        assertEq(reentrantEas.reentryRevertSelector(), RenderReceipts.DuplicateReceipt.selector);

        assertEq(reentrantEas.attestCalls(), 1);
        assertEq(r.receiptCount(), 1);
        assertEq(r.receiptsByEarner(earner), 1);
    }

    /// @dev The owner administers coordinators but is not itself a coordinator;
    ///      issuing requires explicit self-authorization (authority separation).
    function test_issueReceipt_ownerIsNotImplicitlyAuthorized() public {
        _arm();

        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(owner);
        receipts.issueReceipt(earner, keccak256("j"), 10, 0, bytes32(0), bytes32(0));
    }

    // --- revokeReceipt ---

    function test_revokeReceipt_happyPath_flipsStateDecrementsAndRevokesUid() public {
        _arm();
        bytes32 jobId = keccak256("revoke-me");
        vm.prank(coordinator);
        bytes32 uid = receipts.issueReceipt(earner, jobId, 10, 1, bytes32(0), bytes32(0));
        assertEq(receipts.receiptCount(), 1);
        assertEq(receipts.receiptsByEarner(earner), 1);
        assertEq(receipts.receiptUid(jobId), uid);
        assertFalse(receipts.receiptRevoked(jobId));

        vm.expectEmit(true, true, true, true);
        emit ReceiptRevoked(uid, earner, jobId);
        vm.prank(coordinator);
        receipts.revokeReceipt(jobId);

        // Fence flipped to revoked; receipt no longer live; revoked tally up.
        assertTrue(receipts.receiptRevoked(jobId));
        assertTrue(receipts.receiptIssued(jobId)); // stays set — cannot be re-issued
        assertEq(receipts.receiptCount(), 0);
        assertEq(receipts.receiptsByEarner(earner), 0);
        assertEq(receipts.revokedCount(), 1);

        // EAS.revoke was called once on the stored uid with the canonical schema, and
        // the faithful mock marked that attestation revoked.
        assertEq(eas.revokeCalls(), 1);
        assertEq(eas.lastRevokedUid(), uid);
        assertEq(eas.lastRevokeSchema(), receipts.schemaUid());
        assertTrue(eas.isRevoked(uid));
    }

    function test_revokeReceipt_revertsNotAuthorized() public {
        _arm();
        bytes32 jobId = keccak256("j");
        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(stranger);
        receipts.revokeReceipt(jobId);

        // The rejected revoke changed nothing.
        assertFalse(receipts.receiptRevoked(jobId));
        assertEq(receipts.receiptCount(), 1);
        assertEq(receipts.revokedCount(), 0);
        assertEq(eas.revokeCalls(), 0);
    }

    /// @dev The owner administers coordinators but is not itself one; revoking, like
    ///      issuing, requires explicit self-authorization.
    function test_revokeReceipt_ownerIsNotImplicitlyAuthorized() public {
        _arm();
        bytes32 jobId = keccak256("j");
        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(owner);
        receipts.revokeReceipt(jobId);
    }

    /// @dev Auth is checked before the issued/revoked guards, so an unauthorized caller
    ///      gets NotAuthorized even for a never-issued job — no state oracle leak.
    function test_revokeReceipt_unauthorizedRevertsBeforeNotIssued() public {
        _arm();
        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(stranger);
        receipts.revokeReceipt(keccak256("never"));
    }

    function test_revokeReceipt_revertsNotIssued() public {
        _arm();
        bytes32 jobId = keccak256("never-issued");
        vm.expectRevert(abi.encodeWithSelector(RenderReceipts.NotIssued.selector, jobId));
        vm.prank(coordinator);
        receipts.revokeReceipt(jobId);
    }

    function test_revokeReceipt_doubleRevokeReverts() public {
        _arm();
        bytes32 jobId = keccak256("once");
        vm.startPrank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));
        receipts.revokeReceipt(jobId);

        vm.expectRevert(abi.encodeWithSelector(RenderReceipts.AlreadyRevoked.selector, jobId));
        receipts.revokeReceipt(jobId);
        vm.stopPrank();

        // The rejected second revoke left the post-first-revoke values untouched.
        assertEq(receipts.receiptCount(), 0);
        assertEq(receipts.revokedCount(), 1);
        assertEq(eas.revokeCalls(), 1);
    }

    /// @dev A revoked job's fence stays set, so it can never be re-issued (no
    ///      resurrection of a withdrawn attestation).
    function test_revokeReceipt_revokedJobCannotBeReissued() public {
        _arm();
        bytes32 jobId = keccak256("revoked-then-reissue");
        vm.startPrank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));
        receipts.revokeReceipt(jobId);

        vm.expectRevert(abi.encodeWithSelector(RenderReceipts.DuplicateReceipt.selector, jobId));
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));
        vm.stopPrank();

        assertEq(receipts.receiptCount(), 0); // not resurrected
        assertEq(receipts.revokedCount(), 1);
        assertEq(eas.attestCalls(), 1); // the blocked re-issue never re-attested
    }

    function test_revokeReceipt_deauthorizedCoordinatorReverts() public {
        _arm();
        bytes32 jobId = keccak256("j");
        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));
        vm.prank(owner);
        receipts.setCoordinator(coordinator, false);

        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        vm.prank(coordinator);
        receipts.revokeReceipt(jobId);
    }

    /// @dev Revocation is gated to the coordinator SET, not the issuing address: a
    ///      different authorized coordinator can revoke (the chosen design — issuer-only
    ///      would strand receipts when a coordinator key rotates).
    function test_revokeReceipt_anyAuthorizedCoordinatorCanRevoke() public {
        _arm();
        address coordinatorB = address(0xC0DE2);
        vm.prank(owner);
        receipts.setCoordinator(coordinatorB, true);

        bytes32 jobId = keccak256("cross-coordinator");
        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        vm.prank(coordinatorB);
        receipts.revokeReceipt(jobId);

        assertTrue(receipts.receiptRevoked(jobId));
        assertEq(receipts.receiptCount(), 0);
        assertEq(receipts.revokedCount(), 1);
    }

    /// @dev Revoke decrements only the STORED earner's slot (from issue time), never a
    ///      caller-supplied one: a second earner's count is untouched and the per-earner
    ///      partition of receiptCount is preserved.
    function test_revokeReceipt_decrementsOnlyTheStoredEarner() public {
        _arm();
        address earnerB = address(0xEA34);
        bytes32 jobA = keccak256("a");
        bytes32 jobB = keccak256("b");
        vm.startPrank(coordinator);
        receipts.issueReceipt(earner, jobA, 10, 0, bytes32(0), bytes32(0));
        receipts.issueReceipt(earnerB, jobB, 10, 0, bytes32(0), bytes32(0));
        receipts.revokeReceipt(jobA);
        vm.stopPrank();

        assertEq(receipts.receiptsByEarner(earner), 0);
        assertEq(receipts.receiptsByEarner(earnerB), 1);
        assertEq(receipts.receiptCount(), 1);
        assertEq(receipts.revokedCount(), 1);
        assertEq(
            receipts.receiptsByEarner(earner) + receipts.receiptsByEarner(earnerB), receipts.receiptCount()
        );
    }

    /// @dev receiptRevoked is set (and counters decremented) before the external
    ///      EAS.revoke, so a hostile EAS reentering revokeReceipt with the same jobId
    ///      mid-revoke is rejected by AlreadyRevoked: exactly one EAS.revoke, one
    ///      decrement, counters consistent. (Fails if the flag moved after the call.)
    function test_revokeReceipt_reentrantRevokeCannotDoubleDecrement() public {
        ReentrantRevokeEAS reentrantEas = new ReentrantRevokeEAS();
        RenderReceipts r = new RenderReceipts(address(reentrantEas), owner, address(region), RENDER_FEE_RATE);

        bytes32 jobId = keccak256("reentrant-revoke-job");
        reentrantEas.arm(r, jobId);

        vm.startPrank(owner);
        r.registerSchema(address(registry));
        r.setCoordinator(coordinator, true);
        r.setCoordinator(address(reentrantEas), true); // reentry's msg.sender is the EAS
        vm.stopPrank();

        vm.prank(coordinator);
        r.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));
        assertEq(r.receiptCount(), 1);

        vm.prank(coordinator);
        r.revokeReceipt(jobId);

        // The reentrant revoke hit the AlreadyRevoked guard (state flipped before the call).
        assertTrue(reentrantEas.reentered());
        assertTrue(reentrantEas.reentryReverted());
        assertEq(reentrantEas.reentryRevertSelector(), RenderReceipts.AlreadyRevoked.selector);

        // Exactly one outer revoke reached EAS; counters decremented exactly once.
        assertEq(reentrantEas.revokeCalls(), 1);
        assertTrue(r.receiptRevoked(jobId));
        assertEq(r.receiptCount(), 0);
        assertEq(r.receiptsByEarner(earner), 0);
        assertEq(r.revokedCount(), 1);
    }

    /// @dev Cross-function reentrancy: issueReceipt sets receiptIssued before its attest
    ///      but receiptUid only after, so a hostile EAS reentering revokeReceipt during
    ///      attest finds the receipt issued with a still-zero uid. revokeReceipt's uid==0
    ///      guard rejects it with NotIssued (self-contained, not relying on EAS), and the
    ///      outer issue completes with clean, consistent state. (Without the guard the
    ///      reentry would revert in EAS instead, with a different selector.)
    function test_issueReceipt_reentrantRevokeDuringAttestIsRejected() public {
        ReentrantIssueRevokeEAS reentrantEas = new ReentrantIssueRevokeEAS();
        RenderReceipts r = new RenderReceipts(address(reentrantEas), owner, address(region), RENDER_FEE_RATE);

        bytes32 jobId = keccak256("reentrant-revoke-during-attest");
        reentrantEas.arm(r, jobId);

        vm.startPrank(owner);
        r.registerSchema(address(registry));
        r.setCoordinator(coordinator, true);
        r.setCoordinator(address(reentrantEas), true); // reentry's msg.sender is the EAS
        vm.stopPrank();

        vm.prank(coordinator);
        bytes32 uid = r.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        // The reentrant revoke hit revokeReceipt's uid==0 guard (NotIssued), not EAS.
        assertTrue(reentrantEas.reentered());
        assertTrue(reentrantEas.reentryReverted());
        assertEq(reentrantEas.reentryRevertSelector(), RenderReceipts.NotIssued.selector);

        // The outer issue completed cleanly: one live receipt, not revoked, uid persisted.
        assertEq(r.receiptCount(), 1);
        assertEq(r.receiptsByEarner(earner), 1);
        assertEq(r.revokedCount(), 0);
        assertFalse(r.receiptRevoked(jobId));
        assertTrue(r.receiptIssued(jobId));
        assertEq(r.receiptUid(jobId), uid);
    }

    // --- render fee-share routing ---

    function test_issueReceipt_routesFeeToClaimedRegion() public {
        _arm();
        _fundCoordinator(1 ether);

        uint64 renderSeconds = 1000;
        uint256 expectedFee = RENDER_FEE_RATE * renderSeconds;
        bytes32 jobId = keccak256("fee-job");
        uint256 coordBefore = token.balanceOf(coordinator);

        vm.expectEmit(true, true, false, true);
        emit RenderFeeRouted(jobId, claimedRegion, expectedFee);

        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, renderSeconds, 0, bytes32(0), claimedRegionId);

        assertEq(region.accruedFees(claimedRegion), expectedFee);
        assertEq(token.balanceOf(coordinator), coordBefore - expectedFee);
        // RenderReceipts nets zero — the fee passed straight through, no standing balance
        // and no standing allowance survive the deposit.
        assertEq(token.balanceOf(address(receipts)), 0);
        assertEq(token.allowance(address(receipts), address(region)), 0);
        // The region holder can withdraw exactly the routed fee — end to end.
        vm.prank(regionHolder);
        region.claimFees(claimedRegion);
        assertEq(token.balanceOf(regionHolder), expectedFee);
    }

    /// @dev The fee is `renderFeeRate * renderSeconds` per receipt; multiple receipts
    ///      accrue additively into the region pool.
    function test_issueReceipt_feeScalesWithRenderSecondsAndAccrues() public {
        _arm();
        _fundCoordinator(1 ether);

        vm.startPrank(coordinator);
        receipts.issueReceipt(earner, keccak256("s1"), 100, 0, bytes32(0), claimedRegionId);
        receipts.issueReceipt(earner, keccak256("s2"), 250, 0, bytes32(0), claimedRegionId);
        vm.stopPrank();

        assertEq(region.accruedFees(claimedRegion), RENDER_FEE_RATE * (100 + 250));
    }

    /// @dev The fee routes to the RECEIPT'S OWN region (its regionId field), not a fixed
    ///      one: a receipt naming region B accrues there, leaving the setUp region at zero.
    function test_issueReceipt_routesToTheReceiptsOwnRegion() public {
        _arm();
        _fundCoordinator(1 ether);

        uint256 regionB = uint256(keccak256("region-B"));
        bytes32 regionBId = keccak256("region-B");
        address holderB = address(0x5E6105);
        token.transfer(holderB, STAKE);
        vm.startPrank(holderB);
        token.approve(address(region), STAKE);
        region.claim(regionB, STAKE);
        vm.stopPrank();

        vm.prank(coordinator);
        receipts.issueReceipt(earner, keccak256("jb"), 500, 0, bytes32(0), regionBId);

        assertEq(region.accruedFees(regionB), RENDER_FEE_RATE * 500);
        assertEq(region.accruedFees(claimedRegion), 0);
    }

    /// @dev THE divergence from ArtifactTemplate: an unclaimed/unknown region SKIPS the fee
    ///      route but the attestation still issues. A render receipt is the canonical proof
    ///      of validated work and must never be bricked by whether someone staked the
    ///      region (depositFees would revert UnknownRegion); the coordinator pays nothing.
    function test_issueReceipt_unknownRegionSkipsFeeButStillAttests() public {
        _arm();
        _fundCoordinator(1 ether);
        uint256 coordBefore = token.balanceOf(coordinator);

        bytes32 unknownRegion = keccak256("never-claimed");
        bytes32 jobId = keccak256("unknown-region-job");

        vm.prank(coordinator);
        bytes32 uid = receipts.issueReceipt(earner, jobId, 1000, 3, keccak256("out"), unknownRegion);

        assertTrue(receipts.receiptIssued(jobId));
        assertEq(receipts.receiptCount(), 1);
        assertEq(eas.attestCalls(), 1);
        assertEq(receipts.receiptUid(jobId), uid);
        // No fee routed: coordinator untouched, the unknown region accrued nothing.
        assertEq(token.balanceOf(coordinator), coordBefore);
        assertEq(region.accruedFees(uint256(unknownRegion)), 0);
    }

    /// @dev A zero fee (renderSeconds == 0) skips the route even for a claimed region —
    ///      mirrors ArtifactTemplate's rarity-0 skip. The receipt still issues.
    function test_issueReceipt_zeroRenderSecondsSkipsFeeEvenForClaimedRegion() public {
        _arm();
        _fundCoordinator(1 ether);
        uint256 coordBefore = token.balanceOf(coordinator);
        bytes32 jobId = keccak256("zero-seconds");

        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 0, 2, bytes32(0), claimedRegionId);

        assertTrue(receipts.receiptIssued(jobId));
        assertEq(receipts.receiptCount(), 1);
        assertEq(token.balanceOf(coordinator), coordBefore);
        assertEq(region.accruedFees(claimedRegion), 0);
    }

    /// @dev A claimed region with the coordinator unable to COVER the fee reverts the whole
    ///      issue (issues nothing) — full rollback, so an underfunded coordinator never
    ///      attests without paying the region. Insufficient balance variant.
    function test_issueReceipt_insufficientBalanceRevertsAndIssuesNothing() public {
        _arm();
        uint64 renderSeconds = 1000;
        uint256 fee = RENDER_FEE_RATE * renderSeconds;
        // Coordinator approves max but holds one wei less than the fee.
        token.transfer(coordinator, fee - 1);
        vm.prank(coordinator);
        token.approve(address(receipts), type(uint256).max);

        bytes32 jobId = keccak256("poor-coord");
        vm.expectRevert(
            abi.encodeWithSelector(IERC20Errors.ERC20InsufficientBalance.selector, coordinator, fee - 1, fee)
        );
        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, renderSeconds, 0, bytes32(0), claimedRegionId);

        // Full rollback: never issued, never attested, region accrued nothing.
        assertFalse(receipts.receiptIssued(jobId));
        assertEq(receipts.receiptCount(), 0);
        assertEq(eas.attestCalls(), 0);
        assertEq(region.accruedFees(claimedRegion), 0);
    }

    /// @dev Insufficient ALLOWANCE variant — the coordinator holds the funds but
    ///      under-approves RenderReceipts; same full rollback.
    function test_issueReceipt_insufficientAllowanceRevertsAndIssuesNothing() public {
        _arm();
        uint64 renderSeconds = 1000;
        uint256 fee = RENDER_FEE_RATE * renderSeconds;
        token.transfer(coordinator, 1 ether);
        vm.prank(coordinator);
        token.approve(address(receipts), fee - 1);

        bytes32 jobId = keccak256("under-approved");
        vm.expectRevert(
            abi.encodeWithSelector(
                IERC20Errors.ERC20InsufficientAllowance.selector, address(receipts), fee - 1, fee
            )
        );
        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, renderSeconds, 0, bytes32(0), claimedRegionId);

        assertFalse(receipts.receiptIssued(jobId));
        assertEq(receipts.receiptCount(), 0);
        assertEq(eas.attestCalls(), 0);
    }

    /// @dev CEI under a hostile fee token: the token reenters issueReceipt with the same
    ///      jobId during the fee pull, but the receipt fence (set before the route) rejects
    ///      it with DuplicateReceipt — exactly one receipt, one attest, one deposit. Proves
    ///      the fee route's external call cannot reopen the double-issue window.
    function test_issueReceipt_reentrantFeeTokenCannotDoubleIssue() public {
        ReentrantFeeToken evil = new ReentrantFeeToken(); // this test holds the supply
        MockEAS localEas = new MockEAS();
        RegionAuthority evilRegion = new RegionAuthority(address(evil), STAKE, owner);
        RenderReceipts r = new RenderReceipts(address(localEas), owner, address(evilRegion), RENDER_FEE_RATE);

        // Claim the region BEFORE arming, so claim's own transferFrom doesn't reenter.
        evil.transfer(regionHolder, STAKE);
        vm.startPrank(regionHolder);
        evil.approve(address(evilRegion), STAKE);
        evilRegion.claim(claimedRegion, STAKE);
        vm.stopPrank();

        vm.startPrank(owner);
        r.registerSchema(address(registry));
        r.setCoordinator(coordinator, true);
        r.setCoordinator(address(evil), true); // the reentry's msg.sender is the token
        vm.stopPrank();

        evil.transfer(coordinator, 1 ether);
        vm.prank(coordinator);
        evil.approve(address(r), type(uint256).max);

        bytes32 jobId = keccak256("reentrant-fee-job");
        evil.arm(r, earner, jobId);

        vm.prank(coordinator);
        r.issueReceipt(earner, jobId, 1, 0, bytes32(0), claimedRegionId);

        assertTrue(evil.reentered());
        assertTrue(evil.reentryReverted());
        assertEq(evil.reentryRevertSelector(), RenderReceipts.DuplicateReceipt.selector);

        // One receipt, one attestation, one deposit — the fence held under the hostile pull.
        assertEq(localEas.attestCalls(), 1);
        assertEq(r.receiptCount(), 1);
        assertEq(evilRegion.accruedFees(claimedRegion), RENDER_FEE_RATE * 1);
    }

    // --- batch issuance (issueReceipts) ---

    /// @dev Build one batch element; field order mirrors issueReceipt's positional args.
    function _req(address e, bytes32 jobId, uint64 rs, uint16 jk, bytes32 oh, bytes32 rid)
        internal
        pure
        returns (RenderReceipts.ReceiptRequest memory)
    {
        return RenderReceipts.ReceiptRequest(e, jobId, rs, jk, oh, rid);
    }

    /// @dev The uid the (mock) EAS derives for an element — keccak(schema, recipient, data),
    ///      identical to the single-attest path, so a batch-issued uid is predictable and
    ///      equals what issueReceipt would have minted for the same args.
    function _predictUid(address e, bytes32 jobId, uint64 rs, uint16 jk, bytes32 oh, bytes32 rid)
        internal
        view
        returns (bytes32)
    {
        bytes memory data = abi.encode(e, jobId, rs, jk, oh, rid);
        return keccak256(abi.encode(registry.FIXED_UID(), e, data));
    }

    function test_issueReceipts_happyPath_issuesAllViaOneMultiAttest() public {
        _arm();
        address earnerB = address(0xEA34);
        address earnerC = address(0xEA56);

        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](3);
        items[0] = _req(earner, keccak256("b-j1"), 10, 0, keccak256("o1"), bytes32(0));
        items[1] = _req(earnerB, keccak256("b-j2"), 20, 1, keccak256("o2"), bytes32(0));
        items[2] = _req(earner, keccak256("b-j3"), 30, 3, keccak256("o3"), bytes32(0));

        vm.prank(coordinator);
        bytes32[] memory uids = receipts.issueReceipts(items);

        // One multiAttest, zero single attests — the batch path was taken, not N issues.
        assertEq(eas.multiAttestCalls(), 1);
        assertEq(eas.attestCalls(), 0);
        assertEq(eas.lastBatchSize(), 3);

        // Every job issued, counted, and uid-mapped to ITS OWN attestation (FM2).
        assertEq(uids.length, 3);
        assertEq(receipts.receiptCount(), 3);
        for (uint256 i = 0; i < 3; i++) {
            bytes32 expected = _predictUid(
                items[i].earner,
                items[i].jobId,
                items[i].renderSeconds,
                items[i].jobKind,
                items[i].outputHash,
                items[i].regionId
            );
            assertEq(uids[i], expected);
            assertTrue(receipts.receiptIssued(items[i].jobId));
            assertEq(receipts.receiptUid(items[i].jobId), expected);
        }
        // Per-earner counts: earner has 2 (j1,j3), earnerB 1 (j2), earnerC none; they sum.
        assertEq(receipts.receiptsByEarner(earner), 2);
        assertEq(receipts.receiptsByEarner(earnerB), 1);
        assertEq(receipts.receiptsByEarner(earnerC), 0);
        // Distinct jobs -> distinct uids: no collision, no mis-map.
        assertTrue(uids[0] != uids[1] && uids[1] != uids[2] && uids[0] != uids[2]);
    }

    function test_issueReceipts_emitsReceiptIssuedPerElement() public {
        _arm();
        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](2);
        items[0] = _req(earner, keccak256("e1"), 10, 0, bytes32(0), bytes32(0));
        items[1] = _req(earner, keccak256("e2"), 20, 1, bytes32(0), bytes32(0));

        bytes32 uid0 = _predictUid(earner, items[0].jobId, 10, 0, bytes32(0), bytes32(0));
        bytes32 uid1 = _predictUid(earner, items[1].jobId, 20, 1, bytes32(0), bytes32(0));

        vm.expectEmit(true, true, true, true);
        emit ReceiptIssued(uid0, earner, items[0].jobId, 0, 10);
        vm.expectEmit(true, true, true, true);
        emit ReceiptIssued(uid1, earner, items[1].jobId, 1, 20);

        vm.prank(coordinator);
        receipts.issueReceipts(items);
    }

    /// @dev FM2: each jobId maps to ITS OWN multiAttest uid end-to-end — a revoke routed by
    ///      jobId reaches the right attestation. Reverse-order revoke so a mis-index can't
    ///      pass by coincidence; the per-earner decrements also track the stored earner.
    function test_issueReceipts_eachJobMapsToOwnUidAndIsRevocable() public {
        _arm();
        address earnerB = address(0xEA34);
        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](2);
        items[0] = _req(earner, keccak256("m-j1"), 11, 0, keccak256("mo1"), bytes32(0));
        items[1] = _req(earnerB, keccak256("m-j2"), 22, 2, keccak256("mo2"), bytes32(0));

        vm.prank(coordinator);
        receipts.issueReceipts(items);

        bytes32 uid0 = _predictUid(earner, items[0].jobId, 11, 0, keccak256("mo1"), bytes32(0));
        bytes32 uid1 = _predictUid(earnerB, items[1].jobId, 22, 2, keccak256("mo2"), bytes32(0));
        assertTrue(uid0 != uid1);
        assertEq(receipts.receiptUid(items[0].jobId), uid0);
        assertEq(receipts.receiptUid(items[1].jobId), uid1);

        vm.startPrank(coordinator);
        receipts.revokeReceipt(items[1].jobId);
        assertEq(eas.lastRevokedUid(), uid1);
        receipts.revokeReceipt(items[0].jobId);
        assertEq(eas.lastRevokedUid(), uid0);
        vm.stopPrank();

        assertEq(receipts.receiptCount(), 0);
        assertEq(receipts.revokedCount(), 2);
        assertEq(receipts.receiptsByEarner(earner), 0);
        assertEq(receipts.receiptsByEarner(earnerB), 0);
    }

    /// @dev FM1: a jobId repeated WITHIN one batch double-issues unless the fence is checked
    ///      per element. The second occurrence hits the flag the first set -> revert, whole
    ///      batch rolled back (reverts in phase 1, before any attestation).
    function test_issueReceipts_intraBatchDuplicateReverts() public {
        _arm();
        bytes32 dup = keccak256("dup-job");
        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](3);
        items[0] = _req(earner, keccak256("ok-1"), 10, 0, bytes32(0), bytes32(0));
        items[1] = _req(earner, dup, 10, 0, bytes32(0), bytes32(0));
        items[2] = _req(earner, dup, 10, 0, bytes32(0), bytes32(0));

        vm.prank(coordinator);
        vm.expectRevert(abi.encodeWithSelector(RenderReceipts.DuplicateReceipt.selector, dup));
        receipts.issueReceipts(items);

        assertEq(receipts.receiptCount(), 0);
        assertFalse(receipts.receiptIssued(keccak256("ok-1")));
        assertFalse(receipts.receiptIssued(dup));
        assertEq(eas.multiAttestCalls(), 0);
    }

    /// @dev FM1 (cross-call): the per-job fence also rejects a batch element whose job was
    ///      already issued by a prior single issueReceipt; the prior receipt is untouched.
    function test_issueReceipts_revertsOnJobAlreadyIssuedSingly() public {
        _arm();
        bytes32 jobId = keccak256("prior-job");
        vm.prank(coordinator);
        receipts.issueReceipt(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](2);
        items[0] = _req(earner, keccak256("new-job"), 10, 0, bytes32(0), bytes32(0));
        items[1] = _req(earner, jobId, 10, 0, bytes32(0), bytes32(0));

        vm.prank(coordinator);
        vm.expectRevert(abi.encodeWithSelector(RenderReceipts.DuplicateReceipt.selector, jobId));
        receipts.issueReceipts(items);

        assertEq(receipts.receiptCount(), 1);
        assertFalse(receipts.receiptIssued(keccak256("new-job")));
    }

    /// @dev FM3: one element's fee route failing (coordinator funded for only one fee, two
    ///      claimed-region elements) must roll the WHOLE batch back — never some jobs
    ///      attested-but-unpaid. Phase-4 routing runs after multiAttest, so this proves the
    ///      post-attestation revert unwinds the attestations too.
    function test_issueReceipts_partialFeeFailureRevertsWholeBatch() public {
        _arm();
        uint64 rs = 1000;
        uint256 oneFee = RENDER_FEE_RATE * rs;
        _fundCoordinator(oneFee);
        uint256 coordBefore = token.balanceOf(coordinator);

        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](2);
        items[0] = _req(earner, keccak256("fee-ok"), rs, 0, bytes32(0), claimedRegionId);
        items[1] = _req(earner, keccak256("fee-fail"), rs, 0, bytes32(0), claimedRegionId);

        vm.prank(coordinator);
        vm.expectRevert();
        receipts.issueReceipts(items);

        assertEq(receipts.receiptCount(), 0);
        assertFalse(receipts.receiptIssued(keccak256("fee-ok")));
        assertFalse(receipts.receiptIssued(keccak256("fee-fail")));
        assertEq(token.balanceOf(coordinator), coordBefore);
        assertEq(region.accruedFees(claimedRegion), 0);
    }

    /// @dev FM4: an empty batch reverts rather than touching EAS / emitting / bumping the
    ///      counter for zero work.
    function test_issueReceipts_emptyBatchReverts() public {
        _arm();
        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](0);
        vm.prank(coordinator);
        vm.expectRevert(RenderReceipts.EmptyBatch.selector);
        receipts.issueReceipts(items);
        assertEq(eas.multiAttestCalls(), 0);
        assertEq(receipts.receiptCount(), 0);
    }

    /// @dev Gas-envelope seam (the batch twin of MatchSettlement's MAX_FIELD bound): a batch
    ///      of EXACTLY MAX_BATCH issues — the cap is inclusive, so it never rejects a
    ///      legitimate max-size settle wave. Zero-region requests skip the fee route, so this
    ///      drives the full O(n) fence/attest/persist loop at the boundary. Reads the cap off
    ///      the contract so it tracks the constant in lock-step (FM4: a hardcoded 64 would
    ///      drift if the bound moved).
    function test_issueReceipts_atMaxBatchIssues() public {
        _arm();
        uint256 max = receipts.MAX_BATCH();
        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](max);
        for (uint256 i = 0; i < max; i++) {
            items[i] = _req(earner, keccak256(abi.encode("maxbatch", i)), 10, 0, bytes32(0), bytes32(0));
        }

        vm.prank(coordinator);
        bytes32[] memory uids = receipts.issueReceipts(items);

        assertEq(uids.length, max);
        assertEq(receipts.receiptCount(), max);
        assertEq(eas.multiAttestCalls(), 1);
        assertEq(eas.lastBatchSize(), max);
    }

    /// @dev FM1/FM4: a batch of MAX_BATCH + 1 reverts BatchTooLarge BEFORE any attestation —
    ///      the gas envelope is a contract guarantee, not a trust assumption, even from an
    ///      authorized-but-buggy coordinator. Rejected AT the boundary (not one under), and
    ///      nothing is attested or counted (the whole call reverts in the pre-loop guard).
    function test_issueReceipts_aboveMaxBatchReverts() public {
        _arm();
        uint256 over = receipts.MAX_BATCH() + 1;
        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](over);
        for (uint256 i = 0; i < over; i++) {
            items[i] = _req(earner, keccak256(abi.encode("overbatch", i)), 10, 0, bytes32(0), bytes32(0));
        }

        vm.prank(coordinator);
        vm.expectRevert(RenderReceipts.BatchTooLarge.selector);
        receipts.issueReceipts(items);

        assertEq(eas.multiAttestCalls(), 0);
        assertEq(receipts.receiptCount(), 0);
    }

    function test_issueReceipts_revertsNotAuthorized() public {
        _arm();
        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](1);
        items[0] = _req(earner, keccak256("na"), 10, 0, bytes32(0), bytes32(0));
        vm.prank(stranger);
        vm.expectRevert(RenderReceipts.NotAuthorized.selector);
        receipts.issueReceipts(items);
    }

    function test_issueReceipts_revertsSchemaNotSet() public {
        // Authorize the coordinator but leave the schema unregistered: auth is checked first,
        // so this reaches the schema gate.
        vm.prank(owner);
        receipts.setCoordinator(coordinator, true);
        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](1);
        items[0] = _req(earner, keccak256("ss"), 10, 0, bytes32(0), bytes32(0));
        vm.prank(coordinator);
        vm.expectRevert(RenderReceipts.SchemaNotSet.selector);
        receipts.issueReceipts(items);
    }

    /// @dev The per-element fee route + skip policy matches issueReceipt: a claimed region
    ///      routes renderFeeRate*renderSeconds, a zero-fee or unclaimed-region element skips
    ///      (still attested), and the contract nets zero token across the batch.
    function test_issueReceipts_routesFeesPerElementAndSkips() public {
        _arm();
        _fundCoordinator(1 ether);
        uint256 coordBefore = token.balanceOf(coordinator);
        bytes32 unknownRegion = keccak256("never-claimed");

        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](4);
        items[0] = _req(earner, keccak256("p1"), 100, 0, bytes32(0), claimedRegionId);
        items[1] = _req(earner, keccak256("p2"), 0, 0, bytes32(0), claimedRegionId);
        items[2] = _req(earner, keccak256("p3"), 250, 0, bytes32(0), claimedRegionId);
        items[3] = _req(earner, keccak256("p4"), 500, 0, bytes32(0), unknownRegion);

        vm.prank(coordinator);
        receipts.issueReceipts(items);

        uint256 expectedRouted = RENDER_FEE_RATE * (100 + 250);
        assertEq(region.accruedFees(claimedRegion), expectedRouted);
        assertEq(token.balanceOf(coordinator), coordBefore - expectedRouted);
        assertEq(receipts.receiptCount(), 4);
        // No standing balance/allowance survives the batch (every route nets to zero).
        assertEq(token.balanceOf(address(receipts)), 0);
        assertEq(token.allowance(address(receipts), address(region)), 0);
    }

    /// @dev A one-element batch is byte-for-byte equivalent to a single issueReceipt: same
    ///      uid, same persisted state, same forwarded EAS payload.
    function test_issueReceipts_singleElementMatchesIssueReceipt() public {
        _arm();
        bytes32 jobId = keccak256("equiv-job");
        uint64 rs = 1234;
        uint16 jk = 3;
        bytes32 oh = keccak256("equiv-out");

        RenderReceipts.ReceiptRequest[] memory items = new RenderReceipts.ReceiptRequest[](1);
        items[0] = _req(earner, jobId, rs, jk, oh, bytes32(0));

        vm.prank(coordinator);
        bytes32[] memory uids = receipts.issueReceipts(items);

        assertEq(uids[0], _predictUid(earner, jobId, rs, jk, oh, bytes32(0)));
        assertEq(receipts.receiptUid(jobId), uids[0]);
        assertEq(receipts.receiptCount(), 1);
        assertEq(receipts.receiptsByEarner(earner), 1);
        assertEq(eas.lastData(), abi.encode(earner, jobId, rs, jk, oh, bytes32(0)));
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
