// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice The slice of AgentRegistry this contract depends on: the identity gate
///         (`isRegistered`) checked when a match opens so escrow only ever involves
///         live agent identities, the reputation write-back (`recordMatchResult`)
///         called on settlement, and the staked $BLCKFLD token (`TOKEN`) read once at
///         construction so the wager-escrow token can never desync from the
///         identity/bond token.
interface IAgentRegistry {
    function isRegistered(address agent) external view returns (bool);
    function recordMatchResult(address agent, int256 reputationDelta) external;
    function TOKEN() external view returns (address);
}

/// @notice On-chain settlement for agent-vs-agent (A2A) ranked matches — the
///         agent-economy loop closer. An authorized result attester records a
///         finalized match (the winning seat and a commitment to the arena
///         `ReplayRecord` keccak digest), the contract settles an OPTIONAL two-sided
///         $BLCKFLD wager escrow, and writes the ranked result back to AgentRegistry
///         reputation.
///
///         Trust model — the same split the rest of the stack uses:
///         - The ATTESTER (the match service) is trusted ONLY to name the true
///           outcome of a match it formed. It can never drain escrow: every payout
///           goes to a match PARTICIPANT (the named winner) or back to a funder, so
///           it can misattribute the winner BETWEEN the two real agents but never pay
///           itself or a third party. This is the trust RenderReceipts already places
///           in its coordinator naming the earner.
///         - The AGENTS control their own money: each funds its OWN seat
///           (`msg.sender`, no payer parameter), and an agent whose opponent never
///           funds can `reclaim` without involving the attester. Reputation, by
///           contrast, an agent can never move itself — only this contract (an
///           authorized AgentRegistry writer) does. The fixed `settle`/`settleDraw`
///           move a fixed owner-set magnitude (the attester names only WHO won). The
///           variable `settleRanked`/`settleDrawRanked` (inert until the owner sets a
///           `maxRatingDelta` cap) let the attester ALSO supply the skill-scaled Elo
///           magnitude the off-chain rating curve computed — but BOUNDED by that cap,
///           so it can scale standing within an owner-set ceiling, never inflate it
///           arbitrarily. WHO wins is always the attester's to name; HOW MUCH is either
///           fixed or attester-chosen within a cap — never unbounded.
///
///         Idempotency: a `matchId` (the arena match UUID) is single-use. `openMatch`
///         requires a fresh slot, and every resolution (`settle` / `settleRanked` /
///         `settleDraw` / `settleDrawRanked` / `cancelMatch` / `refundExpired`)
///         requires the match is still `Open`, flipping it to a terminal state BEFORE
///         any external call (checks-effects-interactions) — the variable paths inherit
///         the fence by sharing the same internal mechanics as the fixed ones. A
///         replayed settlement is therefore rejected, not a double-pay or a
///         double-count — the same per-id fence as `ComputeMeter.spendOnce`.
contract MatchSettlement is Ownable2Step {
    using SafeERC20 for IERC20;

    /// @dev `None` is the zero default, so an untouched `matchId` reads `None` and
    ///      `openMatch` gates on it. `Open` → funded/played; the three terminal states
    ///      (`Settled`, `Cancelled`, `Expired`) are each reached at most once and never
    ///      left. `Expired` is a deadline self-refund — mechanically a cancel, kept
    ///      distinct so a void caused by a vanished attester is observable as such
    ///      (a rising `Expired` count is an attester-liveness alarm) rather than
    ///      conflated with an attester's deliberate `cancelMatch`.
    enum Status {
        None,
        Open,
        Settled,
        Cancelled,
        Expired
    }

    struct Match {
        address agentA;
        address agentB;
        /// @dev Per-SEAT wager in $BLCKFLD. 0 = a reputation-only match with no
        ///      escrow; funding is then irrelevant and the payout path is skipped. A
        ///      decisive winner takes the `2 * stake` pot.
        uint256 stake;
        /// @dev The committed replay digest (arena `ReplayRecord.digest`), set on
        ///      settle or draw. Zero until resolved; readers gate on `status`.
        bytes32 replayHash;
        /// @dev The decisive winner, set on `settle`. Stays zero after a draw or
        ///      before settlement — distinguish via `status == Settled`.
        address winner;
        bool aFunded;
        bool bFunded;
        Status status;
        /// @dev Wall-clock instant (set at `openMatch` to `block.timestamp +
        ///      settleWindow`) at and after which either side may `refundExpired` a
        ///      still-`Open` match. Packs into the same slot as `winner`/the flags/
        ///      `status`, so recording it is free on the SSTORE `openMatch` already does.
        uint64 deadline;
    }

    IAgentRegistry public immutable registry;

    /// @notice The $BLCKFLD token wagers are escrowed in — real, transferable tokens.
    ///         Bound at construction to exactly `registry.TOKEN()` so the escrow token
    ///         can never desync from the identity/bond token (mirrors RenderReceipts
    ///         binding its fee token from RegionAuthority).
    IERC20 public immutable token;

    /// @notice The fixed reputation magnitude a decisive ranked match moves: the
    ///         winner gains it, the loser loses it. Owner-set, kept non-zero (a ranked
    ///         match must move standing) and within `int256` range (it is applied
    ///         signed). The attester names the winner but cannot choose this
    ///         magnitude, so it can never inflate an agent's standing arbitrarily.
    uint256 public reputationDelta;

    /// @notice The cap on the MAGNITUDE of a variable reputation delta supplied to
    ///         `settleRanked`/`settleDrawRanked` — the bound that keeps the attester's
    ///         per-match power finite once it can choose how much standing moves. The
    ///         attester names the winner AND (for the variable path) the skill-scaled
    ///         Elo magnitude the off-chain rating curve computed, but never beyond this
    ///         owner-set ceiling, so a compromised attester can mis-scale a single match
    ///         within the cap but can never inflate standing arbitrarily.
    ///
    ///         Zero (the default) DISABLES the variable path entirely — every
    ///         `settleRanked`/`settleDrawRanked` reverts, and the contract behaves
    ///         exactly as the fixed-magnitude-only design until the owner opts in. Set it
    ///         >= the largest K-factor the rating curve is configured with, since the
    ///         core `ranked_delta` is bounded by K; a cap below K would reject a
    ///         legitimate settlement. Bounded `<= int256.max` so the signed application
    ///         and its negation can never overflow.
    uint256 public maxRatingDelta;

    /// @notice Lower/upper bounds and the construction default for `settleWindow`. The
    ///         floor is generous (a match plays in seconds and an attester settles in
    ///         one tx, so an hour is ~60× headroom) so a still-in-progress match can
    ///         never be refunded out from under a merely-slow attester; the ceiling caps
    ///         how long a both-funded escrow stays locked when the attester truly
    ///         vanishes. The default is what every deploy starts with (the constructor
    ///         takes no window arg, to keep its signature stable), tunable in-range
    ///         post-deploy.
    uint64 public constant MIN_SETTLE_WINDOW = 1 hours;
    uint64 public constant MAX_SETTLE_WINDOW = 30 days;
    uint64 public constant DEFAULT_SETTLE_WINDOW = 1 days;

    /// @notice How long after `openMatch` the attester has to resolve a match before
    ///         either participant may `refundExpired` it. Owner-set within
    ///         [`MIN_SETTLE_WINDOW`, `MAX_SETTLE_WINDOW`]; the per-match `deadline` is
    ///         frozen from this value at open, so retuning the window never moves an
    ///         already-open match's deadline.
    uint64 public settleWindow;

    mapping(bytes32 matchId => Match) public matches;
    mapping(address attester => bool authorized) public resultAttesters;

    event MatchOpened(bytes32 indexed matchId, address indexed agentA, address indexed agentB, uint256 stake);
    event MatchFunded(bytes32 indexed matchId, address indexed agent, uint256 stake);
    event MatchReclaimed(bytes32 indexed matchId, address indexed agent, uint256 stake);
    event MatchSettled(
        bytes32 indexed matchId,
        address indexed winner,
        address indexed loser,
        bytes32 replayHash,
        uint256 payout
    );
    event MatchDrawn(
        bytes32 indexed matchId, address indexed agentA, address indexed agentB, bytes32 replayHash
    );
    event MatchCancelled(bytes32 indexed matchId, uint256 refundA, uint256 refundB);
    event MatchExpired(bytes32 indexed matchId, uint256 refundA, uint256 refundB);
    event AttesterSet(address indexed attester, bool authorized);
    event ReputationDeltaSet(uint256 reputationDelta);
    event MaxRatingDeltaSet(uint256 maxRatingDelta);
    event SettleWindowSet(uint64 settleWindow);

    error NotAttester();
    error ZeroRegistry();
    error ZeroToken();
    error ZeroReputationDelta();
    error ReputationDeltaTooLarge();
    error MatchExists();
    error MatchNotOpen();
    error SameAgent();
    error AgentNotRegistered(address agent);
    error NotParticipant();
    error NoWager();
    error AlreadyFunded();
    error NotFunded();
    error OpponentFunded();
    error NotFullyFunded();
    error InvalidWinner();
    error ZeroReplayHash();
    error NotExpired();
    error SettleWindowOutOfRange();
    error VariableSettleDisabled();
    error NegativeWinnerDelta();
    error RatingDeltaTooLarge();

    constructor(address registry_, address owner_, uint256 reputationDelta_) Ownable(owner_) {
        if (registry_ == address(0)) revert ZeroRegistry();
        if (reputationDelta_ == 0) revert ZeroReputationDelta();
        if (reputationDelta_ > uint256(type(int256).max)) revert ReputationDeltaTooLarge();
        registry = IAgentRegistry(registry_);
        // Bind escrow to exactly the token AgentRegistry bonds in, so a wager can
        // never be funded in a different token than the identity it settles against.
        // Reject a zero token (a misconfigured registry) here rather than letting
        // every fund/payout revert opaquely at the SafeERC20 call (mirrors RenderReceipts).
        IERC20 t = IERC20(IAgentRegistry(registry_).TOKEN());
        if (address(t) == address(0)) revert ZeroToken();
        token = t;
        reputationDelta = reputationDelta_;
        // The constructor takes no window argument (its signature is fixed by the
        // deploy script), so every deploy starts at the in-range default; the owner
        // tunes it with `setSettleWindow` before or after go-live.
        settleWindow = DEFAULT_SETTLE_WINDOW;
    }

    modifier onlyAttester() {
        if (!resultAttesters[msg.sender]) revert NotAttester();
        _;
    }

    /// @notice Authorize (or revoke) a result attester — the match service that opens
    ///         and settles matches. Owner-only, the single trust root for who may name
    ///         an outcome.
    function setAttester(address attester, bool authorized) external onlyOwner {
        resultAttesters[attester] = authorized;
        emit AttesterSet(attester, authorized);
    }

    /// @notice Owner sets the per-match reputation magnitude. Zero is rejected so a
    ///         ranked match always moves standing; an out-of-`int256`-range value is
    ///         rejected so the signed application can never overflow.
    function setReputationDelta(uint256 newDelta) external onlyOwner {
        if (newDelta == 0) revert ZeroReputationDelta();
        if (newDelta > uint256(type(int256).max)) revert ReputationDeltaTooLarge();
        reputationDelta = newDelta;
        emit ReputationDeltaSet(newDelta);
    }

    /// @notice Owner sets the variable-delta magnitude cap. Zero is allowed and DISABLES
    ///         the variable settle path (the default posture — the contract is
    ///         fixed-magnitude-only until the owner opts in); a positive value enables
    ///         `settleRanked`/`settleDrawRanked` with `|delta| <= newMax`. Rejected above
    ///         `int256.max` so the signed application and its negation can never overflow.
    function setMaxRatingDelta(uint256 newMax) external onlyOwner {
        if (newMax > uint256(type(int256).max)) revert RatingDeltaTooLarge();
        maxRatingDelta = newMax;
        emit MaxRatingDeltaSet(newMax);
    }

    /// @notice Owner sets the attester-resolution window. Bounded to
    ///         [`MIN_SETTLE_WINDOW`, `MAX_SETTLE_WINDOW`] so it can be neither so short
    ///         that an in-progress match is refunded out from under the attester nor so
    ///         long that a vanished-attester escrow stays locked unreasonably. Only
    ///         affects matches opened AFTER the change — an open match's deadline was
    ///         frozen at its open.
    function setSettleWindow(uint64 newWindow) external onlyOwner {
        if (newWindow < MIN_SETTLE_WINDOW || newWindow > MAX_SETTLE_WINDOW) {
            revert SettleWindowOutOfRange();
        }
        settleWindow = newWindow;
        emit SettleWindowSet(newWindow);
    }

    /// @notice Open a fresh match between two registered agents with a per-seat
    ///         `stake` (0 for a reputation-only match). Attester-gated: the match
    ///         service that formed the pair declares the roster and the agreed stake;
    ///         the agents then consent to the wager by funding their own seats.
    ///         `matchId` is single-use — a non-`None` slot reverts, so a settled or
    ///         cancelled match can never be re-opened under its id.
    ///
    ///         Both agents must be currently registered: escrow then only ever
    ///         involves live identities, and since reputation persists across
    ///         deregistration the later `recordMatchResult` cannot revert for an
    ///         opponent who deregisters before settlement (a loss is undodgeable).
    function openMatch(bytes32 matchId, address agentA, address agentB, uint256 stake) external onlyAttester {
        if (agentA == agentB) revert SameAgent();
        if (!registry.isRegistered(agentA)) revert AgentNotRegistered(agentA);
        if (!registry.isRegistered(agentB)) revert AgentNotRegistered(agentB);
        Match storage m = matches[matchId];
        if (m.status != Status.None) revert MatchExists();
        m.agentA = agentA;
        m.agentB = agentB;
        m.stake = stake;
        m.status = Status.Open;
        // Freeze the self-refund deadline from the window in force at open, so a later
        // `setSettleWindow` cannot retroactively move this match's deadline. `+` is
        // checked (0.8) and bounded (block.timestamp + ≤30 days ≪ uint64 max), so it
        // can neither wrap nor be pushed to an unreachable instant.
        m.deadline = uint64(block.timestamp) + settleWindow;
        emit MatchOpened(matchId, agentA, agentB, stake);
    }

    /// @notice Fund the caller's own seat of an open wager match. Only a participant
    ///         may fund, only once, and only its OWN stake (`msg.sender`) — there is
    ///         no payer parameter, so no one funds (or is charged for) another seat.
    ///         The flag is set before the token pull (CEI), so a reentrant token can
    ///         neither double-fund nor be double-charged.
    function fund(bytes32 matchId) external {
        Match storage m = matches[matchId];
        if (m.status != Status.Open) revert MatchNotOpen();
        if (m.stake == 0) revert NoWager();
        bool isA = msg.sender == m.agentA;
        if (!isA && msg.sender != m.agentB) revert NotParticipant();
        if (isA ? m.aFunded : m.bFunded) revert AlreadyFunded();
        if (isA) m.aFunded = true;
        else m.bFunded = true;
        token.safeTransferFrom(msg.sender, address(this), m.stake);
        emit MatchFunded(matchId, msg.sender, m.stake);
    }

    /// @notice Reclaim the caller's own stake while its opponent has NOT funded — the
    ///         no-show self-rescue that takes the attester out of the refund path for
    ///         the common "opponent never showed" case. Blocked once BOTH seats are
    ///         funded (the match is live; only the attester resolves it). Clears the
    ///         funded flag before refunding (CEI), so a later `cancelMatch` cannot
    ///         double-refund the same stake and a reentrant token cannot drain a
    ///         second one. The match stays `Open` and re-fundable (the caller merely
    ///         pulled out before the opponent committed).
    function reclaim(bytes32 matchId) external {
        Match storage m = matches[matchId];
        if (m.status != Status.Open) revert MatchNotOpen();
        bool isA = msg.sender == m.agentA;
        if (!isA && msg.sender != m.agentB) revert NotParticipant();
        if (isA) {
            if (!m.aFunded) revert NotFunded();
            if (m.bFunded) revert OpponentFunded();
            m.aFunded = false;
        } else {
            if (!m.bFunded) revert NotFunded();
            if (m.aFunded) revert OpponentFunded();
            m.bFunded = false;
        }
        token.safeTransfer(msg.sender, m.stake);
        emit MatchReclaimed(matchId, msg.sender, m.stake);
    }

    /// @notice Settle an open match with a DECISIVE winner and the replay-record
    ///         commitment. Attester-gated. The winner MUST be one of the two
    ///         participants (never a third party or the attester), and a wager match
    ///         MUST have BOTH seats funded before any payout — so escrow is never
    ///         released on a partial/zero-funded match (an unfunded wager match is
    ///         resolved by `cancelMatch` instead). Flips the match to `Settled` (the
    ///         idempotency fence) before any external call, then writes reputation
    ///         (+delta winner / −delta loser) and pays the winner the `2 * stake` pot.
    function settle(bytes32 matchId, address winner, bytes32 replayHash) external onlyAttester {
        _applyDecisive(matchId, winner, replayHash, int256(reputationDelta));
    }

    /// @notice Settle a decisive match with a per-match VARIABLE reputation magnitude —
    ///         the skill-scaled Elo delta the off-chain rating curve (arena
    ///         `ranked_delta`) computed for THIS pairing, the winner's signed gain. The
    ///         winner receives `+ratingDelta`, the loser `-ratingDelta` (the contract
    ///         owns the negation, so the on-chain pair stays zero-sum from the single
    ///         delta the match service carries). Attester-gated, same escrow/winner/
    ///         replay rules and idempotency fence as `settle`.
    ///
    ///         The delta is BOUNDED `0 <= ratingDelta <= maxRatingDelta`: a decisive Elo
    ///         result never moves the winner DOWN (`K*(1-E) >= 0`), and it can be exactly
    ///         0 when a heavy favourite's expected score rounds the gain away — so a
    ///         zero-gain win still settles (pays the pot, counts the match) rather than
    ///         reverting. A negative delta (a "win" that penalizes the winner) is
    ///         rejected, and a delta over the cap is rejected — the attester scales
    ///         standing only within the owner-set ceiling. Reverts `VariableSettleDisabled`
    ///         while the cap is 0 (the variable path is off by default).
    function settleRanked(bytes32 matchId, address winner, bytes32 replayHash, int256 ratingDelta)
        external
        onlyAttester
    {
        if (maxRatingDelta == 0) revert VariableSettleDisabled();
        if (ratingDelta < 0) revert NegativeWinnerDelta();
        if (uint256(ratingDelta) > maxRatingDelta) revert RatingDeltaTooLarge();
        _applyDecisive(matchId, winner, replayHash, ratingDelta);
    }

    /// @notice Settle an open match as a DRAW: commit the replay hash, count the match
    ///         for both agents with no reputation change, and refund each its OWN
    ///         stake (never the pot to one side). A draw is a real ranked result —
    ///         distinct from `cancelMatch`, which voids a match and commits no replay —
    ///         so it must be settleable without stranding the escrow. Same both-funded
    ///         precondition and idempotency fence as `settle`.
    function settleDraw(bytes32 matchId, bytes32 replayHash) external onlyAttester {
        _applyDraw(matchId, replayHash, int256(0));
    }

    /// @notice Settle a draw with a per-match VARIABLE reputation delta — `agentA`'s
    ///         signed change (`agentB` receives its negation). Unlike `settleDraw`, which
    ///         moves no standing, a real Elo draw between UNEQUAL agents moves the
    ///         favourite DOWN toward the underdog, so `ratingDeltaA` may be negative,
    ///         zero (an even pairing), or positive (`agentA` the underdog). Bounded by
    ///         magnitude only — `|ratingDeltaA| <= maxRatingDelta`, either sign — since a
    ///         draw carries no winner. Attester-gated, same both-funded precondition,
    ///         per-seat refund, and idempotency fence as `settleDraw`; reverts
    ///         `VariableSettleDisabled` while the cap is 0.
    function settleDrawRanked(bytes32 matchId, bytes32 replayHash, int256 ratingDeltaA)
        external
        onlyAttester
    {
        if (maxRatingDelta == 0) revert VariableSettleDisabled();
        int256 cap = int256(maxRatingDelta);
        if (ratingDeltaA > cap || ratingDeltaA < -cap) revert RatingDeltaTooLarge();
        _applyDraw(matchId, replayHash, ratingDeltaA);
    }

    /// @notice Void an open match and refund every funded seat — the recovery path for
    ///         a match that cannot produce a result (both funded but unplayable, or a
    ///         no-show the participant did not self-`reclaim`). Attester-gated. Records
    ///         NO reputation and commits NO replay: a cancelled match never happened.
    ///         Flips to `Cancelled` (fence) and clears the funded flags before
    ///         refunding, so the stakes cannot be double-refunded nor the match later
    ///         settled.
    function cancelMatch(bytes32 matchId) external onlyAttester {
        Match storage m = matches[matchId];
        if (m.status != Status.Open) revert MatchNotOpen();
        (uint256 refundA, uint256 refundB) = _refundBoth(m, Status.Cancelled);
        emit MatchCancelled(matchId, refundA, refundB);
    }

    /// @notice Permissionless self-refund of an open match the attester has not resolved
    ///         by its `deadline` — the escape hatch that removes the attester-liveness
    ///         dependency for a BOTH-funded match (`reclaim` only rescues a stake before
    ///         the opponent funds). Callable by ANYONE once `block.timestamp >= deadline`
    ///         while the match is still `Open`: every refund goes to the funded
    ///         participants (never the caller), so it can only return stranded escrow to
    ///         its rightful owners — there is no griefing incentive and no need to gate
    ///         the caller. Behaves EXACTLY like `cancelMatch` — refunds funded seats,
    ///         records NO reputation, commits NO replay — so a match cannot be turned
    ///         into a result by stalling past the deadline; it only ever becomes a void.
    ///         Resolves to the distinct `Expired` state. The `Open` fence keeps it
    ///         single-shot against a late `settle` that lands first (whichever flips the
    ///         status wins; the other reverts `MatchNotOpen`).
    function refundExpired(bytes32 matchId) external {
        Match storage m = matches[matchId];
        if (m.status != Status.Open) revert MatchNotOpen();
        if (block.timestamp < m.deadline) revert NotExpired();
        (uint256 refundA, uint256 refundB) = _refundBoth(m, Status.Expired);
        emit MatchExpired(matchId, refundA, refundB);
    }

    /// @dev Shared void-and-refund mechanics for `cancelMatch` and `refundExpired`: flip
    ///      to the given terminal `status` and clear the funded flags BEFORE refunding
    ///      (checks-effects-interactions), so neither path can double-refund a stake nor
    ///      be reentered to drain a second one, and a voided match can never later
    ///      settle. Returns the per-seat refunds for the caller to event. Keeping the
    ///      two paths on ONE helper means their refund logic can never drift apart.
    function _refundBoth(Match storage m, Status terminal)
        private
        returns (uint256 refundA, uint256 refundB)
    {
        uint256 stake = m.stake;
        refundA = m.aFunded ? stake : 0;
        refundB = m.bFunded ? stake : 0;
        m.status = terminal;
        m.aFunded = false;
        m.bFunded = false;
        if (refundA != 0) token.safeTransfer(m.agentA, refundA);
        if (refundB != 0) token.safeTransfer(m.agentB, refundB);
    }

    /// @dev Shared decisive-settlement mechanics for the fixed `settle` and the variable
    ///      `settleRanked`. `deltaWinner` is the signed reputation the WINNER receives;
    ///      the loser receives its exact negation, so reputation stays zero-sum for ANY
    ///      delta the caller supplies. The caller validates the delta per its policy
    ///      (a fixed non-zero magnitude, or a bounded variable one) BEFORE this runs;
    ///      here the negation is always safe because both callers bound
    ///      `|deltaWinner| <= int256.max`, so it is never `int256.min`. Flips the match
    ///      to `Settled` (the idempotency fence) BEFORE any external call (CEI), so a
    ///      replayed or reentrant settle reverts `MatchNotOpen`. Keeping both settle
    ///      paths on ONE helper means their escrow, fence, and CEI ordering can never
    ///      drift apart — only the magnitude policy differs.
    function _applyDecisive(bytes32 matchId, address winner, bytes32 replayHash, int256 deltaWinner) private {
        Match storage m = matches[matchId];
        if (m.status != Status.Open) revert MatchNotOpen();
        if (replayHash == bytes32(0)) revert ZeroReplayHash();
        bool winnerIsA = winner == m.agentA;
        if (!winnerIsA && winner != m.agentB) revert InvalidWinner();
        uint256 stake = m.stake;
        if (stake != 0 && !(m.aFunded && m.bFunded)) revert NotFullyFunded();

        address loser = winnerIsA ? m.agentB : m.agentA;
        m.status = Status.Settled;
        m.winner = winner;
        m.replayHash = replayHash;

        registry.recordMatchResult(winner, deltaWinner);
        registry.recordMatchResult(loser, -deltaWinner);

        uint256 payout = stake * 2;
        if (payout != 0) token.safeTransfer(winner, payout);
        emit MatchSettled(matchId, winner, loser, replayHash, payout);
    }

    /// @dev Shared draw-settlement mechanics for the fixed `settleDraw` (delta 0) and the
    ///      variable `settleDrawRanked`. `deltaA` is `agentA`'s signed change; `agentB`
    ///      receives its exact negation, keeping a draw zero-sum too. Same both-funded
    ///      precondition, `Settled` fence, per-seat refund, and CEI ordering as the
    ///      decisive path. The negation is safe for the same reason: both callers bound
    ///      `|deltaA| <= int256.max`.
    function _applyDraw(bytes32 matchId, bytes32 replayHash, int256 deltaA) private {
        Match storage m = matches[matchId];
        if (m.status != Status.Open) revert MatchNotOpen();
        if (replayHash == bytes32(0)) revert ZeroReplayHash();
        uint256 stake = m.stake;
        if (stake != 0 && !(m.aFunded && m.bFunded)) revert NotFullyFunded();

        address agentA = m.agentA;
        address agentB = m.agentB;
        m.status = Status.Settled;
        m.replayHash = replayHash;

        registry.recordMatchResult(agentA, deltaA);
        registry.recordMatchResult(agentB, -deltaA);

        if (stake != 0) {
            token.safeTransfer(agentA, stake);
            token.safeTransfer(agentB, stake);
        }
        emit MatchDrawn(matchId, agentA, agentB, replayHash);
    }
}
