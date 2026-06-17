// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {ERC1155} from "@openzeppelin/contracts/token/ERC1155/ERC1155.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice The slice of ComputeMeter this contract depends on. Held as an interface
///         so the mint-fee debit is one external call against a fixed ABI and tests
///         can substitute a meter (real, reentrant, or no-op) at construction.
interface IComputeMeter {
    function spend(address buyer, uint256 amount, bytes32 jobId) external;
}

/// @notice Player-authored artifact templates (weapons, gear, structures).
///         Each mint debits a rarity-scaled $BLCKFLD fee from the recipient's
///         ComputeMeter credit before the ERC-1155 units are minted; an
///         insufficient balance reverts the mint. Each templateId points to an
///         offchain USD artifact manifest committed in a Merkle root tied to the
///         template URI.
contract ArtifactTemplate is ERC1155, Ownable2Step {
    address public minter;

    /// @notice ComputeMeter that holds buyer compute credit. This contract must be
    ///         in its `authorizedSpenders` set (wired at deploy) or every
    ///         fee-charging mint reverts `NotAuthorized` at the meter.
    IComputeMeter public immutable computeMeter;

    /// @notice Mint fee at full rarity, per ERC-1155 unit, in $BLCKFLD compute
    ///         credit. The charged fee scales linearly with `rarity` (see `_mintFee`).
    ///         Owner-set and kept strictly positive so the global fee gate cannot be
    ///         opened (mirrors RegionAuthority's non-zero stake floor).
    uint256 public mintFeeRate;

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

    /// @notice Optional hard cap on `mintedByTemplate[id]`, set at register time.
    ///         0 = uncapped — the default for every template (an unset slot reads
    ///         0), preserving pre-cap behavior; a non-zero value caps cumulative
    ///         mints for that template, enforcing scarcity for high-rarity tiers.
    mapping(uint256 templateId => uint256) public templateMaxSupply;

    event TemplateRegistered(
        uint256 indexed templateId, address indexed author, uint16 rarity, bytes32 manifest
    );
    event Minted(address indexed to, uint256 indexed templateId, uint256 amount);
    event MinterSet(address indexed minter);
    event MintFeeRateSet(uint256 rate);

    error NotMinter();
    error UnknownTemplate();
    error ZeroAuthor();
    error InvalidRarity(uint16 rarity);
    error ZeroRecipient();
    error ZeroAmount();
    error ZeroComputeMeter();
    error ZeroFeeRate();
    error SupplyExceeded(uint256 templateId, uint256 wouldMint, uint256 maxSupply);

    constructor(address owner_, string memory baseUri_, address computeMeter_, uint256 mintFeeRate_)
        ERC1155(baseUri_)
        Ownable(owner_)
    {
        if (computeMeter_ == address(0)) revert ZeroComputeMeter();
        if (mintFeeRate_ == 0) revert ZeroFeeRate();
        computeMeter = IComputeMeter(computeMeter_);
        mintFeeRate = mintFeeRate_;
    }

    function setMinter(address minter_) external onlyOwner {
        minter = minter_;
        emit MinterSet(minter_);
    }

    /// @notice Owner sets the per-unit full-rarity mint fee. Zero is rejected so the
    ///         fee gate can never be globally disabled.
    function setMintFeeRate(uint256 rate) external onlyOwner {
        if (rate == 0) revert ZeroFeeRate();
        mintFeeRate = rate;
        emit MintFeeRateSet(rate);
    }

    function setURI(string calldata newURI) external onlyOwner {
        _setURI(newURI);
    }

    /// @param maxSupply Hard cap on cumulative units mintable for this template;
    ///        0 leaves it uncapped. The id is always freshly minted
    ///        (`++nextTemplateId`), so a template's cap is fixed at registration and
    ///        can never be retroactively lowered below an already-minted count.
    function registerTemplate(address author, uint16 rarity, bytes32 manifest, uint256 maxSupply)
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
        templateMaxSupply[templateId] = maxSupply;
        ++templatesByAuthor[author];
        emit TemplateRegistered(templateId, author, rarity, manifest);
    }

    function mint(address to, uint256 templateId, uint256 amount, bytes calldata data) external {
        if (msg.sender != minter) revert NotMinter();
        if (to == address(0)) revert ZeroRecipient();
        if (amount == 0) revert ZeroAmount();
        Template storage t = templates[templateId];
        if (t.author == address(0)) revert UnknownTemplate();

        // Supply cap (effects-before-interaction): reject BEFORE any counter write or
        // external call, so a reentrant minter sees the cap enforced and a batched
        // `amount` can't slip past. `cap == 0` is uncapped (the default). Exact-fill
        // (wouldMint == cap) succeeds; one unit over reverts. `amount == 0` already
        // reverted above, so wouldMint strictly increases here.
        uint256 cap = templateMaxSupply[templateId];
        uint256 wouldMint = mintedByTemplate[templateId] + amount;
        if (cap != 0 && wouldMint > cap) revert SupplyExceeded(templateId, wouldMint, cap);

        uint256 fee = _mintFee(amount, t.rarity);

        // CEI: write the supply counters (effects) before the two external
        // interactions below — the meter `spend` and `_mint`, either of which can
        // reenter (a misbehaving meter during spend; a minter that is also the
        // recipient via the ERC-1155 receiver hook during _mint). A reentrant call
        // therefore sees counters that already include this mint.
        totalMinted += amount;
        mintedByTemplate[templateId] = wouldMint;

        // Charge the recipient's compute credit before the units are minted, so an
        // insufficient balance reverts the whole mint. The spend's jobId carries the
        // templateId (these are artifact mints, not the render-job EAS UIDs the meter
        // also serves). A rarity-0 template (the 0-bps common tier) yields a zero fee
        // and skips the debit entirely.
        if (fee != 0) computeMeter.spend(to, fee, bytes32(templateId));

        _mint(to, templateId, amount, data);
        emit Minted(to, templateId, amount);
    }

    /// @dev Rarity-scaled mint fee in $BLCKFLD compute credit:
    ///      ceil(mintFeeRate * amount * rarity / MAX_RARITY). Rounds UP so any
    ///      positive rarity costs at least one unit — a low rarity must never round
    ///      down to a free mint. rarity == 0 (the 0-bps common tier) yields exactly
    ///      zero. The multiply is checked arithmetic: it reverts only on the
    ///      astronomical overflow of mintFeeRate * amount * rarity, reachable solely
    ///      from absurd owner-set rate / coordinator-set amount values.
    function _mintFee(uint256 amount, uint16 rarity) internal view returns (uint256) {
        uint256 numerator = mintFeeRate * amount * rarity;
        if (numerator == 0) return 0;
        return (numerator - 1) / MAX_RARITY + 1;
    }
}
