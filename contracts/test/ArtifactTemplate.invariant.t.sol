// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ArtifactTemplate, IComputeMeter} from "../src/ArtifactTemplate.sol";

/// @notice No-op ComputeMeter. These suites exercise supply accounting (balances,
///         per-template/per-author counters), which is independent of the mint fee,
///         over random amounts up to 1e24 that no finite credit could cover. A
///         permissive meter keeps every mint landing so the property under test —
///         not credit provisioning — is what's fuzzed; fee correctness is pinned in
///         the unit suite against a real ComputeMeter.
contract NoopMeter is IComputeMeter {
    function spend(address, uint256, bytes32) external {}
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// @notice Stateful handler that acts as the minter.  Because the invariant test's
///         setUp() calls art.setMinter(address(handler)), calls from the handler to
///         art pass the minter check without any prank.  All mint recipients are EOA
///         actors so the ERC-1155 onERC1155Received hook is never triggered.
contract ArtifactTemplateHandler is Test {
    ArtifactTemplate public art;

    /// @dev Cap the live template set so the supply invariant's per-call
    ///      balance sweep stays bounded (mirrors RegionAuthority's bounded tile
    ///      space). Plenty of diversity for accounting; keeps the run fast.
    uint256 public constant MAX_TEMPLATES = 16;

    address[] public actors;
    uint256[] public ids;

    uint256 public ghost_registered;
    mapping(address => uint256) public ghost_registeredByAuthor;
    mapping(uint256 => uint256) public ghost_supply;

    constructor(ArtifactTemplate art_, address[] memory actors_) {
        art = art_;
        actors = actors_;
    }

    // -----------------------------------------------------------------------
    // Handler functions
    // -----------------------------------------------------------------------

    function register(uint16 rarity, bytes32 manifest, uint256 authorSeed) external {
        if (ids.length >= MAX_TEMPLATES) return;
        address author = actors[authorSeed % actors.length];
        // Keep rarity in [0, MAX_RARITY] so every register lands (the guard would
        // otherwise revert-discard the call and waste fuzz budget); actors are all
        // non-zero, so the author guard never trips here.
        rarity = rarity % (art.MAX_RARITY() + 1);
        // Handler IS the minter; no prank needed.
        uint256 id = art.registerTemplate(author, rarity, manifest);
        ids.push(id);
        ghost_registered++;
        ghost_registeredByAuthor[author]++;
    }

    function mint(uint256 idSeed, uint256 toSeed, uint256 amount) external {
        if (ids.length == 0) return;
        uint256 id = ids[idSeed % ids.length];
        address to = actors[toSeed % actors.length];
        amount = bound(amount, 1, 1e24);
        // Handler IS the minter; no prank needed.
        art.mint(to, id, amount, "");
        ghost_supply[id] += amount;
    }

    // -----------------------------------------------------------------------
    // Accessors for the invariant contract
    // -----------------------------------------------------------------------

    function idsLength() external view returns (uint256) {
        return ids.length;
    }

    function actorsLength() external view returns (uint256) {
        return actors.length;
    }

    function actorAt(uint256 i) external view returns (address) {
        return actors[i];
    }
}

// ---------------------------------------------------------------------------
// Invariant test
// ---------------------------------------------------------------------------

contract ArtifactTemplateInvariantTest is Test {
    ArtifactTemplate art;
    ArtifactTemplateHandler handler;

    address owner = address(0xA11CE);
    string constant BASE_URI = "ipfs://base/{id}.json";

    function setUp() public {
        art = new ArtifactTemplate(owner, BASE_URI, address(new NoopMeter()), 1);

        address[] memory actors = new address[](3);
        actors[0] = address(0xA1);
        actors[1] = address(0xB0B);
        actors[2] = address(0xCA7);

        handler = new ArtifactTemplateHandler(art, actors);

        // Handler becomes the minter so its direct calls to art are authorized.
        vm.prank(owner);
        art.setMinter(address(handler));

        targetContract(address(handler));
    }

    /// @dev nextTemplateId must equal the number of registered templates.
    function invariant_nextIdEqualsRegisteredCount() public view {
        assertEq(art.nextTemplateId(), handler.ghost_registered());
    }

    /// @dev Sum of balanceOf(actor, id) over all actors equals the total minted for
    ///      that id (no transfers or burns occur during the run, so accounting is exact).
    function invariant_supplyEqualsSummedBalances() public view {
        uint256 len = handler.idsLength();
        for (uint256 i = 0; i < len; i++) {
            uint256 id = handler.ids(i);
            uint256 sum = 0;
            uint256 actorsLen = handler.actorsLength();
            for (uint256 j = 0; j < actorsLen; j++) {
                sum += art.balanceOf(handler.actorAt(j), id);
            }
            assertEq(sum, handler.ghost_supply(id));
        }
    }

    /// @dev Per-author registered count tracked on-chain matches the ghost
    ///      (creator-leaderboard read path).
    function invariant_templatesByAuthorMatchesGhost() public view {
        uint256 actorsLen = handler.actorsLength();
        for (uint256 i = 0; i < actorsLen; i++) {
            address actor = handler.actorAt(i);
            assertEq(art.templatesByAuthor(actor), handler.ghost_registeredByAuthor(actor));
        }
    }

    /// @dev Per-template minted units match the supply ghost, and across templates
    ///      they sum to totalMinted (artifact-popularity read path).
    function invariant_mintAccountingMatches() public view {
        uint256 len = handler.idsLength();
        uint256 summed = 0;
        for (uint256 i = 0; i < len; i++) {
            uint256 id = handler.ids(i);
            assertEq(art.mintedByTemplate(id), handler.ghost_supply(id));
            summed += art.mintedByTemplate(id);
        }
        assertEq(art.totalMinted(), summed);
    }
}

// ---------------------------------------------------------------------------
// Fuzz tests
// ---------------------------------------------------------------------------

contract ArtifactTemplateFuzzTest is Test {
    ArtifactTemplate art;

    address owner = address(0xA11CE);
    address minter = address(0x1117E2);
    address player = address(0xB0B);
    address author = address(0xA47403);

    string constant BASE_URI = "ipfs://base/{id}.json";

    function setUp() public {
        art = new ArtifactTemplate(owner, BASE_URI, address(new NoopMeter()), 1);
        vm.prank(owner);
        art.setMinter(minter);
    }

    /// @dev Two consecutive mints to the same player accumulate correctly.
    function testFuzz_mint_accumulatesBalance(uint256 a, uint256 b) public {
        a = bound(a, 1, 1e24);
        b = bound(b, 1, 1e24);

        vm.prank(minter);
        uint256 id = art.registerTemplate(author, 1000, keccak256("manifest"));

        vm.startPrank(minter);
        art.mint(player, id, a, "");
        art.mint(player, id, b, "");
        vm.stopPrank();

        assertEq(art.balanceOf(player, id), a + b);
    }

    /// @dev Registering n templates assigns sequential ids starting at 1 and
    ///      leaves nextTemplateId == n.
    function testFuzz_registerTemplate_sequentialIds(uint8 n) public {
        n = uint8(bound(uint256(n), 1, 50));

        vm.startPrank(minter);
        for (uint256 i = 0; i < n; i++) {
            uint256 id = art.registerTemplate(author, uint16(i % 10001), keccak256(abi.encode(i)));
            assertEq(id, i + 1);
        }
        vm.stopPrank();

        assertEq(art.nextTemplateId(), n);
    }

    /// @dev Minting an unregistered template id must revert with UnknownTemplate.
    function testFuzz_mint_unknownTemplateReverts(uint256 id) public {
        // No templates registered, so every id is unknown.
        vm.assume(id > 0); // id==0 is also unknown; just keep the space interesting
        vm.expectRevert(ArtifactTemplate.UnknownTemplate.selector);
        vm.prank(minter);
        art.mint(player, id, 1, "");
    }
}
