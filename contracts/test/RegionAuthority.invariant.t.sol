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
    // Fees deposited and not yet paid out (claimFees/withdraw), regardless of whether
    // they currently sit in accruedFees or withdrawable — a transfer/burn only moves
    // them between those buckets, so this aggregate is unchanged by either.
    uint256 public ghost_unclaimedFees;

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

    function depositFees(uint256 fromSeed, uint256 idxSeed, uint256 amount) external {
        if (activeTokenIds.length == 0) return;
        uint256 tokenId = activeTokenIds[idxSeed % activeTokenIds.length];
        address from = actors[fromSeed % actors.length];
        uint256 bal = token.balanceOf(from);
        if (bal == 0) return;
        amount = bound(amount, 1, bal);

        vm.prank(from);
        region.depositFees(tokenId, amount);
        ghost_unclaimedFees += amount;
    }

    function claimFees(uint256 idxSeed) external {
        if (activeTokenIds.length == 0) return;
        uint256 tokenId = activeTokenIds[idxSeed % activeTokenIds.length];
        uint256 accrued = region.accruedFees(tokenId);
        if (accrued == 0) return; // would revert NothingToClaim

        vm.prank(holderOf[tokenId]);
        region.claimFees(tokenId);
        ghost_unclaimedFees -= accrued;
    }

    function withdraw(uint256 actorSeed) external {
        address actor = actors[actorSeed % actors.length];
        uint256 w = region.withdrawable(actor);
        if (w == 0) return; // would revert NothingToClaim

        vm.prank(actor);
        region.withdraw();
        ghost_unclaimedFees -= w;
    }

    function transferRegion(uint256 idxSeed, uint256 toSeed) external {
        if (activeTokenIds.length == 0) return;
        uint256 tokenId = activeTokenIds[idxSeed % activeTokenIds.length];
        address from = holderOf[tokenId];
        address to = actors[toSeed % actors.length];
        if (to == from) return;

        vm.prank(from);
        region.transferFrom(from, to, tokenId);
        holderOf[tokenId] = to; // accrued settles to from's withdrawable; aggregate unchanged
    }

    function activeCount() external view returns (uint256) {
        return activeTokenIds.length;
    }

    /// @dev The contract's own books: Σ stakes + Σ accrued (over active regions) +
    ///      Σ withdrawable (over all actors, the only addresses that ever hold a
    ///      balance here). Reconciled against the real token balance by the invariant.
    function internalAccountingSum() external view returns (uint256) {
        uint256 sum;
        for (uint256 i = 0; i < activeTokenIds.length; i++) {
            uint256 id = activeTokenIds[i];
            (uint256 staked,) = region.stakes(id);
            sum += staked + region.accruedFees(id);
        }
        for (uint256 i = 0; i < actors.length; i++) {
            sum += region.withdrawable(actors[i]);
        }
        return sum;
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

    /// @dev Conservation against independently-tracked ghosts: the region's real
    ///      balance is exactly the active stakes plus every deposited-but-unpaid fee
    ///      (deposit args in, claimFees/withdraw payouts out) — transfers and burns
    ///      only shuffle fees between accrued and withdrawable, never the total.
    function invariant_balanceEqualsStakesPlusUnclaimedFees() public view {
        assertEq(token.balanceOf(address(region)), handler.ghost_activeStakeSum() + handler.ghost_unclaimedFees());
    }

    /// @dev The contract's internal books reconcile to its balance: balance ==
    ///      Σ stakes + Σ accrued + Σ withdrawable. Catches any drift between the
    ///      accounting mappings and the real tokens regardless of the ghosts.
    function invariant_balanceReconcilesInternalAccounting() public view {
        assertEq(token.balanceOf(address(region)), handler.internalAccountingSum());
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
        // Non-zero but below the floor reverts StakeTooLow; amount==0 reverts
        // ZeroStake (checked first), covered separately in the unit suite.
        amount = bound(amount, 1, STAKE - 1);

        vm.expectRevert(RegionAuthority.StakeTooLow.selector);
        vm.prank(alice);
        region.claim(TILE, amount);
    }
}
