// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Ownable2Step, Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice On-chain identity for agents that play ranked. The auth source the
///         match service reads to authenticate a ranked agent seat and weight it
///         by reputation, and that match settlement writes results back to.
///
///         An identity is keyed by the agent's own address: you register
///         YOURSELF (`msg.sender`), never a passed-in address, so registration is
///         bound to a proven key — the transaction signature is the ownership
///         proof, and there is no victim-address parameter to impersonate. This is
///         the on-chain twin of the Agent Gateway recovering the `join_digest`
///         signer and matching it to the claimed `agent_id`: the match service
///         recovers that same signer and reads `isRegistered`/`reputationOf` here.
///
///         Reputation is keyed by address and PERSISTS across deregistration —
///         only the `registered` flag and the bond clear — so a negative standing
///         cannot be laundered by deregistering and re-registering the same key; a
///         fresh key is a new, neutral, bond-gated identity. Only an owner-
///         authorized reputation writer (the settlement contract) may move it.
contract AgentRegistry is Ownable2Step {
    using SafeERC20 for IERC20;

    IERC20 public immutable TOKEN;

    /// @notice The $BLCKFLD bond floor a registration must meet. May be 0 — a bond
    ///         is optional anti-Sybil weight, not a gate. Owner-set; a raise only
    ///         binds subsequent registrations (existing identities are grandfathered).
    uint256 public minBond;

    struct Agent {
        /// @dev Currently active. Cleared on deregister; the rest of the record
        ///      survives so the identity's history is not reset.
        bool registered;
        /// @dev Timestamp of the current active registration. Non-zero once an
        ///      address has ever registered — the "is a known identity" sentinel.
        uint64 registeredAt;
        /// @dev Count of settled ranked matches recorded against this identity. A
        ///      monotonic HUD/weighting read; never reset.
        uint64 matchesSettled;
        /// @dev $BLCKFLD locked by the active registration, refunded on deregister.
        uint256 bond;
        /// @dev Post-match reputation, signed so a loss lowers it. Moved only by
        ///      `recordMatchResult`; survives deregistration.
        int256 reputation;
        /// @dev A non-authoritative display label. The address is the identity, so
        ///      handles are not unique and are never used to authenticate.
        bytes32 handle;
    }

    mapping(address agent => Agent) public agents;

    /// @notice Addresses allowed to write match results (reputation). The match
    ///         settlement contract is authorized here; an agent can never write its
    ///         own standing.
    mapping(address writer => bool authorized) public reputationWriters;

    event Registered(address indexed agent, bytes32 handle, uint256 bond);
    event Deregistered(address indexed agent, uint256 refundedBond);
    event MatchRecorded(
        address indexed agent, int256 reputationDelta, int256 newReputation, uint64 matchesSettled
    );
    event ReputationWriterSet(address indexed writer, bool authorized);
    event MinBondSet(uint256 minBond);
    event HandleChanged(address indexed agent, bytes32 handle);

    error AlreadyRegistered();
    error NotRegistered();
    error BondTooLow();
    error NotReputationWriter();
    error ZeroToken();

    constructor(address token_, uint256 minBond_, address owner_) Ownable(owner_) {
        if (token_ == address(0)) revert ZeroToken();
        TOKEN = IERC20(token_);
        minBond = minBond_;
    }

    /// @notice Register the caller as a ranked agent identity with a display
    ///         `handle` and a `bond` (>= `minBond`). The identity is `msg.sender`,
    ///         so this can only ever register a key the caller controls. A returning
    ///         identity (previously deregistered) re-bonds and goes active again
    ///         while keeping its accrued reputation and match count — re-registering
    ///         is not a way to wipe a bad standing.
    function register(bytes32 handle, uint256 bond) external {
        Agent storage a = agents[msg.sender];
        if (a.registered) revert AlreadyRegistered();
        if (bond < minBond) revert BondTooLow();
        // Effects before the token pull; reputation/matchesSettled are deliberately
        // left untouched so a returning identity keeps its history.
        a.registered = true;
        a.registeredAt = uint64(block.timestamp);
        a.bond = bond;
        a.handle = handle;
        if (bond > 0) TOKEN.safeTransferFrom(msg.sender, address(this), bond);
        emit Registered(msg.sender, handle, bond);
    }

    /// @notice Deactivate the caller's identity and refund its bond. The reputation,
    ///         handle, and match count persist (the identity can return via
    ///         `register` with its history intact). CEI: the bond is zeroed and the
    ///         flag cleared before the refund, so a reentrant token recipient finds
    ///         nothing to deregister again.
    function deregister() external {
        Agent storage a = agents[msg.sender];
        if (!a.registered) revert NotRegistered();
        uint256 refund = a.bond;
        a.registered = false;
        a.bond = 0;
        if (refund > 0) TOKEN.safeTransfer(msg.sender, refund);
        emit Deregistered(msg.sender, refund);
    }

    /// @notice Rename the caller's display `handle` in place. A convenience for an
    ///         active identity that mistyped its label or wants to rebrand without
    ///         `deregister`/`register` — which would refund the bond, drop the
    ///         active flag, and reset `registeredAt`. Self-sovereign like `register`:
    ///         there is no address parameter, so it only ever renames `msg.sender`'s
    ///         own identity, and it is gated to a currently-registered caller (the
    ///         same gate as `deregister`). The handle is a non-authoritative, non-
    ///         unique display label the match service never authenticates on, so a
    ///         rename moves nothing it reads — it touches ONLY `handle`, leaving
    ///         reputation, bond, match count, and the registration timestamps intact
    ///         (a rename is not a path to reset a standing or re-bond).
    function setHandle(bytes32 handle) external {
        Agent storage a = agents[msg.sender];
        if (!a.registered) revert NotRegistered();
        a.handle = handle;
        emit HandleChanged(msg.sender, handle);
    }

    /// @notice Apply a ranked match's reputation delta to `agent` and count the
    ///         settlement. Restricted to an owner-authorized reputation writer (the
    ///         settlement contract) or the owner — an agent can never inflate its
    ///         own standing. The agent must be a known identity (ever registered),
    ///         so a loss cannot be dodged by deregistering before settlement, but a
    ///         never-registered address cannot be scored.
    function recordMatchResult(address agent, int256 reputationDelta) external {
        if (!reputationWriters[msg.sender] && msg.sender != owner()) revert NotReputationWriter();
        Agent storage a = agents[agent];
        if (a.registeredAt == 0) revert NotRegistered();
        int256 updated = a.reputation + reputationDelta;
        a.reputation = updated;
        a.matchesSettled += 1;
        emit MatchRecorded(agent, reputationDelta, updated, a.matchesSettled);
    }

    /// @notice Authorize (or revoke) a reputation writer — the match settlement
    ///         contract. Owner-only, the single trust root for who may move standing.
    function setReputationWriter(address writer, bool authorized) external onlyOwner {
        reputationWriters[writer] = authorized;
        emit ReputationWriterSet(writer, authorized);
    }

    function setMinBond(uint256 newMinBond) external onlyOwner {
        minBond = newMinBond;
        emit MinBondSet(newMinBond);
    }

    /// @notice Whether `agent` is a currently-active identity — the predicate the
    ///         match service gates a ranked seat on after recovering the join signer.
    function isRegistered(address agent) external view returns (bool) {
        return agents[agent].registered;
    }

    /// @notice Whether `agent` was EVER registered — the predicate for scoring an
    ///         identity that may have since deregistered. Mirrors the `registeredAt != 0`
    ///         gate `recordMatchResult` applies (a loss cannot be dodged by deregistering
    ///         before settlement), so a settle path that must tolerate a deregistered
    ///         participant checks this rather than the current `isRegistered` flag.
    function wasRegistered(address agent) external view returns (bool) {
        return agents[agent].registeredAt != 0;
    }

    /// @notice The agent's reputation, for seat weighting. Defined for any address
    ///         (0 for an unknown one), so a reader needs no separate existence check.
    function reputationOf(address agent) external view returns (int256) {
        return agents[agent].reputation;
    }
}
