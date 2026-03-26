// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ArtifactTemplate} from "../src/ArtifactTemplate.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

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
