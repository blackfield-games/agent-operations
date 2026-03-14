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
}
