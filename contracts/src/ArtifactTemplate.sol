// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {ERC1155} from "@openzeppelin/contracts/token/ERC1155/ERC1155.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @notice The slice of ComputeMeter this contract depends on. Held as an interface
///         so the mint-fee debit is one external call against a fixed ABI and tests
///         can substitute a meter (real, reentrant, or no-op) at construction.
interface IComputeMeter {
    function spend(address buyer, uint256 amount, bytes32 jobId) external;
}

/// @notice The slice of RegionAuthority this contract depends on: the fee-share
///         intake (`depositFees`) and the staked $BLCKFLD token it pulls. Held as an
///         interface so the royalty deposit is one external call against a fixed ABI,
///         and the royalty token is read from `TOKEN()` at construction so the two can
///         never desync.
interface IRegionAuthority {
    function depositFees(uint256 tokenId, uint256 amount) external;
    function TOKEN() external view returns (address);
}

/// @notice Player-authored artifact templates (weapons, gear, structures).
///         Each mint debits a rarity-scaled $BLCKFLD fee from the recipient's
///         ComputeMeter credit AND routes a separate rarity-scaled real-$BLCKFLD
///         royalty to the minted artifact's region fee pool (RegionAuthority), before
///         the ERC-1155 units are minted; insufficient credit, insufficient royalty
///         balance/allowance, or an unknown region all revert the whole mint. Each
///         templateId points to an offchain USD artifact manifest committed in a
///         Merkle root tied to the template URI.
contract ArtifactTemplate is ERC1155, Ownable2Step {
    using SafeERC20 for IERC20;

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

    /// @notice RegionAuthority that receives the per-mint region royalty via
    ///         `depositFees`. Wired at deploy and immutable; the region must be claimed
    ///         (have a holder) or the deposit reverts `UnknownRegion`, unwinding the mint.
    IRegionAuthority public immutable regionAuthority;

    /// @notice The $BLCKFLD token the region royalty is paid in — real, transferable
    ///         tokens, NOT the burned ComputeMeter credit the compute fee debits. Read
    ///         from `regionAuthority.TOKEN()` at construction so it always matches the
    ///         token `depositFees` pulls (the approve target can never desync).
    IERC20 public immutable royaltyToken;

    /// @notice Region royalty at full rarity, per ERC-1155 unit, in real $BLCKFLD. The
    ///         charged royalty scales linearly with `rarity` (see `_scaledFee`) and is
    ///         ADDITIVE to and independent of the burned compute fee. Owner-set and kept
    ///         strictly positive (mirrors `mintFeeRate`) so the royalty gate can't be
    ///         globally opened.
    uint256 public royaltyRate;

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

    /// @notice Cumulative ERC-1155 units burned across all templates — the destroy-side
    ///         counterpart to `totalMinted`. Kept SEPARATE from the mint counters on
    ///         purpose: a burn lowers circulating supply but never `mintedByTemplate`, so
    ///         the supply cap keeps bounding cumulative MINTS and burning frees no cap
    ///         headroom (a mint->burn->mint can never slip past a template's `maxSupply`).
    uint256 public totalBurned;

    /// @notice Cumulative units burned per template, the destroy-side counterpart to
    ///         `mintedByTemplate`. A template's circulating supply is
    ///         `mintedByTemplate[id] - burnedByTemplate[id]`; across all templates these
    ///         sum to `totalBurned`.
    mapping(uint256 templateId => uint256 amount) public burnedByTemplate;

    event TemplateRegistered(
        uint256 indexed templateId, address indexed author, uint16 rarity, bytes32 manifest
    );
    event Minted(address indexed to, uint256 indexed templateId, uint256 amount);
    event Burned(address indexed from, uint256 indexed templateId, uint256 amount);
    event MinterSet(address indexed minter);
    event MintFeeRateSet(uint256 rate);
    event RoyaltyRateSet(uint256 rate);
    event URISet(string uri);
    event RoyaltyRouted(uint256 indexed templateId, uint256 indexed regionId, uint256 amount);

    error NotMinter();
    error UnknownTemplate();
    error ZeroAuthor();
    error InvalidRarity(uint16 rarity);
    error ZeroRecipient();
    error ZeroAmount();
    error ZeroComputeMeter();
    error ZeroFeeRate();
    error ZeroRegionAuthority();
    error ZeroRoyaltyRate();
    error ZeroRoyaltyToken();
    error ZeroMinter();
    error SupplyExceeded(uint256 templateId, uint256 wouldMint, uint256 maxSupply);

    constructor(
        address owner_,
        string memory baseUri_,
        address computeMeter_,
        uint256 mintFeeRate_,
        address regionAuthority_,
        uint256 royaltyRate_
    ) ERC1155(baseUri_) Ownable(owner_) {
        if (computeMeter_ == address(0)) revert ZeroComputeMeter();
        if (mintFeeRate_ == 0) revert ZeroFeeRate();
        if (regionAuthority_ == address(0)) revert ZeroRegionAuthority();
        if (royaltyRate_ == 0) revert ZeroRoyaltyRate();
        computeMeter = IComputeMeter(computeMeter_);
        mintFeeRate = mintFeeRate_;
        regionAuthority = IRegionAuthority(regionAuthority_);
        // Bind the royalty token to exactly what RegionAuthority pulls in depositFees,
        // so the approve target can never desync from the deposit target. Reject a zero
        // token (a misconfigured RegionAuthority) here rather than letting every paid
        // mint revert opaquely at the SafeERC20 call — completes the non-zero contract
        // on every constructor address.
        IERC20 token = IERC20(IRegionAuthority(regionAuthority_).TOKEN());
        if (address(token) == address(0)) revert ZeroRoyaltyToken();
        royaltyToken = token;
        royaltyRate = royaltyRate_;
    }

    /// @notice Owner sets the sole authorized minter. Zero is rejected so the minter gate
    ///         can never be bricked into a permanent revert: `registerTemplate`/`mint` both
    ///         require `msg.sender == minter`, and no real sender is `address(0)`, so a
    ///         zero minter would silently fail every mint with `NotMinter` (mirrors the
    ///         setMintFeeRate/setRoyaltyRate "never globally disable the gate" guards). The
    ///         constructor's bootstrap state (`minter == 0`, minting disabled until wired)
    ///         is a one-way door out of zero; to pause, rotate to a controlled holding
    ///         address, not back to zero.
    function setMinter(address minter_) external onlyOwner {
        if (minter_ == address(0)) revert ZeroMinter();
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

    /// @notice Owner sets the per-unit full-rarity region royalty. Zero is rejected so
    ///         the royalty gate can never be globally disabled (mirrors setMintFeeRate).
    function setRoyaltyRate(uint256 rate) external onlyOwner {
        if (rate == 0) revert ZeroRoyaltyRate();
        royaltyRate = rate;
        emit RoyaltyRateSet(rate);
    }

    /// @notice Owner rotates the ERC1155 metadata base for every token. OZ's `_setURI`
    ///         assigns storage and emits nothing, so this is the only signal an off-chain
    ///         indexer keying its artifact-metadata cache off config events (as it does for
    ///         `MintFeeRateSet`/`RoyaltyRateSet`) gets that the base moved.
    function setURI(string calldata newURI) external onlyOwner {
        _setURI(newURI);
        emit URISet(newURI);
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

    /// @param regionId RegionAuthority tokenId (region coordinate hash) the mint's
    ///        royalty accrues to. Used only when a royalty is charged (rarity > 0); a
    ///        rarity-0 mint ignores it. The region must be claimed or the deposit
    ///        reverts `UnknownRegion`. The caller names the region — correct
    ///        attribution is off-chain policy, not enforced on-chain (see depositFees).
    function mint(address to, uint256 templateId, uint256 amount, uint256 regionId, bytes calldata data)
        external
    {
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

        uint256 fee = _scaledFee(mintFeeRate, amount, t.rarity);
        uint256 royalty = _scaledFee(royaltyRate, amount, t.rarity);

        // CEI: commit the supply counters (effects) before EVERY external interaction
        // below — the meter `spend`, the royalty pull + deposit, and `_mint` — any of
        // which can reenter (a misbehaving meter/token; a minter that is also the
        // recipient via the ERC-1155 receiver hook). A reentrant call therefore sees
        // counters that already include this mint, so the supply cap holds and the
        // royalty is never double-routed.
        totalMinted += amount;
        mintedByTemplate[templateId] = wouldMint;

        // Charge the recipient's compute credit, so an insufficient balance reverts the
        // whole mint. The spend's jobId carries the templateId (these are artifact
        // mints, not the render-job EAS UIDs the meter also serves). A rarity-0 template
        // (the 0-bps common tier) yields a zero fee and skips the debit entirely.
        if (fee != 0) computeMeter.spend(to, fee, bytes32(templateId));

        // Route the region royalty: pull real $BLCKFLD from the recipient and deposit
        // it into the artifact's region fee pool. Distinct from the burned compute fee
        // above — its own rate, its own (transferable) token. rarity-0 skips it;
        // otherwise an unknown/unclaimed regionId reverts inside depositFees, unwinding
        // the whole mint. The allowance is set to exactly `royalty` and fully consumed
        // by depositFees, so no standing allowance survives the call.
        if (royalty != 0) {
            royaltyToken.safeTransferFrom(to, address(this), royalty);
            royaltyToken.forceApprove(address(regionAuthority), royalty);
            regionAuthority.depositFees(regionId, royalty);
            emit RoyaltyRouted(templateId, regionId, royalty);
        }

        _mint(to, templateId, amount, data);
        emit Minted(to, templateId, amount);
    }

    /// @notice Destroy `amount` units of `templateId` held by `from`. A holder consumes
    ///         or salvages their own artifact; an operator `from` approved via
    ///         `setApprovalForAll` may burn on their behalf (the standard ERC-1155
    ///         allowance). Deliberately NOT `minter`-gated: destruction is the holder's
    ///         prerogative — the minter can create artifacts, never unilaterally destroy
    ///         a player's. A zero amount is rejected (mirrors mint's `ZeroAmount`) so a
    ///         no-op burn can't spam a `Burned` event.
    /// @dev Bumps the SEPARATE burn counters, never the cumulative mint counters: the
    ///      supply cap bounds `mintedByTemplate` (cumulative mints), so a burn frees no
    ///      cap headroom and a mint->burn->mint can never exceed a template's `maxSupply`.
    ///      Burning fires no ERC-1155 receiver hook (the destination is `address(0)`), so
    ///      there is no reentrancy surface; `_burn` reverts `ERC1155InsufficientBalance`
    ///      before any counter moves.
    function burn(address from, uint256 templateId, uint256 amount) external {
        if (from != msg.sender && !isApprovedForAll(from, msg.sender)) {
            revert ERC1155MissingApprovalForAll(msg.sender, from);
        }
        if (amount == 0) revert ZeroAmount();
        _burn(from, templateId, amount);
        totalBurned += amount;
        burnedByTemplate[templateId] += amount;
        emit Burned(from, templateId, amount);
    }

    /// @dev Rarity-scaled per-mint charge in `rate` units, shared by the compute fee
    ///      (`mintFeeRate`, burned credit) and the region royalty (`royaltyRate`, real
    ///      $BLCKFLD): ceil(rate * amount * rarity / MAX_RARITY). Rounds UP so any
    ///      positive rarity costs at least one unit — a low rarity must never round down
    ///      to a free mint. rarity == 0 (the 0-bps common tier) yields exactly zero. The
    ///      multiply is checked arithmetic: it reverts only on the astronomical overflow
    ///      of rate * amount * rarity, reachable solely from absurd owner-set rate /
    ///      coordinator-set amount values.
    function _scaledFee(uint256 rate, uint256 amount, uint16 rarity) internal pure returns (uint256) {
        uint256 numerator = rate * amount * rarity;
        if (numerator == 0) return 0;
        return (numerator - 1) / MAX_RARITY + 1;
    }
}
