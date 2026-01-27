// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Burns $TOKEN to credit render-jobs against a buyer's compute budget.
///         Buyers (players, region authorities, the game itself) deposit $TOKEN
///         which is debited per validated render-second / NPC-tick / mint-permit.
contract ComputeMeter is Ownable2Step {
    using SafeERC20 for IERC20;

    IERC20 public immutable TOKEN;

    /// @dev address(0xdead) — Clanker $TOKEN may not implement ERC20Burnable.
    address public constant BURN_ADDRESS = 0x000000000000000000000000000000000000dEaD;

    mapping(address buyer => uint256 credit) public credit;
    mapping(address spender => bool authorized) public authorizedSpenders;

    event Deposited(address indexed buyer, uint256 amount, uint256 newCredit);
    event Spent(address indexed buyer, address indexed spender, uint256 amount, bytes32 jobId);
    event SpenderSet(address indexed spender, bool authorized);

    error NotAuthorized();
    error InsufficientCredit();

    constructor(address token_, address owner_) Ownable(owner_) {
        TOKEN = IERC20(token_);
    }

    /// @notice Buyer deposits $TOKEN; tokens are burned and credit is recorded 1:1.
    function deposit(uint256 amount) external {
        TOKEN.safeTransferFrom(msg.sender, BURN_ADDRESS, amount);
        unchecked {
            credit[msg.sender] += amount;
        }
        emit Deposited(msg.sender, amount, credit[msg.sender]);
    }

    /// @notice Authorized spender (coordinator contract) debits a buyer's credit per job.
    /// @dev `jobId` is the EAS attestation UID for the validated render.
    function spend(address buyer, uint256 amount, bytes32 jobId) external {
        if (!authorizedSpenders[msg.sender]) revert NotAuthorized();
        uint256 c = credit[buyer];
        if (c < amount) revert InsufficientCredit();
        unchecked {
            credit[buyer] = c - amount;
        }
        emit Spent(buyer, msg.sender, amount, jobId);
    }

    function setSpender(address spender, bool authorized) external onlyOwner {
        authorizedSpenders[spender] = authorized;
        emit SpenderSet(spender, authorized);
    }
}
