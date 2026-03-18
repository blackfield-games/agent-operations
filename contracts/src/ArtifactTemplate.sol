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

    struct Template {
        address author;
        uint16 rarity; // 0-10000 basis points, drives mint cost
        bytes32 manifest; // sha256 of offchain USD artifact bundle
    }

    mapping(uint256 templateId => Template) public templates;
    uint256 public nextTemplateId;

    event TemplateRegistered(
        uint256 indexed templateId, address indexed author, uint16 rarity, bytes32 manifest
    );
    event Minted(address indexed to, uint256 indexed templateId, uint256 amount);
    event MinterSet(address indexed minter);

    error NotMinter();
    error UnknownTemplate();

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
        templateId = ++nextTemplateId;
        templates[templateId] = Template({author: author, rarity: rarity, manifest: manifest});
        emit TemplateRegistered(templateId, author, rarity, manifest);
    }

    function mint(address to, uint256 templateId, uint256 amount, bytes calldata data) external {
        if (msg.sender != minter) revert NotMinter();
        if (templates[templateId].author == address(0)) revert UnknownTemplate();
        _mint(to, templateId, amount, data);
        emit Minted(to, templateId, amount);
    }
}
