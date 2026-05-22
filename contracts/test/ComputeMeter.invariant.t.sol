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
}
