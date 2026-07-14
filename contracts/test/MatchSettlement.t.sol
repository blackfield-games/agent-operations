// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {MatchSettlement, IEAS, ISchemaRegistry} from "../src/MatchSettlement.sol";
import {AgentRegistry} from "../src/AgentRegistry.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 10_000_000 ether);
    }
}

/// @dev Records the last attest so a test can decode its payload, and derives a
///      deterministic uid from (schema, recipient, data) — distinct per settled result.
contract MockEAS is IEAS {
    uint256 public attestCalls;
    bytes32 public lastSchema;
    address public lastRecipient;
    bool public lastRevocable;
    bytes public lastData;
    uint256 public revokeCalls;
    bytes32 public lastRevokedUid;
    bytes32 public lastRevokedSchema;

    function attest(IEAS.AttestationRequest calldata request) external payable returns (bytes32) {
        attestCalls++;
        lastSchema = request.schema;
        lastRecipient = request.data.recipient;
        lastRevocable = request.data.revocable;
        lastData = request.data.data;
        return keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
    }

    function revoke(IEAS.RevocationRequest calldata request) external payable {
        revokeCalls++;
        lastRevokedUid = request.data.uid;
        lastRevokedSchema = request.schema;
    }
}

/// @dev Returns a distinct uid per schema string so the 1v1 and field schemas never collide.
contract MockSchemaRegistry is ISchemaRegistry {
    function register(string calldata schema, address, bool) external pure returns (bytes32) {
        return keccak256(bytes(schema));
    }
}

/// @dev On its FIRST attest, re-enters settle() for the same match — proving the Settled
///      fence (written before the attest) rejects the reentrant settle: no double-settle,
///      no second attestation. Mirrors RenderReceipts' ReentrantEAS.
contract ReentrantEAS is IEAS {
    MatchSettlement public target;
    bytes32 public reMatchId;
    address public reWinner;
    bytes32 public reHash;
    uint256 public attestCalls;
    bool public reentered;
    bool public reentryReverted;
    bytes4 public reentryRevertSelector;

    function arm(MatchSettlement target_, bytes32 matchId_, address winner_, bytes32 hash_) external {
        target = target_;
        reMatchId = matchId_;
        reWinner = winner_;
        reHash = hash_;
    }

    function attest(IEAS.AttestationRequest calldata request) external payable returns (bytes32) {
        attestCalls++;
        if (!reentered) {
            reentered = true;
            try target.settle(reMatchId, reWinner, reHash) {}
            catch (bytes memory err) {
                reentryReverted = true;
                reentryRevertSelector = bytes4(err);
            }
        }
        return keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
    }

    function revoke(IEAS.RevocationRequest calldata) external payable {}
}

/// @dev On its FIRST revoke, re-enters revokeAttestation for the same match — proving the
///      matchAttestationRevoked fence (set before the external EAS.revoke, CEI) rejects the
///      reentrant revoke: exactly one revoke, one EAS.revoke call. `attest` is benign so the
///      settle that mints the attestation runs normally.
contract ReentrantRevokeEAS is IEAS {
    MatchSettlement public target;
    bytes32 public reMatchId;
    uint256 public revokeCalls;
    bool public reentered;
    bool public reentryReverted;
    bytes4 public reentryRevertSelector;

    function arm(MatchSettlement target_, bytes32 matchId_) external {
        target = target_;
        reMatchId = matchId_;
    }

    function attest(IEAS.AttestationRequest calldata request) external payable returns (bytes32) {
        return keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
    }

    function revoke(IEAS.RevocationRequest calldata) external payable {
        revokeCalls++;
        if (!reentered) {
            reentered = true;
            try target.revokeAttestation(reMatchId) {}
            catch (bytes memory err) {
                reentryReverted = true;
                reentryRevertSelector = bytes4(err);
            }
        }
    }
}

contract MatchSettlementTest is Test {
    MatchSettlement settlement;
    AgentRegistry registry;
    MockToken token;
    MockEAS eas;

    address owner = address(0xA11CE);
    address attester = address(0xA77E57E5);
    address alice = address(0xA1);
    address bob = address(0xB0B);
    address carol = address(0xCA201); // never registered

    uint256 constant STAKE = 100 ether;
    uint256 constant REP_DELTA = 10 ether;
    uint256 constant RATING_CAP = 50 ether;
    bytes32 constant MATCH = bytes32("match-1");
    bytes32 constant HASH = bytes32("replay-digest");

    event MatchOpened(bytes32 indexed matchId, address indexed agentA, address indexed agentB, uint256 stake);
    event MatchFunded(bytes32 indexed matchId, address indexed agent, uint256 stake);
    event MatchReclaimed(bytes32 indexed matchId, address indexed agent, uint256 stake);
    event MatchSettled(
        bytes32 indexed matchId,
        address indexed winner,
        address indexed loser,
        bytes32 replayHash,
        uint256 payout
    );
    event MatchDrawn(
        bytes32 indexed matchId, address indexed agentA, address indexed agentB, bytes32 replayHash
    );
    event MatchCancelled(bytes32 indexed matchId, uint256 refundA, uint256 refundB);
    event MatchExpired(bytes32 indexed matchId, uint256 refundA, uint256 refundB);
    event FieldMatchOpened(bytes32 indexed matchId, address[] agents, uint256 stake);
    event FieldMatchFunded(bytes32 indexed matchId, address indexed agent, uint256 stake);
    event FieldMatchReclaimed(bytes32 indexed matchId, address indexed agent, uint256 stake);
    event FieldMatchCancelled(bytes32 indexed matchId, uint256 totalRefunded);
    event FieldMatchExpired(bytes32 indexed matchId, uint256 totalRefunded);
    event AttesterSet(address indexed attester, bool authorized);
    event ReputationDeltaSet(uint256 reputationDelta);
    event MaxRatingDeltaSet(uint256 maxRatingDelta);
    event SettleWindowSet(uint64 settleWindow);
    event SchemaRegistered(bytes32 indexed matchUid, bytes32 indexed fieldUid);
    event MatchAttested(bytes32 indexed matchId, bytes32 indexed uid);
    event MatchAttestationRevoked(bytes32 indexed matchId, bytes32 indexed uid);

    function setUp() public {
        token = new MockToken();
        registry = new AgentRegistry(address(token), 0, owner);
        eas = new MockEAS();
        settlement = new MatchSettlement(address(eas), address(registry), owner, REP_DELTA);

        vm.startPrank(owner);
        registry.setReputationWriter(address(settlement), true);
        settlement.setAttester(attester, true);
        settlement.registerSchema(address(new MockSchemaRegistry()));
        vm.stopPrank();

        // Two live agent identities; carol stays unregistered.
        _registerAndFund(alice, bytes32("alice-bot"));
        _registerAndFund(bob, bytes32("bob-bot"));
    }

    function _registerAndFund(address who, bytes32 handle) internal {
        assertTrue(token.transfer(who, 10_000 ether));
        vm.startPrank(who);
        token.approve(address(registry), type(uint256).max);
        token.approve(address(settlement), type(uint256).max);
        registry.register(handle, 0);
        vm.stopPrank();
    }

    function _open(bytes32 id, uint256 stake) internal {
        vm.prank(attester);
        settlement.openMatch(id, alice, bob, stake);
    }

    function _fundBoth(bytes32 id) internal {
        vm.prank(alice);
        settlement.fund(id);
        vm.prank(bob);
        settlement.fund(id);
    }

    function _status(bytes32 id) internal view returns (MatchSettlement.Status) {
        (,,,,,,, MatchSettlement.Status s,) = settlement.matches(id);
        return s;
    }

    function _deadline(bytes32 id) internal view returns (uint64) {
        (,,,,,,,, uint64 d) = settlement.matches(id);
        return d;
    }

    // --- construction ---

    function test_constructor() public view {
        assertEq(address(settlement.registry()), address(registry));
        assertEq(address(settlement.token()), address(token));
        assertEq(settlement.reputationDelta(), REP_DELTA);
        assertEq(settlement.owner(), owner);
    }

    function test_constructor_bindsTokenToRegistry() public view {
        assertEq(address(settlement.token()), address(registry.TOKEN()));
    }

    function test_constructor_revertsZeroRegistry() public {
        vm.expectRevert(MatchSettlement.ZeroRegistry.selector);
        new MatchSettlement(address(eas), address(0), owner, REP_DELTA);
    }

    function test_constructor_revertsZeroReputationDelta() public {
        vm.expectRevert(MatchSettlement.ZeroReputationDelta.selector);
        new MatchSettlement(address(eas), address(registry), owner, 0);
    }

    function test_constructor_revertsReputationDeltaTooLarge() public {
        vm.expectRevert(MatchSettlement.ReputationDeltaTooLarge.selector);
        new MatchSettlement(address(eas), address(registry), owner, uint256(type(int256).max) + 1);
    }

    function test_constructor_revertsZeroToken() public {
        AgentRegistry zeroTokenRegistry = new AgentRegistry(address(0), 0, owner);
        vm.expectRevert(MatchSettlement.ZeroToken.selector);
        new MatchSettlement(address(eas), address(zeroTokenRegistry), owner, REP_DELTA);
    }

    // --- registerSchema (the single deploy step that arms every settle path; onlyOwner) ---

    /// @dev registerSchema wires the two DISTINCT schemas (1v1 vs field) — the only signal an
    ///      off-chain indexer gets telling it which uid decodes which attestation shape. Assert the
    ///      emit's two indexed topics AND both storage slots on a FRESH (unwired) settlement so the
    ///      bytes32(0)->set arming is observable and a fieldSchemaUid=matchUid desync is caught (the
    ///      return tuple alone would not catch it). MockSchemaRegistry.register returns
    ///      keccak256(bytes(schema)), so both uids are deterministic from the verbatim src literals.
    function test_registerSchema_onlyOwnerAndEmits() public {
        bytes32 expMatch = keccak256(
            bytes(
                "bytes32 matchId, address agentA, address agentB, address winner, bytes32 replayHash, int256 deltaA"
            )
        );
        bytes32 expField = keccak256(
            bytes("bytes32 matchId, address[] agents, int256[] deltas, bytes32 replayHash, uint256 pot")
        );

        MatchSettlement fresh = new MatchSettlement(address(eas), address(registry), owner, REP_DELTA);
        assertEq(fresh.schemaUid(), bytes32(0), "fresh settlement starts unarmed");
        assertEq(fresh.fieldSchemaUid(), bytes32(0), "fresh settlement starts unarmed");

        // Construct the registry before the prank so prank applies to registerSchema, not the CREATE.
        MockSchemaRegistry schemaReg = new MockSchemaRegistry();
        vm.expectEmit(true, true, false, false);
        emit SchemaRegistered(expMatch, expField);
        vm.prank(owner);
        (bytes32 matchUid, bytes32 fieldUid) = fresh.registerSchema(address(schemaReg));

        assertEq(matchUid, expMatch, "returned 1v1 uid");
        assertEq(fieldUid, expField, "returned field uid");
        assertTrue(matchUid != fieldUid, "the 1v1 and field schemas must be distinct uids");
        assertEq(fresh.schemaUid(), expMatch, "stored 1v1 schema uid");
        assertEq(fresh.fieldSchemaUid(), expField, "stored field schema uid");

        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, alice));
        vm.prank(alice);
        fresh.registerSchema(address(schemaReg));
    }

    // --- openMatch (FM2: attester gating; idempotency: single-use id) ---

    function test_openMatch_storesAndEmits() public {
        vm.expectEmit(true, true, true, true);
        emit MatchOpened(MATCH, alice, bob, STAKE);
        vm.prank(attester);
        settlement.openMatch(MATCH, alice, bob, STAKE);

        (
            address agentA,
            address agentB,
            uint256 stake,
            bytes32 replayHash,
            address winner,
            bool aFunded,
            bool bFunded,
            MatchSettlement.Status status,
            uint64 deadline
        ) = settlement.matches(MATCH);
        assertEq(agentA, alice);
        assertEq(agentB, bob);
        assertEq(stake, STAKE);
        assertEq(replayHash, bytes32(0));
        assertEq(winner, address(0));
        assertFalse(aFunded);
        assertFalse(bFunded);
        assertTrue(status == MatchSettlement.Status.Open);
        assertEq(deadline, uint64(block.timestamp) + settlement.settleWindow());
    }

    /// @dev FM2: only an authorized attester may open (and thus later settle) a match.
    function test_openMatch_revertsNotAttester() public {
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        vm.prank(alice);
        settlement.openMatch(MATCH, alice, bob, STAKE);
    }

    function test_openMatch_revertsSameAgent() public {
        vm.expectRevert(MatchSettlement.SameAgent.selector);
        vm.prank(attester);
        settlement.openMatch(MATCH, alice, alice, STAKE);
    }

    /// @dev FM3: escrow only ever involves registered participants — opening against
    ///      an unregistered address is rejected up front.
    function test_openMatch_revertsAgentNotRegistered() public {
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.AgentNotRegistered.selector, carol));
        vm.prank(attester);
        settlement.openMatch(MATCH, alice, carol, STAKE);
    }

    function test_openMatch_revertsMatchExists() public {
        _open(MATCH, STAKE);
        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.openMatch(MATCH, alice, bob, STAKE);
    }

    function test_openMatch_zeroStakeAllowed() public {
        _open(MATCH, 0);
        assertTrue(_status(MATCH) == MatchSettlement.Status.Open);
    }

    // --- fund (FM3: only participants, only own seat, only once) ---

    function test_fund_pullsStakeFromBothSeats() public {
        _open(MATCH, STAKE);

        uint256 aBefore = token.balanceOf(alice);
        vm.expectEmit(true, true, false, true);
        emit MatchFunded(MATCH, alice, STAKE);
        vm.prank(alice);
        settlement.fund(MATCH);
        assertEq(token.balanceOf(alice), aBefore - STAKE);
        assertEq(token.balanceOf(address(settlement)), STAKE);

        vm.prank(bob);
        settlement.fund(MATCH);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE);

        (,,,,, bool aFunded, bool bFunded,,) = settlement.matches(MATCH);
        assertTrue(aFunded);
        assertTrue(bFunded);
    }

    function test_fund_revertsNotParticipant() public {
        _open(MATCH, STAKE);
        assertTrue(token.transfer(carol, STAKE));
        vm.prank(carol);
        token.approve(address(settlement), type(uint256).max);

        vm.expectRevert(MatchSettlement.NotParticipant.selector);
        vm.prank(carol);
        settlement.fund(MATCH);
    }

    function test_fund_revertsAlreadyFunded() public {
        _open(MATCH, STAKE);
        vm.prank(alice);
        settlement.fund(MATCH);
        vm.expectRevert(MatchSettlement.AlreadyFunded.selector);
        vm.prank(alice);
        settlement.fund(MATCH);
    }

    function test_fund_revertsNoWager() public {
        _open(MATCH, 0);
        vm.expectRevert(MatchSettlement.NoWager.selector);
        vm.prank(alice);
        settlement.fund(MATCH);
    }

    function test_fund_revertsMatchNotOpen() public {
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(alice);
        settlement.fund(MATCH);
    }

    function test_fund_revertsWhenNotApproved() public {
        _open(MATCH, STAKE);
        vm.prank(alice);
        token.approve(address(settlement), 0);
        vm.expectRevert();
        vm.prank(alice);
        settlement.fund(MATCH);
    }

    // --- reclaim (no-show self-rescue) ---

    function test_reclaim_beforeOpponentFunds() public {
        _open(MATCH, STAKE);
        uint256 before = token.balanceOf(alice);
        vm.prank(alice);
        settlement.fund(MATCH);

        vm.expectEmit(true, true, false, true);
        emit MatchReclaimed(MATCH, alice, STAKE);
        vm.prank(alice);
        settlement.reclaim(MATCH);

        assertEq(token.balanceOf(alice), before, "stake fully returned");
        assertEq(token.balanceOf(address(settlement)), 0);
        (,,,,, bool aFunded,,,) = settlement.matches(MATCH);
        assertFalse(aFunded, "seat marked unfunded again");
        // Match is still open and re-fundable.
        vm.prank(alice);
        settlement.fund(MATCH);
        assertEq(token.balanceOf(address(settlement)), STAKE);
    }

    /// @dev Once both seats are funded the match is live; a participant can no longer
    ///      unilaterally pull out — only the attester resolves it.
    function test_reclaim_revertsOpponentFunded() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.expectRevert(MatchSettlement.OpponentFunded.selector);
        vm.prank(alice);
        settlement.reclaim(MATCH);
    }

    function test_reclaim_revertsNotFunded() public {
        _open(MATCH, STAKE);
        vm.expectRevert(MatchSettlement.NotFunded.selector);
        vm.prank(alice);
        settlement.reclaim(MATCH);
    }

    function test_reclaim_revertsNotParticipant() public {
        _open(MATCH, STAKE);
        vm.expectRevert(MatchSettlement.NotParticipant.selector);
        vm.prank(carol);
        settlement.reclaim(MATCH);
    }

    // --- settle (FM1 idempotency, FM2 gating, FM3 escrow integrity) ---

    function test_settle_paysWinnerAndRecordsReputation() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);

        uint256 aBefore = token.balanceOf(alice);
        vm.expectEmit(true, true, true, true);
        emit MatchSettled(MATCH, alice, bob, HASH, 2 * STAKE);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);

        assertEq(token.balanceOf(alice), aBefore + 2 * STAKE, "winner takes the pot");
        assertEq(token.balanceOf(address(settlement)), 0, "escrow emptied");
        assertEq(registry.reputationOf(alice), int256(REP_DELTA));
        assertEq(registry.reputationOf(bob), -int256(REP_DELTA));

        (,,, bytes32 replayHash, address winner,,, MatchSettlement.Status status,) = settlement.matches(MATCH);
        assertEq(replayHash, HASH);
        assertEq(winner, alice);
        assertTrue(status == MatchSettlement.Status.Settled);

        (,, uint64 aMatches,,,) = registry.agents(alice);
        (,, uint64 bMatches,,,) = registry.agents(bob);
        assertEq(aMatches, 1);
        assertEq(bMatches, 1);
    }

    function test_settle_winnerCanBeB() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        uint256 bBefore = token.balanceOf(bob);
        vm.prank(attester);
        settlement.settle(MATCH, bob, HASH);
        assertEq(token.balanceOf(bob), bBefore + 2 * STAKE);
        assertEq(registry.reputationOf(bob), int256(REP_DELTA));
        assertEq(registry.reputationOf(alice), -int256(REP_DELTA));
    }

    function test_settle_zeroStakeRecordsReputationOnly() public {
        _open(MATCH, 0);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);
        assertEq(token.balanceOf(address(settlement)), 0);
        assertEq(registry.reputationOf(alice), int256(REP_DELTA));
        assertEq(registry.reputationOf(bob), -int256(REP_DELTA));
    }

    function test_settle_revertsNotAttester() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        vm.prank(alice);
        settlement.settle(MATCH, alice, HASH);
    }

    /// @dev FM2: an attester cannot name an arbitrary (non-participant) winner to
    ///      drain the pot to an outsider.
    function test_settle_revertsInvalidWinner() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.expectRevert(MatchSettlement.InvalidWinner.selector);
        vm.prank(attester);
        settlement.settle(MATCH, carol, HASH);
    }

    function test_settle_revertsZeroReplayHash() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.expectRevert(MatchSettlement.ZeroReplayHash.selector);
        vm.prank(attester);
        settlement.settle(MATCH, alice, bytes32(0));
    }

    /// @dev FM3: a wager match cannot be settled (paid out) until BOTH seats funded.
    function test_settle_revertsNotFullyFunded_onlyOne() public {
        _open(MATCH, STAKE);
        vm.prank(alice);
        settlement.fund(MATCH);
        vm.expectRevert(MatchSettlement.NotFullyFunded.selector);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);
    }

    function test_settle_revertsNotFullyFunded_neither() public {
        _open(MATCH, STAKE);
        vm.expectRevert(MatchSettlement.NotFullyFunded.selector);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);
    }

    /// @dev FM1: a replayed settlement is a no-op (reverts MatchNotOpen) — no double
    ///      pay and no double reputation count.
    function test_settle_idempotentOnReplay() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);

        uint256 aAfter = token.balanceOf(alice);
        int256 repAfter = registry.reputationOf(alice);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);

        assertEq(token.balanceOf(alice), aAfter, "no second payout");
        assertEq(registry.reputationOf(alice), repAfter, "no second reputation move");
    }

    /// @dev Escrow liveness does not hard-depend on the reputation-writer grant: if the
    ///      settlement is deauthorized, settle reverts and pays out nothing (rolled
    ///      back), but funds remain recoverable via cancelMatch.
    function test_settle_revertsWhenNotReputationWriter() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(owner);
        registry.setReputationWriter(address(settlement), false);

        vm.expectRevert(AgentRegistry.NotReputationWriter.selector);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE, "escrow untouched on revert");

        vm.prank(attester);
        settlement.cancelMatch(MATCH);
        assertEq(token.balanceOf(address(settlement)), 0, "funds recovered via cancel");
    }

    // --- settleDraw ---

    function test_settleDraw_refundsBothNoReputationChange() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);

        vm.expectEmit(true, true, true, true);
        emit MatchDrawn(MATCH, alice, bob, HASH);
        vm.prank(attester);
        settlement.settleDraw(MATCH, HASH);

        assertEq(token.balanceOf(alice), aBefore + STAKE, "alice refunded own stake");
        assertEq(token.balanceOf(bob), bBefore + STAKE, "bob refunded own stake");
        assertEq(token.balanceOf(address(settlement)), 0);
        assertEq(registry.reputationOf(alice), 0, "draw moves no reputation");
        assertEq(registry.reputationOf(bob), 0);

        (,,, bytes32 replayHash,,,, MatchSettlement.Status status,) = settlement.matches(MATCH);
        assertEq(replayHash, HASH, "draw still commits the replay");
        assertTrue(status == MatchSettlement.Status.Settled);
        (,, uint64 aMatches,,,) = registry.agents(alice);
        assertEq(aMatches, 1, "a draw still counts as a played match");
    }

    function test_settleDraw_zeroStake() public {
        _open(MATCH, 0);
        vm.prank(attester);
        settlement.settleDraw(MATCH, HASH);
        assertEq(registry.reputationOf(alice), 0);
        (,, uint64 aMatches,,,) = registry.agents(alice);
        assertEq(aMatches, 1);
    }

    function test_settleDraw_revertsNotAttester() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        vm.prank(bob);
        settlement.settleDraw(MATCH, HASH);
    }

    function test_settleDraw_revertsZeroReplayHash() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.expectRevert(MatchSettlement.ZeroReplayHash.selector);
        vm.prank(attester);
        settlement.settleDraw(MATCH, bytes32(0));
    }

    function test_settleDraw_revertsNotFullyFunded() public {
        _open(MATCH, STAKE);
        vm.prank(alice);
        settlement.fund(MATCH);
        vm.expectRevert(MatchSettlement.NotFullyFunded.selector);
        vm.prank(attester);
        settlement.settleDraw(MATCH, HASH);
    }

    function test_settleDraw_idempotent() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settleDraw(MATCH, HASH);
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settleDraw(MATCH, HASH);
    }

    /// @dev _applyDraw records reputation for both seats before refunding stake, so a
    ///      deauthorized writer must revert the whole draw — the last settle path without
    ///      a NotReputationWriter propagation test (settle/settleField/settleFieldWager
    ///      each have one). settleDrawRanked shares _applyDraw, so this covers both draw
    ///      entries. Assert status stays Open and escrow is untouched: the write precedes
    ///      the refund, so a CEI slip that refunded first would drain escrow on the revert.
    function test_settleDraw_revertsWhenNotReputationWriter() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(owner);
        registry.setReputationWriter(address(settlement), false);

        vm.expectRevert(AgentRegistry.NotReputationWriter.selector);
        vm.prank(attester);
        settlement.settleDraw(MATCH, HASH);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE, "escrow untouched on revert");
        assertTrue(_status(MATCH) == MatchSettlement.Status.Open, "match remains Open");

        vm.prank(attester);
        settlement.cancelMatch(MATCH);
        assertEq(token.balanceOf(address(settlement)), 0, "funds recovered via cancel");
    }

    // --- cancelMatch ---

    function test_cancelMatch_refundsBothFundedSeats() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);

        vm.expectEmit(true, false, false, true);
        emit MatchCancelled(MATCH, STAKE, STAKE);
        vm.prank(attester);
        settlement.cancelMatch(MATCH);

        assertEq(token.balanceOf(alice), aBefore + STAKE);
        assertEq(token.balanceOf(bob), bBefore + STAKE);
        assertEq(token.balanceOf(address(settlement)), 0);
        assertEq(registry.reputationOf(alice), 0, "cancel records no reputation");
        assertTrue(_status(MATCH) == MatchSettlement.Status.Cancelled);
    }

    function test_cancelMatch_refundsOnlyFundedSeat() public {
        _open(MATCH, STAKE);
        vm.prank(alice);
        settlement.fund(MATCH);
        uint256 aBefore = token.balanceOf(alice);

        vm.expectEmit(true, false, false, true);
        emit MatchCancelled(MATCH, STAKE, 0);
        vm.prank(attester);
        settlement.cancelMatch(MATCH);
        assertEq(token.balanceOf(alice), aBefore + STAKE);
        assertEq(token.balanceOf(address(settlement)), 0);
    }

    function test_cancelMatch_noFundsWhenNoneFunded() public {
        _open(MATCH, STAKE);
        vm.prank(attester);
        settlement.cancelMatch(MATCH);
        assertEq(token.balanceOf(address(settlement)), 0);
        assertTrue(_status(MATCH) == MatchSettlement.Status.Cancelled);
    }

    function test_cancelMatch_revertsNotAttester() public {
        _open(MATCH, STAKE);
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        vm.prank(alice);
        settlement.cancelMatch(MATCH);
    }

    /// @dev FM1: a cancelled match is terminal — neither re-settleable nor re-openable
    ///      under its id, and a second cancel reverts (no double refund).
    function test_cancelMatch_isTerminal() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.cancelMatch(MATCH);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.cancelMatch(MATCH);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);

        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.openMatch(MATCH, alice, bob, STAKE);
    }

    // --- refundExpired (deadline self-refund for a vanished attester) ---

    /// @dev Happy path: a both-funded match the attester never resolves can be voided
    ///      by anyone once past its deadline — both seats refunded, no reputation, no
    ///      replay, and the distinct Expired terminal state.
    function test_refundExpired_refundsBothFundedSeatsAfterDeadline() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);

        vm.warp(_deadline(MATCH));
        vm.expectEmit(true, false, false, true);
        emit MatchExpired(MATCH, STAKE, STAKE);
        settlement.refundExpired(MATCH);

        assertEq(token.balanceOf(alice), aBefore + STAKE, "alice refunded own stake");
        assertEq(token.balanceOf(bob), bBefore + STAKE, "bob refunded own stake");
        assertEq(token.balanceOf(address(settlement)), 0, "escrow emptied");
        assertTrue(_status(MATCH) == MatchSettlement.Status.Expired);

        (,,, bytes32 replayHash, address winner,,,,) = settlement.matches(MATCH);
        assertEq(replayHash, bytes32(0), "no replay committed");
        assertEq(winner, address(0), "no winner named");
    }

    /// @dev Callable by ANYONE (no caller gate) — the refunds still go to the
    ///      participants, never the caller, so there is no griefing or theft vector.
    function test_refundExpired_permissionlessRefundsParticipantsNotCaller() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);
        uint256 carolBefore = token.balanceOf(carol);

        vm.warp(_deadline(MATCH));
        vm.prank(carol); // an unrelated third party triggers the void
        settlement.refundExpired(MATCH);

        assertEq(token.balanceOf(alice), aBefore + STAKE);
        assertEq(token.balanceOf(bob), bBefore + STAKE);
        assertEq(token.balanceOf(carol), carolBefore, "caller gains nothing");
    }

    function test_refundExpired_refundsOnlyFundedSeat() public {
        _open(MATCH, STAKE);
        vm.prank(alice);
        settlement.fund(MATCH);
        uint256 aBefore = token.balanceOf(alice);

        vm.warp(_deadline(MATCH));
        vm.expectEmit(true, false, false, true);
        emit MatchExpired(MATCH, STAKE, 0);
        settlement.refundExpired(MATCH);
        assertEq(token.balanceOf(alice), aBefore + STAKE);
        assertEq(token.balanceOf(address(settlement)), 0);
    }

    /// @dev A zero-stake (reputation-only) match still expires cleanly — no token
    ///      movement, no reputation, just the terminal flip.
    function test_refundExpired_zeroStakeMatch() public {
        _open(MATCH, 0);
        vm.warp(_deadline(MATCH));
        settlement.refundExpired(MATCH);
        assertTrue(_status(MATCH) == MatchSettlement.Status.Expired);
        assertEq(registry.reputationOf(alice), 0);
    }

    /// @dev FM2: the deadline must not refund a match still in progress — before it
    ///      passes, refundExpired reverts and the escrow is untouched.
    function test_refundExpired_revertsBeforeDeadline() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.warp(uint256(_deadline(MATCH)) - 1);
        vm.expectRevert(MatchSettlement.NotExpired.selector);
        settlement.refundExpired(MATCH);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE, "escrow held while in-window");
        assertTrue(_status(MATCH) == MatchSettlement.Status.Open);
    }

    /// @dev Boundary: the refund is permitted at exactly the deadline (the guard is
    ///      `block.timestamp < deadline`, so `==` passes).
    function test_refundExpired_succeedsAtExactDeadline() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.warp(_deadline(MATCH));
        settlement.refundExpired(MATCH);
        assertTrue(_status(MATCH) == MatchSettlement.Status.Expired);
    }

    function test_refundExpired_revertsMatchNotOpen_whenNeverOpened() public {
        vm.warp(block.timestamp + 365 days);
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        settlement.refundExpired(MATCH);
    }

    /// @dev FM1: a match the attester DID settle cannot then be expired — even past the
    ///      deadline the Settled fence wins, so the refund can't double-resolve a paid
    ///      match. The settle path also has no deadline guard, so a slow-but-alive
    ///      attester can still settle after the window opens (it just races the refund).
    function test_refundExpired_revertsAfterSettle() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);

        vm.warp(_deadline(MATCH));
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        settlement.refundExpired(MATCH);
        assertTrue(_status(MATCH) == MatchSettlement.Status.Settled, "stays settled");
    }

    /// @dev FM1 (the converse race): once the deadline passes either resolution can land
    ///      first, and whichever flips the status wins — a settle after a refund reverts
    ///      MatchNotOpen, so there is never a double payout/refund.
    function test_refundExpired_thenSettleReverts() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.warp(_deadline(MATCH));
        settlement.refundExpired(MATCH);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);
    }

    /// @dev FM1: an expired match is terminal — not re-refundable, not settleable, not
    ///      cancellable, not re-openable under its id (no double refund).
    function test_refundExpired_isTerminal() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.warp(_deadline(MATCH));
        settlement.refundExpired(MATCH);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        settlement.refundExpired(MATCH);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.cancelMatch(MATCH);

        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.openMatch(MATCH, alice, bob, STAKE);
    }

    /// @dev FM3: stalling past the deadline cannot turn a match into a result an agent
    ///      could use to dodge a loss — the expired match records NO reputation and
    ///      counts for NEITHER agent, exactly like cancelMatch (a void, never a win).
    function test_refundExpired_recordsNoReputationOrMatchCount() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.warp(_deadline(MATCH));
        settlement.refundExpired(MATCH);

        assertEq(registry.reputationOf(alice), 0, "no reputation moved");
        assertEq(registry.reputationOf(bob), 0);
        (,, uint64 aMatches,,,) = registry.agents(alice);
        (,, uint64 bMatches,,,) = registry.agents(bob);
        assertEq(aMatches, 0, "expired match counts for neither agent");
        assertEq(bMatches, 0);
    }

    // --- setSettleWindow ---

    function test_constructor_defaultSettleWindow() public view {
        assertEq(settlement.settleWindow(), settlement.DEFAULT_SETTLE_WINDOW());
    }

    function test_setSettleWindow_onlyOwnerAndEmits() public {
        vm.expectEmit(false, false, false, true);
        emit SettleWindowSet(2 days);
        vm.prank(owner);
        settlement.setSettleWindow(2 days);
        assertEq(settlement.settleWindow(), 2 days);

        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, alice));
        vm.prank(alice);
        settlement.setSettleWindow(3 days);
    }

    function test_setSettleWindow_revertsBelowMin() public {
        uint64 belowMin = settlement.MIN_SETTLE_WINDOW() - 1;
        vm.expectRevert(MatchSettlement.SettleWindowOutOfRange.selector);
        vm.prank(owner);
        settlement.setSettleWindow(belowMin);
    }

    function test_setSettleWindow_revertsAboveMax() public {
        uint64 aboveMax = settlement.MAX_SETTLE_WINDOW() + 1;
        vm.expectRevert(MatchSettlement.SettleWindowOutOfRange.selector);
        vm.prank(owner);
        settlement.setSettleWindow(aboveMax);
    }

    /// @dev FM2: the window is generously bounded — both inclusive endpoints are
    ///      accepted, so the floor can't be set so short an in-progress match expires.
    function test_setSettleWindow_acceptsBoundaries() public {
        vm.startPrank(owner);
        settlement.setSettleWindow(settlement.MIN_SETTLE_WINDOW());
        assertEq(settlement.settleWindow(), settlement.MIN_SETTLE_WINDOW());
        settlement.setSettleWindow(settlement.MAX_SETTLE_WINDOW());
        assertEq(settlement.settleWindow(), settlement.MAX_SETTLE_WINDOW());
        vm.stopPrank();
    }

    /// @dev FM2: retuning the window only affects matches opened AFTER the change — an
    ///      already-open match's deadline was frozen at its open and never moves.
    function test_setSettleWindow_onlyAffectsFutureMatches() public {
        _open(MATCH, STAKE);
        uint64 frozen = _deadline(MATCH);

        uint64 maxWindow = settlement.MAX_SETTLE_WINDOW();
        vm.prank(owner);
        settlement.setSettleWindow(maxWindow);
        assertEq(_deadline(MATCH), frozen, "open match deadline unchanged by retune");

        bytes32 later = bytes32("match-2");
        _open(later, STAKE);
        assertEq(_deadline(later), uint64(block.timestamp) + maxWindow, "new match uses the retuned window");
    }

    // --- admin ---

    function test_setAttester_onlyOwnerAndEmits() public {
        vm.expectEmit(true, false, false, true);
        emit AttesterSet(carol, true);
        vm.prank(owner);
        settlement.setAttester(carol, true);
        assertTrue(settlement.resultAttesters(carol));

        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, alice));
        vm.prank(alice);
        settlement.setAttester(bob, true);
    }

    function test_setReputationDelta_onlyOwnerAndEmits() public {
        vm.expectEmit(false, false, false, true);
        emit ReputationDeltaSet(42 ether);
        vm.prank(owner);
        settlement.setReputationDelta(42 ether);
        assertEq(settlement.reputationDelta(), 42 ether);

        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, alice));
        vm.prank(alice);
        settlement.setReputationDelta(1);
    }

    function test_setReputationDelta_revertsZero() public {
        vm.expectRevert(MatchSettlement.ZeroReputationDelta.selector);
        vm.prank(owner);
        settlement.setReputationDelta(0);
    }

    function test_setReputationDelta_revertsTooLarge() public {
        vm.expectRevert(MatchSettlement.ReputationDeltaTooLarge.selector);
        vm.prank(owner);
        settlement.setReputationDelta(uint256(type(int256).max) + 1);
    }

    function test_setReputationDelta_appliesUpdatedMagnitude() public {
        vm.prank(owner);
        settlement.setReputationDelta(42 ether);
        _open(MATCH, 0);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);
        assertEq(registry.reputationOf(alice), 42 ether);
        assertEq(registry.reputationOf(bob), -42 ether);
    }

    // --- variable-delta settle (settleRanked / settleDrawRanked + maxRatingDelta) ---
    //
    // The arena rating curve computes a skill-scaled Elo delta off-chain (a favoured win
    // earns less, an upset more; a draw moves the favourite down) and the match service
    // submits it. These pin the on-chain consumer: the owner-set cap that bounds the
    // attester's per-match magnitude power, the +d/-d zero-sum split the contract owns,
    // and the sign rules (a decisive winner never moves down; a draw moves either way).

    function _enableVariable(uint256 cap) internal {
        vm.prank(owner);
        settlement.setMaxRatingDelta(cap);
    }

    function test_setMaxRatingDelta_onlyOwnerAndEmits() public {
        assertEq(settlement.maxRatingDelta(), 0, "variable path off by default");
        vm.expectEmit(false, false, false, true);
        emit MaxRatingDeltaSet(RATING_CAP);
        vm.prank(owner);
        settlement.setMaxRatingDelta(RATING_CAP);
        assertEq(settlement.maxRatingDelta(), RATING_CAP);

        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, alice));
        vm.prank(alice);
        settlement.setMaxRatingDelta(1);
    }

    function test_setMaxRatingDelta_allowsZeroToDisable() public {
        _enableVariable(RATING_CAP);
        _enableVariable(0); // owner can turn the variable path back off
        assertEq(settlement.maxRatingDelta(), 0);
        _open(MATCH, 0);
        vm.expectRevert(MatchSettlement.VariableSettleDisabled.selector);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, 1 ether);
    }

    function test_setMaxRatingDelta_revertsTooLarge() public {
        vm.expectRevert(MatchSettlement.RatingDeltaTooLarge.selector);
        vm.prank(owner);
        settlement.setMaxRatingDelta(uint256(type(int256).max) + 1);
    }

    function test_settleRanked_paysWinnerAndRecordsVariableReputation() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        int256 delta = 25 ether;

        uint256 aBefore = token.balanceOf(alice);
        vm.expectEmit(true, true, true, true);
        emit MatchSettled(MATCH, alice, bob, HASH, 2 * STAKE);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, delta);

        assertEq(token.balanceOf(alice), aBefore + 2 * STAKE, "winner takes the pot");
        assertEq(token.balanceOf(address(settlement)), 0, "escrow emptied");
        assertEq(registry.reputationOf(alice), delta, "winner gains the variable delta");
        assertEq(registry.reputationOf(bob), -delta, "loser loses exactly the negation");
        assertTrue(_status(MATCH) == MatchSettlement.Status.Settled);
        (,, uint64 aMatches,,,) = registry.agents(alice);
        assertEq(aMatches, 1);
    }

    function test_settleRanked_winnerCanBeB() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        int256 delta = 18 ether;
        vm.prank(attester);
        settlement.settleRanked(MATCH, bob, HASH, delta);
        assertEq(registry.reputationOf(bob), delta);
        assertEq(registry.reputationOf(alice), -delta);
    }

    /// @dev A heavily-favoured winner's Elo gain can round to 0 (K*(1-E) truncates); the
    ///      match must still settle — pot paid, match counted — not revert. A d>0-only
    ///      misread would wrongly reject this legitimate win.
    function test_settleRanked_zeroDeltaStillSettles() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        uint256 aBefore = token.balanceOf(alice);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, 0);
        assertEq(token.balanceOf(alice), aBefore + 2 * STAKE, "zero-gain win still takes the pot");
        assertEq(registry.reputationOf(alice), 0, "no reputation moved on a rounded-to-zero win");
        assertEq(registry.reputationOf(bob), 0);
        assertTrue(_status(MATCH) == MatchSettlement.Status.Settled);
        (,, uint64 aMatches,,,) = registry.agents(alice);
        assertEq(aMatches, 1, "a zero-delta win still counts");
    }

    function test_settleRanked_atCapBoundary() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, 0);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, int256(RATING_CAP));
        assertEq(registry.reputationOf(alice), int256(RATING_CAP));
        assertEq(registry.reputationOf(bob), -int256(RATING_CAP));
    }

    function test_settleRanked_revertsDisabledByDefault() public {
        _open(MATCH, 0);
        vm.expectRevert(MatchSettlement.VariableSettleDisabled.selector);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, 1 ether);
    }

    /// @dev A decisive winner must never LOSE reputation on a win — a negative delta is
    ///      a malformed result, rejected before any state change.
    function test_settleRanked_revertsNegativeDelta() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, 0);
        vm.expectRevert(MatchSettlement.NegativeWinnerDelta.selector);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, -1);
    }

    function test_settleRanked_revertsTooLarge() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, 0);
        vm.expectRevert(MatchSettlement.RatingDeltaTooLarge.selector);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, int256(RATING_CAP) + 1);
    }

    function test_settleRanked_revertsNotAttester() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        vm.prank(alice);
        settlement.settleRanked(MATCH, alice, HASH, 1 ether);
    }

    /// @dev The variable path shares _applyDecisive with the fixed settle, so it inherits
    ///      the escrow + winner-validity + replay-hash guards once the magnitude checks
    ///      pass.
    function test_settleRanked_inheritsDecisiveGuards() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        vm.prank(alice);
        settlement.fund(MATCH);
        vm.expectRevert(MatchSettlement.NotFullyFunded.selector);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, 5 ether);

        vm.prank(bob);
        settlement.fund(MATCH);
        vm.expectRevert(MatchSettlement.InvalidWinner.selector);
        vm.prank(attester);
        settlement.settleRanked(MATCH, carol, HASH, 5 ether);

        vm.expectRevert(MatchSettlement.ZeroReplayHash.selector);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, bytes32(0), 5 ether);
    }

    /// @dev FM1 across the variable surface: a settled match is single-shot regardless of
    ///      which settle form resolved it — a settleRanked replay AND a fixed settle both
    ///      revert MatchNotOpen.
    function test_settleRanked_idempotentAcrossForms() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, 7 ether);
        int256 repAfter = registry.reputationOf(alice);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settleRanked(MATCH, alice, HASH, 7 ether);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);

        assertEq(registry.reputationOf(alice), repAfter, "no second reputation move");
    }

    /// @dev The signature Elo behaviour the fixed settleDraw cannot express: a draw between
    ///      a favourite (agentA) and an underdog moves the favourite DOWN. agentA takes the
    ///      negative delta, agentB its positive negation — still zero-sum, stakes refunded.
    function test_settleDrawRanked_favouriteMovesDown() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);
        int256 deltaA = -12 ether; // agentA was favoured; a draw costs it standing

        vm.expectEmit(true, true, true, true);
        emit MatchDrawn(MATCH, alice, bob, HASH);
        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, deltaA);

        assertEq(token.balanceOf(alice), aBefore + STAKE, "alice refunded own stake");
        assertEq(token.balanceOf(bob), bBefore + STAKE, "bob refunded own stake");
        assertEq(registry.reputationOf(alice), deltaA, "favourite moves down on a draw");
        assertEq(registry.reputationOf(bob), -deltaA, "underdog moves up by the negation");
        assertTrue(_status(MATCH) == MatchSettlement.Status.Settled);
    }

    function test_settleDrawRanked_underdogMovesUp() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, 0);
        int256 deltaA = 9 ether; // agentA the underdog; a draw earns it standing
        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, deltaA);
        assertEq(registry.reputationOf(alice), deltaA);
        assertEq(registry.reputationOf(bob), -deltaA);
    }

    /// @dev An even pairing draws to no movement — identical to the fixed settleDraw.
    function test_settleDrawRanked_zeroDeltaIsEvenDraw() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, 0);
        assertEq(registry.reputationOf(alice), 0);
        assertEq(registry.reputationOf(bob), 0);
        assertEq(token.balanceOf(address(settlement)), 0, "both refunded");
    }

    function test_settleDrawRanked_atCapBoundaryBothSigns() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, 0);
        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, int256(RATING_CAP));
        assertEq(registry.reputationOf(alice), int256(RATING_CAP));
        assertEq(registry.reputationOf(bob), -int256(RATING_CAP));

        bytes32 id2 = bytes32("match-2");
        _open(id2, 0);
        vm.prank(attester);
        settlement.settleDrawRanked(id2, HASH, -int256(RATING_CAP));
        assertEq(registry.reputationOf(alice), 0, "+cap then -cap nets to zero");
        assertEq(registry.reputationOf(bob), 0);
        assertTrue(_status(id2) == MatchSettlement.Status.Settled);
    }

    /// @dev Even a zero delta reverts while the cap is 0 — the explicit disabled guard,
    ///      not merely the magnitude bound, gates the path off by default.
    function test_settleDrawRanked_revertsDisabledByDefault() public {
        _open(MATCH, 0);
        vm.expectRevert(MatchSettlement.VariableSettleDisabled.selector);
        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, 0);
    }

    function test_settleDrawRanked_revertsTooLargePositive() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, 0);
        vm.expectRevert(MatchSettlement.RatingDeltaTooLarge.selector);
        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, int256(RATING_CAP) + 1);
    }

    function test_settleDrawRanked_revertsTooLargeNegative() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, 0);
        vm.expectRevert(MatchSettlement.RatingDeltaTooLarge.selector);
        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, -int256(RATING_CAP) - 1);
    }

    function test_settleDrawRanked_idempotent() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, 0);
        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, 3 ether);
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, 3 ether);
    }

    function test_settleDrawRanked_revertsNotAttester() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, 0);
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        vm.prank(bob);
        settlement.settleDrawRanked(MATCH, HASH, 0);
    }

    // --- settleField (multi-seat FFA/3+ reputation-only settle of ranked_field_delta) ---
    //
    // The arena rating curve generalizes the 1v1 Elo delta to an N-seat placement field
    // (arena-core ranked_field_delta): a zero-sum per-seat vector the match service submits.
    // These pin the on-chain consumer: the +Σ=0 conservation the contract enforces, the
    // distinct-registered-agent integrity, the per-seat magnitude cap, and that a field
    // settle reuses the shared Status fence (one settlement record per matchId).

    address dave = address(0xDA1E);
    address eve = address(0xE7E);

    event MatchFieldSettled(bytes32 indexed matchId, bytes32 replayHash, uint256 seats);

    function _registerAgent(address who, bytes32 handle) internal {
        assertTrue(token.transfer(who, 10_000 ether));
        vm.startPrank(who);
        token.approve(address(registry), type(uint256).max);
        registry.register(handle, 0);
        vm.stopPrank();
    }

    function _field(address a0, address a1, address a2) internal pure returns (address[] memory ag) {
        ag = new address[](3);
        ag[0] = a0;
        ag[1] = a1;
        ag[2] = a2;
    }

    function _deltas(int256 d0, int256 d1, int256 d2) internal pure returns (int256[] memory ds) {
        ds = new int256[](3);
        ds[0] = d0;
        ds[1] = d1;
        ds[2] = d2;
    }

    /// @dev FM1 conservation: a balanced 3-seat field settles each seat to EXACTLY its own
    ///      signed delta, the reputations sum to 0 (nothing minted/burned), each match is
    ///      counted, and the seat-count event fires. Distinct non-zero deltas make a
    ///      mis-indexed or dropped write observable.
    function test_settleField_balancedVectorConservesReputation() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether); // Σ = 0

        vm.expectEmit(true, false, false, true);
        emit MatchFieldSettled(MATCH, HASH, 3);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        assertEq(registry.reputationOf(alice), 30 ether, "seat 0 gains its delta");
        assertEq(registry.reputationOf(bob), -10 ether, "seat 1 takes its delta");
        assertEq(registry.reputationOf(dave), -20 ether, "seat 2 takes its delta");
        assertEq(
            registry.reputationOf(alice) + registry.reputationOf(bob) + registry.reputationOf(dave),
            0,
            "the field is zero-sum on-chain"
        );
        (,, uint64 aMatches,,,) = registry.agents(alice);
        (,, uint64 dMatches,,,) = registry.agents(dave);
        assertEq(aMatches, 1, "alice's match counted");
        assertEq(dMatches, 1, "dave's match counted");
        assertTrue(_status(MATCH) == MatchSettlement.Status.Settled, "field settle flips the shared fence");
    }

    /// @dev FM1 revert: a vector that does NOT sum to zero would mint/burn reputation, so it
    ///      reverts NonZeroSum and writes nothing — the matchId stays free for a corrected
    ///      result (the registration is never burned by a bad report).
    function test_settleField_revertsNonZeroSum() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -10 ether); // Σ = +10 ether

        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.NonZeroSum.selector, int256(10 ether)));
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        assertEq(registry.reputationOf(alice), 0, "no seat moved");
        assertEq(registry.reputationOf(bob), 0);
        assertEq(registry.reputationOf(dave), 0);
        assertTrue(
            _status(MATCH) == MatchSettlement.Status.None, "the matchId is still free after a bad report"
        );
    }

    /// @dev FM1 n=2: the field path reduces to the 1v1 +d/-d symmetry at two seats — a
    ///      reputation-only 2-seat field with no escrow settles zero-sum.
    function test_settleField_twoSeatFieldIsZeroSumSymmetry() public {
        _enableVariable(RATING_CAP);
        address[] memory ag = new address[](2);
        ag[0] = alice;
        ag[1] = bob;
        int256[] memory ds = new int256[](2);
        ds[0] = 14 ether;
        ds[1] = -14 ether;
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);
        assertEq(registry.reputationOf(alice), 14 ether);
        assertEq(registry.reputationOf(bob), -14 ether);
    }

    /// @dev FM1 (fuzzed revert): over ARBITRARY signed 3-seat vectors within the cap, any
    ///      vector that does not sum to exactly 0 reverts NonZeroSum(sum) and moves no
    ///      reputation — the conservation law holds across the fuzz domain, not just the
    ///      hand-picked points above. Each seat is bounded within ±cap so only the sum
    ///      check (never the per-seat cap) can trip, isolating the zero-sum invariant.
    function testFuzz_settleField_revertsAnyNonZeroSum(int256 d0, int256 d1, int256 d2) public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        int256 cap = int256(RATING_CAP);
        d0 = bound(d0, -cap, cap);
        d1 = bound(d1, -cap, cap);
        d2 = bound(d2, -cap, cap);
        int256 sum = d0 + d1 + d2;
        vm.assume(sum != 0);

        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(d0, d1, d2);
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.NonZeroSum.selector, sum));
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        assertEq(registry.reputationOf(alice), 0, "no seat moved on a non-zero-sum field");
        assertEq(registry.reputationOf(bob), 0);
        assertEq(registry.reputationOf(dave), 0);
        assertTrue(_status(MATCH) == MatchSettlement.Status.None, "matchId stays free after a bad report");
    }

    /// @dev FM1 (fuzzed conservation): a balanced vector built from two fuzzed seats (the
    ///      third absorbs the negation of their sum) settles each seat to EXACTLY its own
    ///      delta and sums to 0 on-chain — conservation pinned over the fuzz domain, not a
    ///      single fixed triple. The two free seats are bounded to ±cap/2 so the absorbing
    ///      seat stays within ±cap (the cap is never the reason this path settles).
    function testFuzz_settleField_balancedVectorConserves(int256 d0, int256 d1) public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        int256 half = int256(RATING_CAP) / 2;
        d0 = bound(d0, -half, half);
        d1 = bound(d1, -half, half);
        int256 d2 = -(d0 + d1);

        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(d0, d1, d2);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        assertEq(registry.reputationOf(alice), d0, "seat 0 settled to its own delta");
        assertEq(registry.reputationOf(bob), d1, "seat 1 settled to its own delta");
        assertEq(registry.reputationOf(dave), d2, "seat 2 absorbs the negation");
        assertEq(
            registry.reputationOf(alice) + registry.reputationOf(bob) + registry.reputationOf(dave),
            0,
            "the fuzzed field is zero-sum on-chain"
        );
    }

    /// @dev FM2: agents/deltas length mismatch reverts before any write (a read past the
    ///      shorter vector would mis-settle).
    function test_settleField_revertsLengthMismatch() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = new int256[](2); // shorter than agents
        ds[0] = 5 ether;
        ds[1] = -5 ether;
        vm.expectRevert(MatchSettlement.LengthMismatch.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);
    }

    /// @dev FM2: an empty field and a one-seat field both revert FieldTooSmall — a field is
    ///      >= 2 seats (a singleton is degenerate; sum-zero would force its sole delta to 0).
    function test_settleField_revertsEmptyAndSingleton() public {
        _enableVariable(RATING_CAP);
        address[] memory empty = new address[](0);
        int256[] memory emptyD = new int256[](0);
        vm.expectRevert(MatchSettlement.FieldTooSmall.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, empty, emptyD, HASH);

        address[] memory one = new address[](1);
        one[0] = alice;
        int256[] memory oneD = new int256[](1);
        oneD[0] = 0;
        vm.expectRevert(MatchSettlement.FieldTooSmall.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, one, oneD, HASH);
    }

    /// @dev FM2 bound: a roster over MAX_FIELD reverts FieldTooLarge — the hard gas bound on
    ///      the O(n²) scan + n external writes, so a single settle can never approach the
    ///      block limit even from a buggy/compromised attester. MAX_FIELD itself settles.
    function test_settleField_revertsOverMaxField() public {
        _enableVariable(RATING_CAP);
        uint256 max = settlement.MAX_FIELD();

        // MAX_FIELD + 1 seats: reverts before touching the registry (all-zero deltas, so only
        // the size bound can trip). Agents need not be registered — the bound is checked first.
        uint256 over = max + 1;
        address[] memory big = new address[](over);
        int256[] memory bigD = new int256[](over);
        for (uint256 i = 0; i < over; i++) {
            big[i] = address(uint160(0x1000 + i));
        }
        vm.expectRevert(MatchSettlement.FieldTooLarge.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, big, bigD, HASH);

        // Exactly MAX_FIELD registered seats with a zero-sum vector settles (the bound is
        // inclusive): seat 0 takes +RATING_CAP, seat 1 −RATING_CAP, the rest 0.
        address[] memory full = new address[](max);
        int256[] memory fullD = new int256[](max);
        for (uint256 i = 0; i < max; i++) {
            address a = address(uint160(0x2000 + i));
            full[i] = a;
            _registerAgent(a, bytes32(uint256(0x5000 + i)));
        }
        fullD[0] = int256(RATING_CAP);
        fullD[1] = -int256(RATING_CAP);
        vm.prank(attester);
        settlement.settleField(MATCH, full, fullD, HASH);
        assertEq(registry.reputationOf(full[0]), int256(RATING_CAP), "MAX_FIELD settles inclusively");
        assertEq(registry.reputationOf(full[1]), -int256(RATING_CAP));
        assertTrue(_status(MATCH) == MatchSettlement.Status.Settled);
    }

    /// @dev FM2: a duplicated agent reverts DuplicateAgent (it would double-write that agent's
    ///      reputation and break the field's zero-sum intent) — caught before any write even
    ///      though the vector sums to zero.
    function test_settleField_revertsDuplicateAgent() public {
        _enableVariable(RATING_CAP);
        address[] memory ag = _field(alice, bob, alice); // alice twice
        int256[] memory ds = _deltas(10 ether, -20 ether, 10 ether); // Σ = 0

        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.DuplicateAgent.selector, alice));
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        assertEq(registry.reputationOf(alice), 0, "no partial write on a duplicate");
        assertEq(registry.reputationOf(bob), 0);
        assertTrue(_status(MATCH) == MatchSettlement.Status.None);
    }

    /// @dev FM2: an unregistered seat reverts AgentNotRegistered (carol never registered) —
    ///      the same liveness guard as the 1v1 paths, applied per seat.
    function test_settleField_revertsUnregisteredAgent() public {
        _enableVariable(RATING_CAP);
        address[] memory ag = _field(alice, bob, carol); // carol unregistered
        int256[] memory ds = _deltas(10 ether, -5 ether, -5 ether); // Σ = 0
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.AgentNotRegistered.selector, carol));
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);
    }

    /// @dev The complement of the never-registered case: a seat that WAS registered but
    ///      deregistered AFTER the match is still scored. A match settles once it is played
    ///      and reputation persists across deregistration (recordMatchResult gates on
    ///      registeredAt != 0), so a losing agent cannot dodge its negative delta by leaving —
    ///      and because the field's deltas must sum to zero the attester cannot drop the leaver
    ///      either, so tolerating it is what keeps the whole field settleable. Mirrors the 1v1
    ///      and settleFieldWager paths, which check registration at open, not at settle.
    function test_settleField_scoresDeregisteredSeat() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether); // dave (seat 2) is the loser

        vm.prank(dave);
        registry.deregister(); // dave bails after playing
        assertFalse(registry.isRegistered(dave), "dave is no longer active");

        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        assertEq(
            registry.reputationOf(dave), -20 ether, "the deregistered loser is still scored (loss not dodged)"
        );
        assertEq(registry.reputationOf(alice), 20 ether, "the winner still gains");
        (,, uint64 daveMatches,,,) = registry.agents(dave);
        assertEq(daveMatches, 1, "the settled match is counted against the deregistered identity");
        assertTrue(
            _status(MATCH) == MatchSettlement.Status.Settled, "the field settles despite a deregistered seat"
        );
    }

    /// @dev FM3 cap: a per-seat delta over the owner-set ceiling reverts RatingDeltaTooLarge,
    ///      either sign — the attester scales standing only within the cap, same as the 1v1
    ///      variable path. The over-cap seat is balanced so ONLY the cap (not the sum) trips.
    function test_settleField_revertsDeltaOverCap() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        int256 over = int256(RATING_CAP) + 1;
        address[] memory ag = _field(alice, bob, dave);

        int256[] memory hi = _deltas(over, -over, 0); // Σ = 0 but seat 0 over +cap
        vm.expectRevert(MatchSettlement.RatingDeltaTooLarge.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, hi, HASH);

        int256[] memory lo = _deltas(-over, over, 0); // Σ = 0 but seat 0 below -cap
        vm.expectRevert(MatchSettlement.RatingDeltaTooLarge.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, lo, HASH);
    }

    /// @dev FM3 boundary: deltas exactly at ±cap (and a 0 seat) settle — the cap is inclusive.
    function test_settleField_atCapBoundarySettles() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(int256(RATING_CAP), -int256(RATING_CAP), 0); // Σ = 0, at ±cap
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);
        assertEq(registry.reputationOf(alice), int256(RATING_CAP));
        assertEq(registry.reputationOf(bob), -int256(RATING_CAP));
        assertEq(registry.reputationOf(dave), 0);
    }

    /// @dev FM3 idempotency: a re-settle of the same matchId reverts on the Status fence and
    ///      writes no second time — the reputations hold at their first-settle values.
    function test_settleField_idempotentOnReSettle() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        assertEq(registry.reputationOf(alice), 30 ether, "no second write");
        (,, uint64 aMatches,,,) = registry.agents(alice);
        assertEq(aMatches, 1, "still one counted match");
    }

    /// @dev FM3 disabled-by-default: while maxRatingDelta is 0 the field path reverts
    ///      VariableSettleDisabled — even a perfectly balanced vector — so the contract is
    ///      byte-identical to fixed-only until the owner opts in.
    function test_settleField_revertsDisabledByDefault() public {
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);
        vm.expectRevert(MatchSettlement.VariableSettleDisabled.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);
    }

    /// @dev FM4 authority: a non-attester caller reverts NotAttester BEFORE any state change
    ///      (the matchId stays free) — only the authorized match service names results.
    function test_settleField_revertsNotAttester() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        vm.prank(bob);
        settlement.settleField(MATCH, ag, ds, HASH);
        assertTrue(
            _status(MATCH) == MatchSettlement.Status.None, "no state change from an unauthorized caller"
        );
    }

    /// @dev FM4 writer authority: if AgentRegistry has not authorized this contract as a
    ///      reputation writer the per-seat record reverts NotReputationWriter — the same
    ///      dependency the 1v1 paths have. CEI means the fence has already flipped, but the
    ///      revert rolls the whole settle back, so nothing is half-written.
    function test_settleField_revertsWhenNotReputationWriter() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        vm.prank(owner);
        registry.setReputationWriter(address(settlement), false); // revoke

        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);
        vm.expectRevert(AgentRegistry.NotReputationWriter.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        assertTrue(_status(MATCH) == MatchSettlement.Status.None, "the failed settle rolled the fence back");
        assertEq(registry.reputationOf(alice), 0, "nothing half-written");
    }

    /// @dev Cross-path exclusivity (the reused fence): a field-settled matchId can never be
    ///      reopened as a 1v1, and an opened 1v1 matchId can never be field-settled — one
    ///      settlement record per matchId, both directions.
    function test_settleField_oneRecordPerMatchId_bothDirections() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);

        // field-settle, then a 1v1 openMatch on the same id is refused.
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);
        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.openMatch(MATCH, alice, bob, 0);

        // the other direction: an opened 1v1 id cannot be field-settled.
        bytes32 id2 = bytes32("match-2");
        _open(id2, 0);
        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.settleField(id2, ag, ds, HASH);
    }

    /// @dev A zero replay hash reverts — a field settle commits a real replay digest, same as
    ///      the decisive/draw paths.
    function test_settleField_revertsZeroReplayHash() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);
        vm.expectRevert(MatchSettlement.ZeroReplayHash.selector);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, bytes32(0));
    }

    // --- field-wager escrow (N-seat open / fund / reclaim / cancel / expire) ---
    //
    // The N-seat analog of the 1v1 wager: openFieldMatch escrows an arbitrary 2..=MAX_FIELD
    // roster, each agent funds its OWN seat (fundField) into a uint256 funded bitmap, and
    // until the sibling payout settle lands the only resolutions are reclaimField (no-show
    // self-rescue), cancelFieldMatch (attester void) and refundFieldExpired (deadline). These
    // pin per-seat fund/refund integrity, roster validation, the shared id fence, and CEI.

    bytes32 constant FIELD = bytes32("field-1");

    function _roster3() internal returns (address[] memory ag) {
        _registerAndFund(dave, "dave-bot"); // approves the settlement so dave can fund its seat
        ag = _field(alice, bob, dave);
    }

    function _openField(bytes32 id, address[] memory ag, uint256 stake) internal {
        vm.prank(attester);
        settlement.openFieldMatch(id, ag, stake);
    }

    function _fundField(bytes32 id, address who) internal {
        vm.prank(who);
        settlement.fundField(id);
    }

    function _fieldStatus(bytes32 id) internal view returns (MatchSettlement.Status s) {
        (,, s,,) = settlement.fieldMatches(id);
    }

    function _fundedBits(bytes32 id) internal view returns (uint256 bits) {
        (, bits,,,) = settlement.fieldMatches(id);
    }

    // --- openFieldMatch: roster integrity + shared fence (FM2) ---

    function test_openFieldMatch_storesRosterAndEmits() public {
        address[] memory ag = _roster3();

        vm.expectEmit(true, false, false, true);
        emit FieldMatchOpened(FIELD, ag, STAKE);
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, ag, STAKE);

        address[] memory stored = settlement.fieldRoster(FIELD);
        assertEq(stored.length, 3, "roster length");
        assertEq(stored[0], alice);
        assertEq(stored[1], bob);
        assertEq(stored[2], dave);
        (uint256 stake, uint256 bits, MatchSettlement.Status status, uint64 deadline,) =
            settlement.fieldMatches(FIELD);
        assertEq(stake, STAKE);
        assertEq(bits, 0, "nothing funded at open");
        assertTrue(status == MatchSettlement.Status.Open);
        assertEq(deadline, uint64(block.timestamp) + settlement.settleWindow());
    }

    function test_openFieldMatch_revertsNonAttester() public {
        address[] memory ag = _roster3();
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        settlement.openFieldMatch(FIELD, ag, STAKE); // no attester prank
    }

    function test_openFieldMatch_revertsEmptyAndSingleton() public {
        address[] memory empty = new address[](0);
        vm.expectRevert(MatchSettlement.FieldTooSmall.selector);
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, empty, STAKE);

        address[] memory one = new address[](1);
        one[0] = alice;
        vm.expectRevert(MatchSettlement.FieldTooSmall.selector);
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, one, STAKE);
    }

    function test_openFieldMatch_revertsOverMaxField() public {
        uint256 over = settlement.MAX_FIELD() + 1;
        address[] memory big = new address[](over);
        for (uint256 i = 0; i < over; i++) {
            big[i] = address(uint160(0x6000 + i)); // size bound trips before any registry touch
        }
        vm.expectRevert(MatchSettlement.FieldTooLarge.selector);
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, big, STAKE);
    }

    /// @dev FM4 gas bound: MAX_FIELD seats open, fund (the bitmap fills to all-ones over
    ///      MAX_FIELD bits), and cancel-refund all — the O(n) open/refund stays bounded at the
    ///      ceiling, even though every seat is a distinct funded identity.
    function test_openFieldMatch_atMaxFieldOpensFundsRefunds() public {
        uint256 max = settlement.MAX_FIELD();
        address[] memory full = new address[](max);
        for (uint256 i = 0; i < max; i++) {
            address a = address(uint160(0x7000 + i));
            full[i] = a;
            _registerAndFund(a, bytes32(uint256(0x9000 + i)));
        }
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, full, STAKE);
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Open, "MAX_FIELD opens inclusively");

        for (uint256 i = 0; i < max; i++) {
            _fundField(FIELD, full[i]);
        }
        assertEq(_fundedBits(FIELD), (uint256(1) << max) - 1, "all MAX_FIELD seats funded");
        assertEq(token.balanceOf(address(settlement)), max * STAKE, "escrow holds every seat");

        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);
        assertEq(token.balanceOf(address(settlement)), 0, "every seat refunded at the MAX_FIELD edge");
    }

    function test_openFieldMatch_revertsDuplicateAgent() public {
        address[] memory ag = _field(alice, bob, alice); // alice in seats 0 and 2
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.DuplicateAgent.selector, alice));
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, ag, STAKE);
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.None, "no record on a duplicate roster");
    }

    function test_openFieldMatch_revertsUnregisteredAgent() public {
        address[] memory ag = _field(alice, bob, carol); // carol never registered
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.AgentNotRegistered.selector, carol));
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, ag, STAKE);
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.None, "no record on an unregistered seat");
    }

    function test_openFieldMatch_revertsReopenSameId() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, ag, STAKE);
    }

    /// @dev A field WAGER needs a positive stake. A zero-stake openFieldMatch could never
    ///      fund a seat (fundField reverts NoWager at stake 0) so settleFieldWager's
    ///      full-funding precondition is unreachable — the id would be burned into a
    ///      dead-end reachable only by cancel/expire, never settled. The no-escrow
    ///      reputation-only field is settleField (direct, no open), so openFieldMatch
    ///      rejects stake 0 up front. The guard precedes _requireFreshId, so a rejected
    ///      open does NOT consume the single-use id — a corrected positive open still works.
    function test_openFieldMatch_revertsZeroStake() public {
        address[] memory ag = _roster3();
        vm.prank(attester);
        vm.expectRevert(MatchSettlement.NoWager.selector);
        settlement.openFieldMatch(FIELD, ag, 0);
        _openField(FIELD, ag, STAKE);
        assertTrue(
            _fieldStatus(FIELD) == MatchSettlement.Status.Open, "the same id reopens with a positive stake"
        );
    }

    /// @dev FM2 shared fence: a matchId is at most ONE record across all three openers. A
    ///      field-wager id can never also be a 1v1 (openMatch) nor a settleField id, and
    ///      vice-versa — _requireFreshId cross-checks both mappings so no id collides.
    function test_sharedFence_idIsExclusiveAcrossKinds() public {
        _enableVariable(RATING_CAP); // settleField needs the variable path enabled
        address[] memory ag = _roster3();
        int256[] memory ds = _deltas(10 ether, -10 ether, 0); // Σ = 0, within cap

        // 1v1 first -> a field reuse of that id is blocked.
        _open(MATCH, STAKE);
        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.openFieldMatch(MATCH, ag, STAKE);

        // field first -> both a 1v1 open and a settleField on that id are blocked.
        _openField(FIELD, ag, STAKE);
        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.openMatch(FIELD, alice, bob, STAKE);
        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.settleField(FIELD, ag, ds, HASH);

        // settleField id -> a field reuse of that id is blocked.
        bytes32 fid = bytes32("field-settled");
        vm.prank(attester);
        settlement.settleField(fid, ag, ds, HASH);
        vm.expectRevert(MatchSettlement.MatchExists.selector);
        vm.prank(attester);
        settlement.openFieldMatch(fid, ag, STAKE);
    }

    /// @dev FM3 cross-contamination safety: the 1v1 resolution paths must NOT operate on a
    ///      field id. A stray cancelMatch(fieldId) that ran would void the field WITHOUT
    ///      refunding its seats, stranding stake. Each 1v1 path reads matches[id].status (None
    ///      for a field id) -> MatchNotOpen, so the field stays Open and fully escrowed.
    function test_crossKind_1v1PathsRejectFieldId() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        _fundField(FIELD, bob);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.cancelMatch(FIELD); // would strand the field's seats if it ran

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(alice);
        settlement.fund(FIELD);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        settlement.refundExpired(FIELD);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settle(FIELD, alice, HASH);

        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Open, "field not voided by a 1v1 path");
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE, "field escrow intact");
    }

    /// @dev FM3 cross-contamination safety, reverse: the field resolution paths reject a 1v1
    ///      id (fieldMatches[id].status is None), so a field call can never void a 1v1 escrow.
    function test_crossKind_fieldPathsReject1v1Id() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.cancelFieldMatch(MATCH);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(alice);
        settlement.fundField(MATCH);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(alice);
        settlement.reclaimField(MATCH);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        settlement.refundFieldExpired(MATCH);

        assertTrue(_status(MATCH) == MatchSettlement.Status.Open, "1v1 not voided by a field path");
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE);
    }

    // --- fundField + reclaimField: per-seat integrity (FM1) ---

    function test_fundField_marksOnlyCallerSeatAndPulls() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);

        vm.expectEmit(true, true, false, true);
        emit FieldMatchFunded(FIELD, bob, STAKE);
        _fundField(FIELD, bob); // seat 1

        assertEq(_fundedBits(FIELD), uint256(1) << 1, "exactly seat 1 funded, not seat 0 or 2");
        assertEq(token.balanceOf(address(settlement)), STAKE, "one seat escrowed");
    }

    function test_fundField_eachSeatFundsOwn() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        _fundField(FIELD, dave);
        _fundField(FIELD, bob);
        assertEq(_fundedBits(FIELD), 0x7, "all three seats funded (bits 0,1,2)");
        assertEq(token.balanceOf(address(settlement)), 3 * STAKE);
    }

    function test_fundField_revertsNonMember() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _registerAgent(eve, "eve-bot"); // registered, but not on this roster
        vm.expectRevert(MatchSettlement.NotParticipant.selector);
        vm.prank(eve);
        settlement.fundField(FIELD);
    }

    function test_fundField_revertsAlreadyFunded() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        vm.expectRevert(MatchSettlement.AlreadyFunded.selector);
        vm.prank(alice);
        settlement.fundField(FIELD);
    }

    function test_fundField_revertsNotOpen() public {
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(alice);
        settlement.fundField(bytes32("never-opened"));
    }

    function test_reclaimField_soleFunderRescuesAndStaysOpen() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        uint256 before = token.balanceOf(alice);

        vm.expectEmit(true, true, false, true);
        emit FieldMatchReclaimed(FIELD, alice, STAKE);
        vm.prank(alice);
        settlement.reclaimField(FIELD);

        assertEq(token.balanceOf(alice), before + STAKE, "stake returned");
        assertEq(_fundedBits(FIELD), 0, "seat cleared");
        assertEq(token.balanceOf(address(settlement)), 0, "no escrow held");
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Open, "match stays open, re-fundable");

        _fundField(FIELD, alice); // re-fundable after a reclaim
        assertEq(_fundedBits(FIELD), 1);
    }

    function test_reclaimField_revertsWhenPeerFunded() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        _fundField(FIELD, bob);
        vm.expectRevert(MatchSettlement.OpponentFunded.selector);
        vm.prank(alice);
        settlement.reclaimField(FIELD); // a peer has funded -> the field is live
    }

    function test_reclaimField_revertsNotFunded() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, bob); // a peer funded, but alice never did
        vm.expectRevert(MatchSettlement.NotFunded.selector);
        vm.prank(alice);
        settlement.reclaimField(FIELD);
    }

    function test_reclaimField_revertsNonMember() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _registerAgent(eve, "eve-bot");
        vm.expectRevert(MatchSettlement.NotParticipant.selector);
        vm.prank(eve);
        settlement.reclaimField(FIELD);
    }

    // --- cancelFieldMatch + refundFieldExpired: refund every funded seat, no stranded stake (FM3) ---

    function test_cancelFieldMatch_refundsExactlyFundedSeats() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice); // seat 0
        _fundField(FIELD, dave); // seat 2 — bob (seat 1) does NOT fund
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);
        uint256 dBefore = token.balanceOf(dave);

        vm.expectEmit(true, false, false, true);
        emit FieldMatchCancelled(FIELD, 2 * STAKE);
        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);

        assertEq(token.balanceOf(alice), aBefore + STAKE, "funded seat 0 refunded");
        assertEq(token.balanceOf(dave), dBefore + STAKE, "funded seat 2 refunded");
        assertEq(token.balanceOf(bob), bBefore, "unfunded seat 1 gets nothing");
        assertEq(token.balanceOf(address(settlement)), 0, "no stranded stake");
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Cancelled);
        assertEq(_fundedBits(FIELD), 0, "funded set cleared");
    }

    function test_cancelFieldMatch_revertsNonAttester() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        settlement.cancelFieldMatch(FIELD);
    }

    function test_cancelFieldMatch_noFundedSeatsRefundsZero() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        vm.expectEmit(true, false, false, true);
        emit FieldMatchCancelled(FIELD, 0);
        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Cancelled);
    }

    function test_cancelFieldMatch_revertsDoubleCancel() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);
    }

    function test_fundField_revertsAfterCancel() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(alice);
        settlement.fundField(FIELD);
    }

    function test_refundFieldExpired_permissionlessPastDeadline() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        _fundField(FIELD, bob);
        _fundField(FIELD, dave);

        (,,, uint64 deadline,) = settlement.fieldMatches(FIELD);
        vm.warp(deadline);

        uint256 carolBefore = token.balanceOf(carol);
        uint256 aBefore = token.balanceOf(alice);
        vm.expectEmit(true, false, false, true);
        emit FieldMatchExpired(FIELD, 3 * STAKE);
        vm.prank(carol); // an unrelated third party triggers the void
        settlement.refundFieldExpired(FIELD);

        assertEq(token.balanceOf(carol), carolBefore, "caller gains nothing");
        assertEq(token.balanceOf(alice), aBefore + STAKE, "each funded seat refunded");
        assertEq(token.balanceOf(address(settlement)), 0, "all escrow returned");
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Expired);
    }

    function test_refundFieldExpired_refundsExactlyFundedSeats() public {
        // The _refundField partial-funding branch through the DEADLINE gate — the twin of
        // test_cancelFieldMatch_refundsExactlyFundedSeats (attester cancel) and the field analog of
        // the 1v1 test_refundExpired_refundsOnlyFundedSeat: only funded seats are refunded, no
        // stranded stake, under a distinct Expired terminal.
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice); // seat 0
        _fundField(FIELD, dave); // seat 2 — bob (seat 1) does NOT fund
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);
        uint256 dBefore = token.balanceOf(dave);

        (,,, uint64 deadline,) = settlement.fieldMatches(FIELD);
        vm.warp(deadline);

        vm.expectEmit(true, false, false, true);
        emit FieldMatchExpired(FIELD, 2 * STAKE);
        vm.prank(carol); // permissionless third party triggers the void
        settlement.refundFieldExpired(FIELD);

        assertEq(token.balanceOf(alice), aBefore + STAKE, "funded seat 0 refunded");
        assertEq(token.balanceOf(dave), dBefore + STAKE, "funded seat 2 refunded");
        assertEq(token.balanceOf(bob), bBefore, "unfunded seat 1 gets nothing");
        assertEq(token.balanceOf(address(settlement)), 0, "no stranded stake");
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Expired);
        assertEq(_fundedBits(FIELD), 0, "funded set cleared");
    }

    function test_refundFieldExpired_noFundedSeatsRefundsZero() public {
        // The zero-funded emit twin of test_cancelFieldMatch_noFundedSeatsRefundsZero, under the
        // Expired terminal: a never-funded field past deadline voids to Expired emitting a 0 refund.
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        (,,, uint64 deadline,) = settlement.fieldMatches(FIELD);
        vm.warp(deadline);
        vm.expectEmit(true, false, false, true);
        emit FieldMatchExpired(FIELD, 0);
        vm.prank(carol);
        settlement.refundFieldExpired(FIELD);
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Expired);
        assertEq(token.balanceOf(address(settlement)), 0, "nothing to refund, nothing stranded");
    }

    function test_refundFieldExpired_revertsBeforeDeadline() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        vm.expectRevert(MatchSettlement.NotExpired.selector);
        settlement.refundFieldExpired(FIELD);
    }

    function test_refundFieldExpired_revertsNotOpen() public {
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        settlement.refundFieldExpired(bytes32("never-opened"));
    }

    /// @dev FM4 single-shot: a voided field can never be re-resolved — a cancelled field can't
    ///      then be expired (nor cancelled twice), so a void can't later become a settle nor a
    ///      double-refund.
    function test_fieldVoidIsSingleShot() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);

        (,,, uint64 deadline,) = settlement.fieldMatches(FIELD);
        vm.warp(deadline);
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        settlement.refundFieldExpired(FIELD); // can't expire an already-cancelled field
    }

    /// @dev FM1/FM3: over a RANDOM funded subset of a roster, cancel refunds EXACTLY the funded
    ///      seats — each once, none twice, no unfunded seat — and the contract holds no stranded
    ///      stake afterward. The fund mask exercises an arbitrary per-seat pattern.
    function testFuzz_cancelFieldMatch_refundsExactlyFundedSubset(uint256 mask, uint256 nSeed) public {
        uint256 n = bound(nSeed, 2, 8);
        address[] memory ag = new address[](n);
        for (uint256 i = 0; i < n; i++) {
            address a = address(uint160(0xB000 + i));
            ag[i] = a;
            _registerAndFund(a, bytes32(uint256(0xC000 + i)));
        }
        _openField(FIELD, ag, STAKE);

        uint256 funded;
        uint256 fundedCount;
        for (uint256 i = 0; i < n; i++) {
            if (mask & (1 << i) != 0) {
                _fundField(FIELD, ag[i]);
                funded |= (1 << i);
                fundedCount++;
            }
        }
        assertEq(_fundedBits(FIELD), funded, "bitmap matches the funded mask");
        assertEq(token.balanceOf(address(settlement)), fundedCount * STAKE, "escrow == funded seats");

        uint256[] memory before = new uint256[](n);
        for (uint256 i = 0; i < n; i++) {
            before[i] = token.balanceOf(ag[i]);
        }
        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);

        for (uint256 i = 0; i < n; i++) {
            uint256 expected = before[i] + ((funded & (1 << i)) != 0 ? STAKE : 0);
            assertEq(token.balanceOf(ag[i]), expected, "each seat refunded iff it funded");
        }
        assertEq(token.balanceOf(address(settlement)), 0, "no stranded stake");
    }

    // --- settleFieldWager: placement pot distribution + reputation (FM1-FM4) ---
    //
    // The Settled resolution of a field wager: one attester-gated, fenced call distributes
    // the funded pot by an attester-supplied placement split (bounded to sum == pot) AND
    // writes the zero-sum per-seat reputation, atomically. payouts/deltas align 1:1 with
    // the stored roster in canonical seat order. Requires every seat funded.

    event FieldMatchWagerSettled(bytes32 indexed matchId, bytes32 replayHash, uint256 seats, uint256 pot);

    function _openFundField3(uint256 stake) internal returns (address[] memory ag) {
        ag = _roster3(); // [alice, bob, dave]; registers + funds dave
        _openField(FIELD, ag, stake);
        _fundField(FIELD, alice);
        _fundField(FIELD, bob);
        _fundField(FIELD, dave);
    }

    function _payouts3(uint256 p0, uint256 p1, uint256 p2) internal pure returns (uint256[] memory ps) {
        ps = new uint256[](3);
        ps[0] = p0;
        ps[1] = p1;
        ps[2] = p2;
    }

    function _fieldReplayHash(bytes32 id) internal view returns (bytes32 rh) {
        (,,,, rh) = settlement.fieldMatches(id);
    }

    // FM1 + FM2: a placement split distributes the pot to EXACTLY the right seats and writes
    // the reputation vector. Distinct payouts make the seat->payout mapping mutation-checkable
    // (a positional swap fails). Pins the pot conservation, the event, and the replay commit.
    function test_settleFieldWager_distributesPotAndWritesReputation() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256 pot = 3 * STAKE;
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);
        uint256 dBefore = token.balanceOf(dave);

        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether); // sum == pot (300)
        int256[] memory ds = _deltas(20 ether, 0, -20 ether); // zero-sum, within cap

        vm.expectEmit(true, false, false, true);
        emit FieldMatchWagerSettled(FIELD, HASH, 3, pot);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);

        assertEq(token.balanceOf(alice), aBefore + 150 ether, "seat 0 paid its placement share");
        assertEq(token.balanceOf(bob), bBefore + 90 ether, "seat 1 paid its placement share");
        assertEq(token.balanceOf(dave), dBefore + 60 ether, "seat 2 paid its placement share");
        assertEq(token.balanceOf(address(settlement)), 0, "pot fully distributed, nothing stranded");
        assertEq(registry.reputationOf(alice), int256(20 ether), "seat 0 reputation");
        assertEq(registry.reputationOf(bob), 0, "seat 1 reputation");
        assertEq(registry.reputationOf(dave), -int256(20 ether), "seat 2 reputation");
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Settled, "settled");
        assertEq(_fieldReplayHash(FIELD), HASH, "replay digest committed durably");
    }

    // A zero payout for a last-place seat is valid as long as the whole vector sums to the
    // pot — winner-takes-all still writes every seat's reputation.
    function test_settleFieldWager_winnerTakesAllZeroForOthers() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256 pot = 3 * STAKE;
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);
        uint256 dBefore = token.balanceOf(dave);

        uint256[] memory ps = _payouts3(pot, 0, 0);
        int256[] memory ds = _deltas(40 ether, -20 ether, -20 ether);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);

        assertEq(token.balanceOf(alice), aBefore + pot, "winner takes the whole pot");
        assertEq(token.balanceOf(bob), bBefore, "a zero-payout seat receives nothing");
        assertEq(token.balanceOf(dave), dBefore, "a zero-payout seat receives nothing");
        assertEq(registry.reputationOf(bob), -int256(20 ether), "a zero-payout seat still moves reputation");
    }

    // FM1: sum(payouts) must equal the pot EXACTLY — both an under- and over-payment revert.
    function test_settleFieldWager_revertsUnderConservingPot() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256 pot = 3 * STAKE;
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 59 ether); // sum = pot - 1
        int256[] memory ds = _deltas(0, 0, 0);
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.PayoutMismatch.selector, pot - 1 ether, pot));
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
        assertEq(token.balanceOf(address(settlement)), pot, "no payout on an under-conserving split");
    }

    function test_settleFieldWager_revertsOverConservingPot() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256 pot = 3 * STAKE;
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 61 ether); // sum = pot + 1
        int256[] memory ds = _deltas(0, 0, 0);
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.PayoutMismatch.selector, pot + 1 ether, pot));
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
        assertEq(token.balanceOf(address(settlement)), pot, "no payout on an over-conserving split");
    }

    // FM1 fuzz: ANY non-conserving split reverts and moves nothing, leaving the id free.
    function testFuzz_settleFieldWager_revertsNonConservingPot(uint256 p0, uint256 p1, uint256 p2) public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256 pot = 3 * STAKE;
        p0 = bound(p0, 0, pot);
        p1 = bound(p1, 0, pot);
        p2 = bound(p2, 0, pot);
        uint256 sum = p0 + p1 + p2;
        vm.assume(sum != pot);
        uint256[] memory ps = _payouts3(p0, p1, p2);
        int256[] memory ds = _deltas(0, 0, 0); // zero-sum, so only the pot check can trip
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.PayoutMismatch.selector, sum, pot));
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
        assertEq(token.balanceOf(address(settlement)), pot, "no value minted or stranded");
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Open, "id free for a corrected split");
    }

    // FM1 fuzz: any conserving split distributes the WHOLE pot, each seat exactly its share.
    function testFuzz_settleFieldWager_conservingPotDistributes(uint256 p0, uint256 p1) public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256 pot = 3 * STAKE;
        p0 = bound(p0, 0, pot);
        p1 = bound(p1, 0, pot - p0);
        uint256 p2 = pot - p0 - p1; // the third seat absorbs the remainder => sum == pot exactly
        uint256 aBefore = token.balanceOf(alice);
        uint256 bBefore = token.balanceOf(bob);
        uint256 dBefore = token.balanceOf(dave);

        uint256[] memory ps = _payouts3(p0, p1, p2);
        int256[] memory ds = _deltas(0, 0, 0);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);

        assertEq(token.balanceOf(alice), aBefore + p0, "seat 0 share");
        assertEq(token.balanceOf(bob), bBefore + p1, "seat 1 share");
        assertEq(token.balanceOf(dave), dBefore + p2, "seat 2 share");
        assertEq(token.balanceOf(address(settlement)), 0, "the whole pot is distributed");
    }

    // FM2: the payout/delta vectors must match the seat count.
    function test_settleFieldWager_revertsPayoutLengthMismatch() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256[] memory ps = new uint256[](2); // n == 3
        ps[0] = 150 ether;
        ps[1] = 150 ether;
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.expectRevert(MatchSettlement.LengthMismatch.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
    }

    function test_settleFieldWager_revertsDeltaLengthMismatch() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(100 ether, 100 ether, 100 ether);
        int256[] memory ds = new int256[](2); // n == 3
        ds[0] = 10 ether;
        ds[1] = -10 ether;
        vm.expectRevert(MatchSettlement.LengthMismatch.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
    }

    // FM3: a non-zero-sum reputation vector reverts BEFORE any payout (atomicity — even a
    // perfectly-conserving pot is not released if the reputation half is invalid).
    function test_settleFieldWager_revertsNonZeroSumDeltas() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256 pot = 3 * STAKE;
        uint256[] memory ps = _payouts3(100 ether, 100 ether, 100 ether); // conserving
        int256[] memory ds = _deltas(20 ether, 0, -10 ether); // sum = +10
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.NonZeroSum.selector, int256(10 ether)));
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
        assertEq(token.balanceOf(address(settlement)), pot, "no pot released when reputation is invalid");
    }

    // FM3: each |delta| is bounded by the same per-match magnitude cap as settleField.
    function test_settleFieldWager_revertsDeltaOverCap() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(100 ether, 100 ether, 100 ether);
        int256[] memory ds = _deltas(int256(RATING_CAP) + 1, -int256(RATING_CAP) - 1, 0);
        vm.expectRevert(MatchSettlement.RatingDeltaTooLarge.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
    }

    // FM3 atomicity: if the registry rejects the reputation write the WHOLE settle reverts —
    // no pot is released, the match stays Open, and the escrow is still recoverable via cancel
    // (escrow liveness does not hard-depend on the reputation-writer grant). Mirrors the 1v1.
    function test_settleFieldWager_revertsWhenNotReputationWriter() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256 pot = 3 * STAKE;
        vm.prank(owner);
        registry.setReputationWriter(address(settlement), false);

        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.expectRevert(AgentRegistry.NotReputationWriter.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
        assertEq(token.balanceOf(address(settlement)), pot, "no half-settle: pot untouched on revert");
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Open, "match remains Open");

        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);
        assertEq(token.balanceOf(address(settlement)), 0, "funds recovered via cancel");
    }

    // FM3 idempotency: a replayed settle reverts on the fence — no second payout or reputation.
    function test_settleFieldWager_idempotentReSettleReverts() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
        int256 aRep = registry.reputationOf(alice);

        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
        assertEq(registry.reputationOf(alice), aRep, "no second reputation write");
        assertEq(token.balanceOf(address(settlement)), 0, "no second payout");
    }

    // FM3: the reputation path must be enabled (a wager settle always writes per-seat deltas).
    function test_settleFieldWager_revertsVariableDisabled() public {
        _openFundField3(STAKE); // variable NOT enabled
        uint256[] memory ps = _payouts3(100 ether, 100 ether, 100 ether);
        int256[] memory ds = _deltas(0, 0, 0);
        vm.expectRevert(MatchSettlement.VariableSettleDisabled.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
    }

    function test_settleFieldWager_revertsZeroReplayHash() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.expectRevert(MatchSettlement.ZeroReplayHash.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, bytes32(0));
    }

    // FM4: a wager settle requires EVERY seat funded — a partial field reverts, untouched.
    function test_settleFieldWager_revertsPartiallyFunded() public {
        _enableVariable(RATING_CAP);
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        _fundField(FIELD, bob); // dave's seat is NOT funded
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.expectRevert(MatchSettlement.NotFullyFunded.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE, "no payout on a partial field");
        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Open);
    }

    function test_settleFieldWager_revertsUnfundedField() public {
        _enableVariable(RATING_CAP);
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE); // nobody funds
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.expectRevert(MatchSettlement.NotFullyFunded.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
    }

    // A zero-stake field can no longer reach settleFieldWager: openFieldMatch rejects
    // stake 0 up front (NoWager — see test_openFieldMatch_revertsZeroStake), so the field
    // is unconstructible here. The fundedBits==0 -> NotFullyFunded edge this once covered
    // is covered by test_settleFieldWager_revertsUnfundedField (a positive-stake field
    // nobody funds).

    function test_settleFieldWager_revertsNonAttester() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        vm.prank(alice); // a participant, not the attester
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
    }

    function test_settleFieldWager_revertsAfterCancel() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);
    }

    // Cross-kind: a 1v1 openMatch id is not a field record, so a wager settle of it reverts
    // on the field status fence (the shared id space never cross-contaminates).
    function test_settleFieldWager_revertsOn1v1Id() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        uint256[] memory ps = new uint256[](0);
        int256[] memory ds = new int256[](0);
        vm.expectRevert(MatchSettlement.MatchNotOpen.selector);
        vm.prank(attester);
        settlement.settleFieldWager(MATCH, ps, ds, HASH);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE, "the 1v1 escrow is untouched");
    }

    // --- fieldSeatOf: O(1) agent->seat inverse of fieldRoster (FM1-FM4) ---
    //
    // fieldSeatOf returns the raw seatPlus1: 0 = non-member, k = seat k-1, so
    // fieldRoster(m)[fieldSeatOf(m,a)-1] == a. The map is written once at open and never
    // cleared, so the seat survives every terminal resolution for post-hoc audits.

    /// @dev FM1 encoding: each seat resolves to its 1-based index (distinct values catch a
    ///      positional swap) and round-trips through fieldRoster's 0-based array.
    function test_fieldSeatOf_resolvesEachSeatAndRoundTrips() public {
        address[] memory ag = _roster3(); // [alice, bob, dave]
        _openField(FIELD, ag, STAKE);

        assertEq(settlement.fieldSeatOf(FIELD, alice), 1, "seat 0 -> seatPlus1 1");
        assertEq(settlement.fieldSeatOf(FIELD, bob), 2, "seat 1 -> seatPlus1 2");
        assertEq(settlement.fieldSeatOf(FIELD, dave), 3, "seat 2 -> seatPlus1 3");

        address[] memory roster = settlement.fieldRoster(FIELD);
        assertEq(roster[settlement.fieldSeatOf(FIELD, alice) - 1], alice, "round-trips seat 0");
        assertEq(roster[settlement.fieldSeatOf(FIELD, bob) - 1], bob, "round-trips seat 1");
        assertEq(roster[settlement.fieldSeatOf(FIELD, dave) - 1], dave, "round-trips seat 2");
    }

    /// @dev FM1/FM3: an address absent from the roster reads the default 0 and never reverts —
    ///      the 0 sentinel is unambiguous because a real seat is always >= 1.
    function test_fieldSeatOf_nonMemberReturnsZero() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        assertEq(settlement.fieldSeatOf(FIELD, carol), 0, "non-roster agent is not a member");
        assertEq(settlement.fieldSeatOf(FIELD, address(0)), 0, "zero address is not a member");
    }

    /// @dev FM3: a never-opened id reads the default map for any agent and never reverts.
    function test_fieldSeatOf_unknownMatchReturnsZero() public view {
        bytes32 ghost = bytes32("never-opened");
        assertEq(settlement.fieldSeatOf(ghost, alice), 0, "unknown match, known agent");
        assertEq(settlement.fieldSeatOf(ghost, carol), 0, "unknown match, unknown agent");
    }

    /// @dev FM2 post-close: settleFieldWager leaves the seat map intact, so the seat is still
    ///      resolvable after settlement — the lookup a payout/reputation audit needs.
    function test_fieldSeatOf_persistsAfterSettle() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether); // sum == 3*STAKE
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);

        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Settled, "settled");
        assertEq(settlement.fieldSeatOf(FIELD, alice), 1, "seat survives settle");
        assertEq(settlement.fieldSeatOf(FIELD, bob), 2, "seat survives settle");
        assertEq(settlement.fieldSeatOf(FIELD, dave), 3, "seat survives settle");
    }

    /// @dev FM2 post-close: cancelFieldMatch (the attester void) likewise never clears the seat.
    function test_fieldSeatOf_persistsAfterCancel() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);

        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Cancelled, "cancelled");
        assertEq(settlement.fieldSeatOf(FIELD, alice), 1, "seat survives cancel");
        assertEq(settlement.fieldSeatOf(FIELD, dave), 3, "seat survives cancel");
    }

    /// @dev FM2 post-close: the deadline self-refund (refundFieldExpired) preserves the seat too.
    function test_fieldSeatOf_persistsAfterExpire() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        (,,, uint64 deadline,) = settlement.fieldMatches(FIELD);
        vm.warp(deadline);
        settlement.refundFieldExpired(FIELD);

        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Expired, "expired");
        assertEq(settlement.fieldSeatOf(FIELD, bob), 2, "seat survives expire");
        assertEq(settlement.fieldSeatOf(FIELD, dave), 3, "seat survives expire");
    }

    /// @dev FM2: isFieldSeatFunded reads the funded bit per seat, so a funded subset reports
    ///      true only for the seats that paid. Funding seats 0 and 2 but not 1 also pins the
    ///      bit index: an off-by-one reading bit `seatPlus1` (not `seatPlus1 - 1`) would read
    ///      bob's clear bit for alice and fail the first assert.
    function test_isFieldSeatFunded_reflectsFundedSubset() public {
        address[] memory ag = _roster3(); // [alice, bob, dave]
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);
        _fundField(FIELD, dave); // bob's seat left unfunded

        assertTrue(settlement.isFieldSeatFunded(FIELD, alice), "seat 0 funded");
        assertFalse(settlement.isFieldSeatFunded(FIELD, bob), "seat 1 unfunded");
        assertTrue(settlement.isFieldSeatFunded(FIELD, dave), "seat 2 funded");
        assertFalse(settlement.isFieldSeatFunded(FIELD, carol), "non-member -> false");
    }

    /// @dev FM3 pre-close: the live bit flips false -> true -> false across a seat's fund then
    ///      sole-funder reclaim, mirroring the 1v1 aFunded/bFunded flag.
    function test_isFieldSeatFunded_flipsFalseTrueFalseAcrossFundReclaim() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);

        assertFalse(settlement.isFieldSeatFunded(FIELD, alice), "unfunded -> false");
        _fundField(FIELD, alice);
        assertTrue(settlement.isFieldSeatFunded(FIELD, alice), "funded -> true");
        vm.prank(alice);
        settlement.reclaimField(FIELD); // alice is the sole funder, so reclaim is allowed
        assertFalse(settlement.isFieldSeatFunded(FIELD, alice), "reclaimed -> false");
    }

    /// @dev FM3 post-close: settleFieldWager never clears fundedBits, so the funded set survives
    ///      settlement — the funded-status the payout/reputation audit reads back.
    function test_isFieldSeatFunded_persistsAfterSettle() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether); // sum == 3*STAKE
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);

        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Settled, "settled");
        assertTrue(settlement.isFieldSeatFunded(FIELD, alice), "funded set survives settle");
        assertTrue(settlement.isFieldSeatFunded(FIELD, bob), "funded set survives settle");
        assertTrue(settlement.isFieldSeatFunded(FIELD, dave), "funded set survives settle");
    }

    /// @dev FM3 post-close: the attester void (cancelFieldMatch) refunds every seat and clears
    ///      fundedBits, so the funded status returns to false — where the seat MAP (fieldSeatOf)
    ///      persists. The two views intentionally diverge post-void: the seat is historical, the
    ///      escrow is gone, and isFieldSeatFunded tracks the escrow.
    function test_isFieldSeatFunded_clearedAfterCancel() public {
        _openFundField3(STAKE);
        assertTrue(settlement.isFieldSeatFunded(FIELD, alice), "funded before cancel");

        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);

        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Cancelled, "cancelled");
        assertFalse(settlement.isFieldSeatFunded(FIELD, alice), "cancel refunds -> not funded");
        assertFalse(settlement.isFieldSeatFunded(FIELD, dave), "cancel refunds -> not funded");
        assertEq(settlement.fieldSeatOf(FIELD, alice), 1, "seat persists though funded status cleared");
    }

    /// @dev FM3 post-close: the deadline self-refund (refundFieldExpired) likewise zeroes
    ///      fundedBits, so the funded status clears on expiry too.
    function test_isFieldSeatFunded_clearedAfterExpire() public {
        _openFundField3(STAKE);
        assertTrue(settlement.isFieldSeatFunded(FIELD, bob), "funded before expire");

        (,,, uint64 deadline,) = settlement.fieldMatches(FIELD);
        vm.warp(deadline);
        settlement.refundFieldExpired(FIELD);

        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Expired, "expired");
        assertFalse(settlement.isFieldSeatFunded(FIELD, bob), "expire refunds -> not funded");
        assertFalse(settlement.isFieldSeatFunded(FIELD, dave), "expire refunds -> not funded");
    }

    /// @dev FM2: a non-member and an unknown match both read the 0 seat sentinel and return
    ///      false without reverting — probed against a match that DOES have a funded seat, so a
    ///      false answer is about membership, not an empty funded set.
    function test_isFieldSeatFunded_falseForNonMemberAndUnknownMatch() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);
        _fundField(FIELD, alice);

        assertFalse(settlement.isFieldSeatFunded(FIELD, carol), "non-roster agent -> false");
        assertFalse(settlement.isFieldSeatFunded(FIELD, address(0)), "zero address -> false");
        bytes32 ghost = bytes32("never-opened");
        assertFalse(settlement.isFieldSeatFunded(ghost, alice), "unknown match, known agent -> false");
        assertFalse(settlement.isFieldSeatFunded(ghost, carol), "unknown match, unknown agent -> false");
    }

    /// @dev FM3: the aggregate readiness predicate is true only once EVERY seat has funded —
    ///      exactly settleFieldWager's NotFullyFunded precondition. A partial roster (2 of 3) is
    ///      not ready. Funding one short then completing it pins the full mask, not just "any".
    function test_isFieldFullyFunded_trueOnlyWhenEverySeatFunded() public {
        address[] memory ag = _roster3();
        _openField(FIELD, ag, STAKE);

        assertFalse(settlement.isFieldFullyFunded(FIELD), "opened, unfunded -> not ready");
        _fundField(FIELD, alice);
        _fundField(FIELD, dave); // bob still unfunded: 2 of 3
        assertFalse(settlement.isFieldFullyFunded(FIELD), "partial roster -> not ready");
        _fundField(FIELD, bob);
        assertTrue(settlement.isFieldFullyFunded(FIELD), "every seat funded -> ready");
    }

    /// @dev FM1: the empty-roster guard. An unknown matchId has n == 0 and fundedBits == 0, so a
    ///      naive `fundedBits == (1<<n)-1` computes 0 == 0 == true and would report a match that
    ///      does not exist as settle-ready. The n == 0 short-circuit returns false.
    function test_isFieldFullyFunded_falseForUnknownMatch() public view {
        assertFalse(settlement.isFieldFullyFunded(bytes32("never-opened")), "unknown match -> not funded");
    }

    /// @dev FM3 post-close: settleFieldWager never clears fundedBits, so the field still reads
    ///      fully funded after settlement — the funded set is on-chain history.
    function test_isFieldFullyFunded_persistsAfterSettle() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        assertTrue(settlement.isFieldFullyFunded(FIELD), "fully funded before settle");

        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether); // sum == 3*STAKE
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);

        assertTrue(_fieldStatus(FIELD) == MatchSettlement.Status.Settled, "settled");
        assertTrue(settlement.isFieldFullyFunded(FIELD), "settle never clears fundedBits");
    }

    /// @dev FM3 post-close: the attester void refunds every seat and zeroes fundedBits, so the
    ///      field is no longer fully funded once cancelled (the stakes went back).
    function test_isFieldFullyFunded_clearedAfterCancel() public {
        _openFundField3(STAKE);
        assertTrue(settlement.isFieldFullyFunded(FIELD), "fully funded before cancel");

        vm.prank(attester);
        settlement.cancelFieldMatch(FIELD);
        assertFalse(settlement.isFieldFullyFunded(FIELD), "cancel refunds -> not fully funded");
    }

    /// @dev FM3 post-close: the deadline self-refund likewise zeroes fundedBits.
    function test_isFieldFullyFunded_clearedAfterExpire() public {
        _openFundField3(STAKE);
        assertTrue(settlement.isFieldFullyFunded(FIELD), "fully funded before expire");

        (,,, uint64 deadline,) = settlement.fieldMatches(FIELD);
        vm.warp(deadline);
        settlement.refundFieldExpired(FIELD);
        assertFalse(settlement.isFieldFullyFunded(FIELD), "expire refunds -> not fully funded");
    }

    // --- EAS attestation (FM1 reentrancy fence, FM3 encoding round-trip, FM4 persistence) ---

    /// @dev Wire a fresh settlement (a second reputation writer alongside the setUp one) and
    ///      let alice/bob fund its escrow, optionally registering the schemas.
    function _wireFresh(MatchSettlement s, bool withSchema) internal {
        vm.startPrank(owner);
        registry.setReputationWriter(address(s), true);
        s.setAttester(attester, true);
        s.setMaxRatingDelta(RATING_CAP);
        if (withSchema) s.registerSchema(address(new MockSchemaRegistry()));
        vm.stopPrank();
        vm.prank(alice);
        token.approve(address(s), type(uint256).max);
        vm.prank(bob);
        token.approve(address(s), type(uint256).max);
    }

    /// @dev FM3/FM4: a decisive settle attests the 1v1 shape to the winner and the payload
    ///      decodes back EXACTLY — with seat B winning, so agentA carries the NEGATED delta
    ///      (a swapped-seat or dropped-negation encoding is caught). The uid persists.
    function test_settle_attestsDecisiveResultThatDecodesBack() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        int256 delta = 25 ether;

        vm.prank(attester);
        settlement.settleRanked(MATCH, bob, HASH, delta); // seat B wins

        assertEq(eas.attestCalls(), 1, "one attestation per settled match");
        assertEq(eas.lastSchema(), settlement.schemaUid(), "attested under the 1v1 schema");
        assertEq(eas.lastRecipient(), bob, "recipient is the decisive winner");
        assertTrue(eas.lastRevocable(), "revocable for the future dispute path");

        (bytes32 mId, address agentA, address agentB, address winner, bytes32 replayHash, int256 deltaA) =
            abi.decode(eas.lastData(), (bytes32, address, address, address, bytes32, int256));
        assertEq(mId, MATCH, "matchId");
        assertEq(agentA, alice, "seat A preserved in order");
        assertEq(agentB, bob, "seat B preserved in order");
        assertEq(winner, bob, "winner named");
        assertEq(replayHash, HASH, "replay digest");
        assertEq(deltaA, -delta, "agentA carries the negated winner delta");

        bytes32 expected = keccak256(abi.encode(settlement.schemaUid(), bob, eas.lastData()));
        assertEq(settlement.matchAttestationUid(MATCH), expected, "uid of the exact payload persisted");
    }

    /// @dev The attest half of the settlement lifecycle emit pair: every settle path funnels
    ///      through `_attestSettled`, which emits `MatchAttested(matchId, uid)` — the topic
    ///      off-chain indexers key on to locate a match's canonical settlement proof. Unlike
    ///      the revoke sibling, the emit fires DURING settle, so the uid can't be read back
    ///      first; reconstruct it from the same 1v1 payload the decode test above pins
    ///      (`+REP_DELTA` for an agentA win) and MockEAS's `keccak256(schema, recipient, data)`.
    ///      Both indexed topics are checked, so a dropped, arg-transposed, or stale-uid emit
    ///      reddens while the storage assertions stay green.
    function test_settle_emitsMatchAttested() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);

        bytes memory data = abi.encode(MATCH, alice, bob, alice, HASH, int256(REP_DELTA));
        bytes32 uid = keccak256(abi.encode(settlement.schemaUid(), alice, data));

        vm.expectEmit(true, true, false, false);
        emit MatchAttested(MATCH, uid);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);
    }

    /// @dev FM4 draw edge: a draw shares the 1v1 schema but names NO winner (winner==0,
    ///      recipient==0) while still carrying agentA's signed delta. A schema assuming a
    ///      decisive pair would mis-encode this.
    function test_settleDraw_attestsDrawWithZeroWinner() public {
        _enableVariable(RATING_CAP);
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        int256 deltaA = 15 ether;

        vm.prank(attester);
        settlement.settleDrawRanked(MATCH, HASH, deltaA);

        assertEq(eas.attestCalls(), 1);
        assertEq(eas.lastSchema(), settlement.schemaUid(), "draw shares the 1v1 schema");
        assertEq(eas.lastRecipient(), address(0), "a draw names no recipient");

        (bytes32 mId, address agentA, address agentB, address winner, bytes32 replayHash, int256 dA) =
            abi.decode(eas.lastData(), (bytes32, address, address, address, bytes32, int256));
        assertEq(mId, MATCH);
        assertEq(agentA, alice);
        assertEq(agentB, bob);
        assertEq(winner, address(0), "winner==0 marks the draw");
        assertEq(replayHash, HASH);
        assertEq(dA, deltaA, "agentA's signed draw delta");
        assertTrue(settlement.matchAttestationUid(MATCH) != bytes32(0), "draw persisted a uid");
    }

    /// @dev FM4 field edge: the reputation-only settleField attests the full roster + delta
    ///      vector against the DISTINCT field schema, with a zero pot.
    function test_settleField_attestsRosterWithZeroPot() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);

        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        assertEq(eas.attestCalls(), 1);
        assertEq(eas.lastSchema(), settlement.fieldSchemaUid(), "attested under the field schema");
        assertEq(eas.lastRecipient(), address(0));

        (bytes32 mId, address[] memory agents, int256[] memory deltas, bytes32 replayHash, uint256 pot) =
            abi.decode(eas.lastData(), (bytes32, address[], int256[], bytes32, uint256));
        assertEq(mId, MATCH);
        assertEq(agents.length, 3, "full roster carried");
        assertEq(agents[0], alice);
        assertEq(agents[1], bob);
        assertEq(agents[2], dave);
        assertEq(deltas[0], 30 ether);
        assertEq(deltas[1], -10 ether);
        assertEq(deltas[2], -20 ether);
        assertEq(replayHash, HASH);
        assertEq(pot, 0, "reputation-only field carries a zero pot");
        assertTrue(settlement.matchAttestationUid(MATCH) != bytes32(0));
    }

    /// @dev FM4 field-wager edge: settleFieldWager attests the roster + delta vector AND the
    ///      funded pot (stake * seats) against the field schema. The field schema (not the 1v1
    ///      pair) is what round-trips a 3-seat wager.
    function test_settleFieldWager_attestsRosterAndPot() public {
        _enableVariable(RATING_CAP);
        address[] memory ag = _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);

        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);

        assertEq(eas.attestCalls(), 1);
        assertEq(eas.lastSchema(), settlement.fieldSchemaUid());
        assertEq(eas.lastRecipient(), address(0));

        (bytes32 mId, address[] memory agents, int256[] memory deltas, bytes32 replayHash, uint256 pot) =
            abi.decode(eas.lastData(), (bytes32, address[], int256[], bytes32, uint256));
        assertEq(mId, FIELD);
        assertEq(agents[0], ag[0]);
        assertEq(agents[1], ag[1]);
        assertEq(agents[2], ag[2]);
        assertEq(deltas[0], 20 ether);
        assertEq(deltas[1], 0);
        assertEq(deltas[2], -20 ether);
        assertEq(replayHash, HASH);
        assertEq(pot, 3 * STAKE, "wager pot = stake * seats");
        assertTrue(settlement.matchAttestationUid(FIELD) != bytes32(0));
    }

    // --- attestation revoke (dispute/correction path) ---

    /// @dev The happy path: an attester revokes a settled 1v1 result's attestation. EAS.revoke
    ///      is called ONCE with the persisted uid and the 1v1 schema; the match is flagged
    ///      revoked. Crucially the settled ESCROW payout and REPUTATION are NOT reversed — a
    ///      revoke retracts only the portable claim, mirroring RenderReceipts.revokeReceipt.
    function test_revokeAttestation_retractsA1v1ResultWithoutReversingSettlement() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH); // alice wins the 2*STAKE pot

        bytes32 uid = settlement.matchAttestationUid(MATCH);
        uint256 aliceAfterSettle = token.balanceOf(alice);
        int256 aliceRepAfterSettle = registry.reputationOf(alice);
        int256 bobRepAfterSettle = registry.reputationOf(bob);

        vm.prank(attester);
        settlement.revokeAttestation(MATCH);

        assertEq(eas.revokeCalls(), 1, "exactly one EAS.revoke");
        assertEq(eas.lastRevokedUid(), uid, "revoked the persisted attestation uid");
        assertEq(eas.lastRevokedSchema(), settlement.schemaUid(), "revoked under the 1v1 schema");
        assertTrue(settlement.matchAttestationRevoked(MATCH), "match flagged revoked");
        assertEq(
            settlement.matchAttestationUid(MATCH),
            uid,
            "uid pointer retained (points at the revoked attestation)"
        );

        // The settlement itself is FINAL: revoke moves no funds and no reputation.
        assertEq(token.balanceOf(alice), aliceAfterSettle, "winner keeps the pot; revoke is not a clawback");
        assertEq(token.balanceOf(address(settlement)), 0, "no escrow resurrected");
        assertEq(registry.reputationOf(alice), aliceRepAfterSettle, "winner reputation unchanged");
        assertEq(registry.reputationOf(bob), bobRepAfterSettle, "loser reputation unchanged");
    }

    function test_revokeAttestation_emitsMatchAttestationRevoked() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);
        bytes32 uid = settlement.matchAttestationUid(MATCH);

        vm.expectEmit(true, true, false, false);
        emit MatchAttestationRevoked(MATCH, uid);
        vm.prank(attester);
        settlement.revokeAttestation(MATCH);
    }

    /// @dev A draw is a 1v1 result, so its attestation revokes under the 1v1 schema.
    function test_revokeAttestation_drawUsesTheOneVOneSchema() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settleDraw(MATCH, HASH);

        vm.prank(attester);
        settlement.revokeAttestation(MATCH);
        assertEq(eas.lastRevokedSchema(), settlement.schemaUid(), "draw revokes under the 1v1 schema");
    }

    /// @dev The reputation-only settleField records into the `matches` slot (never setting
    ///      agentA) yet attests under the FIELD schema — this pins that the revoke hands EAS
    ///      the field schema for it, not the 1v1 schema its `matches` residence might suggest.
    function test_revokeAttestation_settleFieldUsesTheFieldSchema() public {
        _enableVariable(RATING_CAP);
        _registerAgent(dave, "dave-bot");
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);

        vm.prank(attester);
        settlement.revokeAttestation(MATCH);
        assertEq(eas.revokeCalls(), 1);
        assertEq(
            eas.lastRevokedSchema(),
            settlement.fieldSchemaUid(),
            "field-reputation revokes under the field schema"
        );
        assertEq(eas.lastRevokedUid(), settlement.matchAttestationUid(MATCH), "revoked the persisted uid");
    }

    function test_revokeAttestation_fieldWagerUsesTheFieldSchema() public {
        _enableVariable(RATING_CAP);
        _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);

        vm.prank(attester);
        settlement.revokeAttestation(FIELD);
        assertEq(
            eas.lastRevokedSchema(), settlement.fieldSchemaUid(), "field-wager revokes under the field schema"
        );
        assertTrue(settlement.matchAttestationRevoked(FIELD));
    }

    function test_revokeAttestation_revertsForNeverSettledMatch() public {
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.NotAttested.selector, MATCH));
        vm.prank(attester);
        settlement.revokeAttestation(MATCH);
    }

    /// @dev An opened-and-funded but UNSETTLED match has no uid yet, so it cannot be revoked
    ///      (NotAttested) — the uid is the settled-and-attested marker.
    function test_revokeAttestation_revertsForOpenButUnsettledMatch() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.NotAttested.selector, MATCH));
        vm.prank(attester);
        settlement.revokeAttestation(MATCH);
        assertEq(eas.revokeCalls(), 0, "no EAS.revoke for an unattested match");
    }

    function test_revokeAttestation_revertsWhenAlreadyRevoked() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);

        vm.prank(attester);
        settlement.revokeAttestation(MATCH);
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.AttestationAlreadyRevoked.selector, MATCH));
        vm.prank(attester);
        settlement.revokeAttestation(MATCH);
        assertEq(eas.revokeCalls(), 1, "a second revoke does not reach EAS");
    }

    function test_revokeAttestation_revertsForNonAttester() public {
        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);

        vm.expectRevert(MatchSettlement.NotAttester.selector);
        vm.prank(bob); // a registered agent, but not an attester
        settlement.revokeAttestation(MATCH);
        assertFalse(settlement.matchAttestationRevoked(MATCH), "non-attester cannot revoke");
        assertEq(eas.revokeCalls(), 0);
    }

    /// @dev CEI: a malicious EAS re-entering revokeAttestation during EAS.revoke hits the
    ///      matchAttestationRevoked fence (written before the external call) and reverts
    ///      AttestationAlreadyRevoked — exactly one revoke, one EAS.revoke.
    function test_revokeAttestation_reentrantEasCannotDoubleRevoke() public {
        ReentrantRevokeEAS reEas = new ReentrantRevokeEAS();
        MatchSettlement s = new MatchSettlement(address(reEas), address(registry), owner, REP_DELTA);
        _wireFresh(s, true);
        vm.prank(owner);
        s.setAttester(address(reEas), true); // grant so the reentry tests the fence, not the attester gate

        vm.prank(attester);
        s.openMatch(MATCH, alice, bob, STAKE);
        vm.prank(alice);
        s.fund(MATCH);
        vm.prank(bob);
        s.fund(MATCH);
        vm.prank(attester);
        s.settle(MATCH, alice, HASH);

        reEas.arm(s, MATCH);
        vm.prank(attester);
        s.revokeAttestation(MATCH);

        assertEq(reEas.revokeCalls(), 1, "one outer EAS.revoke, no reentrant second");
        assertTrue(reEas.reentered(), "the mock did attempt the reentry");
        assertTrue(reEas.reentryReverted(), "the reentrant revokeAttestation reverted");
        assertEq(
            reEas.reentryRevertSelector(),
            MatchSettlement.AttestationAlreadyRevoked.selector,
            "reentry blocked by the AlreadyRevoked fence"
        );
        assertTrue(s.matchAttestationRevoked(MATCH), "still revoked, exactly once");
    }

    // --- live/revoked attestation counters (the RenderReceipts.receiptCount twin) ---

    /// @dev The live counter rises by exactly one on a decisive settle AND on a
    ///      structurally-distinct draw (both funnel the SHARED counter through
    ///      _attestSettled — no missed path, no double-count), then a revoke moves ONE from
    ///      live to revoked, and a guarded re-revoke can't drive the live count below the
    ///      attests that raised it.
    function test_attestationCounters_riseOnSettleFallOnRevoke() public {
        assertEq(settlement.liveAttestationCount(), 0, "starts at zero");
        assertEq(settlement.revokedAttestationCount(), 0);

        _open(MATCH, STAKE);
        _fundBoth(MATCH);
        vm.prank(attester);
        settlement.settle(MATCH, alice, HASH);
        assertEq(settlement.liveAttestationCount(), 1, "a decisive settle adds one live attestation");
        assertEq(settlement.revokedAttestationCount(), 0, "nothing revoked yet");

        bytes32 m2 = bytes32("match-2");
        _open(m2, STAKE);
        _fundBoth(m2);
        vm.prank(attester);
        settlement.settleDraw(m2, HASH);
        assertEq(settlement.liveAttestationCount(), 2, "a second, distinct settle shape shares the counter");

        vm.prank(attester);
        settlement.revokeAttestation(MATCH);
        assertEq(settlement.liveAttestationCount(), 1, "revoke decrements the live count");
        assertEq(settlement.revokedAttestationCount(), 1, "revoke increments the revoked count");

        // Re-revoking the first is guarded (AlreadyRevoked), so the live count can never be
        // driven below the attests that raised it.
        vm.prank(attester);
        vm.expectRevert(abi.encodeWithSelector(MatchSettlement.AttestationAlreadyRevoked.selector, MATCH));
        settlement.revokeAttestation(MATCH);
        assertEq(settlement.liveAttestationCount(), 1, "a guarded re-revoke does not double-decrement");
        assertEq(settlement.revokedAttestationCount(), 1, "nor double-count the revoke");
    }

    /// @dev FM2 the field call sites count too: settleField (reputation-only) and
    ///      settleFieldWager (escrowed) are the other two _attestSettled call sites, so each
    ///      must also add exactly one to the shared live counter.
    function test_attestationCount_risesOnBothFieldSettlePaths() public {
        _enableVariable(RATING_CAP);

        // Field-wager first — _openFundField3 registers + funds the [alice, bob, dave] roster.
        _openFundField3(STAKE);
        uint256[] memory ps = _payouts3(150 ether, 90 ether, 60 ether);
        int256[] memory ds2 = _deltas(20 ether, 0, -20 ether);
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds2, HASH);
        assertEq(settlement.liveAttestationCount(), 1, "settleFieldWager adds one via the shared counter");

        // Reputation-only settleField (the other field call site) — dave already registered.
        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);
        vm.prank(attester);
        settlement.settleField(MATCH, ag, ds, HASH);
        assertEq(settlement.liveAttestationCount(), 2, "settleField adds the fourth call site's one");
        assertEq(settlement.revokedAttestationCount(), 0, "no revokes on the field paths");
    }

    /// @dev FM1: the attest is the LAST interaction, after the Settled fence and the payout.
    ///      A reentrant EAS re-entering settle() is rejected by the fence (MatchNotOpen) — no
    ///      double-settle, no double-pay, exactly one attestation.
    function test_settle_reentrantEasCannotDoubleSettle() public {
        ReentrantEAS reEas = new ReentrantEAS();
        MatchSettlement s = new MatchSettlement(address(reEas), address(registry), owner, REP_DELTA);
        _wireFresh(s, true);
        vm.prank(owner);
        s.setAttester(address(reEas), true); // grant so the reentry tests the fence, not the attester gate

        vm.prank(attester);
        s.openMatch(MATCH, alice, bob, STAKE);
        vm.prank(alice);
        s.fund(MATCH);
        vm.prank(bob);
        s.fund(MATCH);

        reEas.arm(s, MATCH, alice, HASH);
        uint256 aliceBefore = token.balanceOf(alice);

        vm.prank(attester);
        s.settle(MATCH, alice, HASH);

        assertTrue(reEas.reentered(), "reentry fired");
        assertTrue(reEas.reentryReverted(), "reentrant settle blocked by the Settled fence");
        assertEq(
            reEas.reentryRevertSelector(),
            MatchSettlement.MatchNotOpen.selector,
            "the fence, not another revert"
        );
        assertEq(reEas.attestCalls(), 1, "settled and attested exactly once");
        assertEq(token.balanceOf(alice), aliceBefore + 2 * STAKE, "winner paid the pot exactly once");
        assertEq(token.balanceOf(address(s)), 0, "no escrow drained beyond the pot");
        assertTrue(s.matchAttestationUid(MATCH) != bytes32(0), "single attestation persisted");
    }

    /// @dev FM2: settle-liveness is coupled to schema registration — a 1v1 settle before
    ///      registerSchema reverts SchemaNotSet (the guard fires before any effect).
    function test_settle_revertsWhenSchemaUnregistered() public {
        MatchSettlement s = new MatchSettlement(address(new MockEAS()), address(registry), owner, REP_DELTA);
        _wireFresh(s, false); // schema deliberately NOT registered

        vm.prank(attester);
        s.openMatch(MATCH, alice, bob, STAKE);
        vm.prank(alice);
        s.fund(MATCH);
        vm.prank(bob);
        s.fund(MATCH);

        vm.expectRevert(MatchSettlement.SchemaNotSet.selector);
        vm.prank(attester);
        s.settle(MATCH, alice, HASH);
    }

    /// @dev FM2 field variant: settleField reverts SchemaNotSet on the field schema guard.
    function test_settleField_revertsWhenSchemaUnregistered() public {
        MatchSettlement s = new MatchSettlement(address(new MockEAS()), address(registry), owner, REP_DELTA);
        _wireFresh(s, false);
        _registerAgent(dave, "dave-bot");

        address[] memory ag = _field(alice, bob, dave);
        int256[] memory ds = _deltas(30 ether, -10 ether, -20 ether);

        vm.expectRevert(MatchSettlement.SchemaNotSet.selector);
        vm.prank(attester);
        s.settleField(MATCH, ag, ds, HASH);
    }

    /// @dev FM2 wager variant: settleFieldWager reverts SchemaNotSet on the field-schema guard —
    ///      the last field-settle consumer that was unpinned (settle + settleField each have a twin).
    ///      Open+fund the 3-seat field first so the schema guard, not the status/NotFullyFunded checks
    ///      it sits above, is what reverts: a reorder that dropped it below them would redden this.
    function test_settleFieldWager_revertsWhenSchemaUnregistered() public {
        MatchSettlement s = new MatchSettlement(address(new MockEAS()), address(registry), owner, REP_DELTA);
        _wireFresh(s, false); // variable path enabled (maxRatingDelta set); field schema NOT registered
        _registerAgent(dave, "dave-bot");
        vm.prank(dave);
        token.approve(address(s), type(uint256).max); // _wireFresh approves only alice + bob

        address[] memory ag = _field(alice, bob, dave);
        vm.prank(attester);
        s.openFieldMatch(FIELD, ag, STAKE);
        vm.prank(alice);
        s.fundField(FIELD);
        vm.prank(bob);
        s.fundField(FIELD);
        vm.prank(dave);
        s.fundField(FIELD);

        // Valid, pot-conserving vectors so ONLY the schema guard can revert — with the guard deleted the
        // settle would attest under a zero schema and succeed, reddening this expectRevert.
        uint256[] memory ps = _payouts3(STAKE, STAKE, STAKE);
        int256[] memory ds = _deltas(20 ether, 0, -20 ether);

        vm.expectRevert(MatchSettlement.SchemaNotSet.selector);
        vm.prank(attester);
        s.settleFieldWager(FIELD, ps, ds, HASH);
    }

    /// @dev FM2 draw variant: settleDraw reverts SchemaNotSet on the 1v1 schema guard in
    ///      _applyDraw — the last settle shape that was unpinned (settle + both field twins
    ///      each have one). Open+fund both seats first so the schema guard, not the
    ///      status/NotFullyFunded checks it sits above, is what reverts: a reorder that
    ///      dropped it below them would redden this.
    function test_settleDraw_revertsWhenSchemaUnregistered() public {
        MatchSettlement s = new MatchSettlement(address(new MockEAS()), address(registry), owner, REP_DELTA);
        _wireFresh(s, false); // schema deliberately NOT registered

        vm.prank(attester);
        s.openMatch(MATCH, alice, bob, STAKE);
        vm.prank(alice);
        s.fund(MATCH);
        vm.prank(bob);
        s.fund(MATCH);

        vm.expectRevert(MatchSettlement.SchemaNotSet.selector);
        vm.prank(attester);
        s.settleDraw(MATCH, HASH);
    }

    // --- Ownable2Step ---

    function test_ownership_twoStepTransfer() public {
        address newOwner = address(0xDEAD);

        vm.prank(owner);
        settlement.transferOwnership(newOwner);

        // Pending; owner unchanged until accepted.
        assertEq(settlement.owner(), owner);
        assertEq(settlement.pendingOwner(), newOwner);

        // A non-pending account cannot accept.
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, carol));
        vm.prank(carol);
        settlement.acceptOwnership();

        vm.prank(newOwner);
        settlement.acceptOwnership();
        assertEq(settlement.owner(), newOwner);
        assertEq(settlement.pendingOwner(), address(0));

        // Old owner is now powerless over the attester allowlist.
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, owner));
        vm.prank(owner);
        settlement.setAttester(attester, true);
    }
}

/// @dev Shared payout callback so HookToken can drive a reentrancy attacker.
interface IPayoutReceiver {
    function onPayout() external;
}

/// @notice ERC-20 with an armed recipient hook (ERC777-style) that hands control to
///         the recipient mid-transfer so it can reenter. Disarmed by default so
///         setup transfers (register bond, fund stake) are normal.
contract HookToken is ERC20 {
    bool armed;

    constructor() ERC20("Hook", "HOOK") {
        _mint(msg.sender, 10_000_000 ether);
    }

    function arm() external {
        armed = true;
    }

    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        if (armed && to.code.length > 0) {
            armed = false; // one-shot, so the reentrant payout doesn't recurse
            IPayoutReceiver(to).onPayout();
        }
    }
}

/// @notice A registered agent + authorized attester that reenters settle() when it
///         receives its winning pot. Being an attester isolates the test to the CEI
///         status fence: the ONLY thing stopping a second payout is that settle marks
///         the match Settled before paying — not the attester gate. This fails if the
///         status update is moved after the token transfer.
contract ReentrantWinner is IPayoutReceiver {
    MatchSettlement settlement;
    AgentRegistry registry;
    HookToken token;
    bytes32 matchId;
    bytes32 replayHash;
    bool public reentered;
    bool public reentryReverted;

    constructor(MatchSettlement settlement_, AgentRegistry registry_, HookToken token_) {
        settlement = settlement_;
        registry = registry_;
        token = token_;
    }

    function setup(bytes32 matchId_, bytes32 replayHash_) external {
        matchId = matchId_;
        replayHash = replayHash_;
        token.approve(address(registry), type(uint256).max);
        token.approve(address(settlement), type(uint256).max);
        registry.register(bytes32("evil-bot"), 0);
    }

    function fund() external {
        settlement.fund(matchId);
    }

    function onPayout() external {
        reentered = true;
        try settlement.settle(matchId, address(this), replayHash) {}
        catch {
            reentryReverted = true;
        }
    }
}

contract MatchSettlementReentrancyTest is Test {
    MatchSettlement settlement;
    AgentRegistry registry;
    HookToken token;
    MockEAS eas;
    ReentrantWinner attacker;

    address owner = address(0xA11CE);
    address attester = address(0xA77E57E5);
    address bob = address(0xB0B);

    uint256 constant STAKE = 100 ether;
    uint256 constant REP_DELTA = 10 ether;
    bytes32 constant MATCH = bytes32("match-evil");
    bytes32 constant HASH = bytes32("replay-digest");

    function setUp() public {
        token = new HookToken();
        registry = new AgentRegistry(address(token), 0, owner);
        eas = new MockEAS();
        settlement = new MatchSettlement(address(eas), address(registry), owner, REP_DELTA);
        attacker = new ReentrantWinner(settlement, registry, token);

        vm.startPrank(owner);
        registry.setReputationWriter(address(settlement), true);
        settlement.setAttester(attester, true);
        settlement.setAttester(address(attacker), true); // grant so reentry tests the fence, not the gate
        settlement.registerSchema(address(new MockSchemaRegistry()));
        vm.stopPrank();

        // bob: a normal registered, funded opponent.
        token.transfer(bob, 10_000 ether);
        vm.startPrank(bob);
        token.approve(address(registry), type(uint256).max);
        token.approve(address(settlement), type(uint256).max);
        registry.register(bytes32("bob-bot"), 0);
        vm.stopPrank();

        // Fund the attacker so it can register + post its stake.
        token.transfer(address(attacker), 10_000 ether);
        attacker.setup(MATCH, HASH);

        vm.prank(attester);
        settlement.openMatch(MATCH, address(attacker), bob, STAKE);
        attacker.fund();
        vm.prank(bob);
        settlement.fund(MATCH);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE);
    }

    function test_settle_reentrantWinnerCannotDoubleClaim() public {
        uint256 before = token.balanceOf(address(attacker));
        token.arm();
        vm.prank(attester);
        settlement.settle(MATCH, address(attacker), HASH);

        assertTrue(attacker.reentered(), "reentry fired");
        assertTrue(attacker.reentryReverted(), "reentry blocked by the CEI status fence");
        assertEq(token.balanceOf(address(attacker)), before + 2 * STAKE, "won the pot exactly once");
        assertEq(token.balanceOf(address(settlement)), 0, "no escrow drained beyond the pot");
    }
}

/// @notice A registered agent that reenters refundExpired when it receives its expired
///         refund. refundExpired is permissionless, so reentering it isolates the
///         _refundBoth CEI fence shared by cancelMatch and refundExpired: the only thing
///         stopping a second refund is that the helper flips the match terminal and
///         clears the funded flags BEFORE the token transfer. Fails if those effects
///         move after the transfer.
contract ReentrantRefundClaimer is IPayoutReceiver {
    MatchSettlement settlement;
    AgentRegistry registry;
    HookToken token;
    bytes32 matchId;
    bool public reentered;
    bool public reentryReverted;

    constructor(MatchSettlement settlement_, AgentRegistry registry_, HookToken token_) {
        settlement = settlement_;
        registry = registry_;
        token = token_;
    }

    function setup(bytes32 matchId_) external {
        matchId = matchId_;
        token.approve(address(registry), type(uint256).max);
        token.approve(address(settlement), type(uint256).max);
        registry.register(bytes32("refund-bot"), 0);
    }

    function fund() external {
        settlement.fund(matchId);
    }

    function onPayout() external {
        reentered = true;
        try settlement.refundExpired(matchId) {}
        catch {
            reentryReverted = true;
        }
    }
}

contract MatchSettlementRefundReentrancyTest is Test {
    MatchSettlement settlement;
    AgentRegistry registry;
    HookToken token;
    ReentrantRefundClaimer attacker;

    address owner = address(0xA11CE);
    address attester = address(0xA77E57E5);
    address bob = address(0xB0B);

    uint256 constant STAKE = 100 ether;
    uint256 constant REP_DELTA = 10 ether;
    bytes32 constant MATCH = bytes32("match-refund-evil");

    function setUp() public {
        token = new HookToken();
        registry = new AgentRegistry(address(token), 0, owner);
        settlement = new MatchSettlement(address(new MockEAS()), address(registry), owner, REP_DELTA);
        attacker = new ReentrantRefundClaimer(settlement, registry, token);

        vm.startPrank(owner);
        registry.setReputationWriter(address(settlement), true);
        settlement.setAttester(attester, true);
        vm.stopPrank();

        token.transfer(bob, 10_000 ether);
        vm.startPrank(bob);
        token.approve(address(registry), type(uint256).max);
        token.approve(address(settlement), type(uint256).max);
        registry.register(bytes32("bob-bot"), 0);
        vm.stopPrank();

        token.transfer(address(attacker), 10_000 ether);
        attacker.setup(MATCH);

        vm.prank(attester);
        settlement.openMatch(MATCH, address(attacker), bob, STAKE);
        attacker.fund();
        vm.prank(bob);
        settlement.fund(MATCH);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE);
    }

    function test_refundExpired_reentrantClaimerCannotDoubleRefund() public {
        (,,,,,,,, uint64 deadline) = settlement.matches(MATCH);
        vm.warp(deadline);
        uint256 attackerBefore = token.balanceOf(address(attacker));
        uint256 bobBefore = token.balanceOf(bob);

        token.arm();
        settlement.refundExpired(MATCH);

        assertTrue(attacker.reentered(), "reentry fired");
        assertTrue(attacker.reentryReverted(), "reentry blocked by the CEI terminal-state fence");
        assertEq(
            token.balanceOf(address(attacker)), attackerBefore + STAKE, "refunded its stake exactly once"
        );
        assertEq(token.balanceOf(bob), bobBefore + STAKE, "opponent still refunded after the reentry attempt");
        assertEq(token.balanceOf(address(settlement)), 0, "no escrow drained beyond the two stakes");
    }
}

/// @notice A field-roster member that reenters refundFieldExpired when it receives its
///         expired refund. refundFieldExpired is permissionless, so reentering it isolates
///         the _refundField CEI fence shared by cancelFieldMatch and refundFieldExpired: the
///         only thing stopping a second refund is that the helper flips the field terminal
///         and clears fundedBits BEFORE any transfer. Fails if those effects move after the
///         transfer (the N-seat analog of ReentrantRefundClaimer).
contract ReentrantFieldClaimer is IPayoutReceiver {
    MatchSettlement settlement;
    AgentRegistry registry;
    HookToken token;
    bytes32 matchId;
    bool public reentered;
    bool public reentryReverted;

    constructor(MatchSettlement settlement_, AgentRegistry registry_, HookToken token_) {
        settlement = settlement_;
        registry = registry_;
        token = token_;
    }

    function setup(bytes32 matchId_) external {
        matchId = matchId_;
        token.approve(address(registry), type(uint256).max);
        token.approve(address(settlement), type(uint256).max);
        registry.register(bytes32("field-evil"), 0);
    }

    function fund() external {
        settlement.fundField(matchId);
    }

    function onPayout() external {
        reentered = true;
        try settlement.refundFieldExpired(matchId) {}
        catch {
            reentryReverted = true;
        }
    }
}

contract MatchSettlementFieldRefundReentrancyTest is Test {
    MatchSettlement settlement;
    AgentRegistry registry;
    HookToken token;
    ReentrantFieldClaimer attacker;

    address owner = address(0xA11CE);
    address attester = address(0xA77E57E5);
    address bob = address(0xB0B);

    uint256 constant STAKE = 100 ether;
    uint256 constant REP_DELTA = 10 ether;
    bytes32 constant FIELD = bytes32("field-refund-evil");

    function setUp() public {
        token = new HookToken();
        registry = new AgentRegistry(address(token), 0, owner);
        settlement = new MatchSettlement(address(new MockEAS()), address(registry), owner, REP_DELTA);
        attacker = new ReentrantFieldClaimer(settlement, registry, token);

        vm.startPrank(owner);
        registry.setReputationWriter(address(settlement), true);
        settlement.setAttester(attester, true);
        vm.stopPrank();

        token.transfer(bob, 10_000 ether);
        vm.startPrank(bob);
        token.approve(address(registry), type(uint256).max);
        token.approve(address(settlement), type(uint256).max);
        registry.register(bytes32("bob-bot"), 0);
        vm.stopPrank();

        token.transfer(address(attacker), 10_000 ether);
        attacker.setup(FIELD);

        address[] memory ag = new address[](2);
        ag[0] = address(attacker);
        ag[1] = bob;
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, ag, STAKE);
        attacker.fund();
        vm.prank(bob);
        settlement.fundField(FIELD);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE);
    }

    function test_refundField_reentrantClaimerCannotDoubleRefund() public {
        (,,, uint64 deadline,) = settlement.fieldMatches(FIELD);
        vm.warp(deadline);
        uint256 attackerBefore = token.balanceOf(address(attacker));
        uint256 bobBefore = token.balanceOf(bob);

        token.arm();
        settlement.refundFieldExpired(FIELD);

        assertTrue(attacker.reentered(), "reentry fired");
        assertTrue(attacker.reentryReverted(), "reentry blocked by the CEI terminal-state fence");
        assertEq(
            token.balanceOf(address(attacker)), attackerBefore + STAKE, "refunded its stake exactly once"
        );
        assertEq(token.balanceOf(bob), bobBefore + STAKE, "peer still refunded after the reentry attempt");
        assertEq(token.balanceOf(address(settlement)), 0, "no escrow drained beyond the two stakes");
    }
}

contract MatchSettlementFieldSettleReentrancyTest is Test {
    MatchSettlement settlement;
    AgentRegistry registry;
    HookToken token;
    ReentrantFieldClaimer attacker;

    address owner = address(0xA11CE);
    address attester = address(0xA77E57E5);
    address bob = address(0xB0B);

    uint256 constant STAKE = 100 ether;
    uint256 constant REP_DELTA = 10 ether;
    uint256 constant RATING_CAP = 50 ether;
    bytes32 constant FIELD = bytes32("field-settle-evil");
    bytes32 constant HASH = bytes32("replay-digest");

    function setUp() public {
        token = new HookToken();
        registry = new AgentRegistry(address(token), 0, owner);
        settlement = new MatchSettlement(address(new MockEAS()), address(registry), owner, REP_DELTA);
        attacker = new ReentrantFieldClaimer(settlement, registry, token);

        vm.startPrank(owner);
        registry.setReputationWriter(address(settlement), true);
        settlement.setAttester(attester, true);
        settlement.setMaxRatingDelta(RATING_CAP); // wager settle writes per-seat reputation
        settlement.registerSchema(address(new MockSchemaRegistry()));
        vm.stopPrank();

        token.transfer(bob, 10_000 ether);
        vm.startPrank(bob);
        token.approve(address(registry), type(uint256).max);
        token.approve(address(settlement), type(uint256).max);
        registry.register(bytes32("bob-bot"), 0);
        vm.stopPrank();

        token.transfer(address(attacker), 10_000 ether);
        attacker.setup(FIELD);

        address[] memory ag = new address[](2);
        ag[0] = address(attacker);
        ag[1] = bob;
        vm.prank(attester);
        settlement.openFieldMatch(FIELD, ag, STAKE);
        attacker.fund();
        vm.prank(bob);
        settlement.fundField(FIELD);
        assertEq(token.balanceOf(address(settlement)), 2 * STAKE);
    }

    /// @dev settleFieldWager's CEI fence in isolation. The attacker reenters the
    ///      permissionless refundFieldExpired when it receives its pot share, AFTER the
    ///      deadline — so the reentrant refund would double-drain the pot if the settle had
    ///      not flipped the match to `Settled` BEFORE paying. The terminal fence is the only
    ///      thing stopping it; this fails if the status flip moves after the payout loop.
    function test_settleFieldWager_reentrantClaimerCannotDoubleDrain() public {
        (,,, uint64 deadline,) = settlement.fieldMatches(FIELD);
        vm.warp(deadline); // past the deadline: a reentrant refundFieldExpired is otherwise live
        uint256 attackerBefore = token.balanceOf(address(attacker));
        uint256 bobBefore = token.balanceOf(bob);

        uint256[] memory ps = new uint256[](2);
        ps[0] = STAKE + 10 ether; // attacker (seat 0) — non-zero so the payout hook fires
        ps[1] = STAKE - 10 ether; // bob (seat 1); sum == 2*STAKE (the pot)
        int256[] memory ds = new int256[](2);
        ds[0] = 20 ether;
        ds[1] = -20 ether;

        token.arm();
        vm.prank(attester);
        settlement.settleFieldWager(FIELD, ps, ds, HASH);

        assertTrue(attacker.reentered(), "reentry fired on payout");
        assertTrue(attacker.reentryReverted(), "reentry blocked by the CEI terminal-state fence");
        assertEq(
            token.balanceOf(address(attacker)),
            attackerBefore + STAKE + 10 ether,
            "attacker paid its share once"
        );
        assertEq(token.balanceOf(bob), bobBefore + STAKE - 10 ether, "peer paid its share once");
        assertEq(token.balanceOf(address(settlement)), 0, "pot distributed exactly once, no double-drain");
    }
}
