// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {MatchSettlement} from "../src/MatchSettlement.sol";
import {AgentRegistry} from "../src/AgentRegistry.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 100_000_000 ether);
    }
}

/// @notice Stateful handler driving the full match lifecycle (open / fund / reclaim /
///         settle / draw / cancel) against the real AgentRegistry, so the invariants
///         hold over an arbitrary interleaving. Guard conditions early-return rather
///         than revert, so every accepted call actually mutates state.
contract MatchSettlementHandler is Test {
    MatchSettlement public settlement;
    AgentRegistry public registry;
    MockToken public token;

    address[] public actors;
    bytes32[] public matchIds;
    uint256 internal nonce;
    /// @notice Matches resolved via settle or settleDraw (NOT cancel). Each bumps both
    ///         agents' `matchesSettled` by one, so the per-agent sum is twice this.
    uint256 public ghost_settledCount;

    constructor(
        MatchSettlement settlement_,
        AgentRegistry registry_,
        MockToken token_,
        address[] memory actors_
    ) {
        settlement = settlement_;
        registry = registry_;
        token = token_;
        actors = actors_;
    }

    function _match(bytes32 id)
        internal
        view
        returns (address a, address b, uint256 stake, bool aF, bool bF, MatchSettlement.Status s)
    {
        (a, b, stake,,, aF, bF, s,) = settlement.matches(id);
    }

    function openMatch(uint256 aSeed, uint256 bSeed, uint256 stake) external {
        address a = actors[aSeed % actors.length];
        address b = actors[bSeed % actors.length];
        if (a == b) return;
        stake = bound(stake, 0, 100 ether);
        bytes32 id = keccak256(abi.encode("match", nonce++));
        settlement.openMatch(id, a, b, stake);
        matchIds.push(id);
    }

    function fund(uint256 mSeed, bool seatB) external {
        if (matchIds.length == 0) return;
        bytes32 id = matchIds[mSeed % matchIds.length];
        (address a, address b, uint256 stake, bool aF, bool bF, MatchSettlement.Status s) = _match(id);
        if (s != MatchSettlement.Status.Open || stake == 0) return;
        address who = seatB ? b : a;
        if (seatB ? bF : aF) return;
        if (token.balanceOf(who) < stake) return;
        vm.prank(who);
        settlement.fund(id);
    }

    function reclaim(uint256 mSeed, bool seatB) external {
        if (matchIds.length == 0) return;
        bytes32 id = matchIds[mSeed % matchIds.length];
        (address a, address b,, bool aF, bool bF, MatchSettlement.Status s) = _match(id);
        if (s != MatchSettlement.Status.Open) return;
        bool mine = seatB ? bF : aF;
        bool other = seatB ? aF : bF;
        if (!mine || other) return;
        vm.prank(seatB ? b : a);
        settlement.reclaim(id);
    }

    function settle(uint256 mSeed, bool winnerB) external {
        if (matchIds.length == 0) return;
        bytes32 id = matchIds[mSeed % matchIds.length];
        (address a, address b, uint256 stake, bool aF, bool bF, MatchSettlement.Status s) = _match(id);
        if (s != MatchSettlement.Status.Open) return;
        if (stake != 0 && !(aF && bF)) return;
        settlement.settle(id, winnerB ? b : a, keccak256(abi.encode("replay", id)));
        ghost_settledCount++;
    }

    function settleDraw(uint256 mSeed) external {
        if (matchIds.length == 0) return;
        bytes32 id = matchIds[mSeed % matchIds.length];
        (,, uint256 stake, bool aF, bool bF, MatchSettlement.Status s) = _match(id);
        if (s != MatchSettlement.Status.Open) return;
        if (stake != 0 && !(aF && bF)) return;
        settlement.settleDraw(id, keccak256(abi.encode("replay", id)));
        ghost_settledCount++;
    }

    /// @notice Variable decisive settle with a fuzzed winner gain bounded into the valid
    ///         [0, cap] range, interleaved with the fixed path. Holding zero-sum over this
    ///         proves the contract's +d/-d split conserves reputation for an ARBITRARY
    ///         attester-supplied magnitude, not just the fixed one.
    function settleRanked(uint256 mSeed, bool winnerB, int256 ratingDelta) external {
        if (matchIds.length == 0) return;
        bytes32 id = matchIds[mSeed % matchIds.length];
        (address a, address b, uint256 stake, bool aF, bool bF, MatchSettlement.Status s) = _match(id);
        if (s != MatchSettlement.Status.Open) return;
        if (stake != 0 && !(aF && bF)) return;
        ratingDelta = bound(ratingDelta, int256(0), int256(settlement.maxRatingDelta()));
        settlement.settleRanked(id, winnerB ? b : a, keccak256(abi.encode("replay", id)), ratingDelta);
        ghost_settledCount++;
    }

    /// @notice Variable draw settle with a fuzzed SIGNED agentA delta bounded into
    ///         [-cap, cap] — the favourite-moves-down case the fixed draw can't express.
    ///         A negative delta exercises the contract's negation of a negative value;
    ///         zero-sum must still hold (agentB gets the positive negation).
    function settleDrawRanked(uint256 mSeed, int256 ratingDeltaA) external {
        if (matchIds.length == 0) return;
        bytes32 id = matchIds[mSeed % matchIds.length];
        (,, uint256 stake, bool aF, bool bF, MatchSettlement.Status s) = _match(id);
        if (s != MatchSettlement.Status.Open) return;
        if (stake != 0 && !(aF && bF)) return;
        int256 cap = int256(settlement.maxRatingDelta());
        ratingDeltaA = bound(ratingDeltaA, -cap, cap);
        settlement.settleDrawRanked(id, keccak256(abi.encode("replay", id)), ratingDeltaA);
        ghost_settledCount++;
    }

    function cancel(uint256 mSeed) external {
        if (matchIds.length == 0) return;
        bytes32 id = matchIds[mSeed % matchIds.length];
        (,,,,, MatchSettlement.Status s) = _match(id);
        if (s != MatchSettlement.Status.Open) return;
        settlement.cancelMatch(id);
    }

    /// @notice Deadline self-refund. Warps FORWARD to the match's frozen deadline (never
    ///         backward — block.timestamp stays monotonic across the sequence) so the
    ///         permissionless refund is allowed, then voids the match exactly like
    ///         cancel — no reputation, no settled-count bump. Holding the escrow and
    ///         zero-sum invariants over this path proves the new void can't double-refund
    ///         or leak a stray reputation write.
    function refundExpired(uint256 mSeed) external {
        if (matchIds.length == 0) return;
        bytes32 id = matchIds[mSeed % matchIds.length];
        (,,,,, MatchSettlement.Status s) = _match(id);
        if (s != MatchSettlement.Status.Open) return;
        (,,,,,,,, uint64 deadline) = settlement.matches(id);
        if (block.timestamp < deadline) vm.warp(deadline);
        settlement.refundExpired(id);
    }

    /// @notice The escrow the contract MUST be holding: per open match, `stake` for
    ///         each funded seat. Settled/cancelled matches hold nothing.
    function expectedEscrow() external view returns (uint256 total) {
        for (uint256 i = 0; i < matchIds.length; i++) {
            (,, uint256 stake, bool aF, bool bF, MatchSettlement.Status s) = _match(matchIds[i]);
            if (s != MatchSettlement.Status.Open) continue;
            if (aF) total += stake;
            if (bF) total += stake;
        }
    }

    function sumReputation() external view returns (int256 total) {
        for (uint256 i = 0; i < actors.length; i++) {
            total += registry.reputationOf(actors[i]);
        }
    }

    function sumMatchesSettled() external view returns (uint256 total) {
        for (uint256 i = 0; i < actors.length; i++) {
            (,, uint64 m,,,) = registry.agents(actors[i]);
            total += m;
        }
    }
}

contract MatchSettlementInvariantTest is Test {
    MatchSettlement settlement;
    AgentRegistry registry;
    MockToken token;
    MatchSettlementHandler handler;

    address owner = address(0xA11CE);
    uint256 constant REP_DELTA = 7 ether;
    uint256 constant RATING_CAP = 1000 ether;

    function setUp() public {
        token = new MockToken();
        registry = new AgentRegistry(address(token), 0, owner);
        settlement = new MatchSettlement(address(registry), owner, REP_DELTA);

        address[] memory actors = new address[](4);
        actors[0] = address(0xB0B);
        actors[1] = address(0xCA201);
        actors[2] = address(0xDA4E);
        actors[3] = address(0xE3E);

        handler = new MatchSettlementHandler(settlement, registry, token, actors);

        vm.startPrank(owner);
        registry.setReputationWriter(address(settlement), true);
        settlement.setAttester(address(handler), true);
        settlement.setMaxRatingDelta(RATING_CAP); // enable the variable settle handlers
        vm.stopPrank();

        for (uint256 i = 0; i < actors.length; i++) {
            assertTrue(token.transfer(actors[i], 1_000_000 ether));
            vm.startPrank(actors[i]);
            token.approve(address(settlement), type(uint256).max);
            registry.register(bytes32("agent"), 0);
            vm.stopPrank();
        }

        targetContract(address(handler));
    }

    /// @dev Escrow conservation: the contract's $BLCKFLD balance is EXACTLY the stake
    ///      held for funded seats of still-open matches — never more (no value minted)
    ///      and never less (no stake stranded). Catches a double-pay, a missed refund,
    ///      or a release on a partially-funded escrow.
    function invariant_escrowMatchesFundedSeats() public view {
        assertEq(token.balanceOf(address(settlement)), handler.expectedEscrow());
    }

    /// @dev Reputation is zero-sum: every settle — fixed or variable, decisive or draw —
    ///      moves +d to one participant and exactly -d to the other (a fixed draw moves 0;
    ///      a variable draw moves the favourite down and the underdog up by the same |d|),
    ///      so total standing across all agents is invariantly zero for ANY bounded delta
    ///      the attester supplies. A one-sided write, a wrong negation, or a double-count
    ///      breaks it.
    function invariant_reputationIsZeroSum() public view {
        assertEq(handler.sumReputation(), int256(0));
    }

    /// @dev Each settled match (decisive or draw, never a cancel) counts exactly once
    ///      for BOTH agents, so the per-agent `matchesSettled` sum is twice the settled
    ///      count — proving no settlement double-counts or under-counts the match.
    function invariant_matchesSettledCount() public view {
        assertEq(handler.sumMatchesSettled(), 2 * handler.ghost_settledCount());
    }
}
