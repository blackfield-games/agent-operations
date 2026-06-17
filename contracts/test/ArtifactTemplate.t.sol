// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ArtifactTemplate} from "../src/ArtifactTemplate.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC1155Receiver} from "@openzeppelin/contracts/token/ERC1155/IERC1155Receiver.sol";

contract ArtifactTemplateTest is Test {
    ArtifactTemplate art;

    address owner = address(0xA11CE);
    address minter = address(0x1117E2);
    address author = address(0xA47403);
    address player = address(0xB0B);
    address stranger = address(0xBEEF);

    string constant BASE_URI = "ipfs://base/{id}.json";

    event TemplateRegistered(
        uint256 indexed templateId, address indexed author, uint16 rarity, bytes32 manifest
    );
    event Minted(address indexed to, uint256 indexed templateId, uint256 amount);
    event MinterSet(address indexed minter);

    function setUp() public {
        art = new ArtifactTemplate(owner, BASE_URI);
        vm.prank(owner);
        art.setMinter(minter);
    }

    // --- construction ---

    function test_constructor() public view {
        assertEq(art.owner(), owner);
        assertEq(art.uri(0), BASE_URI);
        assertEq(art.minter(), minter);
        assertEq(art.nextTemplateId(), 0);
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
