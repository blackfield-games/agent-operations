// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Burns $BLCKFLD to credit render-jobs against a buyer's compute budget.
///         Buyers (players, region authorities, the game itself) deposit $BLCKFLD
///         which is debited per validated render-second / NPC-tick / mint-permit.
contract ComputeMeter is Ownable2Step {
    using SafeERC20 for IERC20;

    IERC20 public immutable TOKEN;

    /// @dev address(0xdead) — Clanker $TOKEN may not implement ERC20Burnable.
    address public constant BURN_ADDRESS = 0x000000000000000000000000000000000000dEaD;

    mapping(address buyer => uint256 credit) public credit;
    mapping(address spender => bool authorized) public authorizedSpenders;
    /// @notice Jobs already debited via `spendOnce` — the per-job idempotency fence
    ///         for the off-chain render-debit relayer (a crash-retried `spendOnce`
    ///         of an already-spent job reverts instead of double-debiting). NOT
    ///         consulted by `spend`, which is deliberately repeatable (ArtifactTemplate
    ///         charges a per-mint fee with the templateId as `jobId`).
    mapping(bytes32 jobId => bool spent) public spentJobs;
    /// @notice Cumulative $BLCKFLD debited (spent) per buyer across all jobs.
    ///         Monotonic — unlike `credit`, which falls as it is spent — so it
    ///         is the per-buyer "compute consumed" HUD read.
    mapping(address buyer => uint256 spent) public spentByBuyer;
    /// @notice Cumulative $BLCKFLD burned into compute credit across all buyers.
    ///         A global HUD metric ("total compute purchased").
    uint256 public totalBurned;
    /// @notice Cumulative $BLCKFLD debited (spent) across all buyers — the
    ///         global "compute consumed" HUD metric, complementing
    ///         `totalBurned` ("purchased"). Their gap is outstanding credit.
    uint256 public totalSpent;

    /// @notice Hard upper bound on a `spendOnceBatch` — caps the O(n) fence + debit loop so
    ///         one batch can never approach the block gas limit, even from a buggy or
    ///         compromised spender. 64 mirrors `RenderReceipts.MAX_BATCH`: the render-debit
    ///         relayer settles the same per-block render wave through both, so a batch that
    ///         fits `issueReceipts` fits here. It sits well above the coordinator's default
    ///         relay drain chunk, so it never rejects a legitimate batch — it makes the gas
    ///         envelope a contract guarantee rather than a trust assumption.
    uint256 public constant MAX_BATCH = 64;

    event Deposited(address indexed buyer, uint256 amount, uint256 newCredit);
    event Spent(address indexed buyer, address indexed spender, uint256 amount, bytes32 jobId);
    event SpenderSet(address indexed spender, bool authorized);

    error NotAuthorized();
    error InsufficientCredit();
    error AlreadySpent();
    error ZeroAddressBuyer();
    error EmptyBatch();
    error BatchTooLarge();

    constructor(address token_, address owner_) Ownable(owner_) {
        TOKEN = IERC20(token_);
    }

    /// @notice Buyer deposits $TOKEN; tokens are burned and credit is recorded 1:1.
    function deposit(uint256 amount) external {
        _deposit(msg.sender, amount);
    }

    /// @notice Sponsor-fund another address's compute credit: the CALLER's $TOKEN is
    ///         burned and the credit lands on `buyer`, so a treasury / region authority /
    ///         game backend can pre-fund a player's meter (the recipient ArtifactTemplate
    ///         and the render-debit relayer already spend against). Permissionless — it
    ///         only ever ADDS spendable credit to `buyer` at the caller's expense, never
    ///         debits it, so there is no griefing vector. Reverts `ZeroAddressBuyer`
    ///         rather than burn the caller's tokens into unspendable zero-address credit
    ///         (a sponsor fat-finger); `deposit` needs no such guard because it credits
    ///         `msg.sender`, which is never the zero address.
    function depositFor(address buyer, uint256 amount) external {
        if (buyer == address(0)) revert ZeroAddressBuyer();
        _deposit(buyer, amount);
    }

    /// @dev Shared deposit core: pull the CALLER's tokens to the burn address and credit
    ///      `account` 1:1. `msg.sender` is preserved across the internal call, so the
    ///      tokens always come from the external caller while the credit lands on
    ///      `account` — identical transfer + credit + totalBurned accounting for the
    ///      self-deposit (`deposit`) and sponsor-deposit (`depositFor`) entry points, so
    ///      the two can never drift.
    function _deposit(address account, uint256 amount) internal {
        TOKEN.safeTransferFrom(msg.sender, BURN_ADDRESS, amount);
        unchecked {
            credit[account] += amount;
            totalBurned += amount;
        }
        emit Deposited(account, amount, credit[account]);
    }

    /// @notice Authorized spender debits a buyer's credit. REPEATABLE per `jobId`
    ///         (here `jobId` is a traceability tag, not an idempotency key) — used
    ///         by ArtifactTemplate, which charges a mint fee per mint with the
    ///         templateId as `jobId`. Use `spendOnce` for at-most-once render debits.
    function spend(address buyer, uint256 amount, bytes32 jobId) external {
        if (!authorizedSpenders[msg.sender]) revert NotAuthorized();
        _debit(buyer, amount, jobId);
    }

    /// @notice Debit a buyer's credit AT MOST ONCE per `jobId` — the idempotent
    ///         entry point for the off-chain render-debit relayer, whose
    ///         crash-recovery may re-submit the same validated job. A call whose
    ///         `jobId` was already spent reverts `AlreadySpent` (the relay treats it
    ///         as an idempotent success and marks the row). The `AlreadySpent` check
    ///         precedes the credit check, so a replay reports `AlreadySpent`, not a
    ///         misleading `InsufficientCredit`. A first call that reverts (e.g.
    ///         `InsufficientCredit`) rolls back the fence, so the job stays retryable.
    function spendOnce(address buyer, uint256 amount, bytes32 jobId) external {
        if (!authorizedSpenders[msg.sender]) revert NotAuthorized();
        if (spentJobs[jobId]) revert AlreadySpent();
        spentJobs[jobId] = true;
        _debit(buyer, amount, jobId);
    }

    /// @notice One element of a `spendOnceBatch` — the same three fields `spendOnce` takes
    ///         positionally. A struct array (not three parallel vectors) so a batch can
    ///         never be built with mismatched-length columns and the per-job fields stay
    ///         grouped, mirroring `RenderReceipts.ReceiptRequest`.
    struct DebitRequest {
        address buyer;
        uint256 amount;
        bytes32 jobId;
    }

    /// @notice Debit a batch of validated jobs AT MOST ONCE each, in ONE transaction instead
    ///         of N separate `spendOnce` calls — the batched twin of the idempotent
    ///         render-debit path, for a relayer settling the N jobs that landed in one block.
    ///         Every per-job effect of `spendOnce` applies PER ELEMENT: the `spentJobs`
    ///         at-most-once fence, the credit check, the three accounting updates, and the
    ///         `Spent` event — amortized to one base tx cost. This is the debit-side twin of
    ///         `RenderReceipts.issueReceipts`, which the same relayer already uses to attest
    ///         the same per-block wave in one call.
    ///
    ///         An empty batch reverts (`EmptyBatch`) and a batch above `MAX_BATCH` reverts
    ///         (`BatchTooLarge`, the gas-envelope bound) rather than silently debiting nothing
    ///         or running unbounded — a zero-length or oversized call is a caller bug, not a
    ///         no-op. Authorization is identical to `spendOnce` and is checked before any
    ///         effect.
    ///
    ///         Atomicity: every effect runs in this one tx, so ANY element reverting — a jobId
    ///         already spent OR repeated within this batch (it hits the fence an earlier
    ///         element set), or a buyer with insufficient credit — rolls the WHOLE batch back.
    ///         All-or-nothing: never some jobs debited and some not. Only `spendOnce` is
    ///         batched, not the repeatable `spend`: the render-debit relayer is the sole batch
    ///         hot path; `spend`'s per-mint ArtifactTemplate fees are single by nature.
    function spendOnceBatch(DebitRequest[] calldata items) external {
        if (!authorizedSpenders[msg.sender]) revert NotAuthorized();
        uint256 n = items.length;
        if (n == 0) revert EmptyBatch();
        if (n > MAX_BATCH) revert BatchTooLarge();
        for (uint256 i = 0; i < n; ++i) {
            DebitRequest calldata it = items[i];
            if (spentJobs[it.jobId]) revert AlreadySpent();
            spentJobs[it.jobId] = true;
            _debit(it.buyer, it.amount, it.jobId);
        }
    }

    /// @dev Shared debit core: the credit check, the three accounting updates, and
    ///      the event. `msg.sender` is preserved across the internal call, so the
    ///      event's spender is the original external caller.
    function _debit(address buyer, uint256 amount, bytes32 jobId) internal {
        uint256 c = credit[buyer];
        if (c < amount) revert InsufficientCredit();
        unchecked {
            credit[buyer] = c - amount;
            totalSpent += amount;
            spentByBuyer[buyer] += amount;
        }
        emit Spent(buyer, msg.sender, amount, jobId);
    }

    function setSpender(address spender, bool authorized) external onlyOwner {
        authorizedSpenders[spender] = authorized;
        emit SpenderSet(spender, authorized);
    }
}
