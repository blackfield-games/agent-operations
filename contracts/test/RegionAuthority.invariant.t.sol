// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {RegionAuthority} from "../src/RegionAuthority.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC721Errors} from "@openzeppelin/contracts/interfaces/draft-IERC6093.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

/// @notice Stateful handler driving bounded claim/unstake against the region while
///         tracking the set of active tokenIds and their staked amounts so the
///         held-balance invariant can be checked over a run. Mints to EOA actors
///         via prank so no ERC721 receiver hook is required.
contract RegionAuthorityHandler is Test {
    RegionAuthority public region;
    MockToken public token;

    address[] public actors;

    uint256[] public activeTokenIds;
    mapping(uint256 tokenId => address holder) public holderOf;
    mapping(uint256 tokenId => uint256 amount) public stakeOf;
    uint256 public ghost_activeStakeSum;

    constructor(RegionAuthority region_, MockToken token_, address[] memory actors_) {
        region = region_;
        token = token_;
        actors = actors_;
        for (uint256 i = 0; i < actors.length; i++) {
            vm.prank(actors[i]);
            token.approve(address(region), type(uint256).max);
        }
    }

    function claim(uint256 actorSeed, uint256 tileSeed, uint256 amount) external {
        address actor = actors[actorSeed % actors.length];
        // Derive a tile from a bounded space so collisions (AlreadyClaimed) occur.
        uint256 tokenId = uint256(keccak256(abi.encode(tileSeed % 8)));
        // All mints flow through this handler, so a non-zero holder means it's taken.
        if (holderOf[tokenId] != address(0)) return;

        uint256 required = region.stakeRequired();
        uint256 bal = token.balanceOf(actor);
        if (bal < required) return;
        amount = bound(amount, required, bal);

        vm.prank(actor);
        region.claim(tokenId, amount);

        activeTokenIds.push(tokenId);
        holderOf[tokenId] = actor;
        stakeOf[tokenId] = amount;
        ghost_activeStakeSum += amount;
    }

    function unstake(uint256 idxSeed) external {
        if (activeTokenIds.length == 0) return;
        uint256 idx = idxSeed % activeTokenIds.length;
        uint256 tokenId = activeTokenIds[idx];
        address holder = holderOf[tokenId];

        vm.prank(holder);
        region.unstake(tokenId);

        ghost_activeStakeSum -= stakeOf[tokenId];
        delete holderOf[tokenId];
        delete stakeOf[tokenId];

        // swap-and-pop removal from the active set
        activeTokenIds[idx] = activeTokenIds[activeTokenIds.length - 1];
        activeTokenIds.pop();
    }

    function activeCount() external view returns (uint256) {
        return activeTokenIds.length;
    }
}

contract RegionAuthorityInvariantTest is Test {
    RegionAuthority region;
    MockToken token;
    RegionAuthorityHandler handler;

    address owner = address(0xA11CE);
    address[] actors;

    uint256 constant STAKE = 100 ether;

    function setUp() public {
        token = new MockToken();
        region = new RegionAuthority(address(token), STAKE, owner);

        actors = new address[](3);
        actors[0] = address(0xA1);
        actors[1] = address(0xB0B);
        actors[2] = address(0xCA201);

        handler = new RegionAuthorityHandler(region, token, actors);

        for (uint256 i = 0; i < actors.length; i++) {
            assertTrue(token.transfer(actors[i], 10_000 ether));
        }

        targetContract(address(handler));
    }

    /// @dev The region holds exactly the sum of all currently active stakes.
    function invariant_balanceEqualsActiveStakes() public view {
        assertEq(token.balanceOf(address(region)), handler.ghost_activeStakeSum());
    }
}

contract RegionAuthorityFuzzTest is Test {
    RegionAuthority region;
    MockToken token;

    address owner = address(0xA11CE);
    address alice = address(0xA1);

    uint256 constant STAKE = 100 ether;
    uint256 constant TILE = uint256(keccak256("region:1,2,0"));

    function setUp() public {
        token = new MockToken();
        region = new RegionAuthority(address(token), STAKE, owner);

        assertTrue(token.transfer(alice, 10_000 ether));
        vm.prank(alice);
        token.approve(address(region), type(uint256).max);
    }

    function testFuzz_claim_thenUnstake_returnsExactStake(uint256 amount) public {
        amount = bound(amount, STAKE, token.balanceOf(alice));

        uint256 balBefore = token.balanceOf(alice);

        vm.prank(alice);
        region.claim(TILE, amount);
        assertEq(token.balanceOf(address(region)), amount);

        vm.prank(alice);
        region.unstake(TILE);

        // Stake fully restored to the actor.
        assertEq(token.balanceOf(alice), balBefore);
        assertEq(token.balanceOf(address(region)), 0);

        (uint256 staked,) = region.stakes(TILE);
        assertEq(staked, 0);

        // NFT no longer exists.
        vm.expectRevert(abi.encodeWithSelector(IERC721Errors.ERC721NonexistentToken.selector, TILE));
        region.ownerOf(TILE);
    }

    function testFuzz_claim_belowRequired_reverts(uint256 amount) public {
        amount = bound(amount, 0, STAKE - 1);

        vm.expectRevert(RegionAuthority.StakeTooLow.selector);
        vm.prank(alice);
        region.claim(TILE, amount);
    }
}
