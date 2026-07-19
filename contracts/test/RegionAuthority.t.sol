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

    function test_constructor_revertsZeroToken() public {
        // A zero token would deploy a region whose every stake/fee transfer reverts opaquely
        // at SafeERC20; fail loudly at construction instead (mirrors ArtifactTemplate).
        vm.expectRevert(RegionAuthority.ZeroToken.selector);
        new RegionAuthority(address(0), STAKE, owner);
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

    /// @dev totalStaked is the protocol-wide aggregate: it starts at 0, accumulates each
    ///      claim's amount across regions (never overwrites), stays put across a transfer
    ///      (the stake stays locked under the new holder), and falls by exactly the
    ///      unstaked region's STORED amount — so unstaking an over-staked region removes
    ///      that region's real stake, not STAKE, and a claim→unstake round-trips it to 0.
    function test_totalStaked_tracksClaimTransferAndUnstake() public {
        assertEq(region.totalStaked(), 0, "starts at zero");

        uint256 tile2 = uint256(keccak256("region:3,4,0"));
        uint256 over = STAKE + 25 ether;

        vm.prank(alice);
        region.claim(TILE, STAKE);
        assertEq(region.totalStaked(), STAKE, "first claim adds its stake");

        vm.prank(bob);
        region.claim(tile2, over);
        assertEq(region.totalStaked(), STAKE + over, "a second region accumulates, not overwrites");

        // A transfer only moves the holder — the stake stays locked, so the aggregate holds.
        vm.prank(alice);
        region.transferFrom(alice, bob, TILE);
        assertEq(region.totalStaked(), STAKE + over, "a transfer leaves totalStaked unchanged");

        // Unstaking tile2 removes exactly its stored (over-)stake, not STAKE.
        vm.prank(bob);
        region.unstake(tile2);
        assertEq(region.totalStaked(), STAKE, "unstake decrements the stored amount, not a fixed value");

        // The surviving region round-trips the aggregate back to zero.
        vm.prank(bob);
        region.unstake(TILE);
        assertEq(region.totalStaked(), 0, "claim -> unstake round-trips totalStaked to its prior value");
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

    // --- claimFeesBatch (sweep a portfolio of regions in one tx, strict-atomic) ---

    /// @dev The headline: a holder sweeps three regions' fees in ONE call — a
    ///      `FeesClaimed` per region and a SINGLE aggregated transfer of the sum, with
    ///      every `accruedFees` zeroed and the stakes left locked.
    function test_claimFeesBatch_sweepsManyRegionsInOneTransfer() public {
        uint256 t2 = uint256(keccak256("region:3,4,0"));
        uint256 t3 = uint256(keccak256("region:5,6,0"));
        vm.startPrank(alice);
        region.claim(TILE, STAKE);
        region.claim(t2, STAKE);
        region.claim(t3, STAKE);
        vm.stopPrank();
        vm.startPrank(bob);
        region.depositFees(TILE, FEE);
        region.depositFees(t2, 50 ether);
        region.depositFees(t3, 20 ether);
        vm.stopPrank();

        uint256 aliceBefore = token.balanceOf(alice);
        vm.expectEmit(true, true, false, true);
        emit FeesClaimed(TILE, alice, FEE);
        vm.expectEmit(true, true, false, true);
        emit FeesClaimed(t2, alice, 50 ether);
        vm.expectEmit(true, true, false, true);
        emit FeesClaimed(t3, alice, 20 ether);

        uint256[] memory ids = new uint256[](3);
        ids[0] = TILE;
        ids[1] = t2;
        ids[2] = t3;
        vm.prank(alice);
        region.claimFeesBatch(ids);

        uint256 sum = FEE + 50 ether + 20 ether;
        assertEq(token.balanceOf(alice), aliceBefore + sum, "one aggregated transfer of the summed fees");
        assertEq(region.accruedFees(TILE), 0);
        assertEq(region.accruedFees(t2), 0);
        assertEq(region.accruedFees(t3), 0);
        assertEq(token.balanceOf(address(region)), 3 * STAKE, "only fees left; the three stakes stay locked");
    }

    /// @dev Strict-atomic: a zero-fee element reverts the WHOLE batch (mirrors the
    ///      single's `NothingToClaim`) — the funded region is NOT partially swept.
    function test_claimFeesBatch_revertsAtomicallyOnZeroFeeElement() public {
        uint256 t2 = uint256(keccak256("region:3,4,0"));
        vm.startPrank(alice);
        region.claim(TILE, STAKE);
        region.claim(t2, STAKE); // t2 has no accrued fees
        vm.stopPrank();
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        uint256 aliceBefore = token.balanceOf(alice);
        uint256[] memory ids = new uint256[](2);
        ids[0] = TILE;
        ids[1] = t2;
        vm.expectRevert(RegionAuthority.NothingToClaim.selector);
        vm.prank(alice);
        region.claimFeesBatch(ids);

        assertEq(region.accruedFees(TILE), FEE, "the funded region was NOT partially swept");
        assertEq(token.balanceOf(alice), aliceBefore, "no transfer from the reverted batch");
    }

    /// @dev A batch is not a licence to sweep another holder's region: an element the
    ///      caller does not own reverts `NotHolder`, and that region stays untouched —
    ///      fees are never pooled to `msg.sender` across differently-owned regions.
    function test_claimFeesBatch_revertsOnNonHeldElement() public {
        uint256 t2 = uint256(keccak256("region:3,4,0"));
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.claim(t2, STAKE);
        vm.startPrank(bob);
        region.depositFees(TILE, FEE);
        region.depositFees(t2, FEE);
        vm.stopPrank();

        uint256[] memory ids = new uint256[](2);
        ids[0] = TILE;
        ids[1] = t2; // bob's region
        vm.expectRevert(RegionAuthority.NotHolder.selector);
        vm.prank(alice);
        region.claimFeesBatch(ids);

        assertEq(region.accruedFees(TILE), FEE, "alice's region not swept by the reverted batch");
        assertEq(region.accruedFees(t2), FEE, "bob's region untouched, no cross-holder sweep");
    }

    /// @dev An intra-batch duplicate `[X, X]` reverts: the second occurrence reads the
    ///      balance the first zeroed (`NothingToClaim`), so a duplicate can never
    ///      double-claim, and the reverted batch leaves the region fully accrued.
    function test_claimFeesBatch_revertsOnIntraBatchDuplicate() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.depositFees(TILE, FEE);

        uint256 aliceBefore = token.balanceOf(alice);
        uint256[] memory ids = new uint256[](2);
        ids[0] = TILE;
        ids[1] = TILE;
        vm.expectRevert(RegionAuthority.NothingToClaim.selector);
        vm.prank(alice);
        region.claimFeesBatch(ids);

        assertEq(region.accruedFees(TILE), FEE, "no partial claim from the reverted duplicate batch");
        assertEq(token.balanceOf(alice), aliceBefore, "nothing transferred");
    }

    function test_claimFeesBatch_revertsOnEmptyBatch() public {
        uint256[] memory ids = new uint256[](0);
        vm.expectRevert(RegionAuthority.EmptyBatch.selector);
        vm.prank(alice);
        region.claimFeesBatch(ids);
    }

    /// @dev A batch AT exactly MAX_BATCH (64) succeeds and sweeps the full portfolio.
    function test_claimFeesBatch_boundsAtMaxBatch() public {
        uint256 n = region.MAX_BATCH();
        uint256[] memory ids = new uint256[](n);
        vm.startPrank(alice);
        for (uint256 i = 0; i < n; i++) {
            uint256 id = uint256(keccak256(abi.encode("maxbatch", i)));
            region.claim(id, STAKE);
            ids[i] = id;
        }
        vm.stopPrank();
        vm.startPrank(bob);
        for (uint256 i = 0; i < n; i++) {
            region.depositFees(ids[i], FEE);
        }
        vm.stopPrank();

        uint256 aliceBefore = token.balanceOf(alice);
        vm.prank(alice);
        region.claimFeesBatch(ids);
        assertEq(token.balanceOf(alice), aliceBefore + n * FEE, "the full max batch swept");
        assertEq(region.accruedFees(ids[0]), 0);
        assertEq(region.accruedFees(ids[n - 1]), 0);
    }

    /// @dev A batch ABOVE MAX_BATCH reverts BatchTooLarge before touching any state.
    function test_claimFeesBatch_revertsAboveMaxBatch() public {
        uint256[] memory ids = new uint256[](region.MAX_BATCH() + 1);
        vm.expectRevert(RegionAuthority.BatchTooLarge.selector);
        vm.prank(alice);
        region.claimFeesBatch(ids);
    }

    /// @dev CEI under batching: a hostile token reentering `claimFees` on the aggregated
    ///      fee payout finds the region's `accruedFees` already zeroed (Phase 1 zeroes
    ///      before the Phase 2 transfer) and reverts — no double-claim draining the pool.
    function test_claimFeesBatch_reentrantClaimerCannotDoubleClaim() public {
        HookToken evil = new HookToken();
        RegionAuthority r = new RegionAuthority(address(evil), STAKE, owner);
        ReentrantClaimer attacker = new ReentrantClaimer(r, evil);

        evil.transfer(address(attacker), STAKE);
        attacker.claim(TILE, STAKE); // attacker holds TILE; pool holds its STAKE
        evil.approve(address(r), type(uint256).max);
        r.depositFees(TILE, FEE); // accrue FEE; pool now holds STAKE + FEE

        uint256 poolBefore = evil.balanceOf(address(r));
        evil.arm(); // fire the reentry on the fee payout
        attacker.claimFeesBatch();

        assertTrue(attacker.reentered(), "reentry fired");
        assertTrue(attacker.reentryReverted(), "reentry blocked by CEI (accrued zeroed pre-transfer)");
        assertEq(evil.balanceOf(address(attacker)), FEE, "recovered exactly its own fee, not double");
        assertEq(evil.balanceOf(address(r)), poolBefore - FEE, "only the one fee left; the stake is intact");
        assertEq(r.accruedFees(TILE), 0);
    }

    // --- claimBatch (acquire a portfolio of regions in one tx, strict-atomic) ---

    /// @dev The headline: a staker mints three regions with DISTINCT stakes in ONE call —
    ///      a `Staked` per region, `totalStaked` and the single pull netting exactly the
    ///      summed stakes, every NFT minted to the caller with its own recorded stake.
    function test_claimBatch_mintsManyAndPullsSummedStakes() public {
        uint256 t2 = uint256(keccak256("region:3,4,0"));
        uint256 t3 = uint256(keccak256("region:5,6,0"));
        uint256 a1 = STAKE;
        uint256 a2 = STAKE + 50 ether;
        uint256 a3 = 2 * STAKE;
        uint256 sum = a1 + a2 + a3;

        uint256 aliceBefore = token.balanceOf(alice);
        vm.expectEmit(true, true, false, true);
        emit Staked(alice, TILE, a1);
        vm.expectEmit(true, true, false, true);
        emit Staked(alice, t2, a2);
        vm.expectEmit(true, true, false, true);
        emit Staked(alice, t3, a3);

        uint256[] memory ids = new uint256[](3);
        ids[0] = TILE;
        ids[1] = t2;
        ids[2] = t3;
        uint256[] memory amts = new uint256[](3);
        amts[0] = a1;
        amts[1] = a2;
        amts[2] = a3;
        vm.prank(alice);
        region.claimBatch(ids, amts);

        assertEq(region.ownerOf(TILE), alice);
        assertEq(region.ownerOf(t2), alice);
        assertEq(region.ownerOf(t3), alice);
        assertEq(region.balanceOf(alice), 3, "all three minted to the caller");
        assertEq(region.totalStaked(), sum, "totalStaked netted the summed stakes");
        assertEq(token.balanceOf(address(region)), sum, "contract pulled exactly the summed stakes");
        assertEq(token.balanceOf(alice), aliceBefore - sum, "caller paid exactly the summed stakes");
        (uint256 amt2, uint64 stakedAt2) = region.stakes(t2);
        assertEq(amt2, a2, "each region recorded its own stake");
        assertEq(stakedAt2, uint64(block.timestamp));
    }

    /// @dev FM1 strict-atomic: an already-claimed element reverts AlreadyClaimed and the
    ///      WHOLE batch rolls back — the fresh element is NOT minted and no token is pulled.
    function test_claimBatch_revertsOnAlreadyClaimedElement() public {
        uint256 t2 = uint256(keccak256("region:3,4,0"));
        vm.prank(bob);
        region.claim(t2, STAKE); // t2 already held by bob

        uint256 aliceBefore = token.balanceOf(alice);
        uint256[] memory ids = new uint256[](2);
        ids[0] = TILE; // fresh
        ids[1] = t2; // already claimed
        uint256[] memory amts = new uint256[](2);
        amts[0] = STAKE;
        amts[1] = STAKE;
        vm.expectRevert(RegionAuthority.AlreadyClaimed.selector);
        vm.prank(alice);
        region.claimBatch(ids, amts);

        assertFalse(region.regionExists(TILE), "the fresh element was NOT minted by the reverted batch");
        assertEq(region.totalStaked(), STAKE, "only bob's prior stake remains");
        assertEq(token.balanceOf(alice), aliceBefore, "no token pulled from the reverted batch");
    }

    /// @dev FM1: an intra-batch duplicate `[X, X]` reverts — the first occurrence mints X,
    ///      so the second finds it already claimed (AlreadyClaimed) and the whole batch rolls
    ///      back (one stake can never mint two regions or double-pull).
    function test_claimBatch_revertsOnIntraBatchDuplicate() public {
        uint256 aliceBefore = token.balanceOf(alice);
        uint256[] memory ids = new uint256[](2);
        ids[0] = TILE;
        ids[1] = TILE;
        uint256[] memory amts = new uint256[](2);
        amts[0] = STAKE;
        amts[1] = STAKE;
        vm.expectRevert(RegionAuthority.AlreadyClaimed.selector);
        vm.prank(alice);
        region.claimBatch(ids, amts);

        assertFalse(region.regionExists(TILE), "nothing minted by the reverted duplicate batch");
        assertEq(region.totalStaked(), 0);
        assertEq(token.balanceOf(alice), aliceBefore, "no token pulled");
    }

    /// @dev FM1: a too-low stake element reverts StakeTooLow atomically (mirrors claim's
    ///      per-element guard through the shared _claimOne) — no region in the batch mints.
    function test_claimBatch_revertsOnTooLowStakeElement() public {
        uint256 t2 = uint256(keccak256("region:3,4,0"));
        uint256[] memory ids = new uint256[](2);
        ids[0] = TILE;
        ids[1] = t2;
        uint256[] memory amts = new uint256[](2);
        amts[0] = STAKE;
        amts[1] = STAKE - 1; // below the floor
        vm.expectRevert(RegionAuthority.StakeTooLow.selector);
        vm.prank(alice);
        region.claimBatch(ids, amts);

        assertFalse(region.regionExists(TILE), "the valid element did not mint");
        assertEq(region.totalStaked(), 0);
    }

    /// @dev FM1: the two parallel arrays must be equal length — a mismatch reverts
    ///      ArrayLengthMismatch (an amount could otherwise be silently dropped or read OOB).
    function test_claimBatch_revertsOnLengthMismatch() public {
        uint256[] memory ids = new uint256[](2);
        ids[0] = TILE;
        ids[1] = uint256(keccak256("region:3,4,0"));
        uint256[] memory amts = new uint256[](1);
        amts[0] = STAKE;
        vm.expectRevert(RegionAuthority.ArrayLengthMismatch.selector);
        vm.prank(alice);
        region.claimBatch(ids, amts);
    }

    /// @dev FM3 safe-recipient: claimBatch mints via `_safeMint`, so a batch claimed by a
    ///      contract that cannot receive ERC721 reverts the whole call (a mutation to
    ///      `_mint` would silently strand the NFTs). Proves the batch keeps claim's guard.
    function test_claimBatch_revertsForNonReceiver() public {
        NonReceiverClaimer bad = new NonReceiverClaimer(region, token);
        assertTrue(token.transfer(address(bad), STAKE));

        uint256[] memory ids = new uint256[](1);
        ids[0] = TILE;
        uint256[] memory amts = new uint256[](1);
        amts[0] = STAKE;
        vm.expectRevert(abi.encodeWithSelector(IERC721Errors.ERC721InvalidReceiver.selector, address(bad)));
        bad.claimBatch(ids, amts);

        assertFalse(region.regionExists(TILE), "nothing minted to a non-receiver");
        assertEq(token.balanceOf(address(region)), 0, "the reverted mint rolled back its pull");
    }

    function test_claimBatch_revertsOnEmptyBatch() public {
        uint256[] memory ids = new uint256[](0);
        uint256[] memory amts = new uint256[](0);
        vm.expectRevert(RegionAuthority.EmptyBatch.selector);
        vm.prank(alice);
        region.claimBatch(ids, amts);
    }

    /// @dev A batch AT exactly MAX_BATCH (64) succeeds and mints the full portfolio.
    function test_claimBatch_boundsAtMaxBatch() public {
        uint256 n = region.MAX_BATCH();
        uint256[] memory ids = new uint256[](n);
        uint256[] memory amts = new uint256[](n);
        for (uint256 i = 0; i < n; i++) {
            ids[i] = uint256(keccak256(abi.encode("claimbatch", i)));
            amts[i] = STAKE;
        }

        uint256 aliceBefore = token.balanceOf(alice);
        vm.prank(alice);
        region.claimBatch(ids, amts);
        assertEq(region.balanceOf(alice), n, "the full max batch minted");
        assertEq(region.totalStaked(), n * STAKE);
        assertEq(token.balanceOf(alice), aliceBefore - n * STAKE, "pulled the full summed stake");
    }

    /// @dev A batch ABOVE MAX_BATCH reverts BatchTooLarge before touching any state.
    function test_claimBatch_revertsAboveMaxBatch() public {
        uint256 n = region.MAX_BATCH() + 1;
        uint256[] memory ids = new uint256[](n);
        uint256[] memory amts = new uint256[](n);
        vm.expectRevert(RegionAuthority.BatchTooLarge.selector);
        vm.prank(alice);
        region.claimBatch(ids, amts);
    }

    // --- unstakeBatch (exit a portfolio of regions in one tx, strict-atomic) ---

    /// @dev The headline: a holder exits three regions in ONE call — an `Unstaked` per
    ///      region and a SINGLE aggregated refund of the summed stakes, every NFT burned
    ///      and `totalStaked` netted to zero.
    function test_unstakeBatch_exitsManyRegionsInOneTransfer() public {
        uint256 t2 = uint256(keccak256("region:3,4,0"));
        uint256 t3 = uint256(keccak256("region:5,6,0"));
        vm.startPrank(alice);
        region.claim(TILE, STAKE);
        region.claim(t2, STAKE);
        region.claim(t3, STAKE);
        vm.stopPrank();
        assertEq(region.totalStaked(), 3 * STAKE);

        uint256 aliceBefore = token.balanceOf(alice);
        vm.expectEmit(true, true, false, true);
        emit Unstaked(alice, TILE, STAKE);
        vm.expectEmit(true, true, false, true);
        emit Unstaked(alice, t2, STAKE);
        vm.expectEmit(true, true, false, true);
        emit Unstaked(alice, t3, STAKE);

        uint256[] memory ids = new uint256[](3);
        ids[0] = TILE;
        ids[1] = t2;
        ids[2] = t3;
        vm.prank(alice);
        region.unstakeBatch(ids);

        assertEq(
            token.balanceOf(alice), aliceBefore + 3 * STAKE, "one aggregated refund of the summed stakes"
        );
        assertEq(token.balanceOf(address(region)), 0, "pool emptied");
        assertEq(region.totalStaked(), 0, "totalStaked netted the whole batch");
        assertEq(region.balanceOf(alice), 0, "all three NFTs burned");
        (uint256 amt,) = region.stakes(TILE);
        assertEq(amt, 0, "stake record deleted");
    }

    /// @dev FM2: every element's accrued fees settle to `withdrawable` exactly once on
    ///      burn (a batched exit strands no earned fees), and the books reconcile — after
    ///      the batch the only balance left is the settled fees (Σ stake = 0, Σ accrued = 0).
    function test_unstakeBatch_settlesAccruedFeesForEveryElement() public {
        uint256 t2 = uint256(keccak256("region:3,4,0"));
        uint256 t3 = uint256(keccak256("region:5,6,0"));
        vm.startPrank(alice);
        region.claim(TILE, STAKE);
        region.claim(t2, STAKE);
        region.claim(t3, STAKE);
        vm.stopPrank();
        vm.startPrank(bob);
        region.depositFees(TILE, FEE);
        region.depositFees(t2, 50 ether);
        region.depositFees(t3, 20 ether);
        vm.stopPrank();

        uint256 accruedSum = FEE + 50 ether + 20 ether;
        uint256 aliceBefore = token.balanceOf(alice);

        uint256[] memory ids = new uint256[](3);
        ids[0] = TILE;
        ids[1] = t2;
        ids[2] = t3;
        vm.prank(alice);
        region.unstakeBatch(ids);

        assertEq(token.balanceOf(alice), aliceBefore + 3 * STAKE, "stakes refunded immediately");
        assertEq(region.accruedFees(TILE), 0);
        assertEq(region.accruedFees(t2), 0);
        assertEq(region.accruedFees(t3), 0);
        assertEq(region.withdrawable(alice), accruedSum, "every element's accrued fees settled exactly once");
        assertEq(
            token.balanceOf(address(region)), accruedSum, "balance reconciles: only the settled fees remain"
        );

        vm.prank(alice);
        region.withdraw();
        assertEq(
            token.balanceOf(alice), aliceBefore + 3 * STAKE + accruedSum, "fees recovered, nothing stranded"
        );
        assertEq(token.balanceOf(address(region)), 0);
    }

    /// @dev FM1 strict-atomic: an element the caller does not hold reverts NotHolder and
    ///      the WHOLE batch rolls back — nothing burned, no partial refund, no cross-holder
    ///      exit of another owner's region.
    function test_unstakeBatch_revertsOnNonHeldElement() public {
        uint256 t2 = uint256(keccak256("region:3,4,0"));
        vm.prank(alice);
        region.claim(TILE, STAKE);
        vm.prank(bob);
        region.claim(t2, STAKE); // bob's region

        uint256 aliceBefore = token.balanceOf(alice);
        uint256[] memory ids = new uint256[](2);
        ids[0] = TILE;
        ids[1] = t2; // not held by alice
        vm.expectRevert(RegionAuthority.NotHolder.selector);
        vm.prank(alice);
        region.unstakeBatch(ids);

        assertEq(region.ownerOf(TILE), alice, "alice's region not burned by the reverted batch");
        assertEq(region.ownerOf(t2), bob, "bob's region untouched, no cross-holder exit");
        assertEq(region.totalStaked(), 2 * STAKE, "totalStaked unchanged");
        assertEq(token.balanceOf(alice), aliceBefore, "no refund from the reverted batch");
    }

    /// @dev FM1: an intra-batch duplicate `[X, X]` reverts — the first occurrence burns X,
    ///      so the second's `ownerOf` reverts ERC721NonexistentToken, and the whole batch
    ///      rolls back (no double refund of one stake).
    function test_unstakeBatch_revertsOnIntraBatchDuplicate() public {
        vm.prank(alice);
        region.claim(TILE, STAKE);

        uint256 aliceBefore = token.balanceOf(alice);
        uint256[] memory ids = new uint256[](2);
        ids[0] = TILE;
        ids[1] = TILE;
        vm.expectRevert(abi.encodeWithSelector(IERC721Errors.ERC721NonexistentToken.selector, TILE));
        vm.prank(alice);
        region.unstakeBatch(ids);

        assertEq(region.ownerOf(TILE), alice, "the reverted duplicate batch burned nothing");
        assertEq(region.totalStaked(), STAKE, "totalStaked unchanged");
        assertEq(token.balanceOf(alice), aliceBefore, "no refund");
    }

    function test_unstakeBatch_revertsOnEmptyBatch() public {
        uint256[] memory ids = new uint256[](0);
        vm.expectRevert(RegionAuthority.EmptyBatch.selector);
        vm.prank(alice);
        region.unstakeBatch(ids);
    }

    /// @dev A batch AT exactly MAX_BATCH (64) succeeds and exits the full portfolio.
    function test_unstakeBatch_boundsAtMaxBatch() public {
        uint256 n = region.MAX_BATCH();
        uint256[] memory ids = new uint256[](n);
        vm.startPrank(alice);
        for (uint256 i = 0; i < n; i++) {
            uint256 id = uint256(keccak256(abi.encode("unstakebatch", i)));
            region.claim(id, STAKE);
            ids[i] = id;
        }
        vm.stopPrank();
        assertEq(region.totalStaked(), n * STAKE);

        uint256 aliceBefore = token.balanceOf(alice);
        vm.prank(alice);
        region.unstakeBatch(ids);
        assertEq(token.balanceOf(alice), aliceBefore + n * STAKE, "the full max batch exited");
        assertEq(region.totalStaked(), 0);
        assertEq(region.balanceOf(alice), 0, "every NFT burned");
    }

    /// @dev A batch ABOVE MAX_BATCH reverts BatchTooLarge before touching any state.
    function test_unstakeBatch_revertsAboveMaxBatch() public {
        uint256[] memory ids = new uint256[](region.MAX_BATCH() + 1);
        vm.expectRevert(RegionAuthority.BatchTooLarge.selector);
        vm.prank(alice);
        region.unstakeBatch(ids);
    }

    /// @dev CEI under batching: a hostile token reentering `unstake` on the aggregated
    ///      refund finds the NFT already burned (Phase 1 burns before the Phase 2 transfer)
    ///      and reverts — no second withdrawal draining a co-staker's pooled funds.
    function test_unstakeBatch_reentrantHolderCannotDrainPool() public {
        HookToken evil = new HookToken();
        RegionAuthority r = new RegionAuthority(address(evil), STAKE, owner);
        ReentrantHolder attacker = new ReentrantHolder(r, evil);

        // Honest staker funds a distinct region so the pool holds 2*STAKE when the
        // attacker batch-exits — enough to pay a reentrant double-withdraw if it lands.
        uint256 aliceTile = uint256(keccak256("region:9,9,0"));
        evil.transfer(alice, STAKE);
        vm.prank(alice);
        evil.approve(address(r), type(uint256).max);
        vm.prank(alice);
        r.claim(aliceTile, STAKE);

        evil.transfer(address(attacker), STAKE);
        attacker.claim(TILE, STAKE);
        assertEq(evil.balanceOf(address(r)), 2 * STAKE);

        evil.arm(); // fire the holder's reentry on the aggregated refund
        attacker.unstakeBatch();

        assertTrue(attacker.reentered());
        assertTrue(attacker.reentryReverted(), "reentry blocked: the NFT was burned in Phase 1");
        assertEq(evil.balanceOf(address(attacker)), STAKE, "recovered exactly its own stake");
        assertEq(evil.balanceOf(address(r)), STAKE, "honest staker's funds intact");
        assertEq(r.balanceOf(address(attacker)), 0);
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

    function unstakeBatch() external {
        uint256[] memory ids = new uint256[](1);
        ids[0] = tokenId;
        region.unstakeBatch(ids);
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

    function claimFeesBatch() external {
        uint256[] memory ids = new uint256[](1);
        ids[0] = tokenId;
        region.claimFeesBatch(ids);
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

/// @notice Claims a batch but does NOT implement onERC721Received, so `_safeMint` to it
///         reverts — proving claimBatch keeps claim's safe-recipient guard (a `_mint`
///         mutation would silently strand the NFTs here).
contract NonReceiverClaimer {
    RegionAuthority region;

    constructor(RegionAuthority region_, ERC20 token_) {
        region = region_;
        token_.approve(address(region_), type(uint256).max);
    }

    function claimBatch(uint256[] calldata ids, uint256[] calldata amounts) external {
        region.claimBatch(ids, amounts);
    }
}
