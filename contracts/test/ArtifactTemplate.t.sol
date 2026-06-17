// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ArtifactTemplate, IComputeMeter} from "../src/ArtifactTemplate.sol";
import {ComputeMeter} from "../src/ComputeMeter.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC1155Receiver} from "@openzeppelin/contracts/token/ERC1155/IERC1155Receiver.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

contract ArtifactTemplateTest is Test {
    ArtifactTemplate art;
    ComputeMeter meter;
    MockToken token;

    address owner = address(0xA11CE);
    address minter = address(0x1117E2);
    address author = address(0xA47403);
    address player = address(0xB0B);
    address stranger = address(0xBEEF);

    string constant BASE_URI = "ipfs://base/{id}.json";
    uint256 constant FEE_RATE = 1e12;

    event TemplateRegistered(
        uint256 indexed templateId, address indexed author, uint16 rarity, bytes32 manifest
    );
    event Minted(address indexed to, uint256 indexed templateId, uint256 amount);
    event MinterSet(address indexed minter);
    event MintFeeRateSet(uint256 rate);
    // ComputeMeter's debit event, re-declared for expectEmit against the meter.
    event Spent(address indexed buyer, address indexed spender, uint256 amount, bytes32 jobId);

    function setUp() public {
        token = new MockToken();
        meter = new ComputeMeter(address(token), owner);
        art = new ArtifactTemplate(owner, BASE_URI, address(meter), FEE_RATE);

        vm.startPrank(owner);
        art.setMinter(minter);
        meter.setSpender(address(art), true);
        vm.stopPrank();

        // Fund the recipients used across the mint tests; fees are tiny next to this.
        _credit(player, 1_000 ether);
        _credit(stranger, 1_000 ether);
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

    // --- construction ---

    function test_constructor() public view {
        assertEq(art.owner(), owner);
        assertEq(art.uri(0), BASE_URI);
        assertEq(art.minter(), minter);
        assertEq(art.nextTemplateId(), 0);
        assertEq(address(art.computeMeter()), address(meter));
        assertEq(art.mintFeeRate(), FEE_RATE);
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

    // --- setURI ---

    function test_setURI_updatesUri() public {
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
        uint256 id = art.registerTemplate(author, rarity, manifest);

        assertEq(id, 1);
        assertEq(art.nextTemplateId(), 1);

        (address a, uint16 r, bytes32 m) = art.templates(1);
        assertEq(a, author);
        assertEq(r, rarity);
        assertEq(m, manifest);
    }

    function test_registerTemplate_incrementsIds() public {
        vm.startPrank(minter);
        uint256 id1 = art.registerTemplate(author, 1, keccak256("a"));
        uint256 id2 = art.registerTemplate(player, 2, keccak256("b"));
        uint256 id3 = art.registerTemplate(author, 3, keccak256("c"));
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
        art.registerTemplate(author, 1, bytes32(0));
    }

    function test_registerTemplate_revertsZeroAuthor() public {
        // author==0 is the UnknownTemplate sentinel; registering it would consume
        // an id + a leaderboard slot for a template no one can ever mint.
        vm.expectRevert(ArtifactTemplate.ZeroAuthor.selector);
        vm.prank(minter);
        art.registerTemplate(address(0), 1, keccak256("m"));

        assertEq(art.nextTemplateId(), 0);
        assertEq(art.templatesByAuthor(address(0)), 0);
    }

    function test_registerTemplate_revertsRarityAboveMax() public {
        vm.expectRevert(abi.encodeWithSelector(ArtifactTemplate.InvalidRarity.selector, uint16(10001)));
        vm.prank(minter);
        art.registerTemplate(author, 10001, keccak256("m"));

        assertEq(art.nextTemplateId(), 0);
    }

    function test_registerTemplate_acceptsRarityAtMax() public {
        // The bound is inclusive: 10000 bps == 100% is a valid full-scale rarity.
        // Read MAX_RARITY before the prank — an art.* call in the arg list would
        // otherwise consume the prank and registerTemplate would revert NotMinter.
        uint16 maxRarity = art.MAX_RARITY();
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, maxRarity, keccak256("m"));

        (, uint16 r,) = art.templates(id);
        assertEq(r, 10_000);
    }

    // --- mint ---

    function test_mint_happyPath() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1000, keccak256("m"));

        vm.expectEmit(true, true, false, true);
        emit Minted(player, id, 5);

        vm.prank(minter);
        art.mint(player, id, 5, "");

        assertEq(art.balanceOf(player, id), 5);
    }

    function test_mint_revertsNotMinter() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"));

        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(stranger);
        art.mint(player, id, 1, "");
    }

    function test_mint_revertsUnknownTemplate() public {
        vm.expectRevert(ArtifactTemplate.UnknownTemplate.selector);
        vm.prank(minter);
        art.mint(player, 999, 1, "");
    }

    function test_mint_revertsZeroRecipient() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"));

        vm.expectRevert(ArtifactTemplate.ZeroRecipient.selector);
        vm.prank(minter);
        art.mint(address(0), id, 1, "");

        assertEq(art.totalMinted(), 0);
        assertEq(art.mintedByTemplate(id), 0);
    }

    function test_mint_revertsZeroAmount() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"));

        // A zero-amount mint would emit a spurious Minted/TransferSingle and fire
        // the receiver hook for nothing; reject it instead.
        vm.expectRevert(ArtifactTemplate.ZeroAmount.selector);
        vm.prank(minter);
        art.mint(player, id, 0, "");

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
        // rm is both minter and recipient, so it pays the fee on the outer and the
        // reentrant mint — fund it for both.
        _credit(address(rm), 1_000 ether);

        uint256 id = rm.register(author, 1000);
        rm.fire(5, 3); // outer mint 5; the receiver hook reenters and mints 3 more

        assertEq(art.totalMinted(), 8); // commutativity sanity — holds under either ordering
        assertEq(art.mintedByTemplate(id), 8);
        assertEq(art.balanceOf(address(rm), id), 8);
        // CEI discriminator: the outer mint's counter write landed before its hook
        // reentered, so the reentrant call observed the outer 5 already counted.
        assertEq(rm.observedTotalMintedAtHook(), 5);
    }

    function test_mint_multipleAccumulatesBalance() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"));

        vm.startPrank(minter);
        art.mint(player, id, 3, "");
        art.mint(player, id, 4, "");
        vm.stopPrank();

        assertEq(art.balanceOf(player, id), 7);
    }

    function test_mint_passesDataThrough() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"));
        // Non-empty data must not revert for an EOA recipient.
        vm.prank(minter);
        art.mint(player, id, 2, hex"deadbeef");
        assertEq(art.balanceOf(player, id), 2);
    }

    // --- mint fee (ComputeMeter) ---

    function test_mint_chargesRarityScaledFee() public {
        vm.prank(owner);
        art.setMintFeeRate(2e15);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 5000, keccak256("m")); // 50% rarity

        // ceil(2e15 * 4 * 5000 / 10000) = 4e15, exact (no rounding).
        uint256 expectedFee = 4e15;
        uint256 creditBefore = meter.credit(player);

        // The debit is observable as ComputeMeter.Spent(buyer=to, spender=art, fee,
        // jobId=templateId) — the mint carries the templateId as the spend jobId.
        vm.expectEmit(true, true, false, true, address(meter));
        emit Spent(player, address(art), expectedFee, bytes32(id));

        vm.prank(minter);
        art.mint(player, id, 4, "");

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
        uint256 id = art.registerTemplate(author, 1, keccak256("m"));

        uint256 creditBefore = meter.credit(player);
        vm.prank(minter);
        art.mint(player, id, 1, "");

        assertEq(meter.credit(player), creditBefore - 1, "ceil charges 1, not 0");
        assertEq(meter.spentByBuyer(player), 1);
    }

    function test_mint_revertsOnInsufficientCredit() public {
        // poor has no compute credit, so the fee debit reverts and nothing mints.
        address poor = address(0xDEAD11);
        vm.prank(owner);
        art.setMintFeeRate(1e15);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 10_000, keccak256("m"));

        vm.expectRevert(ComputeMeter.InsufficientCredit.selector);
        vm.prank(minter);
        art.mint(poor, id, 1, "");

        assertEq(art.balanceOf(poor, id), 0, "no units minted");
        assertEq(art.totalMinted(), 0, "counter rolled back");
        assertEq(art.mintedByTemplate(id), 0, "per-template counter rolled back");
    }

    function test_mint_zeroRarityIsFree() public {
        // A 0-bps template charges no fee, so even a zero-credit recipient can mint
        // it. The global fee gate stays closed by the non-zero mintFeeRate guard.
        address poor = address(0xF00D);
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 0, keccak256("m"));

        vm.prank(minter);
        art.mint(poor, id, 7, "");

        assertEq(art.balanceOf(poor, id), 7);
        assertEq(meter.spentByBuyer(poor), 0, "no fee debited for rarity-0");
    }

    // --- HUD read-path counters (templatesByAuthor / totalMinted / mintedByTemplate) ---

    function test_registerTemplate_tracksTemplatesByAuthor() public {
        vm.startPrank(minter);
        art.registerTemplate(author, 1, keccak256("a"));
        art.registerTemplate(player, 2, keccak256("b"));
        art.registerTemplate(author, 3, keccak256("c"));
        vm.stopPrank();

        assertEq(art.templatesByAuthor(author), 2);
        assertEq(art.templatesByAuthor(player), 1);
        assertEq(art.templatesByAuthor(stranger), 0);
        // Across authors the per-author counts sum to nextTemplateId.
        assertEq(art.templatesByAuthor(author) + art.templatesByAuthor(player), art.nextTemplateId());
    }

    function test_mint_tracksTotalMintedAndByTemplate() public {
        vm.startPrank(minter);
        uint256 id1 = art.registerTemplate(author, 1, keccak256("a"));
        uint256 id2 = art.registerTemplate(author, 2, keccak256("b"));
        art.mint(player, id1, 5, "");
        art.mint(player, id1, 3, ""); // same template accumulates
        art.mint(stranger, id2, 4, "");
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
        uint256 id = art.registerTemplate(author, 1, keccak256("m"));
        assertEq(art.totalMinted(), 0);

        // Non-minter register: templatesByAuthor must not move.
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(stranger);
        art.registerTemplate(stranger, 1, keccak256("x"));
        assertEq(art.templatesByAuthor(stranger), 0);

        // Unknown-template mint: mint counters must not move.
        vm.expectRevert(ArtifactTemplate.UnknownTemplate.selector);
        vm.prank(minter);
        art.mint(player, 9999, 7, "");
        assertEq(art.totalMinted(), 0);
        assertEq(art.mintedByTemplate(9999), 0);

        // Non-minter mint of a known template: likewise records nothing.
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(stranger);
        art.mint(player, id, 7, "");
        assertEq(art.totalMinted(), 0);
        assertEq(art.mintedByTemplate(id), 0);
    }

    // --- minter rotation interplay ---

    function test_rotatedMinterCannotUseOldKey() public {
        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1, keccak256("m"));

        vm.prank(owner);
        art.setMinter(stranger);

        // Old minter loses rights.
        vm.expectRevert(ArtifactTemplate.NotMinter.selector);
        vm.prank(minter);
        art.mint(player, id, 1, "");

        // New minter works.
        vm.prank(stranger);
        art.mint(player, id, 1, "");
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
}

/// @notice Minter + mint recipient that reenters mint() once from its ERC-1155
///         receiver hook, recording the supply counter it observed mid-hook so a
///         test can prove the outer effects landed before the interaction (CEI).
contract ReentrantMinter is IERC1155Receiver {
    ArtifactTemplate immutable art;
    uint256 public templateId;
    uint256 reentryAmount;
    bool entered;
    uint256 public observedTotalMintedAtHook;

    constructor(ArtifactTemplate art_) {
        art = art_;
    }

    function register(address author, uint16 rarity) external returns (uint256) {
        templateId = art.registerTemplate(author, rarity, keccak256("r"));
        return templateId;
    }

    function fire(uint256 outerAmount, uint256 reentryAmount_) external {
        reentryAmount = reentryAmount_;
        art.mint(address(this), templateId, outerAmount, "");
    }

    function onERC1155Received(address, address, uint256, uint256, bytes calldata) external returns (bytes4) {
        if (!entered) {
            entered = true;
            observedTotalMintedAtHook = art.totalMinted();
            art.mint(address(this), templateId, reentryAmount, "");
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
