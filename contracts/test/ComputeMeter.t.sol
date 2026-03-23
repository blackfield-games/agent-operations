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

contract ComputeMeterTest is Test {
    ComputeMeter meter;
    MockToken token;
    address owner = address(0xA11CE);
    address buyer = address(0xB0B);
    address spender = address(0xC0DE);

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
}
