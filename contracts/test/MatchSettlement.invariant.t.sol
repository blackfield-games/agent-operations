// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {MatchSettlement, IEAS, ISchemaRegistry} from "../src/MatchSettlement.sol";
import {AgentRegistry} from "../src/AgentRegistry.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 100_000_000 ether);
    }
}

/// @dev A non-reverting EAS + registry so every settle path in the handler actually
///      attests and mutates state (an unregistered schema would revert SchemaNotSet and
///      silently make the settle invariants vacuous). The invariants don't read the
///      attestation, only that settling succeeds, so the uid derivation is unobserved.
contract MockEAS is IEAS {
    function attest(IEAS.AttestationRequest calldata request) external payable returns (bytes32) {
        return keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
    }
}

contract MockSchemaRegistry is ISchemaRegistry {
    function register(string calldata schema, address, bool) external pure returns (bytes32) {
        return keccak256(bytes(schema));
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
    bytes32[] public fieldMatchIds;
    uint256 internal nonce;
    /// @notice Matches resolved via settle or settleDraw (NOT cancel). Each bumps both
    ///         agents' `matchesSettled` by one, so the per-agent sum is twice this.
    uint256 public ghost_settledCount;
    /// @notice Seats settled via settleFieldWager. Each seat's recordMatchResult bumps its
    ///         agent's `matchesSettled` once, so a field settle adds `seats` (not a multiple
    ///         of two) to the per-agent sum — tracked apart from the 1v1 count.
    uint256 public ghost_fieldSeatsSettled;

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

    function _fieldMatch(bytes32 id)
        internal
        view
        returns (uint256 stake, uint256 fundedBits, MatchSettlement.Status s)
    {
        (stake, fundedBits, s,,) = settlement.fieldMatches(id);
    }

    /// @notice Open an N-seat field-wager escrow over a distinct prefix of the registered
    ///         actors (all are registered + approved in setUp), interleaved with the 1v1
    ///         lifecycle. The field path writes no reputation, so only the escrow invariant
    ///         is exercised by it — its job here is to prove per-seat funded escrow is
    ///         conserved over an arbitrary fund/reclaim/cancel/expire interleaving.
    function openFieldMatch(uint256 nSeed, uint256 stake) external {
        uint256 n = bound(nSeed, 2, actors.length);
        address[] memory ag = new address[](n);
        for (uint256 i = 0; i < n; i++) {
            ag[i] = actors[i]; // a distinct prefix -> a valid roster
        }
        stake = bound(stake, 0, 100 ether);
        bytes32 id = keccak256(abi.encode("field", nonce++));
        settlement.openFieldMatch(id, ag, stake);
        fieldMatchIds.push(id);
    }

    function fundFieldSeat(uint256 mSeed, uint256 seatSeed) external {
        if (fieldMatchIds.length == 0) return;
        bytes32 id = fieldMatchIds[mSeed % fieldMatchIds.length];
        (uint256 stake, uint256 fundedBits, MatchSettlement.Status s) = _fieldMatch(id);
        if (s != MatchSettlement.Status.Open || stake == 0) return;
        address[] memory ag = settlement.fieldRoster(id);
        uint256 seat = seatSeed % ag.length;
        if (fundedBits & (1 << seat) != 0) return; // seat already funded
        address who = ag[seat];
        if (token.balanceOf(who) < stake) return;
        vm.prank(who);
        settlement.fundField(id);
    }

    function reclaimFieldSeat(uint256 mSeed, uint256 seatSeed) external {
        if (fieldMatchIds.length == 0) return;
        bytes32 id = fieldMatchIds[mSeed % fieldMatchIds.length];
        (, uint256 fundedBits, MatchSettlement.Status s) = _fieldMatch(id);
        if (s != MatchSettlement.Status.Open) return;
        address[] memory ag = settlement.fieldRoster(id);
        uint256 bit = 1 << (seatSeed % ag.length);
        if (fundedBits & bit == 0 || fundedBits != bit) return; // funded only if it is the SOLE funder
        vm.prank(ag[seatSeed % ag.length]);
        settlement.reclaimField(id);
    }

    function cancelFieldMatch(uint256 mSeed) external {
        if (fieldMatchIds.length == 0) return;
        bytes32 id = fieldMatchIds[mSeed % fieldMatchIds.length];
        (,, MatchSettlement.Status s) = _fieldMatch(id);
        if (s != MatchSettlement.Status.Open) return;
        settlement.cancelFieldMatch(id);
    }

    function refundFieldExpired(uint256 mSeed) external {
        if (fieldMatchIds.length == 0) return;
        bytes32 id = fieldMatchIds[mSeed % fieldMatchIds.length];
        (,, MatchSettlement.Status s) = _fieldMatch(id);
        if (s != MatchSettlement.Status.Open) return;
        (,,, uint64 deadline,) = settlement.fieldMatches(id);
        if (block.timestamp < deadline) vm.warp(deadline);
        settlement.refundFieldExpired(id);
    }

    /// @notice Settle a field wager: winner-takes-all pot split (sum == pot) plus a zero-sum
    ///         +d/-d reputation pair, interleaved with the rest. The handler FULLY FUNDS the
    ///         chosen field inline first — the random sequencer effectively never assembles a
    ///         full fund-then-settle on its own, so without this the settle path is dead and
    ///         the field-wager terms of the invariants are never exercised. Holding the
    ///         escrow invariant over this proves the pot payout releases EXACTLY the pot (the
    ///         settled match then holds nothing), the zero-sum invariant proves the per-seat
    ///         deltas conserve reputation, and each seat's recordMatchResult bump is tracked
    ///         in `ghost_fieldSeatsSettled` for the settled-count invariant.
    function settleFieldWager(uint256 winnerSeed, int256 dSeed) external {
        if (fieldMatchIds.length == 0) return;
        // Prefer the most recently opened field (likeliest still Open) so the path is reached
        // reliably; scan back for any Open, non-zero-stake field whose seats can all pay.
        if (settlement.maxRatingDelta() == 0) return;
        for (uint256 k = fieldMatchIds.length; k > 0; k--) {
            bytes32 id = fieldMatchIds[k - 1];
            (uint256 stake, uint256 fundedBits, MatchSettlement.Status s) = _fieldMatch(id);
            if (s != MatchSettlement.Status.Open || stake == 0) continue;
            address[] memory ag = settlement.fieldRoster(id);
            uint256 n = ag.length;

            bool canFundAll = true;
            for (uint256 i = 0; i < n; i++) {
                if (fundedBits & (1 << i) == 0 && token.balanceOf(ag[i]) < stake) {
                    canFundAll = false;
                    break;
                }
            }
            if (!canFundAll) continue;
            for (uint256 i = 0; i < n; i++) {
                if (fundedBits & (1 << i) != 0) continue;
                vm.prank(ag[i]);
                settlement.fundField(id);
            }

            uint256[] memory ps = new uint256[](n);
            ps[winnerSeed % n] = stake * n; // winner-takes-all: a conserving split (sum == pot)
            int256[] memory ds = new int256[](n);
            int256 d = bound(dSeed, int256(0), int256(settlement.maxRatingDelta()));
            ds[0] = d; // n >= 2 from open, so seats 0/1 carry the zero-sum pair
            ds[1] = -d;
            settlement.settleFieldWager(id, ps, ds, keccak256(abi.encode("field-replay", id)));
            ghost_fieldSeatsSettled += n;
            return;
        }
    }

    /// @notice The escrow the contract MUST be holding: per open 1v1 match, `stake` for each
    ///         funded seat; per open field match, `stake` for each set bit of `fundedBits`.
    ///         Settled/cancelled/expired matches of either kind hold nothing.
    function expectedEscrow() external view returns (uint256 total) {
        for (uint256 i = 0; i < matchIds.length; i++) {
            (,, uint256 stake, bool aF, bool bF, MatchSettlement.Status s) = _match(matchIds[i]);
            if (s != MatchSettlement.Status.Open) continue;
            if (aF) total += stake;
            if (bF) total += stake;
        }
        for (uint256 i = 0; i < fieldMatchIds.length; i++) {
            bytes32 id = fieldMatchIds[i];
            (uint256 stake, uint256 fundedBits, MatchSettlement.Status s) = _fieldMatch(id);
            if (s != MatchSettlement.Status.Open) continue;
            address[] memory ag = settlement.fieldRoster(id);
            for (uint256 j = 0; j < ag.length; j++) {
                if (fundedBits & (1 << j) != 0) total += stake;
            }
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
        settlement = new MatchSettlement(address(new MockEAS()), address(registry), owner, REP_DELTA);

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
        settlement.registerSchema(address(new MockSchemaRegistry()));
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

    /// @dev Each settled 1v1 match (decisive or draw, never a cancel) counts exactly once
    ///      for BOTH agents (so it adds two to the per-agent sum), and each field-wager
    ///      settle counts once for each of its seats — so the per-agent `matchesSettled` sum
    ///      is `2 * 1v1-settles + field-seats-settled`, proving no settlement double-counts
    ///      or under-counts a played match across either kind.
    function invariant_matchesSettledCount() public view {
        assertEq(
            handler.sumMatchesSettled(), 2 * handler.ghost_settledCount() + handler.ghost_fieldSeatsSettled()
        );
    }

    /// @dev Coverage guard: prove the field-wager SETTLE path was actually exercised (not just
    ///      open/fund/recover). Without the handler's inline full-funding the random sequencer
    ///      effectively never reaches settleFieldWager, which would silently make the
    ///      field-wager terms of the invariants above a no-op. Fails if that path goes dead.
    function afterInvariant() public view {
        assertGt(handler.ghost_fieldSeatsSettled(), 0, "field-wager settle path never exercised");
    }
}
