// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ArtifactTemplate, IComputeMeter, IRegionAuthority} from "../src/ArtifactTemplate.sol";
import {ComputeMeter} from "../src/ComputeMeter.sol";
import {RegionAuthority} from "../src/RegionAuthority.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC1155Receiver} from "@openzeppelin/contracts/token/ERC1155/IERC1155Receiver.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20Errors, IERC1155Errors} from "@openzeppelin/contracts/interfaces/draft-IERC6093.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

contract ArtifactTemplateTest is Test {
    ArtifactTemplate art;
    ComputeMeter meter;
    RegionAuthority region;
    MockToken token;

    address owner = address(0xA11CE);
    address minter = address(0x1117E2);
    address author = address(0xA47403);
    address player = address(0xB0B);
    address stranger = address(0xBEEF);
    // Region holder (an EOA so RegionAuthority's _safeMint receiver check passes) that
    // earns and claims the royalties deposited against `regionId`.
    address regionHolder = address(0x5E6104);

    string constant BASE_URI = "ipfs://base/{id}.json";
    uint256 constant FEE_RATE = 1e12;
    uint256 constant ROYALTY_RATE = 1e12;
    uint256 constant STAKE = 100 ether;
    // A claimed region (held by this test contract) the royalty deposits into.
    uint256 regionId = uint256(keccak256("region-A"));

    event TemplateRegistered(
        uint256 indexed templateId, address indexed author, uint16 rarity, bytes32 manifest
    );
    event Minted(address indexed to, uint256 indexed templateId, uint256 amount);
    event Burned(address indexed from, uint256 indexed templateId, uint256 amount);
    event MinterSet(address indexed minter);
    event MintFeeRateSet(uint256 rate);
    event RoyaltyRateSet(uint256 rate);
    event URISet(string uri);
    event RoyaltyRouted(uint256 indexed templateId, uint256 indexed regionId, uint256 amount);
    // ComputeMeter's debit event, re-declared for expectEmit against the meter.
    event Spent(address indexed buyer, address indexed spender, uint256 amount, bytes32 jobId);
    // RegionAuthority's intake event, re-declared for expectEmit against the region.
    event FeesDeposited(uint256 indexed tokenId, address indexed from, uint256 amount);

    function setUp() public {
        token = new MockToken(); // this test contract holds the 1M supply
        meter = new ComputeMeter(address(token), owner);
        region = new RegionAuthority(address(token), STAKE, owner);
        art = new ArtifactTemplate(owner, BASE_URI, address(meter), FEE_RATE, address(region), ROYALTY_RATE);

        vm.startPrank(owner);
        art.setMinter(minter);
        meter.setSpender(address(art), true);
        vm.stopPrank();

        // Claim the royalty region so depositFees won't revert UnknownRegion. A dedicated
        // EOA holder stakes and becomes the region's fee earner for the claimFees
        // assertions (an EOA passes RegionAuthority's _safeMint receiver check).
        token.transfer(regionHolder, STAKE);
        vm.startPrank(regionHolder);
        token.approve(address(region), STAKE);
        region.claim(regionId, STAKE);
        vm.stopPrank();

        // Fund the recipients: compute credit for the burned mint fee, AND real
        // $BLCKFLD (approved to art) for the region royalty — two independent ledgers.
        _credit(player, 1_000 ether);
        _credit(stranger, 1_000 ether);
        _fundRoyalty(player, 1_000 ether);
        _fundRoyalty(stranger, 1_000 ether);
    }

    /// @dev Burn `amount` $TOKEN into `who`'s ComputeMeter credit so a fee-charging
    ///      mint to `who` has balance to debit.
    function _credit(address who, uint256 amount) internal {
        token.transfer(who, amount);
        vm.startPrank(who);
        token.approve(address(meter), amount);
        meter.deposit(amount);
        vm.stopPrank();
    }

    /// @dev Give `who` real $BLCKFLD and approve `art` to pull it, so a royalty-charging
    ///      mint to `who` can transfer the royalty. Distinct from `_credit`: that burns
    ///      into compute credit, this leaves a spendable balance.
    function _fundRoyalty(address who, uint256 amount) internal {
        token.transfer(who, amount);
        vm.prank(who);
        token.approve(address(art), type(uint256).max);
    }

    // --- construction ---

    function test_constructor() public view {
        assertEq(art.owner(), owner);
        assertEq(art.uri(0), BASE_URI);
        assertEq(art.minter(), minter);
        assertEq(art.nextTemplateId(), 0);
        assertEq(address(art.computeMeter()), address(meter));
        assertEq(art.mintFeeRate(), FEE_RATE);
        assertEq(address(art.regionAuthority()), address(region));
        assertEq(art.royaltyRate(), ROYALTY_RATE);
        // The royalty token is derived from RegionAuthority.TOKEN(), never desyncing
        // from what depositFees pulls.
        assertEq(address(art.royaltyToken()), address(token));
    }

    // --- setMinter ---

    function test_setMinter_onlyOwner() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, stranger));
        vm.prank(stranger);
        art.setMinter(stranger);
    }

    function test_setMinter_updates() public {
        vm.prank(owner);
        art.setMinter(stranger);
        assertEq(art.minter(), stranger);
    }

    function test_setMinter_emits() public {
        vm.expectEmit(true, false, false, true);
        emit MinterSet(stranger);
        vm.prank(owner);
        art.setMinter(stranger);
    }

    /// @dev FM1: a zero minter is rejected, not silently accepted. Accepting it would
    ///      brick every mint/register (msg.sender == minter is unsatisfiable at zero), so
    ///      the guard reverts before the storage write.
    function test_setMinter_revertsZeroMinter() public {
        vm.expectRevert(ArtifactTemplate.ZeroMinter.selector);
        vm.prank(owner);
        art.setMinter(address(0));
    }

    /// @dev FM2: a rejected setMinter(0) leaves the prior minter untouched (the revert is
    ///      before the write), and a corrective non-zero set still works — the owner keeps
    ///      recovery control, the gate is never half-mutated.
    function test_setMinter_zeroRevertLeavesMinterIntactAndRecoverable() public {
        assertEq(art.minter(), minter);
        vm.prank(owner);
        vm.expectRevert(ArtifactTemplate.ZeroMinter.selector);
        art.setMinter(address(0));
        assertEq(art.minter(), minter);

        vm.prank(owner);
        art.setMinter(stranger);
        assertEq(art.minter(), stranger);
    }

    /// @dev FM3: the guard rejects ONLY zero — it must not over-reach onto a legitimate
    ///      non-zero rotation. address(1), the smallest non-zero, is the boundary the
    ///      guard must still accept.
    function test_setMinter_acceptsSmallestNonZero() public {
        vm.expectEmit(true, false, false, true);
        emit MinterSet(address(1));
        vm.prank(owner);
        art.setMinter(address(1));
        assertEq(art.minter(), address(1));
    }

    /// @dev FM4: the constructor's bootstrap state (minter == 0, minting disabled until
    ///      wired) is untouched by the setter guard — a fresh deploy succeeds with a zero
    ///      minter and mint/register revert NotMinter (not the owner, not anyone), so zero
    ///      stays a valid INITIAL state even though it is no longer a reachable SET state.
    function test_freshDeployBootstrapsDisabledWithZeroMinter() public {
        ArtifactTemplate fresh =
            new ArtifactTemplate(owner, BASE_URI, address(meter), FEE_RATE, address(region), ROYALTY_RATE);
        assertEq(fresh.minter(), address(0));

        vm.prank(owner);
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        fresh.registerTemplate(author, 0, bytes32(0), 0);

        vm.prank(owner);
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        fresh.mint(player, 1, 1, regionId, "");
    }

    // --- setURI ---

    function test_setURI_updatesAndEmits() public {
        vm.expectEmit(false, false, false, true);
        emit URISet("ipfs://v2/{id}.json");
        vm.prank(owner);
        art.setURI("ipfs://v2/{id}.json");
        assertEq(art.uri(123), "ipfs://v2/{id}.json");
    }

    function test_setURI_onlyOwner() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, stranger));
        vm.prank(stranger);
        art.setURI("x");
    }

    // --- registerTemplate ---

    function test_registerTemplate_happyPath() public {
        bytes32 manifest = keccak256("usd-bundle");
        uint16 rarity = 7500;

        vm.expectEmit(true, true, false, true);
        emit TemplateRegistered(1, author, rarity, manifest);

        vm.prank(minter);
        uint256 id = art.registerTemplate(author, rarity, manifest, 0);

        assertEq(id, 1);
        assertEq(art.nextTemplateId(), 1);

        (address a, uint16 r, bytes32 m) = art.templates(1);
        assertEq(a, author);
        assertEq(r, rarity);
        assertEq(m, manifest);
    }

    function test_registerTemplate_incrementsIds() public {
        vm.startPrank(minter);
        uint256 id1 = art.registerTemplate(author, 1, keccak256("a"), 0);
        uint256 id2 = art.registerTemplate(player, 2, keccak256("b"), 0);
        uint256 id3 = art.registerTemplate(author, 3, keccak256("c"), 0);
        vm.stopPrank();

        assertEq(id1, 1);
        assertEq(id2, 2);
        assertEq(id3, 3);
        assertEq(art.nextTemplateId(), 3);

        (address a2,,) = art.templates(2);
        assertEq(a2, player);
    }

    function test_registerTemplate_revertsNotMinter() public {
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(stranger);
        art.registerTemplate(author, 1, bytes32(0), 0);
    }

    function test_registerTemplate_revertsZeroAuthor() public {
        // author==0 is the UnknownTemplate sentinel; registering it would consume
        // an id + a leaderboard slot for a template no one can ever mint.
        vm.expectRevert(ArtifactTemplate.ZeroAuthor.selector);
        vm.prank(minter);
        art.registerTemplate(address(0), 1, keccak256("m"), 0);

        assertEq(art.nextTemplateId(), 0);
        assertEq(art.templatesByAuthor(address(0)), 0);
    }

    function test_registerTemplate_revertsRarityAboveMax() public {
        vm.expectRevert(abi.encodeWithSelector(ArtifactTemplate.InvalidRarity.selector, uint16(10001)));
        vm.prank(minter);
        art.registerTemplate(author, 10001, keccak256("m"), 0);

        assertEq(art.nextTemplateId(), 0);
    }

    function test_registerTemplate_acceptsRarityAtMax() public {
        // The bound is inclusive: 10000 bps == 100% is a valid full-scale rarity.
        // Read MAX_RARITY before the prank — an art.* call in the arg list would
        // otherwise consume the prank and registerTemplate would revert NotMinter.
        uint16 maxRarity = art.MAX_RARITY();
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, maxRarity, keccak256("m"), 0);

        (, uint16 r,) = art.templates(id);
        assertEq(r, 10_000);
    }

    // --- mint ---

    function test_mint_happyPath() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1000, keccak256("m"), 0);

        vm.expectEmit(true, true, false, true);
        emit Minted(player, id, 5);

        vm.prank(minter);
        art.mint(player, id, 5, regionId, "");

        assertEq(art.balanceOf(player, id), 5);
    }

    function test_mint_revertsNotMinter() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"), 0);

        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(stranger);
        art.mint(player, id, 1, regionId, "");
    }

    function test_mint_revertsUnknownTemplate() public {
        vm.expectRevert(ArtifactTemplate.UnknownTemplate.selector);
        vm.prank(minter);
        art.mint(player, 999, 1, regionId, "");
    }

    function test_mint_revertsZeroRecipient() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"), 0);

        vm.expectRevert(ArtifactTemplate.ZeroRecipient.selector);
        vm.prank(minter);
        art.mint(address(0), id, 1, regionId, "");

        assertEq(art.totalMinted(), 0);
        assertEq(art.mintedByTemplate(id), 0);
    }

    function test_mint_revertsZeroAmount() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"), 0);

        // A zero-amount mint would emit a spurious Minted/TransferSingle and fire
        // the receiver hook for nothing; reject it instead.
        vm.expectRevert(ArtifactTemplate.ZeroAmount.selector);
        vm.prank(minter);
        art.mint(player, id, 0, regionId, "");

        assertEq(art.totalMinted(), 0);
        assertEq(art.balanceOf(player, id), 0);
    }

    /// @dev A minter contract that is also the mint recipient reenters mint() from
    ///      its ERC-1155 receiver hook. The counters are unconditional commutative
    ///      adds with no reentrant read-before-write, so the quiescent totals are
    ///      exact under either ordering — the `== 8` checks below are commutativity
    ///      sanity checks, not the CEI proof. The discriminator is the mid-hook
    ///      observation: with effects-before-interaction the reentrant call sees the
    ///      outer mint already counted (the pre-reorder ordering would show 0). The
    ///      reorder is the correct pattern (OZ warns against state writes after the
    ///      acceptance check) and keeps the contract robust to a future counter read
    ///      that gates behavior — it is not patching a live drift bug.
    function test_mint_reentrantMinterKeepsCounterIntegrity() public {
        ReentrantMinter rm = new ReentrantMinter(art);
        vm.prank(owner);
        art.setMinter(address(rm));
        // rm is both minter and recipient, so it pays the compute fee AND the region
        // royalty on the outer and the reentrant mint — fund + approve it for both.
        _credit(address(rm), 1_000 ether);
        _fundRoyalty(address(rm), 1_000 ether);

        uint256 id = rm.register(author, 1000);
        rm.fire(5, 3, regionId); // outer mint 5; the receiver hook reenters and mints 3 more

        assertEq(art.totalMinted(), 8); // commutativity sanity — holds under either ordering
        assertEq(art.mintedByTemplate(id), 8);
        assertEq(art.balanceOf(address(rm), id), 8);
        // CEI discriminator: the outer mint's counter write landed before its hook
        // reentered, so the reentrant call observed the outer 5 already counted.
        assertEq(rm.observedTotalMintedAtHook(), 5);
    }

    function test_mint_multipleAccumulatesBalance() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"), 0);

        vm.startPrank(minter);
        art.mint(player, id, 3, regionId, "");
        art.mint(player, id, 4, regionId, "");
        vm.stopPrank();

        assertEq(art.balanceOf(player, id), 7);
    }

    function test_mint_passesDataThrough() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"), 0);
        // Non-empty data must not revert for an EOA recipient.
        vm.prank(minter);
        art.mint(player, id, 2, regionId, hex"deadbeef");
        assertEq(art.balanceOf(player, id), 2);
    }

    // --- mint fee (ComputeMeter) ---

    function test_mint_chargesRarityScaledFee() public {
        vm.prank(owner);
        art.setMintFeeRate(2e15);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 5000, keccak256("m"), 0); // 50% rarity

        // ceil(2e15 * 4 * 5000 / 10000) = 4e15, exact (no rounding).
        uint256 expectedFee = 4e15;
        uint256 creditBefore = meter.credit(player);

        // The debit is observable as ComputeMeter.Spent(buyer=to, spender=art, fee,
        // jobId=templateId) — the mint carries the templateId as the spend jobId.
        vm.expectEmit(true, true, false, true, address(meter));
        emit Spent(player, address(art), expectedFee, bytes32(id));

        vm.prank(minter);
        art.mint(player, id, 4, regionId, "");

        assertEq(meter.credit(player), creditBefore - expectedFee, "credit debited by fee");
        assertEq(meter.spentByBuyer(player), expectedFee, "spend recorded");
        assertEq(art.balanceOf(player, id), 4, "units still minted");
    }

    function test_mint_feeRoundsUp() public {
        // rate=1, rarity=1, amount=1 -> raw 1/10000 < 1. Floor would round to a free
        // mint; ceil charges exactly 1 unit. Pins the rounding direction.
        vm.prank(owner);
        art.setMintFeeRate(1);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"), 0);

        uint256 creditBefore = meter.credit(player);
        vm.prank(minter);
        art.mint(player, id, 1, regionId, "");

        assertEq(meter.credit(player), creditBefore - 1, "ceil charges 1, not 0");
        assertEq(meter.spentByBuyer(player), 1);
    }

    function test_mint_revertsOnInsufficientCredit() public {
        // poor has no compute credit, so the fee debit reverts and nothing mints.
        address poor = address(0xDEAD11);
        vm.prank(owner);
        art.setMintFeeRate(1e15);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 10_000, keccak256("m"), 0);

        vm.expectRevert(ComputeMeter.InsufficientCredit.selector);
        vm.prank(minter);
        art.mint(poor, id, 1, regionId, "");

        assertEq(art.balanceOf(poor, id), 0, "no units minted");
        assertEq(art.totalMinted(), 0, "counter rolled back");
        assertEq(art.mintedByTemplate(id), 0, "per-template counter rolled back");
    }

    function test_mint_revertsWhenUnauthorizedSpender() public {
        // FM1: until the deploy authorizes ArtifactTemplate as a ComputeMeter spender,
        // every fee-charging mint reverts NotAuthorized at the meter — the on-chain fee
        // wiring is inert. Revoke the setUp authorization and prove the mint reverts
        // even though the recipient is fully funded (the revert is the authorization,
        // not the credit), minting nothing.
        vm.prank(owner);
        meter.setSpender(address(art), false);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 5000, keccak256("m"), 0);

        vm.expectRevert(ComputeMeter.NotAuthorized.selector);
        vm.prank(minter);
        art.mint(player, id, 4, regionId, "");

        assertEq(art.balanceOf(player, id), 0, "no units minted");
        assertEq(art.totalMinted(), 0, "counter rolled back");
        assertEq(art.mintedByTemplate(id), 0, "per-template counter rolled back");
    }

    function test_mint_zeroRarityIsFree() public {
        // A 0-bps template charges no fee, so even a zero-credit recipient can mint
        // it. The global fee gate stays closed by the non-zero mintFeeRate guard.
        address poor = address(0xF00D);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 0, keccak256("m"), 0);

        vm.prank(minter);
        art.mint(poor, id, 7, regionId, "");

        assertEq(art.balanceOf(poor, id), 7);
        assertEq(meter.spentByBuyer(poor), 0, "no fee debited for rarity-0");
    }

    function test_mint_reentrantSpenderKeepsCEI() public {
        // The fee debit is a second external call inside mint (alongside _mint). A
        // misbehaving meter reenters mint() from spend(); CEI orders the supply
        // counters before that interaction, so the reentrant mint observes the outer
        // mint already counted and the quiescent totals stay exact.
        ReentrantSpender rs = new ReentrantSpender();
        ArtifactTemplate art2 =
            new ArtifactTemplate(owner, BASE_URI, address(rs), FEE_RATE, address(region), ROYALTY_RATE);
        vm.prank(owner);
        art2.setMinter(address(rs));
        rs.setArt(art2);
        // player pays the royalty on art2 too (rarity 1000); approve it. Funded in setUp.
        vm.prank(player);
        token.approve(address(art2), type(uint256).max);

        uint256 id = rs.register(author, 1000);
        rs.fire(player, 5, 3, regionId); // outer mint 5; spend reenters and mints 3 more

        assertEq(art2.totalMinted(), 8, "totals exact under reentrancy");
        assertEq(art2.mintedByTemplate(id), 8);
        assertEq(art2.balanceOf(player, id), 8);
        // The discriminator: the outer counter write landed before spend reentered.
        assertEq(rs.observedTotalMintedAtSpend(), 5, "CEI: outer effects precede the spend");
    }

    // --- mint royalty (RegionAuthority) ---

    function test_mint_routesRoyaltyToRegionAndHolderClaims() public {
        // Distinct fee and royalty rates so the two ledgers move by DIFFERENT amounts —
        // proving they're independent flows (FM1), not the same value double-counted.
        vm.startPrank(owner);
        art.setMintFeeRate(2e15);
        art.setRoyaltyRate(3e15);
        vm.stopPrank();
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 5000, keccak256("m"), 0); // 50% rarity

        uint256 expectedFee = 4e15; // ceil(2e15 * 4 * 5000 / 10000)
        uint256 expectedRoyalty = 6e15; // ceil(3e15 * 4 * 5000 / 10000)

        uint256 creditBefore = meter.credit(player);
        uint256 balBefore = token.balanceOf(player);
        uint256 accruedBefore = region.accruedFees(regionId);

        vm.expectEmit(true, true, false, true, address(art));
        emit RoyaltyRouted(id, regionId, expectedRoyalty);

        vm.prank(minter);
        art.mint(player, id, 4, regionId, "");

        // Both ledgers move, by their own rate: burned compute credit AND real $BLCKFLD.
        assertEq(meter.credit(player), creditBefore - expectedFee, "compute credit debited by fee");
        assertEq(token.balanceOf(player), balBefore - expectedRoyalty, "real token debited by royalty");
        assertEq(region.accruedFees(regionId), accruedBefore + expectedRoyalty, "region accrued the royalty");
        assertEq(art.balanceOf(player, id), 4, "units minted");

        // The region holder withdraws exactly the routed royalty.
        uint256 holderBefore = token.balanceOf(regionHolder);
        vm.prank(regionHolder);
        region.claimFees(regionId);
        assertEq(token.balanceOf(regionHolder), holderBefore + expectedRoyalty, "holder claimed royalty");
        assertEq(region.accruedFees(regionId), 0, "accrued zeroed after claim");
    }

    function test_mint_royaltyRoundsUp() public {
        // rate=1, rarity=1, amount=1 -> raw 1/10000 < 1. Floor would route nothing; ceil
        // charges exactly 1. Pins the rounding direction, mirroring the compute fee.
        vm.prank(owner);
        art.setRoyaltyRate(1);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"), 0);

        uint256 balBefore = token.balanceOf(player);
        vm.prank(minter);
        art.mint(player, id, 1, regionId, "");

        assertEq(token.balanceOf(player), balBefore - 1, "ceil charges 1, not 0");
        assertEq(region.accruedFees(regionId), 1, "1 unit accrued to the region");
    }

    function test_mint_zeroRarityDepositsNoRoyalty() public {
        // rarity-0 (the 0-bps common tier) skips the royalty entirely: no real token is
        // pulled and the region accrues nothing, even with a claimed region named.
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 0, keccak256("m"), 0);

        uint256 balBefore = token.balanceOf(player);
        uint256 accruedBefore = region.accruedFees(regionId);
        vm.prank(minter);
        art.mint(player, id, 7, regionId, "");

        assertEq(art.balanceOf(player, id), 7);
        assertEq(token.balanceOf(player), balBefore, "no real token pulled for rarity-0");
        assertEq(region.accruedFees(regionId), accruedBefore, "no royalty deposited for rarity-0");
    }

    function test_mint_revertsUnknownRegion() public {
        // A royalty-charging mint naming an unclaimed region reverts (depositFees guards
        // UnknownRegion) and unwinds the whole mint — including the royalty token pull.
        uint256 ghostRegion = uint256(keccak256("never-claimed"));
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 5000, keccak256("m"), 0);

        uint256 balBefore = token.balanceOf(player);
        vm.expectRevert(RegionAuthority.UnknownRegion.selector);
        vm.prank(minter);
        art.mint(player, id, 4, ghostRegion, "");

        assertEq(art.balanceOf(player, id), 0, "no units minted");
        assertEq(art.totalMinted(), 0, "counters rolled back");
        assertEq(token.balanceOf(player), balBefore, "royalty token pull rolled back");
    }

    function test_mint_revertsOnInsufficientRoyaltyAllowance() public {
        // The recipient holds real $BLCKFLD but never approved art, so the royalty pull
        // reverts. The compute fee (charged first) is funded, so the revert is the
        // royalty allowance — and the whole mint unwinds, minting nothing.
        address payer = address(0xA110C);
        _credit(payer, 1_000 ether); // compute credit (real tokens burned away)
        token.transfer(payer, 1_000 ether); // real balance, but NO approval to art
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 5000, keccak256("m"), 0);

        // royalty = ceil(ROYALTY_RATE * 4 * 5000 / 10000) = 2e12; art is the spender,
        // its allowance from payer is 0. Exact match, so the revert can't be a different
        // (earlier) failure.
        vm.expectRevert(
            abi.encodeWithSelector(IERC20Errors.ERC20InsufficientAllowance.selector, address(art), 0, 2e12)
        );
        vm.prank(minter);
        art.mint(payer, id, 4, regionId, "");

        assertEq(art.balanceOf(payer, id), 0, "no units minted");
        assertEq(art.totalMinted(), 0, "counter rolled back");
        assertEq(meter.spentByBuyer(payer), 0, "compute spend rolled back");
        assertEq(region.accruedFees(regionId), 0, "no royalty routed");
    }

    function test_mint_revertsOnInsufficientRoyaltyBalance() public {
        // The recipient approved art but holds no real $BLCKFLD (all burned into credit),
        // so the royalty pull reverts on balance and the whole mint unwinds.
        address payer = address(0xBA1A);
        _credit(payer, 1_000 ether); // compute credit; leaves 0 real balance
        vm.prank(payer);
        token.approve(address(art), type(uint256).max);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 5000, keccak256("m"), 0);

        // royalty = 2e12 (as above); payer's real balance is 0 after _credit burned it.
        // Exact match, so the revert is specifically the royalty pull, not an earlier one.
        vm.expectRevert(
            abi.encodeWithSelector(IERC20Errors.ERC20InsufficientBalance.selector, payer, 0, 2e12)
        );
        vm.prank(minter);
        art.mint(payer, id, 4, regionId, "");

        assertEq(art.balanceOf(payer, id), 0, "no units minted");
        assertEq(art.totalMinted(), 0, "counter rolled back");
        assertEq(region.accruedFees(regionId), 0, "no royalty routed");
    }

    function test_mint_royaltyCreditsAnyNamedClaimedRegion() public {
        // Attribution is off-chain policy, NOT on-chain-enforced (FM3): the minter names
        // the region and the royalty accrues there regardless of any artifact<->region
        // link. A second claimed region receives a mint's royalty while region A gets none.
        uint256 otherRegion = uint256(keccak256("region-B"));
        address otherHolder = address(0x0EE2);
        token.transfer(otherHolder, STAKE);
        vm.startPrank(otherHolder);
        token.approve(address(region), STAKE);
        region.claim(otherRegion, STAKE);
        vm.stopPrank();

        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 5000, keccak256("m"), 0);
        vm.prank(minter);
        art.mint(player, id, 4, otherRegion, ""); // routed to region B, not setUp's region A

        assertGt(region.accruedFees(otherRegion), 0, "royalty accrued to the named region");
        assertEq(region.accruedFees(regionId), 0, "the unrelated region A got nothing");
    }

    // --- royalty-rate config ---

    function test_setRoyaltyRate_updatesAndEmits() public {
        vm.expectEmit(false, false, false, true);
        emit RoyaltyRateSet(77e12);
        vm.prank(owner);
        art.setRoyaltyRate(77e12);
        assertEq(art.royaltyRate(), 77e12);
    }

    function test_setRoyaltyRate_revertsZero() public {
        vm.expectRevert(ArtifactTemplate.ZeroRoyaltyRate.selector);
        vm.prank(owner);
        art.setRoyaltyRate(0);
    }

    function test_setRoyaltyRate_onlyOwner() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, stranger));
        vm.prank(stranger);
        art.setRoyaltyRate(123);
    }

    function test_mint_hostileRoyaltyTokenCannotReenterMint() public {
        // FM2: a hostile $BLCKFLD that tries to reenter mint() from its royalty
        // transferFrom is blocked by the onlyMinter gate (the token is not the minter), so
        // it can neither double-mint nor double-route. The reentry's NotMinter bubbles up
        // and reverts the whole mint — a hostile fee token can break an honest mint but
        // never exploit it. Access control, not just CEI, closes this vector.
        ReentrantRoyaltyToken evil = new ReentrantRoyaltyToken();
        ReentrantRegionAuthority rauth = new ReentrantRegionAuthority();
        rauth.setToken(address(evil));
        ArtifactTemplate art3 =
            new ArtifactTemplate(owner, BASE_URI, address(meter), FEE_RATE, address(rauth), ROYALTY_RATE);
        vm.startPrank(owner);
        art3.setMinter(minter);
        meter.setSpender(address(art3), true);
        vm.stopPrank();

        vm.prank(minter);
        uint256 id = art3.registerTemplate(author, 1000, keccak256("evil"), 0);
        evil.arm(art3, id, regionId, player, 3); // reentry attempt: 3 more to player

        vm.expectRevert(ArtifactTemplate.NotMinter.selector); // reentry blocked by access control
        vm.prank(minter);
        art3.mint(player, id, 5, regionId, "");

        assertEq(art3.totalMinted(), 0, "hostile-token reentry minted nothing");
        assertEq(art3.balanceOf(player, id), 0);
    }

    // --- supply cap ---

    function test_registerTemplate_storesMaxSupply() public {
        vm.startPrank(minter);
        uint256 capped = art.registerTemplate(author, 0, keccak256("c"), 100);
        uint256 uncapped = art.registerTemplate(author, 0, keccak256("u"), 0);
        vm.stopPrank();
        assertEq(art.templateMaxSupply(capped), 100);
        assertEq(art.templateMaxSupply(uncapped), 0, "0 means uncapped");
    }

    function test_mint_cappedExactFillSucceedsThenOverUnitReverts() public {
        // rarity 0 => free mint, so the cap is exercised in isolation from the fee.
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 0, keccak256("m"), 10);

        // The boundary mint that exactly fills the cap succeeds.
        vm.prank(minter);
        art.mint(player, id, 10, regionId, "");
        assertEq(art.balanceOf(player, id), 10);
        assertEq(art.mintedByTemplate(id), 10);

        // One unit over reverts with the exact SupplyExceeded args and leaves the
        // counters untouched (the check is effects-before-interaction).
        vm.expectRevert(abi.encodeWithSelector(ArtifactTemplate.SupplyExceeded.selector, id, 11, 10));
        vm.prank(minter);
        art.mint(player, id, 1, regionId, "");
        assertEq(art.mintedByTemplate(id), 10, "a rejected mint must not bump the counter");
        assertEq(art.totalMinted(), 10);
    }

    function test_mint_cappedAccumulatesAcrossMintsUpToCap() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 0, keccak256("m"), 10);

        vm.startPrank(minter);
        art.mint(player, id, 4, regionId, "");
        art.mint(player, id, 4, regionId, "");
        art.mint(player, id, 2, regionId, ""); // exact fill at the cap
        vm.stopPrank();
        assertEq(art.mintedByTemplate(id), 10);

        vm.expectRevert(abi.encodeWithSelector(ArtifactTemplate.SupplyExceeded.selector, id, 11, 10));
        vm.prank(minter);
        art.mint(player, id, 1, regionId, "");
    }

    function test_mint_cappedRejectsBatchExceedingCapInOneCall() public {
        // FM1: a single batched amount over the cap must revert, not slip past.
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 0, keccak256("m"), 5);

        vm.expectRevert(abi.encodeWithSelector(ArtifactTemplate.SupplyExceeded.selector, id, 6, 5));
        vm.prank(minter);
        art.mint(player, id, 6, regionId, "");
        assertEq(art.mintedByTemplate(id), 0, "nothing minted past the cap");
        assertEq(art.totalMinted(), 0);
    }

    function test_mint_uncappedAllowsArbitrarilyLargeMint() public {
        // FM2: maxSupply == 0 is UNCAPPED, never a cap of zero — a huge mint lands.
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 0, keccak256("m"), 0);

        vm.prank(minter);
        art.mint(player, id, 1e30, regionId, "");
        assertEq(art.balanceOf(player, id), 1e30);
        assertEq(art.mintedByTemplate(id), 1e30);
    }

    function test_mint_capHoldsUnderReentrancy() public {
        // A reentrant mint cannot breach the cap. cap=8, outer=5, reentry=5: the
        // outer commits 5, then _mint's receiver hook reenters mint(5); the reentry's
        // check is 5+5=10 > 8 → SupplyExceeded, which propagates through the hook and
        // reverts the whole tx, so nothing is committed and the cap stays intact.
        // (This is an all-or-nothing safety check, not the CEI-ordering discriminator:
        // the cap holds under either counter-ordering because the outermost over-cap
        // check unwinds the whole tx. The "counters commit before _mint" ordering
        // itself is pinned by test_mint_reentrantMinterKeepsCounterIntegrity, which
        // asserts the reentrant hook observes the outer mint already counted.)
        ReentrantMinter rm = new ReentrantMinter(art);
        vm.prank(owner);
        art.setMinter(address(rm));
        uint256 id = rm.registerCapped(author, 0, 8); // rarity 0 = free mint, no royalty

        vm.expectRevert(); // SupplyExceeded bubbles up through the receiver hook
        rm.fire(5, 5, regionId);

        // The whole tx reverted: the cap was never breached, nothing minted.
        assertEq(art.mintedByTemplate(id), 0);
        assertEq(art.totalMinted(), 0);
        assertEq(art.balanceOf(address(rm), id), 0);
    }

    // --- fee-rate config ---

    function test_constructor_revertsZeroComputeMeter() public {
        vm.expectRevert(ArtifactTemplate.ZeroComputeMeter.selector);
        new ArtifactTemplate(owner, BASE_URI, address(0), FEE_RATE, address(region), ROYALTY_RATE);
    }

    function test_constructor_revertsZeroFeeRate() public {
        // A zero rate would re-open free mints for every template (mirrors the
        // RegionAuthority zero-stake hole); reject it at construction.
        vm.expectRevert(ArtifactTemplate.ZeroFeeRate.selector);
        new ArtifactTemplate(owner, BASE_URI, address(meter), 0, address(region), ROYALTY_RATE);
    }

    function test_constructor_revertsZeroRegionAuthority() public {
        vm.expectRevert(ArtifactTemplate.ZeroRegionAuthority.selector);
        new ArtifactTemplate(owner, BASE_URI, address(meter), FEE_RATE, address(0), ROYALTY_RATE);
    }

    function test_constructor_revertsZeroRoyaltyRate() public {
        // Mirrors ZeroFeeRate: a zero royalty rate would globally disable the region
        // fee-share intake from artifact mints; reject it at construction.
        vm.expectRevert(ArtifactTemplate.ZeroRoyaltyRate.selector);
        new ArtifactTemplate(owner, BASE_URI, address(meter), FEE_RATE, address(region), 0);
    }

    function test_constructor_revertsZeroRoyaltyToken() public {
        // A RegionAuthority whose TOKEN() is zero (a misconfigured region) would make
        // every paid mint revert opaquely at the SafeERC20 call; reject it at construction.
        ReentrantRegionAuthority zeroTokenRegion = new ReentrantRegionAuthority(); // TOKEN() == 0
        vm.expectRevert(ArtifactTemplate.ZeroRoyaltyToken.selector);
        new ArtifactTemplate(
            owner, BASE_URI, address(meter), FEE_RATE, address(zeroTokenRegion), ROYALTY_RATE
        );
    }

    function test_setMintFeeRate_updatesAndEmits() public {
        vm.expectEmit(false, false, false, true);
        emit MintFeeRateSet(99e12);
        vm.prank(owner);
        art.setMintFeeRate(99e12);
        assertEq(art.mintFeeRate(), 99e12);
    }

    function test_setMintFeeRate_revertsZero() public {
        vm.expectRevert(ArtifactTemplate.ZeroFeeRate.selector);
        vm.prank(owner);
        art.setMintFeeRate(0);
    }

    function test_setMintFeeRate_onlyOwner() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, stranger));
        vm.prank(stranger);
        art.setMintFeeRate(123);
    }

    // --- HUD read-path counters (templatesByAuthor / totalMinted / mintedByTemplate) ---

    function test_registerTemplate_tracksTemplatesByAuthor() public {
        vm.startPrank(minter);
        art.registerTemplate(author, 1, keccak256("a"), 0);
        art.registerTemplate(player, 2, keccak256("b"), 0);
        art.registerTemplate(author, 3, keccak256("c"), 0);
        vm.stopPrank();

        assertEq(art.templatesByAuthor(author), 2);
        assertEq(art.templatesByAuthor(player), 1);
        assertEq(art.templatesByAuthor(stranger), 0);
        // Across authors the per-author counts sum to nextTemplateId.
        assertEq(art.templatesByAuthor(author) + art.templatesByAuthor(player), art.nextTemplateId());
    }

    function test_mint_tracksTotalMintedAndByTemplate() public {
        vm.startPrank(minter);
        uint256 id1 = art.registerTemplate(author, 1, keccak256("a"), 0);
        uint256 id2 = art.registerTemplate(author, 2, keccak256("b"), 0);
        art.mint(player, id1, 5, regionId, "");
        art.mint(player, id1, 3, regionId, ""); // same template accumulates
        art.mint(stranger, id2, 4, regionId, "");
        vm.stopPrank();

        assertEq(art.mintedByTemplate(id1), 8);
        assertEq(art.mintedByTemplate(id2), 4);
        assertEq(art.totalMinted(), 12);
        // Across templates the per-template counts sum to totalMinted.
        assertEq(art.mintedByTemplate(id1) + art.mintedByTemplate(id2), art.totalMinted());
    }

    /// @dev A reverting register/mint must record nothing (mirrors ComputeMeter's
    ///      "a reverting spend records neither" property).
    function test_revertingCallsRecordNothing() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"), 0);
        assertEq(art.totalMinted(), 0);

        // Non-minter register: templatesByAuthor must not move.
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(stranger);
        art.registerTemplate(stranger, 1, keccak256("x"), 0);
        assertEq(art.templatesByAuthor(stranger), 0);

        // Unknown-template mint: mint counters must not move.
        vm.expectRevert(ArtifactTemplate.UnknownTemplate.selector);
        vm.prank(minter);
        art.mint(player, 9999, 7, regionId, "");
        assertEq(art.totalMinted(), 0);
        assertEq(art.mintedByTemplate(9999), 0);

        // Non-minter mint of a known template: likewise records nothing.
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(stranger);
        art.mint(player, id, 7, regionId, "");
        assertEq(art.totalMinted(), 0);
        assertEq(art.mintedByTemplate(id), 0);
    }

    // --- minter rotation interplay ---

    function test_rotatedMinterCannotUseOldKey() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"), 0);

        vm.prank(owner);
        art.setMinter(stranger);

        // Old minter loses rights.
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(minter);
        art.mint(player, id, 1, regionId, "");

        // New minter works.
        vm.prank(stranger);
        art.mint(player, id, 1, regionId, "");
        assertEq(art.balanceOf(player, id), 1);
    }

    // --- supportsInterface (ERC1155) ---

    function test_supportsInterface_erc1155() public view {
        // ERC1155 interface id.
        assertTrue(art.supportsInterface(0xd9b67a26));
        // ERC165.
        assertTrue(art.supportsInterface(0x01ffc9a7));
    }

    // --- ownership ---

    function test_ownership_twoStepTransfer() public {
        vm.prank(owner);
        art.transferOwnership(player);
        assertEq(art.pendingOwner(), player);
        assertEq(art.owner(), owner);

        vm.prank(player);
        art.acceptOwnership();
        assertEq(art.owner(), player);

        vm.prank(player);
        art.setMinter(stranger);
        assertEq(art.minter(), stranger);
    }

    // --- burn ---

    /// @dev Register a rarity-0 (free-mint) template and mint `amount` to `player`, so a
    ///      burn test starts from a known balance without funding the fee/royalty ledgers.
    function _registerAndMint(uint256 amount, uint256 maxSupply) internal returns (uint256 id) {
        vm.prank(minter);
        id = art.registerTemplate(author, 0, keccak256("m"), maxSupply);
        vm.prank(minter);
        art.mint(player, id, amount, regionId, "");
    }

    function test_burn_holderDestroysOwnUnits() public {
        uint256 id = _registerAndMint(5, 0);

        vm.expectEmit(true, true, false, true);
        emit Burned(player, id, 2);
        vm.prank(player);
        art.burn(player, id, 2);

        assertEq(art.balanceOf(player, id), 3, "balance drops by the burned amount");
        assertEq(art.burnedByTemplate(id), 2);
        assertEq(art.totalBurned(), 2);
        // The mint counters are monotonic — a burn never touches them.
        assertEq(art.mintedByTemplate(id), 5, "burn must not lower mintedByTemplate");
        assertEq(art.totalMinted(), 5, "burn must not lower totalMinted");
    }

    /// @dev FM2: an operator the holder approved via setApprovalForAll may burn on the
    ///      holder's behalf — the standard ERC-1155 allowance the salvage/craft flow rides.
    function test_burn_approvedOperatorBurnsOnBehalf() public {
        uint256 id = _registerAndMint(5, 0);

        vm.prank(player);
        art.setApprovalForAll(stranger, true);

        vm.expectEmit(true, true, false, true);
        emit Burned(player, id, 4);
        vm.prank(stranger);
        art.burn(player, id, 4);

        assertEq(art.balanceOf(player, id), 1);
        assertEq(art.burnedByTemplate(id), 4);
    }

    /// @dev FM2: a caller who is neither the holder nor an approved operator cannot destroy
    ///      the holder's artifacts — the burn reverts and leaves the balance + counters intact.
    function test_burn_revertsUnauthorized() public {
        uint256 id = _registerAndMint(5, 0);

        vm.expectRevert(
            abi.encodeWithSelector(IERC1155Errors.ERC1155MissingApprovalForAll.selector, stranger, player)
        );
        vm.prank(stranger);
        art.burn(player, id, 1);

        assertEq(art.balanceOf(player, id), 5, "an unauthorized burn changes nothing");
        assertEq(art.totalBurned(), 0);
    }

    /// @dev FM2: revoking a prior approval closes the operator's burn power again.
    function test_burn_revertsAfterApprovalRevoked() public {
        uint256 id = _registerAndMint(5, 0);
        vm.prank(player);
        art.setApprovalForAll(stranger, true);
        vm.prank(player);
        art.setApprovalForAll(stranger, false);

        vm.expectRevert(
            abi.encodeWithSelector(IERC1155Errors.ERC1155MissingApprovalForAll.selector, stranger, player)
        );
        vm.prank(stranger);
        art.burn(player, id, 1);
    }

    /// @dev FM2/FM3: burning more than the balance reverts with the ERC-1155 balance error
    ///      before any counter moves — no partial burn, no counter drift.
    function test_burn_revertsInsufficientBalance() public {
        uint256 id = _registerAndMint(3, 0);

        vm.expectRevert(
            abi.encodeWithSelector(IERC1155Errors.ERC1155InsufficientBalance.selector, player, 3, 4, id)
        );
        vm.prank(player);
        art.burn(player, id, 4);

        assertEq(art.balanceOf(player, id), 3);
        assertEq(art.burnedByTemplate(id), 0);
        assertEq(art.totalBurned(), 0);
    }

    /// @dev A zero-amount burn is rejected (mirrors mint's ZeroAmount) so it can't spam a
    ///      no-op Burned/TransferSingle event; nothing moves.
    function test_burn_revertsZeroAmount() public {
        uint256 id = _registerAndMint(5, 0);

        vm.expectRevert(ArtifactTemplate.ZeroAmount.selector);
        vm.prank(player);
        art.burn(player, id, 0);

        assertEq(art.balanceOf(player, id), 5);
        assertEq(art.totalBurned(), 0);
    }

    /// @dev FM1 (the headline): burning does NOT free supply-cap headroom. The cap bounds
    ///      cumulative MINTS (mintedByTemplate), not circulating supply, so a holder cannot
    ///      mint to the cap, burn, and mint again past the intended scarcity.
    function test_burn_doesNotFreeSupplyCap() public {
        // Capped at 10, rarity 0 so the cap is exercised free of the fee/royalty.
        uint256 id = _registerAndMint(10, 10); // exact-fill the cap
        assertEq(art.mintedByTemplate(id), 10);

        // Burn 4 — circulating supply is now 6, but mintedByTemplate stays 10.
        vm.prank(player);
        art.burn(player, id, 4);
        assertEq(art.balanceOf(player, id), 6);
        assertEq(art.mintedByTemplate(id), 10, "cumulative mints unchanged by a burn");
        assertEq(art.burnedByTemplate(id), 4);

        // A further mint of even ONE unit still reverts — the cap counts cumulative mints
        // (10), not the 6 in circulation, so burning bought no headroom.
        vm.expectRevert(abi.encodeWithSelector(ArtifactTemplate.SupplyExceeded.selector, id, 11, 10));
        vm.prank(minter);
        art.mint(player, id, 1, regionId, "");
    }

    /// @dev FM3: totalBurned tracks the sum of burnedByTemplate across templates.
    function test_burn_totalBurnedSumsAcrossTemplates() public {
        uint256 a = _registerAndMint(6, 0);
        uint256 b = _registerAndMint(9, 0);

        vm.startPrank(player);
        art.burn(player, a, 2);
        art.burn(player, b, 5);
        art.burn(player, a, 1);
        vm.stopPrank();

        assertEq(art.burnedByTemplate(a), 3);
        assertEq(art.burnedByTemplate(b), 5);
        assertEq(art.totalBurned(), 8, "totalBurned == sum of per-template burns");
        // Mint side is untouched by any burn.
        assertEq(art.totalMinted(), 15);
    }

    // --- burnBatch ---

    function test_burnBatch_destroysMultipleTemplates() public {
        uint256 a = _registerAndMint(6, 0);
        uint256 b = _registerAndMint(9, 0);

        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        (ids[0], ids[1]) = (a, b);
        (amounts[0], amounts[1]) = (2, 5);

        // A Burned event per element, in order.
        vm.expectEmit(true, true, false, true);
        emit Burned(player, a, 2);
        vm.expectEmit(true, true, false, true);
        emit Burned(player, b, 5);
        vm.prank(player);
        art.burnBatch(player, ids, amounts);

        assertEq(art.balanceOf(player, a), 4);
        assertEq(art.balanceOf(player, b), 4);
        assertEq(art.burnedByTemplate(a), 2);
        assertEq(art.burnedByTemplate(b), 5);
        assertEq(art.totalBurned(), 7);
        assertEq(art.mintedByTemplate(a), 6, "burn leaves the mint counters alone");
        assertEq(art.mintedByTemplate(b), 9);
    }

    /// @dev FM3: a batch is atomic — one element that over-burns reverts the WHOLE batch,
    ///      so no earlier element's burn or counter bump commits.
    function test_burnBatch_atomicOnOverBurn() public {
        uint256 a = _registerAndMint(6, 0);
        uint256 b = _registerAndMint(9, 0);

        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        (ids[0], ids[1]) = (a, b);
        (amounts[0], amounts[1]) = (2, 10); // b holds only 9

        vm.expectRevert(
            abi.encodeWithSelector(IERC1155Errors.ERC1155InsufficientBalance.selector, player, 9, 10, b)
        );
        vm.prank(player);
        art.burnBatch(player, ids, amounts);

        // Nothing committed: the first element's 2 did NOT burn.
        assertEq(art.balanceOf(player, a), 6);
        assertEq(art.balanceOf(player, b), 9);
        assertEq(art.burnedByTemplate(a), 0);
        assertEq(art.totalBurned(), 0);
    }

    /// @dev A length mismatch between ids and amounts reverts before any burn.
    function test_burnBatch_revertsLengthMismatch() public {
        uint256 a = _registerAndMint(6, 0);

        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](1);
        (ids[0], ids[1]) = (a, a);
        amounts[0] = 1;

        vm.expectRevert(abi.encodeWithSelector(IERC1155Errors.ERC1155InvalidArrayLength.selector, 2, 1));
        vm.prank(player);
        art.burnBatch(player, ids, amounts);

        assertEq(art.totalBurned(), 0);
    }

    function test_burnBatch_revertsUnauthorized() public {
        uint256 a = _registerAndMint(6, 0);

        uint256[] memory ids = new uint256[](1);
        uint256[] memory amounts = new uint256[](1);
        ids[0] = a;
        amounts[0] = 1;

        vm.expectRevert(
            abi.encodeWithSelector(IERC1155Errors.ERC1155MissingApprovalForAll.selector, stranger, player)
        );
        vm.prank(stranger);
        art.burnBatch(player, ids, amounts);

        assertEq(art.balanceOf(player, a), 6);
    }

    /// @dev The batch mirror of test_burn_revertsZeroAmount: a zero element is rejected so it
    ///      can't spam a no-op Burned event. ERC-1155's balance check is `fromBalance < value`,
    ///      which 0 < 0 passes, so an UNREGISTERED id the holder has never touched sails through
    ///      _burnBatch untouched — the counters never move (the supply invariants are blind to
    ///      this by construction), leaving the event as the only trace an indexer would ingest.
    function test_burnBatch_revertsZeroAmount() public {
        uint256[] memory ids = new uint256[](1);
        uint256[] memory amounts = new uint256[](1);
        ids[0] = 999_999; // never registered, never held
        amounts[0] = 0;

        vm.expectRevert(ArtifactTemplate.ZeroAmount.selector);
        vm.prank(player);
        art.burnBatch(player, ids, amounts);

        assertEq(art.burnedByTemplate(999_999), 0);
        assertEq(art.totalBurned(), 0);
    }

    /// @dev Atomicity over the zero guard: the check sits in the counter loop that runs AFTER
    ///      _burnBatch has already burned, so the revert must unwind the earlier elements too.
    ///      A batch with one zero element burns NOTHING — never a partial salvage the caller
    ///      believes settled in full.
    function test_burnBatch_atomicOnZeroAmount() public {
        uint256 a = _registerAndMint(6, 0);
        uint256 b = _registerAndMint(9, 0);

        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        (ids[0], ids[1]) = (a, b);
        (amounts[0], amounts[1]) = (2, 0); // the first element is a legitimate burn

        vm.expectRevert(ArtifactTemplate.ZeroAmount.selector);
        vm.prank(player);
        art.burnBatch(player, ids, amounts);

        assertEq(art.balanceOf(player, a), 6, "the leading element's 2 did NOT burn");
        assertEq(art.balanceOf(player, b), 9);
        assertEq(art.burnedByTemplate(a), 0);
        assertEq(art.totalBurned(), 0);
    }

    /// @dev The complement of the zero guard: an all-positive salvage still burns unchanged, so
    ///      the guard rejects only the no-op element and never a holder's real mixed batch.
    function test_burnBatch_allPositiveUnaffectedByZeroGuard() public {
        uint256 a = _registerAndMint(6, 0);
        uint256 b = _registerAndMint(9, 0);

        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        (ids[0], ids[1]) = (a, b);
        (amounts[0], amounts[1]) = (1, 9); // b burns to exactly zero balance

        vm.prank(player);
        art.burnBatch(player, ids, amounts);

        assertEq(art.balanceOf(player, a), 5);
        assertEq(art.balanceOf(player, b), 0, "burning the full holding is not a zero AMOUNT");
        assertEq(art.totalBurned(), 10);
    }

    /// @dev Duplicate ids in one batch accumulate into burnedByTemplate correctly (each
    ///      element adds), so the per-template tally never under-counts a repeated id.
    function test_burnBatch_duplicateIdsAccumulate() public {
        uint256 a = _registerAndMint(6, 0);

        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        (ids[0], ids[1]) = (a, a);
        (amounts[0], amounts[1]) = (2, 3);

        vm.prank(player);
        art.burnBatch(player, ids, amounts);

        assertEq(art.balanceOf(player, a), 1, "6 - (2 + 3)");
        assertEq(art.burnedByTemplate(a), 5);
        assertEq(art.totalBurned(), 5);
    }

    // --- mintBatch (create-side batch twin of burnBatch) ---

    /// @dev Register a template without minting, so a mintBatch test controls the mint.
    function _register(uint16 rarity, uint256 maxSupply) internal returns (uint256 id) {
        vm.prank(minter);
        id = art.registerTemplate(author, rarity, keccak256("mb"), maxSupply);
    }

    /// @dev Stake+claim a second region so a batch can route royalties into more than one.
    function _claimRegion(uint256 rid, address holder) internal {
        token.transfer(holder, STAKE);
        vm.startPrank(holder);
        token.approve(address(region), STAKE);
        region.claim(rid, STAKE);
        vm.stopPrank();
    }

    /// @dev Happy path: a mixed loadout of three distinct templates minted to one recipient in
    ///      one call — a Minted per element in order, per-template balances + counters, and the
    ///      region royalty routed for the batch.
    function test_mintBatch_happyPath() public {
        uint256 t1 = _register(5000, 0);
        uint256 t2 = _register(3000, 0);
        uint256 t3 = _register(8000, 0);

        uint256[] memory ids = new uint256[](3);
        uint256[] memory amounts = new uint256[](3);
        uint256[] memory regions = new uint256[](3);
        (ids[0], ids[1], ids[2]) = (t1, t2, t3);
        (amounts[0], amounts[1], amounts[2]) = (2, 5, 1);
        (regions[0], regions[1], regions[2]) = (regionId, regionId, regionId);

        vm.expectEmit(true, true, false, true);
        emit Minted(player, t1, 2);
        vm.expectEmit(true, true, false, true);
        emit Minted(player, t2, 5);
        vm.expectEmit(true, true, false, true);
        emit Minted(player, t3, 1);
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");

        assertEq(art.balanceOf(player, t1), 2);
        assertEq(art.balanceOf(player, t2), 5);
        assertEq(art.balanceOf(player, t3), 1);
        assertEq(art.mintedByTemplate(t1), 2);
        assertEq(art.mintedByTemplate(t2), 5);
        assertEq(art.mintedByTemplate(t3), 1);
        assertEq(art.totalMinted(), 8);
        assertGt(region.accruedFees(regionId), 0, "royalty routed for the batch");
    }

    /// @dev A one-element batch is equivalent to a single mint: same fee debit, same royalty
    ///      route, same Minted, same counters.
    function test_mintBatch_singleElementMatchesMint() public {
        uint256 t = _register(5000, 0);
        uint256 creditBefore = meter.credit(player);
        uint256 accruedBefore = region.accruedFees(regionId);

        uint256[] memory ids = new uint256[](1);
        uint256[] memory amounts = new uint256[](1);
        uint256[] memory regions = new uint256[](1);
        (ids[0], amounts[0], regions[0]) = (t, 4, regionId);

        vm.expectEmit(true, true, false, true);
        emit Minted(player, t, 4);
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");

        assertEq(art.balanceOf(player, t), 4);
        assertEq(art.mintedByTemplate(t), 4);
        assertEq(art.totalMinted(), 4);
        assertLt(meter.credit(player), creditBefore, "fee debited");
        assertGt(region.accruedFees(regionId), accruedBefore, "royalty routed");
    }

    /// @dev A fresh ArtifactTemplate wired to the shared meter+region, minter set, and with
    ///      `player` approved to pull its royalty — for the two-contract equivalence tests.
    function _freshWiredArt() internal returns (ArtifactTemplate x) {
        x = new ArtifactTemplate(owner, BASE_URI, address(meter), FEE_RATE, address(region), ROYALTY_RATE);
        vm.startPrank(owner);
        x.setMinter(minter);
        meter.setSpender(address(x), true);
        vm.stopPrank();
        vm.prank(player);
        token.approve(address(x), type(uint256).max);
    }

    /// @dev The equivalence proof (counters): three templates minted as ONE batch on contract A
    ///      vs as three single mints on an identically-wired contract B leaves totalMinted /
    ///      mintedByTemplate / balances IDENTICAL. Both entry points share _mintOne, so this pins
    ///      that the batch adds nothing but the loop. The royalty-routing partition is the twin
    ///      test below. Mirrors revokeReceipts' counter-equals-three-single-revokes.
    function test_mintBatch_counterEqualsThreeSingleMints() public {
        ArtifactTemplate a = _freshWiredArt();
        ArtifactTemplate b = _freshWiredArt();

        uint256[] memory ids = new uint256[](3);
        uint256[] memory amounts = new uint256[](3);
        uint256[] memory regions = new uint256[](3);
        vm.startPrank(minter);
        for (uint256 i = 0; i < 3; i++) {
            uint16 rarity = uint16(2000 + i * 1500); // 2000, 3500, 5000
            a.registerTemplate(author, rarity, keccak256("m"), 0);
            b.registerTemplate(author, rarity, keccak256("m"), 0);
            ids[i] = i + 1; // both fresh contracts start nextTemplateId at 0
            amounts[i] = 2 + i; // 2, 3, 4
            regions[i] = regionId;
        }
        a.mintBatch(player, ids, amounts, regions, ""); // one batch
        b.mint(player, ids[0], amounts[0], regions[0], ""); // three singles
        b.mint(player, ids[1], amounts[1], regions[1], "");
        b.mint(player, ids[2], amounts[2], regions[2], "");
        vm.stopPrank();

        assertEq(a.totalMinted(), b.totalMinted());
        assertEq(a.totalMinted(), 9); // 2 + 3 + 4
        for (uint256 i = 0; i < 3; i++) {
            assertEq(a.mintedByTemplate(ids[i]), b.mintedByTemplate(ids[i]), "per-template mint count");
            assertEq(a.balanceOf(player, ids[i]), b.balanceOf(player, ids[i]), "per-template balance");
        }
    }

    /// @dev The equivalence proof (royalty partition): a batch spanning TWO regions routes the
    ///      SAME per-region royalty a matching set of single mints does — so the batch never
    ///      mis-attributes or drops a region's share. Deltas are measured in scoped blocks to
    ///      keep the stack shallow.
    function test_mintBatch_royaltyPartitionEqualsSingles() public {
        uint256 regionB = uint256(keccak256("region-B"));
        _claimRegion(regionB, address(0x0EE2));
        ArtifactTemplate a = _freshWiredArt();
        ArtifactTemplate b = _freshWiredArt();

        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        uint256[] memory regions = new uint256[](2);
        vm.startPrank(minter);
        a.registerTemplate(author, 5000, keccak256("m"), 0);
        b.registerTemplate(author, 5000, keccak256("m"), 0);
        a.registerTemplate(author, 3000, keccak256("m"), 0);
        b.registerTemplate(author, 3000, keccak256("m"), 0);
        (ids[0], ids[1]) = (1, 2);
        (amounts[0], amounts[1]) = (3, 4);
        (regions[0], regions[1]) = (regionId, regionB); // two distinct regions

        uint256 batchA;
        uint256 batchB;
        {
            uint256 preA = region.accruedFees(regionId);
            uint256 preB = region.accruedFees(regionB);
            a.mintBatch(player, ids, amounts, regions, "");
            batchA = region.accruedFees(regionId) - preA;
            batchB = region.accruedFees(regionB) - preB;
        }
        {
            uint256 preA = region.accruedFees(regionId);
            uint256 preB = region.accruedFees(regionB);
            b.mint(player, ids[0], amounts[0], regions[0], "");
            b.mint(player, ids[1], amounts[1], regions[1], "");
            assertEq(region.accruedFees(regionId) - preA, batchA, "region A royalty: batch == singles");
            assertEq(region.accruedFees(regionB) - preB, batchB, "region B royalty: batch == singles");
        }
        vm.stopPrank();
        assertGt(batchB, 0, "the second region is actually exercised");
    }

    /// @dev Duplicate ids within a batch accumulate into mintedByTemplate/balance correctly
    ///      (each element adds against the committed count) — the mint twin of
    ///      test_burnBatch_duplicateIdsAccumulate.
    function test_mintBatch_duplicateIdsAccumulate() public {
        uint256 t = _register(0, 0);
        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        uint256[] memory regions = new uint256[](2);
        (ids[0], ids[1]) = (t, t);
        (amounts[0], amounts[1]) = (2, 3);
        (regions[0], regions[1]) = (regionId, regionId);

        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");

        assertEq(art.balanceOf(player, t), 5, "2 + 3");
        assertEq(art.mintedByTemplate(t), 5);
        assertEq(art.totalMinted(), 5);
    }

    /// @dev A capped template repeated within ONE batch is bounded by its LIVE count — each
    ///      element's cap check reads mintedByTemplate fresh, so element 1 sees element 0's mint.
    ///      cap 5, [{t,3},{t,3}] -> element 1's wouldMint = 3+3 = 6 > 5 -> SupplyExceeded, whole
    ///      batch reverts. A batch that pre-checked every cap against the entry-count (a plausible
    ///      "optimization") would let the duplicates collectively breach the cap.
    function test_mintBatch_capHoldsAcrossBatchDuplicates() public {
        uint256 t = _register(0, 5); // cap 5, rarity 0 so the cap is the only gate
        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        uint256[] memory regions = new uint256[](2);
        (ids[0], ids[1]) = (t, t);
        (amounts[0], amounts[1]) = (3, 3);
        (regions[0], regions[1]) = (regionId, regionId);

        vm.expectRevert(abi.encodeWithSelector(ArtifactTemplate.SupplyExceeded.selector, t, 6, 5));
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");

        assertEq(art.mintedByTemplate(t), 0, "whole batch rolled back, cap never breached");
        assertEq(art.balanceOf(player, t), 0);
    }

    /// @dev The batch loop's per-element CEI pinned DIRECTLY, not merely inferred from the
    ///      shared `_mintOne` plus the single-mint `test_mint_capHoldsUnderReentrancy`. Outer
    ///      `mintBatch([{t,3},{t,3}])` on cap 8: element 0 commits 3, then its `_mint` receiver
    ///      hook reenters `mint(t,3)` — which reads the committed 3 and commits 6 — and returns;
    ///      element 1 then reads the LIVE 6 and reverts (6+3=9 > 8 → `SupplyExceeded`), unwinding
    ///      the whole batch. Proves the loop reads live counters ACROSS a reentrancy that itself
    ///      moved the count — not a per-batch entry snapshot — so a reentrant minter cannot use
    ///      the batch path to slip past a cap the single path holds. The batch twin of
    ///      `test_mintBatch_capHoldsAcrossBatchDuplicates` (which has no reentry).
    function test_mintBatch_capHoldsUnderReentrancy() public {
        ReentrantMinter rm = new ReentrantMinter(art);
        vm.prank(owner);
        art.setMinter(address(rm));
        uint256 id = rm.registerCapped(author, 0, 8); // rarity 0 = free mint, no royalty

        // element 1's SupplyExceeded(id, 9, 8) propagates raw — the hook already returned
        // during element 0, so nothing wraps it.
        vm.expectRevert(abi.encodeWithSelector(ArtifactTemplate.SupplyExceeded.selector, id, 9, 8));
        rm.fireBatch(3, 3, 3, regionId);

        assertEq(art.mintedByTemplate(id), 0, "whole batch rolled back, cap never breached");
        assertEq(art.totalMinted(), 0);
        assertEq(art.balanceOf(address(rm), id), 0);
    }

    /// @dev Gas-envelope seam: a batch of EXACTLY MAX_BATCH mints — the cap is inclusive so it
    ///      never rejects a legitimate max-size loadout, and this drives the full O(n)
    ///      cap/fee/royalty/mint loop at the boundary with real (rarity>0) per-element work.
    ///      Reads the cap off the contract so the test tracks the constant in lock-step.
    function test_mintBatch_atMaxBatchMints() public {
        uint256 n = art.MAX_BATCH();
        uint256[] memory ids = new uint256[](n);
        uint256[] memory amounts = new uint256[](n);
        uint256[] memory regions = new uint256[](n);
        for (uint256 i = 0; i < n; i++) {
            ids[i] = _register(100, 0); // 1% rarity -> a small real fee + royalty per element
            amounts[i] = 1;
            regions[i] = regionId;
        }
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");

        assertEq(art.totalMinted(), n);
        assertEq(art.balanceOf(player, ids[0]), 1);
        assertEq(art.balanceOf(player, ids[n - 1]), 1);
        assertGt(region.accruedFees(regionId), 0);
    }

    /// @dev A batch of MAX_BATCH + 1 reverts BatchTooLarge BEFORE any mint — the bound is a
    ///      contract guarantee even from a buggy minter. The elements ARE mintable, so this
    ///      proves the size gate rejects at the boundary independent of element validity.
    function test_mintBatch_aboveMaxBatchReverts() public {
        uint256 id = _register(0, 0);
        uint256 n = art.MAX_BATCH() + 1;
        uint256[] memory ids = new uint256[](n);
        uint256[] memory amounts = new uint256[](n);
        uint256[] memory regions = new uint256[](n);
        for (uint256 i = 0; i < n; i++) {
            (ids[i], amounts[i], regions[i]) = (id, 1, regionId);
        }
        vm.expectRevert(ArtifactTemplate.BatchTooLarge.selector);
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");
        assertEq(art.totalMinted(), 0, "nothing minted over the cap");
    }

    /// @dev An empty batch reverts rather than doing zero work.
    function test_mintBatch_revertsEmptyBatch() public {
        uint256[] memory ids = new uint256[](0);
        uint256[] memory amounts = new uint256[](0);
        uint256[] memory regions = new uint256[](0);
        vm.expectRevert(ArtifactTemplate.EmptyBatch.selector);
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");
    }

    /// @dev Auth is checked before the size/length guards, so a non-minter gets NotMinter even
    ///      for an empty batch — no leak of the batch-shape checks before the auth revert.
    function test_mintBatch_unauthorizedRevertsBeforeEmptyBatch() public {
        uint256[] memory ids = new uint256[](0);
        uint256[] memory amounts = new uint256[](0);
        uint256[] memory regions = new uint256[](0);
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(stranger);
        art.mintBatch(player, ids, amounts, regions, "");
    }

    function test_mintBatch_revertsNotMinter() public {
        uint256 t = _register(0, 0);
        uint256[] memory ids = new uint256[](1);
        uint256[] memory amounts = new uint256[](1);
        uint256[] memory regions = new uint256[](1);
        (ids[0], amounts[0], regions[0]) = (t, 1, regionId);

        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(stranger);
        art.mintBatch(player, ids, amounts, regions, "");
        assertEq(art.totalMinted(), 0);
    }

    function test_mintBatch_revertsZeroRecipient() public {
        uint256 t = _register(0, 0);
        uint256[] memory ids = new uint256[](1);
        uint256[] memory amounts = new uint256[](1);
        uint256[] memory regions = new uint256[](1);
        (ids[0], amounts[0], regions[0]) = (t, 1, regionId);

        vm.expectRevert(ArtifactTemplate.ZeroRecipient.selector);
        vm.prank(minter);
        art.mintBatch(address(0), ids, amounts, regions, "");
    }

    /// @dev amounts shorter than templateIds reverts ArrayLengthMismatch before any mint.
    function test_mintBatch_revertsAmountsLengthMismatch() public {
        uint256 t = _register(0, 0);
        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](1);
        uint256[] memory regions = new uint256[](2);
        (ids[0], ids[1]) = (t, t);
        amounts[0] = 1;
        (regions[0], regions[1]) = (regionId, regionId);

        vm.expectRevert(ArtifactTemplate.ArrayLengthMismatch.selector);
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");
        assertEq(art.totalMinted(), 0);
    }

    /// @dev regionIds shorter than templateIds reverts ArrayLengthMismatch — the third array is
    ///      length-checked here because, unlike an OZ _mintBatch, it is not an ERC-1155 array.
    function test_mintBatch_revertsRegionIdsLengthMismatch() public {
        uint256 t = _register(0, 0);
        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        uint256[] memory regions = new uint256[](1);
        (ids[0], ids[1]) = (t, t);
        (amounts[0], amounts[1]) = (1, 1);
        regions[0] = regionId;

        vm.expectRevert(ArtifactTemplate.ArrayLengthMismatch.selector);
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");
        assertEq(art.totalMinted(), 0);
    }

    /// @dev Whole-batch atomicity (an unknown template mid-batch): reverts UnknownTemplate and
    ///      rolls the leading element's mint AND fee back — never a partial loadout.
    function test_mintBatch_atomicOnUnknownTemplate() public {
        uint256 t1 = _register(5000, 0);
        uint256 missing = 999_999; // never registered

        uint256 creditBefore = meter.credit(player);
        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        uint256[] memory regions = new uint256[](2);
        (ids[0], ids[1]) = (t1, missing);
        (amounts[0], amounts[1]) = (2, 1);
        (regions[0], regions[1]) = (regionId, regionId);

        vm.expectRevert(ArtifactTemplate.UnknownTemplate.selector);
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");

        assertEq(art.balanceOf(player, t1), 0, "leading mint rolled back");
        assertEq(art.totalMinted(), 0);
        assertEq(art.mintedByTemplate(t1), 0);
        assertEq(meter.credit(player), creditBefore, "no fee charged on the reverted batch");
    }

    /// @dev Whole-batch atomicity (a zero amount mid-batch): reverts ZeroAmount, the leading
    ///      element's mint rolled back — the mint twin of test_burnBatch_atomicOnZeroAmount.
    function test_mintBatch_atomicOnZeroAmount() public {
        uint256 t1 = _register(5000, 0);
        uint256 t2 = _register(5000, 0);
        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        uint256[] memory regions = new uint256[](2);
        (ids[0], ids[1]) = (t1, t2);
        (amounts[0], amounts[1]) = (2, 0); // element 1 is a no-op amount
        (regions[0], regions[1]) = (regionId, regionId);

        vm.expectRevert(ArtifactTemplate.ZeroAmount.selector);
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");

        assertEq(art.balanceOf(player, t1), 0, "leading mint rolled back");
        assertEq(art.totalMinted(), 0);
    }

    /// @dev Whole-batch atomicity (a supply-cap breach mid-batch): reverts SupplyExceeded, the
    ///      leading element rolled back.
    function test_mintBatch_atomicOnSupplyCap() public {
        uint256 t1 = _register(5000, 0);
        uint256 tCap = _register(5000, 3); // cap 3
        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        uint256[] memory regions = new uint256[](2);
        (ids[0], ids[1]) = (t1, tCap);
        (amounts[0], amounts[1]) = (2, 4); // 4 > cap 3
        (regions[0], regions[1]) = (regionId, regionId);

        vm.expectRevert(abi.encodeWithSelector(ArtifactTemplate.SupplyExceeded.selector, tCap, 4, 3));
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");

        assertEq(art.balanceOf(player, t1), 0);
        assertEq(art.totalMinted(), 0);
    }

    /// @dev Whole-batch atomicity over the royalty external call: a mid-batch element naming an
    ///      unclaimed region reverts inside depositFees, rolling the ENTIRE batch back — the
    ///      leading element's fee debit AND mint are unwound too.
    function test_mintBatch_atomicOnUnknownRegion() public {
        uint256 t1 = _register(5000, 0);
        uint256 t2 = _register(5000, 0);
        uint256 unknownRegion = uint256(keccak256("never-claimed"));

        uint256 creditBefore = meter.credit(player);
        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        uint256[] memory regions = new uint256[](2);
        (ids[0], ids[1]) = (t1, t2);
        (amounts[0], amounts[1]) = (2, 3);
        (regions[0], regions[1]) = (regionId, unknownRegion); // element 1's region is unclaimed

        vm.expectRevert(RegionAuthority.UnknownRegion.selector);
        vm.prank(minter);
        art.mintBatch(player, ids, amounts, regions, "");

        assertEq(art.balanceOf(player, t1), 0, "leading mint rolled back");
        assertEq(art.totalMinted(), 0);
        assertEq(art.mintedByTemplate(t1), 0);
        assertEq(meter.credit(player), creditBefore, "leading fee debit rolled back");
    }
}

/// @notice Minter + mint recipient that reenters mint() once from its ERC-1155
///         receiver hook, recording the supply counter it observed mid-hook so a
///         test can prove the outer effects landed before the interaction (CEI).
contract ReentrantMinter is IERC1155Receiver {
    ArtifactTemplate immutable art;
    uint256 public templateId;
    uint256 reentryAmount;
    uint256 regionId;
    bool entered;
    uint256 public observedTotalMintedAtHook;

    constructor(ArtifactTemplate art_) {
        art = art_;
    }

    function register(address author, uint16 rarity) external returns (uint256) {
        templateId = art.registerTemplate(author, rarity, keccak256("r"), 0);
        return templateId;
    }

    function registerCapped(address author, uint16 rarity, uint256 maxSupply) external returns (uint256) {
        templateId = art.registerTemplate(author, rarity, keccak256("rc"), maxSupply);
        return templateId;
    }

    function fire(uint256 outerAmount, uint256 reentryAmount_, uint256 regionId_) external {
        reentryAmount = reentryAmount_;
        regionId = regionId_;
        art.mint(address(this), templateId, outerAmount, regionId_, "");
    }

    /// @dev Outer call is a two-element `mintBatch` of `templateId`; the receiver hook still
    ///      reenters a single `mint(reentryAmount)` once, so a test can drive a reentrancy that
    ///      lands BETWEEN batch elements (during element 0's `_mint`) and prove element 1 reads
    ///      the reentrant-moved count.
    function fireBatch(uint256 a0, uint256 a1, uint256 reentryAmount_, uint256 regionId_) external {
        reentryAmount = reentryAmount_;
        regionId = regionId_;
        uint256[] memory ids = new uint256[](2);
        uint256[] memory amounts = new uint256[](2);
        uint256[] memory regions = new uint256[](2);
        (ids[0], ids[1]) = (templateId, templateId);
        (amounts[0], amounts[1]) = (a0, a1);
        (regions[0], regions[1]) = (regionId_, regionId_);
        art.mintBatch(address(this), ids, amounts, regions, "");
    }

    function onERC1155Received(address, address, uint256, uint256, bytes calldata) external returns (bytes4) {
        if (!entered) {
            entered = true;
            observedTotalMintedAtHook = art.totalMinted();
            art.mint(address(this), templateId, reentryAmount, regionId, "");
        }
        return this.onERC1155Received.selector;
    }

    function onERC1155BatchReceived(address, address, uint256[] calldata, uint256[] calldata, bytes calldata)
        external
        pure
        returns (bytes4)
    {
        return this.onERC1155BatchReceived.selector;
    }

    function supportsInterface(bytes4) external pure returns (bool) {
        return true;
    }
}

/// @notice A ComputeMeter stand-in that is also the minter, reentering mint() once
///         from spend() to prove the supply counters (effects) land before the
///         external spend interaction (CEI). Recipient is an EOA, so the only
///         reentrancy path is the spend, not the ERC-1155 receiver hook.
contract ReentrantSpender is IComputeMeter {
    ArtifactTemplate art;
    uint256 public templateId;
    uint256 reentryAmount;
    uint256 regionId;
    address recipient;
    bool entered;
    uint256 public observedTotalMintedAtSpend;

    function setArt(ArtifactTemplate art_) external {
        art = art_;
    }

    function register(address author, uint16 rarity) external returns (uint256) {
        templateId = art.registerTemplate(author, rarity, keccak256("rs"), 0);
        return templateId;
    }

    function fire(address recipient_, uint256 outerAmount, uint256 reentryAmount_, uint256 regionId_)
        external
    {
        recipient = recipient_;
        reentryAmount = reentryAmount_;
        regionId = regionId_;
        art.mint(recipient_, templateId, outerAmount, regionId_, "");
    }

    function spend(address, uint256, bytes32) external {
        if (entered) return;
        entered = true;
        observedTotalMintedAtSpend = art.totalMinted();
        art.mint(recipient, templateId, reentryAmount, regionId, "");
    }
}

/// @notice A hostile $BLCKFLD that tries to reenter mint() once from the royalty
///         transferFrom (the FM2 vector). It is NOT the minter, so the reentry hits the
///         onlyMinter gate — proving access control, not just CEI, blocks a malicious fee
///         token. Pairs with a no-op region so the only behavior under test is the reentry.
contract ReentrantRoyaltyToken {
    ArtifactTemplate art;
    uint256 public templateId;
    uint256 reentryAmount;
    uint256 regionId;
    address recipient;
    bool entered;

    function arm(
        ArtifactTemplate art_,
        uint256 templateId_,
        uint256 regionId_,
        address recipient_,
        uint256 reentryAmount_
    ) external {
        art = art_;
        templateId = templateId_;
        regionId = regionId_;
        recipient = recipient_;
        reentryAmount = reentryAmount_;
    }

    function transferFrom(address, address, uint256) external returns (bool) {
        if (!entered) {
            entered = true;
            art.mint(recipient, templateId, reentryAmount, regionId, "");
        }
        return true;
    }

    function approve(address, uint256) external pure returns (bool) {
        return true;
    }
}

/// @notice RegionAuthority stand-in whose TOKEN() points at the hostile royalty token
///         and whose depositFees is a sink, so the FM2 reentrancy test exercises only
///         the token-reentrancy vector (no real region/staking needed).
contract ReentrantRegionAuthority is IRegionAuthority {
    address public token;

    function setToken(address token_) external {
        token = token_;
    }

    function depositFees(uint256, uint256) external {}

    function TOKEN() external view returns (address) {
        return token;
    }
}
