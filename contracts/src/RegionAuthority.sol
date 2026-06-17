// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {ERC721} from "@openzeppelin/contracts/token/ERC721/ERC721.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Stake $BLCKFLD to claim authority over a world region. Region holders
///         earn a share of compute fees + mint royalties inside their region.
///         tokenId == region coordinate hash (keccak256(region_x, region_y, layer)).
contract RegionAuthority is ERC721, Ownable2Step {
    using SafeERC20 for IERC20;

    IERC20 public immutable TOKEN;
    uint256 public stakeRequired;

    struct Stake {
        uint256 amount;
        uint64 stakedAt;
    }

    mapping(uint256 tokenId => Stake) public stakes;

    event Staked(address indexed holder, uint256 indexed tokenId, uint256 amount);
    event Unstaked(address indexed holder, uint256 indexed tokenId, uint256 amount);
    event StakeRequiredSet(uint256 amount);

    error StakeTooLow();
    error AlreadyClaimed();
    error NotHolder();
    error ZeroStake();

    constructor(address token_, uint256 stakeRequired_, address owner_)
        ERC721("Blackfield Region", "BFLD-RGN")
        Ownable(owner_)
    {
        if (stakeRequired_ == 0) revert ZeroStake();
        TOKEN = IERC20(token_);
        stakeRequired = stakeRequired_;
    }

    function claim(uint256 tokenId, uint256 amount) external {
        // A region authority — which earns a fee share — must lock a positive
        // stake. This holds independent of stakeRequired (which is also kept
        // positive), so no tokenId is ever minted against a zero stake.
        if (amount == 0) revert ZeroStake();
        if (amount < stakeRequired) revert StakeTooLow();
        if (_ownerOf(tokenId) != address(0)) revert AlreadyClaimed();
        TOKEN.safeTransferFrom(msg.sender, address(this), amount);
        stakes[tokenId] = Stake({amount: amount, stakedAt: uint64(block.timestamp)});
        _safeMint(msg.sender, tokenId);
        emit Staked(msg.sender, tokenId, amount);
    }

    function unstake(uint256 tokenId) external {
        if (ownerOf(tokenId) != msg.sender) revert NotHolder();
        uint256 amount = stakes[tokenId].amount;
        delete stakes[tokenId];
        _burn(tokenId);
        TOKEN.safeTransfer(msg.sender, amount);
        emit Unstaked(msg.sender, tokenId, amount);
    }

    function setStakeRequired(uint256 amount) external onlyOwner {
        if (amount == 0) revert ZeroStake();
        stakeRequired = amount;
        emit StakeRequiredSet(amount);
    }
}
