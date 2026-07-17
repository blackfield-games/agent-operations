// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {ERC721} from "@openzeppelin/contracts/token/ERC721/ERC721.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Stake $BLCKFLD to claim authority over a world region. Region holders
///         earn a share of compute fees + mint royalties generated in their region:
///         a fee source deposits real $BLCKFLD against a region via `depositFees`,
///         the current holder withdraws it with `claimFees`, and on transfer the
///         accrued-but-unclaimed balance is settled to the OUTGOING holder (pull
///         ledger, `withdraw`) so a buyer never inherits the seller's earned fees.
///         tokenId == region coordinate hash (keccak256(region_x, region_y, layer)).
///         Fee accounting assumes the immutable, non-fee-on-transfer $BLCKFLD: each
///         deposit credits its full `amount`, so the contract's real balance always
///         equals Σ stakes + Σ accrued + Σ withdrawable.
contract RegionAuthority is ERC721, Ownable2Step {
    using SafeERC20 for IERC20;

    IERC20 public immutable TOKEN;
    uint256 public stakeRequired;

    struct Stake {
        uint256 amount;
        uint64 stakedAt;
    }

    mapping(uint256 tokenId => Stake) public stakes;

    /// @notice Total $BLCKFLD locked as region stake across all live regions — the
    ///         protocol-wide aggregate a HUD/indexer reads directly instead of summing
    ///         every `stakes[tokenId].amount`. Bumped by `claim`, decremented by `unstake`;
    ///         a transfer leaves it unchanged (the stake stays locked under the new holder).
    uint256 public totalStaked;

    /// @notice Fees deposited against a region and not yet claimed by its current
    ///         holder. Settled to the outgoing holder's `withdrawable` on transfer.
    mapping(uint256 tokenId => uint256) public accruedFees;

    /// @notice Fees settled to a (now former) holder on a region transfer/burn,
    ///         claimable any time via `withdraw` independent of NFT ownership.
    mapping(address holder => uint256) public withdrawable;

    event Staked(address indexed holder, uint256 indexed tokenId, uint256 amount);
    event Unstaked(address indexed holder, uint256 indexed tokenId, uint256 amount);
    event StakeRequiredSet(uint256 amount);
    event FeesDeposited(uint256 indexed tokenId, address indexed from, uint256 amount);
    event FeesClaimed(uint256 indexed tokenId, address indexed holder, uint256 amount);
    /// @notice A region's accrued fees settled to its outgoing holder's `withdrawable`
    ///         ledger on transfer/unstake — the push side of `_update`, logged so an
    ///         indexer sees the credit created (and to which region/holder) without
    ///         reconstructing it from `Transfer` + the deposit/claim history.
    event FeesSettled(uint256 indexed tokenId, address indexed holder, uint256 amount);
    event Withdrawn(address indexed holder, uint256 amount);

    error StakeTooLow();
    error AlreadyClaimed();
    error NotHolder();
    error ZeroStake();
    error ZeroToken();
    error ZeroAmount();
    error UnknownRegion();
    error NothingToClaim();

    constructor(address token_, uint256 stakeRequired_, address owner_)
        ERC721("Blackfield Region", "BFLD-RGN")
        Ownable(owner_)
    {
        if (token_ == address(0)) revert ZeroToken();
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
        totalStaked += amount;
        _safeMint(msg.sender, tokenId);
        emit Staked(msg.sender, tokenId, amount);
    }

    function unstake(uint256 tokenId) external {
        if (ownerOf(tokenId) != msg.sender) revert NotHolder();
        uint256 amount = stakes[tokenId].amount;
        totalStaked -= amount;
        delete stakes[tokenId];
        // _burn fires the _update hook below, which settles any accrued fees into
        // this holder's `withdrawable` (claimable via `withdraw`) — so unstaking
        // never strands a region's earned fees. CEI: delete + burn before payout.
        _burn(tokenId);
        TOKEN.safeTransfer(msg.sender, amount);
        emit Unstaked(msg.sender, tokenId, amount);
    }

    /// @notice Deposit `amount` $BLCKFLD as fees earned by a region's holder. Pulls
    ///         real tokens from the caller (a fee source / treasury) and accrues
    ///         them to the region; permissionless because a deposit only ever adds
    ///         value to the current holder. The region must be claimed (have a
    ///         holder) so fees cannot accrue to an unowned tokenId.
    function depositFees(uint256 tokenId, uint256 amount) external {
        if (amount == 0) revert ZeroAmount();
        if (_ownerOf(tokenId) == address(0)) revert UnknownRegion();
        accruedFees[tokenId] += amount;
        TOKEN.safeTransferFrom(msg.sender, address(this), amount);
        emit FeesDeposited(tokenId, msg.sender, amount);
    }

    /// @notice Whether `tokenId` is a currently-claimed region (has a holder). The exact,
    ///         non-reverting counterpart to `depositFees`'s `UnknownRegion` guard
    ///         (`_ownerOf(tokenId) != address(0)`): a fee source can check this first and
    ///         skip the deposit for an unclaimed region rather than reverting its own call.
    function regionExists(uint256 tokenId) external view returns (bool) {
        return _ownerOf(tokenId) != address(0);
    }

    /// @notice Current region holder withdraws the region's accrued fees. CEI: the
    ///         accrued balance is zeroed before the transfer, so a reentrant
    ///         token-recipient sees nothing left to claim (no double-withdraw).
    function claimFees(uint256 tokenId) external {
        if (ownerOf(tokenId) != msg.sender) revert NotHolder();
        uint256 amount = accruedFees[tokenId];
        if (amount == 0) revert NothingToClaim();
        accruedFees[tokenId] = 0;
        TOKEN.safeTransfer(msg.sender, amount);
        emit FeesClaimed(tokenId, msg.sender, amount);
    }

    /// @notice Withdraw fees settled to the caller from a region transfer/burn (the
    ///         pull side of `_update`'s settlement). CEI: zeroed before the transfer.
    function withdraw() external {
        uint256 amount = withdrawable[msg.sender];
        if (amount == 0) revert NothingToClaim();
        withdrawable[msg.sender] = 0;
        TOKEN.safeTransfer(msg.sender, amount);
        emit Withdrawn(msg.sender, amount);
    }

    function setStakeRequired(uint256 amount) external onlyOwner {
        if (amount == 0) revert ZeroStake();
        stakeRequired = amount;
        emit StakeRequiredSet(amount);
    }

    /// @dev On every transfer and burn (not mint), settle the region's accrued fees
    ///      to the OUTGOING holder's pull ledger so a new holder cannot claim fees
    ///      earned under the previous one, emitting `FeesSettled` so the credit is
    ///      observable on-chain. Still no external call here (an event is not a call),
    ///      so a hostile/paused fee token can never brick a transfer or unstake (the
    ///      payout is deferred to the holder's own `withdraw`). The event fires only
    ///      when there are fees to move — a zero-accrued transfer stays log-free.
    function _update(address to, uint256 tokenId, address auth) internal override returns (address) {
        address from = _ownerOf(tokenId);
        if (from != address(0)) {
            uint256 accrued = accruedFees[tokenId];
            if (accrued != 0) {
                accruedFees[tokenId] = 0;
                withdrawable[from] += accrued;
                emit FeesSettled(tokenId, from, accrued);
            }
        }
        return super._update(to, tokenId, auth);
    }
}
