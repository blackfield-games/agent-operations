// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {RegionAuthority} from "../src/RegionAuthority.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC721Errors} from "@openzeppelin/contracts/interfaces/draft-IERC6093.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

/// @dev ERC721 receiver so _safeMint to a contract succeeds where needed.
contract Holder {
    function onERC721Received(address, address, uint256, bytes calldata) external pure returns (bytes4) {
        return this.onERC721Received.selector;
    }
}

contract RegionAuthorityTest is Test {
    RegionAuthority region;
    MockToken token;

    address owner = address(0xA11CE);
    address alice = address(0xA1);
    address bob = address(0xB0B);

    uint256 constant STAKE = 100 ether;
    uint256 constant TILE = uint256(keccak256("region:1,2,0"));

    event Staked(address indexed holder, uint256 indexed tokenId, uint256 amount);
    event Unstaked(address indexed holder, uint256 indexed tokenId, uint256 amount);
    event StakeRequiredSet(uint256 amount);

    function setUp() public {
        token = new MockToken();
        region = new RegionAuthority(address(token), STAKE, owner);

        assertTrue(token.transfer(alice, 10_000 ether));
        assertTrue(token.transfer(bob, 10_000 ether));

        vm.prank(alice);
        token.approve(address(region), type(uint256).max);
        vm.prank(bob);
        token.approve(address(region), type(uint256).max);
    }

    // --- construction / metadata ---

    function test_constructor() public view {
        assertEq(address(region.TOKEN()), address(token));
        assertEq(region.stakeRequired(), STAKE);
        assertEq(region.owner(), owner);
        assertEq(region.name(), "Blackfield Region");
        assertEq(region.symbol(), "BFLD-RGN");
    }

    function test_constructor_revertsZeroStake() public {
        // A zero floor would let a region be claimed for free; reject it at construction.
        vm.expectRevert(RegionAuthority.ZeroStake.selector);
        new RegionAuthority(address(token), 0, owner);
    }

    // --- claim happy path ---

    function test_claim_mintsAndStakes() public {
        vm.expectEmit(true, true, false, true);
        emit Staked(alice, TILE, STAKE);

        vm.prank(alice);
        region.claim(TILE, STAKE);

        assertEq(region.ownerOf(TILE), alice);
        assertEq(region.balanceOf(alice), 1);
        assertEq(token.balanceOf(address(region)), STAKE);
        assertEq(token.balanceOf(alice), 10_000 ether - STAKE);

        (uint256 amount, uint64 stakedAt) = region.stakes(TILE);
        assertEq(amount, STAKE);
        assertEq(stakedAt, uint64(block.timestamp));
    }

    function test_claim_acceptsOverStake() public {
        uint256 over = STAKE + 50 ether;
        vm.prank(alice);
        region.claim(TILE, over);
        (uint256 amount,) = region.stakes(TILE);
        assertEq(amount, over);
        assertEq(token.balanceOf(address(region)), over);
    }

    function test_claim_recordsStakedAtTimestamp() public {
        vm.warp(42_000);
        vm.prank(alice);
        region.claim(TILE, STAKE);
        (, uint64 stakedAt) = region.stakes(TILE);
        assertEq(stakedAt, 42_000);
    }

    // --- claim revert paths ---

    function test_claim_revertsStakeTooLow() public {
        vm.expectRevert(RegionAuthority.StakeTooLow.selector);
        vm.prank(alice);
        region.claim(TILE, STAKE - 1);
    }

    function test_claim_revertsZeroAmount() public {
        // Zero amount is rejected before the StakeTooLow check, so no region is
        // ever minted against a zero stake even if the floor were somehow lowered.
        vm.expectRevert(RegionAuthority.ZeroStake.selector);
        vm.prank(alice);
        region.claim(TILE, 0);

        assertEq(region.balanceOf(alice), 0);
        (uint256 amount,) = region.stakes(TILE);
        assertEq(amount, 0);
    }

    function test_claim_revertsAlreadyClaimed() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);

        vm.expectRevert(RegionAuthority.AlreadyClaimed.selector);
        vm.prank(bob);
        region.claim(TILE, STAKE);
    }

    function test_claim_revertsWhenNotApproved() public {
        address carol = address(0xCA201);
        assertTrue(token.transfer(carol, STAKE));
        // no approval granted
        vm.expectRevert();
        vm.prank(carol);
        region.claim(TILE, STAKE);
    }

    // --- unstake happy path ---

    function test_unstake_burnsAndReturnsTokens() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);

        uint256 balBefore = token.balanceOf(alice);

        vm.expectEmit(true, true, false, true);
        emit Unstaked(alice, TILE, STAKE);

        vm.prank(alice);
        region.unstake(TILE);

        assertEq(token.balanceOf(alice), balBefore + STAKE);
        assertEq(token.balanceOf(address(region)), 0);
        assertEq(region.balanceOf(alice), 0);

        (uint256 amount, uint64 stakedAt) = region.stakes(TILE);
        assertEq(amount, 0);
        assertEq(stakedAt, 0);

        // Token no longer exists.
        vm.expectRevert(abi.encodeWithSelector(IERC721Errors.ERC721NonexistentToken.selector, TILE));
        region.ownerOf(TILE);
    }

    function test_unstake_allowsReclaimAfterward() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(alice);
        region.unstake(TILE);

        // Region is free again; bob can claim it.
        vm.prank(bob);
        region.claim(TILE, STAKE);
        assertEq(region.ownerOf(TILE), bob);
    }

    // --- unstake revert paths ---

    function test_unstake_revertsNotHolder() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);

        vm.expectRevert(RegionAuthority.NotHolder.selector);
        vm.prank(bob);
        region.unstake(TILE);
    }

    function test_unstake_revertsForNonexistentToken() public {
        // ownerOf reverts before reaching the NotHolder check for an unminted token.
        vm.expectRevert(abi.encodeWithSelector(IERC721Errors.ERC721NonexistentToken.selector, TILE));
        vm.prank(alice);
        region.unstake(TILE);
    }

    // --- transfer then unstake by new holder ---

    function test_unstake_byNewHolderAfterTransfer() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);

        vm.prank(alice);
        region.transferFrom(alice, bob, TILE);
        assertEq(region.ownerOf(TILE), bob);

        // Original staker can no longer unstake.
        vm.expectRevert(RegionAuthority.NotHolder.selector);
        vm.prank(alice);
        region.unstake(TILE);

        uint256 bobBefore = token.balanceOf(bob);
        vm.prank(bob);
        region.unstake(TILE);
        // Bob recovers the stake even though alice funded it.
        assertEq(token.balanceOf(bob), bobBefore + STAKE);
    }

    /// @dev Reentrancy proof. A region holder reenters unstake() from the staking
    ///      token's payout hook. Because the attacker IS the holder, the reentry
    ///      passes the `ownerOf == msg.sender` gate — so the only thing stopping a
    ///      second withdrawal is that unstake deletes the stake and burns the NFT
    ///      *before* the payout transfer (CEI). A second honest staker's funds sit
    ///      in the pool, making a double-withdraw genuinely fundable, so this test
    ///      fails (reentry would succeed and drain the pool) if delete/burn were
    ///      moved after the transfer.
    function test_unstake_reentrantHolderCannotDrainPool() public {
        HookToken evil = new HookToken();
        RegionAuthority r = new RegionAuthority(address(evil), STAKE, owner);
        ReentrantHolder attacker = new ReentrantHolder(r, evil);

        // Honest staker funds a distinct region, so the pool holds 2*STAKE when the
        // attacker unstakes — enough to pay a reentrant double-withdraw if it lands.
        uint256 aliceTile = uint256(keccak256("region:9,9,0"));
        evil.transfer(alice, STAKE);
        vm.prank(alice);
        evil.approve(address(r), type(uint256).max);
        vm.prank(alice);
        r.claim(aliceTile, STAKE);

        evil.transfer(address(attacker), STAKE);
        attacker.claim(TILE, STAKE);
        assertEq(evil.balanceOf(address(r)), 2 * STAKE);

        evil.arm(); // fire the holder's reentry on the next payout to it
        attacker.unstake();

        // The reentry fired and reverted (NFT already burned), so the attacker
        // recovered exactly its own stake and the honest staker's funds are intact.
        assertTrue(attacker.reentered());
        assertTrue(attacker.reentryReverted());
        assertEq(evil.balanceOf(address(attacker)), STAKE);
        assertEq(evil.balanceOf(address(r)), STAKE);
        assertEq(r.balanceOf(address(attacker)), 0);
    }

    // --- setStakeRequired ---

    function test_setStakeRequired_updatesAndEmits() public {
        vm.expectEmit(false, false, false, true);
        emit StakeRequiredSet(250 ether);
        vm.prank(owner);
        region.setStakeRequired(250 ether);
        assertEq(region.stakeRequired(), 250 ether);
    }

    function test_setStakeRequired_onlyOwner() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, alice));
        vm.prank(alice);
        region.setStakeRequired(1);
    }

    function test_setStakeRequired_revertsZero() public {
        vm.expectRevert(RegionAuthority.ZeroStake.selector);
        vm.prank(owner);
        region.setStakeRequired(0);
        // Floor unchanged after the rejected call.
        assertEq(region.stakeRequired(), STAKE);
    }

    function test_setStakeRequired_affectsSubsequentClaims() public {
        vm.prank(owner);
        region.setStakeRequired(STAKE * 2);

        vm.expectRevert(RegionAuthority.StakeTooLow.selector);
        vm.prank(alice);
        region.claim(TILE, STAKE);

        vm.prank(alice);
        region.claim(TILE, STAKE * 2);
        assertEq(region.ownerOf(TILE), alice);
    }

    // --- ownership ---

    function test_ownership_twoStepTransfer() public {
        vm.prank(owner);
        region.transferOwnership(bob);
        assertEq(region.pendingOwner(), bob);
        assertEq(region.owner(), owner);

        vm.prank(bob);
        region.acceptOwnership();
        assertEq(region.owner(), bob);

        vm.prank(bob);
        region.setStakeRequired(5);
        assertEq(region.stakeRequired(), 5);
    }
}

/// @notice ERC-20 staking token with an armed recipient hook (ERC777-style), used
///         to hand control to the holder mid-payout so it can reenter unstake().
///         Disarmed by default so funding/claim transfers are normal.
contract HookToken is ERC20 {
    bool armed;

    constructor() ERC20("Hook", "HOOK") {
        _mint(msg.sender, 1_000_000 ether);
    }

    function arm() external {
        armed = true;
    }

    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        if (armed && to != address(0) && to.code.length > 0) {
            armed = false; // one-shot, so the reentrant payout doesn't recurse
            ReentrantHolder(to).onPayout();
        }
    }
}

/// @notice Region holder that reenters unstake() once when it receives the staking
///         token payout. It IS the NFT holder, so the reentry clears the NotHolder
///         gate — only unstake's delete+burn (before the payout) stops a second
///         withdrawal from the pool.
contract ReentrantHolder {
    RegionAuthority region;
    HookToken token;
    uint256 public tokenId;
    bool public reentered;
    bool public reentryReverted;

    constructor(RegionAuthority region_, HookToken token_) {
        region = region_;
        token = token_;
    }

    function claim(uint256 tokenId_, uint256 amount) external {
        tokenId = tokenId_;
        token.approve(address(region), type(uint256).max);
        region.claim(tokenId_, amount);
    }

    function unstake() external {
        region.unstake(tokenId);
    }

    function onPayout() external {
        reentered = true;
        try region.unstake(tokenId) {}
        catch {
            reentryReverted = true;
        }
    }

    function onERC721Received(address, address, uint256, bytes calldata) external pure returns (bytes4) {
        return this.onERC721Received.selector;
    }
}
