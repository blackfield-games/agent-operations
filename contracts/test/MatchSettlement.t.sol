// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {MatchSettlement} from "../src/MatchSettlement.sol";
import {AgentRegistry} from "../src/AgentRegistry.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 10_000_000 ether);
    }
}

contract MatchSettlementTest is Test {
    MatchSettlement settlement;
    AgentRegistry registry;
    MockToken token;

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
    event AttesterSet(address indexed attester, bool authorized);
    event ReputationDeltaSet(uint256 reputationDelta);
    event MaxRatingDeltaSet(uint256 maxRatingDelta);
    event SettleWindowSet(uint64 settleWindow);

    function setUp() public {
        token = new MockToken();
        registry = new AgentRegistry(address(token), 0, owner);
        settlement = new MatchSettlement(address(registry), owner, REP_DELTA);

        vm.startPrank(owner);
        registry.setReputationWriter(address(settlement), true);
        settlement.setAttester(attester, true);
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
        new MatchSettlement(address(0), owner, REP_DELTA);
    }

    function test_constructor_revertsZeroReputationDelta() public {
        vm.expectRevert(MatchSettlement.ZeroReputationDelta.selector);
        new MatchSettlement(address(registry), owner, 0);
    }

    function test_constructor_revertsReputationDeltaTooLarge() public {
        vm.expectRevert(MatchSettlement.ReputationDeltaTooLarge.selector);
        new MatchSettlement(address(registry), owner, uint256(type(int256).max) + 1);
    }

    function test_constructor_revertsZeroToken() public {
        AgentRegistry zeroTokenRegistry = new AgentRegistry(address(0), 0, owner);
        vm.expectRevert(MatchSettlement.ZeroToken.selector);
        new MatchSettlement(address(zeroTokenRegistry), owner, REP_DELTA);
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
        settlement = new MatchSettlement(address(registry), owner, REP_DELTA);
        attacker = new ReentrantWinner(settlement, registry, token);

        vm.startPrank(owner);
        registry.setReputationWriter(address(settlement), true);
        settlement.setAttester(attester, true);
        settlement.setAttester(address(attacker), true); // grant so reentry tests the fence, not the gate
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
        settlement = new MatchSettlement(address(registry), owner, REP_DELTA);
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
