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
    event AttesterSet(address indexed attester, bool authorized);
    event ReputationDeltaSet(uint256 reputationDelta);

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
