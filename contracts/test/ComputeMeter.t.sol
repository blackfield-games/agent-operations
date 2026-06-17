// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ComputeMeter} from "../src/ComputeMeter.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

contract ComputeMeterTest is Test {
    ComputeMeter meter;
    MockToken token;
    address owner = address(0xA11CE);
    address buyer = address(0xB0B);
    address spender = address(0xC0DE);

    event SpenderSet(address indexed spender, bool authorized);
    event Spent(address indexed buyer, address indexed spender, uint256 amount, bytes32 jobId);
    event Deposited(address indexed buyer, uint256 amount, uint256 newCredit);

    function setUp() public {
        token = new MockToken();
        meter = new ComputeMeter(address(token), owner);

        assertTrue(token.transfer(buyer, 1000 ether));
        vm.prank(owner);
        meter.setSpender(spender, true);
    }

    function test_deposit_burnsAndCredits() public {
        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        assertEq(meter.credit(buyer), 100 ether);
        assertEq(token.balanceOf(meter.BURN_ADDRESS()), 100 ether);
        assertEq(token.balanceOf(buyer), 900 ether);
    }

    function test_spend_byAuthorizedSpender() public {
        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        bytes32 jobId = keccak256("job-1");
        vm.prank(spender);
        meter.spend(buyer, 40 ether, jobId);

        assertEq(meter.credit(buyer), 60 ether);
    }

    function test_spend_revertsForUnauthorized() public {
        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        vm.expectRevert(ComputeMeter.NotAuthorized.selector);
        meter.spend(buyer, 10 ether, bytes32(0));
    }

    function test_spend_revertsOnInsufficientCredit() public {
        vm.startPrank(buyer);
        token.approve(address(meter), 50 ether);
        meter.deposit(50 ether);
        vm.stopPrank();

        vm.prank(spender);
        vm.expectRevert(ComputeMeter.InsufficientCredit.selector);
        meter.spend(buyer, 100 ether, bytes32(0));
    }

    function test_spend_zeroAmount_isNoOpButEmits() public {
        // The credit guard is `c < amount`, so amount == 0 always passes (even at
        // zero credit) and debits nothing. This pins that boundary: a zero-amount
        // spend changes no accounting but still emits a Spent event with the jobId.
        // If a future ZeroAmount guard is added, this test flips intentionally.
        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        bytes32 jobId = keccak256("zero-job");
        vm.expectEmit(true, true, false, true);
        emit Spent(buyer, spender, 0, jobId);
        vm.prank(spender);
        meter.spend(buyer, 0, jobId);

        assertEq(meter.credit(buyer), 100 ether, "credit untouched");
        assertEq(meter.totalSpent(), 0, "global spend untouched");
        assertEq(meter.spentByBuyer(buyer), 0, "per-buyer spend untouched");
    }

    function test_spend_ownerIsNotImplicitlyAuthorized() public {
        // Authority separation: the owner gates spenders via setSpender but is not
        // itself a spender. It must self-authorize to debit, so a stray owner key
        // can't drain credit without first emitting an on-chain SpenderSet(owner).
        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        assertFalse(meter.authorizedSpenders(owner));
        vm.prank(owner);
        vm.expectRevert(ComputeMeter.NotAuthorized.selector);
        meter.spend(buyer, 10 ether, keccak256("j"));
    }

    function test_spend_exactDrainToZero() public {
        // The c == amount boundary: `c < amount` is false, so an exact-balance
        // debit succeeds, lands credit at exactly zero, and a further non-zero
        // spend then reverts InsufficientCredit (FM2 boundary, explicit vs fuzz).
        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        vm.prank(spender);
        meter.spend(buyer, 100 ether, keccak256("drain"));

        assertEq(meter.credit(buyer), 0);
        assertEq(meter.spentByBuyer(buyer), 100 ether);
        assertEq(meter.totalSpent(), 100 ether);

        vm.prank(spender);
        vm.expectRevert(ComputeMeter.InsufficientCredit.selector);
        meter.spend(buyer, 1, keccak256("over"));
    }

    function test_deposit_zeroAmount_isNoOp() public {
        // deposit(0) transfers nothing, credits nothing, but still emits the event.
        vm.startPrank(buyer);
        token.approve(address(meter), 1 ether);
        vm.expectEmit(true, false, false, true);
        emit Deposited(buyer, 0, 0);
        meter.deposit(0);
        vm.stopPrank();

        assertEq(meter.credit(buyer), 0);
        assertEq(meter.totalBurned(), 0);
        assertEq(token.balanceOf(buyer), 1000 ether, "no tokens moved");
    }

    function test_totalBurned_accumulates() public {
        assertEq(meter.totalBurned(), 0);

        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        token.approve(address(meter), 50 ether);
        meter.deposit(50 ether);
        vm.stopPrank();

        assertEq(meter.totalBurned(), 150 ether);

        vm.prank(spender);
        meter.spend(buyer, 40 ether, keccak256("j"));

        assertEq(meter.totalBurned(), 150 ether);
    }

    function test_totalSpent_accumulates() public {
        assertEq(meter.totalSpent(), 0);
        assertEq(meter.spentByBuyer(buyer), 0);

        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        vm.startPrank(spender);
        meter.spend(buyer, 40 ether, keccak256("j1"));
        meter.spend(buyer, 10 ether, keccak256("j2"));
        vm.stopPrank();

        // Cumulative spend is monotonic; credit falls by the same total.
        assertEq(meter.totalSpent(), 50 ether);
        assertEq(meter.spentByBuyer(buyer), 50 ether);
        assertEq(meter.credit(buyer), 50 ether);
    }

    function test_spentByBuyer_sumsToTotalSpent() public {
        address buyer2 = address(0xB0B2);
        assertTrue(token.transfer(buyer2, 1000 ether));

        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        vm.startPrank(buyer2);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        vm.startPrank(spender);
        meter.spend(buyer, 30 ether, keccak256("a"));
        meter.spend(buyer2, 70 ether, keccak256("b"));
        meter.spend(buyer, 5 ether, keccak256("c"));
        vm.stopPrank();

        assertEq(meter.spentByBuyer(buyer), 35 ether);
        assertEq(meter.spentByBuyer(buyer2), 70 ether);
        // per-buyer attribution sums to the global total
        assertEq(meter.spentByBuyer(buyer) + meter.spentByBuyer(buyer2), meter.totalSpent());
        assertEq(meter.totalSpent(), 105 ether);
    }

    // --- setSpender access control + toggle ---

    function test_setSpender_onlyOwner() public {
        // Only the owner gates spenders; a buyer cannot self-authorize to drain credit.
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, buyer));
        vm.prank(buyer);
        meter.setSpender(address(0xBEEF), true);
    }

    function test_setSpender_togglesAuthorizationAndEmits() public {
        vm.startPrank(buyer);
        token.approve(address(meter), 100 ether);
        meter.deposit(100 ether);
        vm.stopPrank();

        address temp = address(0xCAFE);

        // Authorizing emits SpenderSet(true) and lets temp debit credit.
        vm.expectEmit(true, false, false, true);
        emit SpenderSet(temp, true);
        vm.prank(owner);
        meter.setSpender(temp, true);
        assertTrue(meter.authorizedSpenders(temp));

        vm.prank(temp);
        meter.spend(buyer, 10 ether, keccak256("j"));
        assertEq(meter.credit(buyer), 90 ether);

        // De-authorizing emits SpenderSet(false); a further spend by temp reverts.
        vm.expectEmit(true, false, false, true);
        emit SpenderSet(temp, false);
        vm.prank(owner);
        meter.setSpender(temp, false);
        assertFalse(meter.authorizedSpenders(temp));

        vm.prank(temp);
        vm.expectRevert(ComputeMeter.NotAuthorized.selector);
        meter.spend(buyer, 10 ether, keccak256("j2"));
    }

    // --- two-step ownership ---

    function test_ownership_twoStepTransfer() public {
        address newOwner = address(0xBEEF);

        vm.prank(owner);
        meter.transferOwnership(newOwner);
        // Transfer is pending until accepted: the current owner stays in control.
        assertEq(meter.pendingOwner(), newOwner);
        assertEq(meter.owner(), owner);

        vm.prank(newOwner);
        meter.acceptOwnership();
        assertEq(meter.owner(), newOwner);

        // The new owner can now gate spenders; the prior owner no longer can.
        vm.prank(newOwner);
        meter.setSpender(address(0xC0FFEE), true);
        assertTrue(meter.authorizedSpenders(address(0xC0FFEE)));

        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, owner));
        vm.prank(owner);
        meter.setSpender(address(0xC0FFEE), false);
    }
}
