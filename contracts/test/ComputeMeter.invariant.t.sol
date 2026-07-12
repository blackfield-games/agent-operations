// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ComputeMeter} from "../src/ComputeMeter.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

/// @notice Stateful handler that drives bounded deposits and spends against the meter
///         while tracking ghost totals so invariants can be checked over a run.
contract ComputeMeterHandler is Test {
    ComputeMeter public meter;
    MockToken public token;

    address[] public actors;
    uint256 public ghost_totalDeposited;
    uint256 public ghost_totalSpent;
    bytes32[] internal spendOnceJobIds;
    uint256 internal spendOnceNonce;
    /// @notice Set true if a replay of an already-spent jobId ever debits again
    ///         (the fence failed). The at-most-once invariant asserts it stays false.
    bool public ghost_replayDoubleSpent;

    constructor(ComputeMeter meter_, MockToken token_, address[] memory actors_) {
        meter = meter_;
        token = token_;
        actors = actors_;
        // Each actor pre-approves the meter for unlimited transfers.
        for (uint256 i = 0; i < actors.length; i++) {
            vm.prank(actors[i]);
            token.approve(address(meter), type(uint256).max);
        }
    }

    function deposit(uint256 actorSeed, uint256 amount) external {
        address actor = actors[actorSeed % actors.length];
        uint256 bal = token.balanceOf(actor);
        if (bal == 0) return;
        amount = bound(amount, 0, bal);
        if (amount == 0) return;

        vm.prank(actor);
        meter.deposit(amount);
        ghost_totalDeposited += amount;
    }

    /// @notice Sponsor-deposit: one actor burns its tokens to credit another actor's
    ///         meter. The burn is a deposit like any other, so the conservation
    ///         invariants must hold identically whether credit was self- or
    ///         sponsor-funded (FM2 — the two entry points share one accounting core).
    function depositFor(uint256 sponsorSeed, uint256 buyerSeed, uint256 amount) external {
        address sponsor = actors[sponsorSeed % actors.length];
        address buyer = actors[buyerSeed % actors.length];
        uint256 bal = token.balanceOf(sponsor);
        if (bal == 0) return;
        amount = bound(amount, 0, bal);
        if (amount == 0) return;

        vm.prank(sponsor);
        meter.depositFor(buyer, amount);
        ghost_totalDeposited += amount;
    }

    function spend(uint256 buyerSeed, uint256 amount) external {
        address buyer = actors[buyerSeed % actors.length];
        uint256 c = meter.credit(buyer);
        if (c == 0) return;
        amount = bound(amount, 0, c);
        if (amount == 0) return;

        // The handler itself is the authorized spender (set in the test setUp).
        vm.prank(address(this));
        meter.spend(buyer, amount, keccak256(abi.encode(buyerSeed, amount)));
        ghost_totalSpent += amount;
    }

    /// @notice Debit via the idempotent path with a FRESH unique jobId, so the
    ///         fence never trips on the happy path (every call is a new job).
    function spendOnce(uint256 buyerSeed, uint256 amount) external {
        address buyer = actors[buyerSeed % actors.length];
        uint256 c = meter.credit(buyer);
        if (c == 0) return;
        amount = bound(amount, 0, c);
        if (amount == 0) return;

        bytes32 jobId = keccak256(abi.encode("spendOnce", spendOnceNonce++));
        vm.prank(address(this));
        meter.spendOnce(buyer, amount, jobId);
        spendOnceJobIds.push(jobId);
        ghost_totalSpent += amount;
    }

    /// @notice Debit a fuzzed batch of FRESH unique jobIds via spendOnceBatch — the batched
    ///         twin of spendOnce. Each element's amount is bounded to its buyer's RUNNING
    ///         credit (tracked across the batch so repeated buyers never overshoot), so the
    ///         happy path debits rather than reverting. The batch's jobIds share the same
    ///         nonce space as spendOnce and are pushed to the replay pool, so the accounting
    ///         and at-most-once invariants must hold over a batch exactly as over N singles.
    function spendOnceBatch(uint256 sizeSeed, uint256 seed) external {
        uint256 size = bound(sizeSeed, 1, 8);
        uint256[] memory remaining = new uint256[](actors.length);
        for (uint256 i = 0; i < actors.length; i++) {
            remaining[i] = meter.credit(actors[i]);
        }

        ComputeMeter.DebitRequest[] memory scratch = new ComputeMeter.DebitRequest[](size);
        uint256 count;
        uint256 batchTotal;
        for (uint256 i = 0; i < size; i++) {
            uint256 ai = uint256(keccak256(abi.encode(seed, i))) % actors.length;
            if (remaining[ai] == 0) continue;
            uint256 amount = bound(uint256(keccak256(abi.encode("amt", seed, i))), 1, remaining[ai]);
            scratch[count++] = ComputeMeter.DebitRequest({
                buyer: actors[ai], amount: amount, jobId: keccak256(abi.encode("spendOnce", spendOnceNonce++))
            });
            remaining[ai] -= amount;
            batchTotal += amount;
        }
        if (count == 0) return;

        ComputeMeter.DebitRequest[] memory items = new ComputeMeter.DebitRequest[](count);
        for (uint256 i = 0; i < count; i++) {
            items[i] = scratch[i];
            spendOnceJobIds.push(scratch[i].jobId);
        }

        vm.prank(address(this));
        meter.spendOnceBatch(items);
        ghost_totalSpent += batchTotal;
    }

    /// @notice Replay a previously-spent jobId — it MUST revert AlreadySpent. If it
    ///         ever debits again, the fence is broken: record it for the invariant.
    function replaySpendOnce(uint256 seed, uint256 buyerSeed, uint256 amount) external {
        if (spendOnceJobIds.length == 0) return;
        bytes32 jobId = spendOnceJobIds[seed % spendOnceJobIds.length];
        address buyer = actors[buyerSeed % actors.length];
        amount = bound(amount, 0, 1000 ether);

        vm.prank(address(this));
        try meter.spendOnce(buyer, amount, jobId) {
            ghost_replayDoubleSpent = true;
        } catch {}
    }

    /// @notice Sum of credit over every tracked actor.
    function sumCredits() external view returns (uint256 total) {
        for (uint256 i = 0; i < actors.length; i++) {
            total += meter.credit(actors[i]);
        }
    }

    /// @notice Sum of cumulative `spentByBuyer` over every tracked actor. Only
    ///         actors ever buy, so this is the full domain of the mapping.
    function sumSpent() external view returns (uint256 total) {
        for (uint256 i = 0; i < actors.length; i++) {
            total += meter.spentByBuyer(actors[i]);
        }
    }

    function actorAt(uint256 i) external view returns (address) {
        return actors[i];
    }
}

contract ComputeMeterInvariantTest is Test {
    ComputeMeter meter;
    MockToken token;
    ComputeMeterHandler handler;

    address owner = address(0xA11CE);
    address[] actors;

    function setUp() public {
        token = new MockToken();
        meter = new ComputeMeter(address(token), owner);

        actors = new address[](3);
        actors[0] = address(0xB0B);
        actors[1] = address(0xCA201);
        actors[2] = address(0xDA4E);

        handler = new ComputeMeterHandler(meter, token, actors);

        // Owner authorizes the handler as a spender.
        vm.prank(owner);
        meter.setSpender(address(handler), true);

        // Fund each actor once from the test's token supply.
        for (uint256 i = 0; i < actors.length; i++) {
            assertTrue(token.transfer(actors[i], 1000 ether));
        }

        targetContract(address(handler));
    }

    /// @dev sum(credit) + totalSpent == totalDeposited (tokens burned 1:1 on deposit).
    function invariant_creditConservation() public view {
        assertEq(handler.sumCredits() + handler.ghost_totalSpent(), handler.ghost_totalDeposited());
    }

    /// @dev Every deposited token lands at BURN_ADDRESS, which started at zero.
    function invariant_allDepositsBurned() public view {
        assertEq(token.balanceOf(meter.BURN_ADDRESS()), handler.ghost_totalDeposited());
    }

    /// @dev The contract's own `totalBurned` HUD accumulator tracks deposits 1:1
    ///      (independent of the ghost creditConservation uses).
    function invariant_totalBurnedMatchesDeposited() public view {
        assertEq(meter.totalBurned(), handler.ghost_totalDeposited());
    }

    /// @dev The contract's own `totalSpent` HUD accumulator matches the ghost sum
    ///      of every debit driven through the handler.
    function invariant_totalSpentMatchesGhost() public view {
        assertEq(meter.totalSpent(), handler.ghost_totalSpent());
    }

    /// @dev Per-buyer `spentByBuyer` partitions the global `totalSpent`: summed
    ///      over every actor it equals the contract's `totalSpent`.
    function invariant_spentByBuyerSumsToTotalSpent() public view {
        assertEq(handler.sumSpent(), meter.totalSpent());
    }

    /// @dev No jobId is ever debited twice via spendOnce: the handler replays
    ///      already-spent jobIds and flags a leak if any debits a second time.
    function invariant_noSpendOnceJobIdDebitedTwice() public view {
        assertFalse(handler.ghost_replayDoubleSpent());
    }
}

contract ComputeMeterFuzzTest is Test {
    ComputeMeter meter;
    MockToken token;

    address owner = address(0xA11CE);
    address buyer = address(0xB0B);
    address spender = address(0xC0DE);

    function setUp() public {
        token = new MockToken();
        meter = new ComputeMeter(address(token), owner);

        assertTrue(token.transfer(buyer, 1000 ether));

        vm.prank(buyer);
        token.approve(address(meter), type(uint256).max);

        vm.prank(owner);
        meter.setSpender(spender, true);
    }

    function testFuzz_deposit_creditsAndBurns(uint256 amount) public {
        amount = bound(amount, 0, token.balanceOf(buyer));

        uint256 burnBefore = token.balanceOf(meter.BURN_ADDRESS());
        uint256 buyerBefore = token.balanceOf(buyer);

        vm.prank(buyer);
        meter.deposit(amount);

        assertEq(meter.credit(buyer), amount);
        assertEq(token.balanceOf(meter.BURN_ADDRESS()), burnBefore + amount);
        assertEq(token.balanceOf(buyer), buyerBefore - amount);
    }

    function testFuzz_depositFor_creditsBuyerBurnsFromSponsor(uint256 amount) public {
        // FM1 + FM2 fuzzed: the sponsor pays, the buyer is credited, and the buyer's own
        // tokens never move — the sponsor-deposit twin of testFuzz_deposit_creditsAndBurns.
        address sponsor = address(0x5907);
        assertTrue(token.transfer(sponsor, 1000 ether));
        vm.prank(sponsor);
        token.approve(address(meter), type(uint256).max);

        amount = bound(amount, 0, token.balanceOf(sponsor));

        uint256 burnBefore = token.balanceOf(meter.BURN_ADDRESS());
        uint256 sponsorBefore = token.balanceOf(sponsor);
        uint256 buyerBefore = token.balanceOf(buyer);

        vm.prank(sponsor);
        meter.depositFor(buyer, amount);

        assertEq(meter.credit(buyer), amount, "credit lands on buyer");
        assertEq(meter.credit(sponsor), 0, "sponsor not credited");
        assertEq(token.balanceOf(meter.BURN_ADDRESS()), burnBefore + amount);
        assertEq(token.balanceOf(sponsor), sponsorBefore - amount, "burned from sponsor");
        assertEq(token.balanceOf(buyer), buyerBefore, "buyer's own tokens untouched");
    }

    function testFuzz_spend_debitsExactly(uint256 dep, uint256 spendAmt) public {
        dep = bound(dep, 0, token.balanceOf(buyer));
        spendAmt = bound(spendAmt, 0, dep);

        vm.prank(buyer);
        meter.deposit(dep);

        vm.prank(spender);
        meter.spend(buyer, spendAmt, keccak256("job"));

        assertEq(meter.credit(buyer), dep - spendAmt);
    }

    function testFuzz_spend_revertsWhenOverCredit(uint256 dep, uint256 over) public {
        dep = bound(dep, 0, token.balanceOf(buyer));
        over = bound(over, dep + 1, type(uint256).max);

        vm.prank(buyer);
        meter.deposit(dep);

        vm.prank(spender);
        vm.expectRevert(ComputeMeter.InsufficientCredit.selector);
        meter.spend(buyer, over, keccak256("job"));
    }

    function testFuzz_spendOnce_secondCallRevertsForAnyJobId(uint256 dep, uint256 spendAmt, bytes32 jobId)
        public
    {
        dep = bound(dep, 1, token.balanceOf(buyer));
        spendAmt = bound(spendAmt, 1, dep);

        vm.prank(buyer);
        meter.deposit(dep);

        vm.startPrank(spender);
        meter.spendOnce(buyer, spendAmt, jobId);
        // For ANY jobId, a second spendOnce reverts — even a zero-amount one.
        vm.expectRevert(ComputeMeter.AlreadySpent.selector);
        meter.spendOnce(buyer, 0, jobId);
        vm.stopPrank();
    }
}
