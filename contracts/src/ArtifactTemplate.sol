// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {ERC1155} from "@openzeppelin/contracts/token/ERC1155/ERC1155.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Player-authored artifact templates (weapons, gear, structures).
///         Minted as ERC-1155 once a $BLCKFLD burn pays the rarity-scaled mint fee
///         via ComputeMeter. Each templateId points to an offchain USD artifact
///         manifest committed in a Merkle root tied to the template URI.
contract ArtifactTemplate is ERC1155, Ownable2Step {
    address public minter;

    /// @notice Upper bound on `rarity`, in basis points (100%). The off-chain
    ///         rarity->fee calculation treats this as the full-scale value, so a
    ///         template above it would over/under-charge the $BLCKFLD mint fee.
    uint16 public constant MAX_RARITY = 10_000;

    struct Template {
        address author;
        uint16 rarity; // 0-10000 basis points, drives mint cost
        bytes32 manifest; // sha256 of offchain USD artifact bundle
    }

    mapping(uint256 templateId => Template) public templates;
    uint256 public nextTemplateId;

    /// @notice Per-author count of registered templates, for the HUD creator
    ///         leaderboard. Across all authors these sum to `nextTemplateId`.
    ///         `author` is also indexed in `TemplateRegistered` for event cross-ref.
    mapping(address author => uint256 count) public templatesByAuthor;

    /// @notice Cumulative ERC-1155 units minted across all templates — the HUD
    ///         "total artifacts minted" metric, the mint-side counterpart to the
    ///         register side's `nextTemplateId`.
    uint256 public totalMinted;

    /// @notice Cumulative units minted per template, for the HUD artifact-
    ///         popularity read. Across all templates these sum to `totalMinted`.
    mapping(uint256 templateId => uint256 amount) public mintedByTemplate;

    event TemplateRegistered(
        uint256 indexed templateId, address indexed author, uint16 rarity, bytes32 manifest
    );
    event Minted(address indexed to, uint256 indexed templateId, uint256 amount);
    event MinterSet(address indexed minter);

    error NotMinter();
    error UnknownTemplate();
    error ZeroAuthor();
    error InvalidRarity(uint16 rarity);
    error ZeroRecipient();
    error ZeroAmount();

    constructor(address owner_, string memory baseUri_) ERC1155(baseUri_) Ownable(owner_) {}

    function setMinter(address minter_) external onlyOwner {
        minter = minter_;
        emit MinterSet(minter_);
    }

    function setURI(string calldata newURI) external onlyOwner {
        _setURI(newURI);
    }

    function registerTemplate(address author, uint16 rarity, bytes32 manifest)
        external
        returns (uint256 templateId)
    {
        if (msg.sender != minter) revert NotMinter();
        // author==0 is the UnknownTemplate sentinel; registering it would brick
        // every future mint of this id. rarity>MAX_RARITY breaks the fee scale.
        if (author == address(0)) revert ZeroAuthor();
        if (rarity > MAX_RARITY) revert InvalidRarity(rarity);
        templateId = ++nextTemplateId;
        templates[templateId] = Template({author: author, rarity: rarity, manifest: manifest});
        ++templatesByAuthor[author];
        emit TemplateRegistered(templateId, author, rarity, manifest);
    }

    function mint(address to, uint256 templateId, uint256 amount, bytes calldata data) external {
        if (msg.sender != minter) revert NotMinter();
        if (to == address(0)) revert ZeroRecipient();
        if (amount == 0) revert ZeroAmount();
        if (templates[templateId].author == address(0)) revert UnknownTemplate();

        // Write the supply counters before _mint, which fires the ERC-1155
        // receiver hook on `to` — a minter that is also the recipient could
        // reenter mint() during that hook. Effects-before-interaction (CEI)
        // means the reentrant call sees counters that already include this mint.
        totalMinted += amount;
        mintedByTemplate[templateId] += amount;

        _mint(to, templateId, amount, data);
        emit Minted(to, templateId, amount);
    }
}
