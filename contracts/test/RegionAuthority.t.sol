// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
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
    event FeesDeposited(uint256 indexed tokenId, address indexed from, uint256 amount);
    event FeesClaimed(uint256 indexed tokenId, address indexed holder, uint256 amount);
    event FeesSettled(uint256 indexed tokenId, address indexed holder, uint256 amount);
    event Withdrawn(address indexed holder, uint256 amount);

    uint256 constant FEE = 30 ether;

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

    function test_claim_stakeTooLowPrecedesAlreadyClaimed() public {
        // Guard ORDER: the stake gate precedes the AlreadyClaimed check. Re-claiming an
        // already-held tile with a TOO-LOW amount reverts StakeTooLow, not AlreadyClaimed
        // — both guards would trip, so this pins that a malformed stake is reported before
        // the tile's claimed state. Complements test_claim_revertsAlreadyClaimed (which
        // re-claims with a VALID amount, so only AlreadyClaimed trips there).
        vm.prank(alice);
        region.claim(TILE, STAKE);

        vm.expectRevert(RegionAuthority.StakeTooLow.selector); // NOT AlreadyClaimed
        vm.prank(bob);
        region.claim(TILE, STAKE - 1);
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

    // --- fee distribution ---

    function test_depositFees_accruesToRegion() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);

        vm.expectEmit(true, true, false, true);
        emit FeesDeposited(TILE, bob, FEE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        assertEq(region.accruedFees(TILE), FEE);
        assertEq(token.balanceOf(address(region)), STAKE + FEE);
    }

    function test_depositFees_revertsZeroAmount() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);

        vm.expectRevert(RegionAuthority.ZeroAmount.selector);
        vm.prank(bob);
        region.depositFees(TILE, 0);
    }

    function test_depositFees_revertsUnknownRegion() public {
        // No one has claimed TILE, so fees have no holder to accrue to.
        vm.expectRevert(RegionAuthority.UnknownRegion.selector);
        vm.prank(bob);
        region.depositFees(TILE, FEE);
    }

    function test_depositFees_zeroAmountPrecedesUnknownRegion() public {
        // Guard ORDER: the amount check precedes the region-existence check. A zero-amount
        // deposit to an UNCLAIMED tile reverts ZeroAmount, not UnknownRegion — both guards
        // would trip, so this pins amount-first, mirroring claim()'s ZeroStake-before-
        // existence order (test_claim_revertsZeroAmount). TILE is never claimed here, so
        // the two sibling reject tests (each of which trips only one guard) leave this open.
        vm.expectRevert(RegionAuthority.ZeroAmount.selector); // NOT UnknownRegion
        vm.prank(bob);
        region.depositFees(TILE, 0);
    }

    /// @dev regionExists is the non-reverting twin of depositFees's UnknownRegion guard:
    ///      false for an unclaimed region (so a fee source can skip rather than revert),
    ///      true once claimed, and false again after the holder unstakes (burns) it. This
    ///      is the exact predicate RenderReceipts reads to decide whether to route a fee.
    function test_regionExists_tracksClaimAndUnstake() public {
        assertFalse(region.regionExists(TILE));

        vm.prank(alice);
        region.claim(TILE, STAKE);
        assertTrue(region.regionExists(TILE));
        // Agrees with depositFees: a claimed region accepts a deposit (no UnknownRegion).
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        vm.prank(alice);
        region.unstake(TILE);
        assertFalse(region.regionExists(TILE));
    }

    /// @dev regionExists survives a transfer — the region stays claimed under its new
    ///      holder (ownership moved, not burned), so a fee route keyed on it keeps landing.
    function test_regionExists_trueAfterTransfer() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);

        vm.prank(alice);
        region.transferFrom(alice, bob, TILE);

        assertTrue(region.regionExists(TILE));
        assertEq(region.ownerOf(TILE), bob);
    }

    function test_claimFees_holderWithdrawsExact() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        uint256 aliceBefore = token.balanceOf(alice);

        vm.expectEmit(true, true, false, true);
        emit FeesClaimed(TILE, alice, FEE);
        vm.prank(alice);
        region.claimFees(TILE);

        assertEq(token.balanceOf(alice), aliceBefore + FEE);
        assertEq(region.accruedFees(TILE), 0);
        // Stake stays locked; only the fees left the contract.
        assertEq(token.balanceOf(address(region)), STAKE);
    }

    function test_claimFees_revertsNotHolder() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        vm.expectRevert(RegionAuthority.NotHolder.selector);
        vm.prank(bob);
        region.claimFees(TILE);
    }

    function test_claimFees_revertsNothingToClaim() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);

        vm.expectRevert(RegionAuthority.NothingToClaim.selector);
        vm.prank(alice);
        region.claimFees(TILE);
    }

    /// @dev The fairness property: a region sale must not hand the buyer the fees the
    ///      seller earned. On transfer the accrued balance settles to the OUTGOING
    ///      holder's pull ledger; the new holder inherits nothing.
    function test_transfer_settlesAccruedToOutgoingHolder() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        vm.prank(alice);
        region.transferFrom(alice, bob, TILE);

        assertEq(region.accruedFees(TILE), 0, "accrued cleared on transfer");
        assertEq(region.withdrawable(alice), FEE, "settled to the seller");
        assertEq(region.withdrawable(bob), 0, "buyer inherits nothing");

        // The new holder has no region fees to claim.
        vm.expectRevert(RegionAuthority.NothingToClaim.selector);
        vm.prank(bob);
        region.claimFees(TILE);
    }

    function test_withdrawable_accumulatesAcrossTwoSettlements() public {
        // Two SEPARATE fee settlements to the SAME holder (no withdraw between) must SUM
        // in withdrawable[alice] — the `+=` accrual in _update. alice holds two regions,
        // each with its own accrued fee, and transfers both away; the second settlement
        // must ADD to the first, not overwrite it. Every other unit test settles once per
        // holder, so this is the only deterministic pin of the accumulation (otherwise a
        // `= accrued` overwrite rides on the fuzz invariant, which double-settles an actor
        // only probabilistically).
        uint256 tile2 = uint256(keccak256("region:3,4,0"));
        uint256 fee1 = 30 ether;
        uint256 fee2 = 45 ether;

        vm.startPrank(alice);
        region.claim(TILE, STAKE);
        region.claim(tile2, STAKE);
        vm.stopPrank();

        vm.startPrank(bob);
        region.depositFees(TILE, fee1);
        region.depositFees(tile2, fee2);
        vm.stopPrank();

        vm.startPrank(alice);
        region.transferFrom(alice, bob, TILE); // _update settles fee1 -> withdrawable[alice]
        region.transferFrom(alice, bob, tile2); // _update settles fee2, ACCUMULATING onto fee1
        vm.stopPrank();

        assertEq(region.withdrawable(alice), fee1 + fee2, "two settlements accumulate, not overwrite");
        assertEq(region.withdrawable(bob), 0, "fees route to the outgoing holder, not the buyer");
    }

    function test_withdraw_paysSettledFees() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);
        vm.prank(alice);
        region.transferFrom(alice, bob, TILE);

        uint256 aliceBefore = token.balanceOf(alice);

        vm.expectEmit(true, false, false, true);
        emit Withdrawn(alice, FEE);
        vm.prank(alice);
        region.withdraw();

        assertEq(token.balanceOf(alice), aliceBefore + FEE);
        assertEq(region.withdrawable(alice), 0);
    }

    function test_withdraw_revertsNothingToClaim() public {
        vm.expectRevert(RegionAuthority.NothingToClaim.selector);
        vm.prank(alice);
        region.withdraw();
    }

    /// @dev Unstaking burns the NFT, whose _update hook settles accrued fees into the
    ///      holder's withdrawable ledger — so the fees are recoverable, not stranded.
    function test_unstake_settlesAccruedNotStranded() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        uint256 aliceBefore = token.balanceOf(alice);
        vm.prank(alice);
        region.unstake(TILE);

        // Stake returned immediately; fees parked in withdrawable.
        assertEq(token.balanceOf(alice), aliceBefore + STAKE);
        assertEq(region.accruedFees(TILE), 0);
        assertEq(region.withdrawable(alice), FEE);

        vm.prank(alice);
        region.withdraw();
        assertEq(token.balanceOf(alice), aliceBefore + STAKE + FEE);
        assertEq(token.balanceOf(address(region)), 0, "nothing stranded");
    }

    /// @dev A region transfer that settles accrued fees emits FeesSettled crediting the
    ///      OUTGOING holder — the push-side log the withdrawable ledger previously created
    ///      silently (its five sibling movements all log). Discriminating: without the
    ///      emit in _update this reddens.
    function test_transfer_emitsFeesSettledToOutgoingHolder() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        vm.expectEmit(true, true, false, true);
        emit FeesSettled(TILE, alice, FEE);
        vm.prank(alice);
        region.transferFrom(alice, bob, TILE);
    }

    /// @dev Unstake burns the NFT, whose _update hook settles AND now logs the fees parked
    ///      into the outgoing holder's withdrawable ledger — the recover path is observable.
    function test_unstake_emitsFeesSettled() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        vm.expectEmit(true, true, false, true);
        emit FeesSettled(TILE, alice, FEE);
        vm.prank(alice);
        region.unstake(TILE);
    }

    /// @dev A transfer with nothing accrued emits NO FeesSettled: the event lives inside
    ///      the `accrued != 0` branch, so a zero-fee region sale stays log-free (no false
    ///      credit signal, no gas on the common path). Mutation-proven — hoisting the emit
    ///      out of the guard reddens this.
    function test_transfer_zeroAccrued_emitsNoFeesSettled() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        // No depositFees — accruedFees[TILE] == 0, so the settlement branch is skipped.

        vm.recordLogs();
        vm.prank(alice);
        region.transferFrom(alice, bob, TILE);

        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 sig = keccak256("FeesSettled(uint256,address,uint256)");
        for (uint256 i = 0; i < logs.length; i++) {
            assertTrue(
                logs[i].topics.length == 0 || logs[i].topics[0] != sig,
                "FeesSettled must not fire when nothing accrued"
            );
        }
        assertEq(region.withdrawable(alice), 0);
    }

    function test_depositFees_newHolderAccruesIndependentlyAfterTransfer() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);
        vm.prank(alice);
        region.transferFrom(alice, bob, TILE);

        // Fresh fees after the sale accrue to bob, the new holder.
        vm.prank(alice);
        region.depositFees(TILE, FEE);
        assertEq(region.accruedFees(TILE), FEE);

        vm.prank(bob);
        region.claimFees(TILE);
        assertEq(region.accruedFees(TILE), 0);
        // alice still holds her pre-sale settlement, untouched.
        assertEq(region.withdrawable(alice), FEE);
    }

    /// @dev Reentrancy proof for claimFees. A holder reenters claimFees() from the
    ///      token payout hook; it IS the holder, so the NotHolder gate is cleared and
    ///      only CEI (accrued zeroed before the transfer) stops a double-withdraw. A
    ///      second region's stake sits in the pool so a double-pay would be fundable;
    ///      the test fails (reentry drains the extra fee) if the zero moved after the
    ///      transfer.
    function test_claimFees_reentrantHolderCannotDoubleWithdraw() public {
        HookToken evil = new HookToken();
        RegionAuthority r = new RegionAuthority(address(evil), STAKE, owner);
        ReentrantClaimer attacker = new ReentrantClaimer(r, evil);

        // Fund the attacker's stake + fees, and an honest staker's stake, so the pool
        // (STAKE + FEE + STAKE) could cover a double FEE payout if CEI were broken.
        evil.transfer(alice, STAKE);
        vm.prank(alice);
        evil.approve(address(r), type(uint256).max);
        vm.prank(alice);
        r.claim(uint256(keccak256("region:9,9,0")), STAKE);

        evil.transfer(address(attacker), STAKE);
        attacker.claim(TILE, STAKE);
        evil.approve(address(r), type(uint256).max);
        r.depositFees(TILE, FEE);
        assertEq(evil.balanceOf(address(r)), 2 * STAKE + FEE);

        evil.arm(); // fire the holder's reentry on the next payout to it
        attacker.claimFees();

        assertTrue(attacker.reentered(), "reentry fired");
        assertTrue(attacker.reentryReverted(), "reentry blocked by CEI");
        assertEq(evil.balanceOf(address(attacker)), FEE, "got exactly one fee");
        assertEq(r.accruedFees(TILE), 0);
        // Honest staker's stake + the attacker's own stake remain in the pool.
        assertEq(evil.balanceOf(address(r)), 2 * STAKE);
    }

    /// @dev Reentrancy proof for withdraw(), the pull side of the transfer
    ///      settlement. The attacker earns a `withdrawable` balance (claim region,
    ///      fees deposited, then transfer the region away so the accrued settles to
    ///      it), then reenters withdraw() from the payout hook. Only zeroing the
    ///      balance before the transfer (CEI) stops a double-withdraw; two stakes sit
    ///      in the pool so a double-pay would be fundable if CEI were broken.
    function test_withdraw_reentrantHolderCannotDoubleWithdraw() public {
        HookToken evil = new HookToken();
        RegionAuthority r = new RegionAuthority(address(evil), STAKE, owner);
        ReentrantWithdrawer attacker = new ReentrantWithdrawer(r, evil);

        evil.transfer(alice, STAKE);
        vm.prank(alice);
        evil.approve(address(r), type(uint256).max);
        vm.prank(alice);
        r.claim(uint256(keccak256("region:9,9,0")), STAKE);

        evil.transfer(address(attacker), STAKE);
        attacker.claim(TILE, STAKE);
        evil.approve(address(r), type(uint256).max);
        r.depositFees(TILE, FEE);
        attacker.transferTo(bob); // _update settles FEE -> withdrawable[attacker]
        assertEq(r.withdrawable(address(attacker)), FEE);
        assertEq(evil.balanceOf(address(r)), 2 * STAKE + FEE);

        evil.arm();
        attacker.withdraw();

        assertTrue(attacker.reentered(), "reentry fired");
        assertTrue(attacker.reentryReverted(), "reentry blocked by CEI");
        assertEq(evil.balanceOf(address(attacker)), FEE, "got exactly one payout");
        assertEq(r.withdrawable(address(attacker)), 0);
        // Both stakes remain (attacker's region now bob's, plus the honest staker's).
        assertEq(evil.balanceOf(address(r)), 2 * STAKE);
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

/// @dev Shared payout callback so HookToken can drive any reentrancy attacker.
interface IPayoutReceiver {
    function onPayout() external;
}

/// @notice ERC-20 staking token with an armed recipient hook (ERC777-style), used
///         to hand control to the recipient mid-payout so it can reenter the
///         contract. Disarmed by default so funding/claim transfers are normal.
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
            IPayoutReceiver(to).onPayout();
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

/// @notice Region holder that reenters claimFees() once when it receives a fee
///         payout. It IS the NFT holder, so the reentry clears the NotHolder gate —
///         only claimFees zeroing the accrued balance before the payout (CEI) stops
///         a second withdrawal from the pool.
contract ReentrantClaimer {
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

    function claimFees() external {
        region.claimFees(tokenId);
    }

    function onPayout() external {
        reentered = true;
        try region.claimFees(tokenId) {}
        catch {
            reentryReverted = true;
        }
    }

    function onERC721Received(address, address, uint256, bytes calldata) external pure returns (bytes4) {
        return this.onERC721Received.selector;
    }
}

/// @notice Holder that earns a `withdrawable` balance (it transfers its region away
///         after fees accrue, settling them to itself) then reenters withdraw() from
///         the payout hook. Only withdraw zeroing the balance before the transfer
///         (CEI) stops a second payout.
contract ReentrantWithdrawer is IPayoutReceiver {
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

    function transferTo(address to) external {
        region.transferFrom(address(this), to, tokenId);
    }

    function withdraw() external {
        region.withdraw();
    }

    function onPayout() external {
        reentered = true;
        try region.withdraw() {}
        catch {
            reentryReverted = true;
        }
    }

    function onERC721Received(address, address, uint256, bytes calldata) external pure returns (bytes4) {
        return this.onERC721Received.selector;
    }
}
