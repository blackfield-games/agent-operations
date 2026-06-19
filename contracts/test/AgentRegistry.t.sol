// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {AgentRegistry} from "../src/AgentRegistry.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable2Step.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

contract AgentRegistryTest is Test {
    AgentRegistry registry;
    MockToken token;

    address owner = address(0xA11CE);
    address alice = address(0xA1);
    address bob = address(0xB0B);
    address settlement = address(0x5E771E);

    uint256 constant BOND = 100 ether;
    bytes32 constant HANDLE = bytes32("alice-bot");

    event Registered(address indexed agent, bytes32 handle, uint256 bond);
    event Deregistered(address indexed agent, uint256 refundedBond);
    event MatchRecorded(
        address indexed agent, int256 reputationDelta, int256 newReputation, uint64 matchesSettled
    );
    event ReputationWriterSet(address indexed writer, bool authorized);
    event MinBondSet(uint256 minBond);

    function setUp() public {
        token = new MockToken();
        registry = new AgentRegistry(address(token), 0, owner);

        assertTrue(token.transfer(alice, 10_000 ether));
        assertTrue(token.transfer(bob, 10_000 ether));
        vm.prank(alice);
        token.approve(address(registry), type(uint256).max);
        vm.prank(bob);
        token.approve(address(registry), type(uint256).max);
    }

    // --- construction ---

    function test_constructor() public view {
        assertEq(address(registry.TOKEN()), address(token));
        assertEq(registry.minBond(), 0);
        assertEq(registry.owner(), owner);
    }

    function test_constructor_acceptsNonZeroMinBond() public {
        AgentRegistry r = new AgentRegistry(address(token), BOND, owner);
        assertEq(r.minBond(), BOND);
    }

    // --- registration (FM1: bound to the caller's key) ---

    function test_register_bindsToSenderAndPullsBond() public {
        vm.expectEmit(true, false, false, true);
        emit Registered(alice, HANDLE, BOND);

        vm.prank(alice);
        registry.register(HANDLE, BOND);

        (
            bool registered,
            uint64 registeredAt,
            uint64 matchesSettled,
            uint256 bond,
            int256 reputation,
            bytes32 handle
        ) = registry.agents(alice);
        assertTrue(registered);
        assertEq(registeredAt, uint64(block.timestamp));
        assertEq(matchesSettled, 0);
        assertEq(bond, BOND);
        assertEq(reputation, 0);
        assertEq(handle, HANDLE);
        assertEq(token.balanceOf(address(registry)), BOND);
        assertEq(token.balanceOf(alice), 10_000 ether - BOND);
    }

    function test_register_allowsZeroBondWhenMinBondZero() public {
        vm.prank(alice);
        registry.register(HANDLE, 0);
        assertTrue(registry.isRegistered(alice));
        assertEq(token.balanceOf(address(registry)), 0, "no tokens pulled for a zero bond");
    }

    /// @dev FM1: there is NO address parameter — an identity is always `msg.sender`,
    ///      so a caller can only ever create the identity for the key it signs with.
    ///      bob registering produces bob's identity, never alice's; neither can forge
    ///      the other's. This is what makes a recovered join_digest signer trustworthy.
    function test_register_isPerSenderAndCannotImpersonate() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);
        vm.prank(bob);
        registry.register(bytes32("bob-bot"), BOND);

        assertTrue(registry.isRegistered(alice));
        assertTrue(registry.isRegistered(bob));
        (,,,,, bytes32 aliceHandle) = registry.agents(alice);
        (,,,,, bytes32 bobHandle) = registry.agents(bob);
        assertEq(aliceHandle, HANDLE);
        assertEq(bobHandle, bytes32("bob-bot"), "bob's registration never overwrote alice's identity");
    }

    function test_register_revertsAlreadyRegistered() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);

        vm.expectRevert(AgentRegistry.AlreadyRegistered.selector);
        vm.prank(alice);
        registry.register(HANDLE, BOND);
    }

    function test_register_revertsBondTooLow() public {
        vm.prank(owner);
        registry.setMinBond(BOND);

        vm.expectRevert(AgentRegistry.BondTooLow.selector);
        vm.prank(alice);
        registry.register(HANDLE, BOND - 1);
        assertFalse(registry.isRegistered(alice));
    }

    function test_register_revertsWhenBondNotApproved() public {
        address carol = address(0xCA201);
        assertTrue(token.transfer(carol, BOND));
        // no approval granted
        vm.expectRevert();
        vm.prank(carol);
        registry.register(HANDLE, BOND);
    }

    // --- deregistration + bond refund ---

    function test_deregister_refundsBondAndDeactivates() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);

        uint256 before = token.balanceOf(alice);
        vm.expectEmit(true, false, false, true);
        emit Deregistered(alice, BOND);
        vm.prank(alice);
        registry.deregister();

        assertEq(token.balanceOf(alice), before + BOND);
        assertEq(token.balanceOf(address(registry)), 0);
        assertFalse(registry.isRegistered(alice));
        (, uint64 registeredAt,, uint256 bond,,) = registry.agents(alice);
        assertEq(bond, 0);
        assertTrue(registeredAt != 0, "the identity record survives deregistration");
    }

    function test_deregister_revertsNotRegistered() public {
        vm.expectRevert(AgentRegistry.NotRegistered.selector);
        vm.prank(alice);
        registry.deregister();
    }

    /// @dev Anti-laundering: an agent cannot wipe a bad standing by deregistering and
    ///      re-registering. Reputation and match count are keyed by address and
    ///      survive; only the active flag and bond cycle.
    function test_reregister_keepsReputationHistory() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);
        vm.prank(owner);
        registry.recordMatchResult(alice, -50 ether);

        vm.prank(alice);
        registry.deregister();
        assertEq(registry.reputationOf(alice), -50 ether, "reputation persists while deregistered");

        vm.prank(alice);
        registry.register(bytes32("alice-v2"), BOND);
        assertEq(registry.reputationOf(alice), -50 ether, "re-registering does not reset the standing");
        (,, uint64 matchesSettled,,,) = registry.agents(alice);
        assertEq(matchesSettled, 1, "the match count survived too");
    }

    /// @dev CEI reentrancy proof. The agent reenters deregister() from the bond
    ///      refund hook; it IS the registrant, so the only thing stopping a second
    ///      refund is that deregister clears `registered`/`bond` BEFORE the transfer.
    ///      A second honest agent's bond sits in the pool, so a double-refund would
    ///      be fundable — this test fails if the state update moved after the refund.
    function test_deregister_reentrantCannotDoubleRefund() public {
        HookToken evil = new HookToken();
        AgentRegistry r = new AgentRegistry(address(evil), 0, owner);
        ReentrantAgent attacker = new ReentrantAgent(r, evil);

        evil.transfer(alice, BOND);
        vm.prank(alice);
        evil.approve(address(r), type(uint256).max);
        vm.prank(alice);
        r.register(HANDLE, BOND);

        evil.transfer(address(attacker), BOND);
        attacker.register(bytes32("evil"), BOND);
        assertEq(evil.balanceOf(address(r)), 2 * BOND);

        evil.arm();
        attacker.deregister();

        assertTrue(attacker.reentered(), "reentry fired");
        assertTrue(attacker.reentryReverted(), "reentry blocked by CEI");
        assertEq(evil.balanceOf(address(attacker)), BOND, "recovered exactly its own bond");
        assertEq(evil.balanceOf(address(r)), BOND, "honest agent's bond intact");
    }

    // --- reputation (FM2: only an authorized writer may move it) ---

    function test_recordMatchResult_byAuthorizedWriter() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);
        vm.prank(owner);
        registry.setReputationWriter(settlement, true);

        vm.expectEmit(true, false, false, true);
        emit MatchRecorded(alice, 30 ether, 30 ether, 1);
        vm.prank(settlement);
        registry.recordMatchResult(alice, 30 ether);

        assertEq(registry.reputationOf(alice), 30 ether);
        (,, uint64 matchesSettled,,,) = registry.agents(alice);
        assertEq(matchesSettled, 1);
    }

    function test_recordMatchResult_ownerMayWriteDirectly() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);
        vm.prank(owner);
        registry.recordMatchResult(alice, 10 ether);
        assertEq(registry.reputationOf(alice), 10 ether);
    }

    /// @dev FM2: the agent cannot inflate its own standing — it is not a writer.
    function test_recordMatchResult_revertsForSelfInflation() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);

        vm.expectRevert(AgentRegistry.NotReputationWriter.selector);
        vm.prank(alice);
        registry.recordMatchResult(alice, 1_000_000 ether);
        assertEq(registry.reputationOf(alice), 0, "self-write changed nothing");
    }

    function test_recordMatchResult_revertsForUnauthorizedThirdParty() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);

        vm.expectRevert(AgentRegistry.NotReputationWriter.selector);
        vm.prank(bob);
        registry.recordMatchResult(alice, 5 ether);
    }

    function test_recordMatchResult_revertsAfterWriterRevoked() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);
        vm.prank(owner);
        registry.setReputationWriter(settlement, true);
        vm.prank(owner);
        registry.setReputationWriter(settlement, false);

        vm.expectRevert(AgentRegistry.NotReputationWriter.selector);
        vm.prank(settlement);
        registry.recordMatchResult(alice, 5 ether);
    }

    function test_recordMatchResult_accumulatesAndGoesNegative() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);
        vm.startPrank(owner);
        registry.recordMatchResult(alice, 40 ether);
        registry.recordMatchResult(alice, -100 ether);
        vm.stopPrank();

        assertEq(registry.reputationOf(alice), -60 ether, "deltas sum and reputation may go negative");
        (,, uint64 matchesSettled,,,) = registry.agents(alice);
        assertEq(matchesSettled, 2);
    }

    function test_recordMatchResult_revertsForNeverRegistered() public {
        vm.expectRevert(AgentRegistry.NotRegistered.selector);
        vm.prank(owner);
        registry.recordMatchResult(alice, 10 ether);
    }

    /// @dev A loss cannot be dodged by deregistering before settlement: a known
    ///      identity (ever registered) is still scorable while deactivated.
    function test_recordMatchResult_scoresADeregisteredIdentity() public {
        vm.prank(alice);
        registry.register(HANDLE, BOND);
        vm.prank(alice);
        registry.deregister();

        vm.prank(owner);
        registry.recordMatchResult(alice, -25 ether);
        assertEq(registry.reputationOf(alice), -25 ether);
    }

    // --- admin: writers + minBond ---

    function test_setReputationWriter_emitsAndOnlyOwner() public {
        vm.expectEmit(true, false, false, true);
        emit ReputationWriterSet(settlement, true);
        vm.prank(owner);
        registry.setReputationWriter(settlement, true);
        assertTrue(registry.reputationWriters(settlement));

        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, alice));
        vm.prank(alice);
        registry.setReputationWriter(bob, true);
    }

    function test_setMinBond_emitsAndOnlyOwner() public {
        vm.expectEmit(false, false, false, true);
        emit MinBondSet(BOND);
        vm.prank(owner);
        registry.setMinBond(BOND);
        assertEq(registry.minBond(), BOND);

        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, alice));
        vm.prank(alice);
        registry.setMinBond(1);
    }

    function test_setMinBond_grandfathersExistingButGatesNew() public {
        vm.prank(alice);
        registry.register(HANDLE, 0); // registered under minBond 0

        vm.prank(owner);
        registry.setMinBond(BOND);

        // alice keeps her zero-bond registration (grandfathered)…
        assertTrue(registry.isRegistered(alice));
        // …but bob must now meet the new floor.
        vm.expectRevert(AgentRegistry.BondTooLow.selector);
        vm.prank(bob);
        registry.register(bytes32("bob"), BOND - 1);
        vm.prank(bob);
        registry.register(bytes32("bob"), BOND);
        assertTrue(registry.isRegistered(bob));
    }

    // --- views ---

    function test_isRegistered_tracksLifecycle() public {
        assertFalse(registry.isRegistered(alice));
        vm.prank(alice);
        registry.register(HANDLE, BOND);
        assertTrue(registry.isRegistered(alice));
        vm.prank(alice);
        registry.deregister();
        assertFalse(registry.isRegistered(alice));
    }

    function test_reputationOf_zeroForUnknownAddress() public view {
        assertEq(registry.reputationOf(address(0xDEAD)), 0);
    }

    // --- ownership ---

    function test_ownership_twoStepTransfer() public {
        vm.prank(owner);
        registry.transferOwnership(bob);
        assertEq(registry.pendingOwner(), bob);
        assertEq(registry.owner(), owner);

        vm.prank(bob);
        registry.acceptOwnership();
        assertEq(registry.owner(), bob);

        vm.prank(bob);
        registry.setMinBond(5);
        assertEq(registry.minBond(), 5);
    }
}

/// @dev Shared payout callback so HookToken can drive a reentrancy attacker.
interface IPayoutReceiver {
    function onPayout() external;
}

/// @notice ERC-20 with an armed recipient hook (ERC777-style) that hands control to
///         the recipient mid-payout so it can reenter. Disarmed by default so
///         funding/bond transfers are normal.
contract HookToken is ERC20 {
    bool armed;

    constructor() ERC20("Hook", "HOOK") {
        _mint(msg.sender, 1_000_000 ether);
    }

    function arm() external {
        armed = true;
    }

    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        if (armed && to != address(0) && to.code.length > 0) {
            armed = false; // one-shot, so the reentrant payout doesn't recurse
            IPayoutReceiver(to).onPayout();
        }
    }
}

/// @notice Registered agent that reenters deregister() once when it receives its bond
///         refund. It IS the registrant, so the reentry clears the NotRegistered gate
///         only if state isn't cleared first — CEI (clear before refund) blocks it.
contract ReentrantAgent is IPayoutReceiver {
    AgentRegistry registry;
    HookToken token;
    bool public reentered;
    bool public reentryReverted;

    constructor(AgentRegistry registry_, HookToken token_) {
        registry = registry_;
        token = token_;
    }

    function register(bytes32 handle, uint256 bond) external {
        token.approve(address(registry), type(uint256).max);
        registry.register(handle, bond);
    }

    function deregister() external {
        registry.deregister();
    }

    function onPayout() external {
        reentered = true;
        try registry.deregister() {}
        catch {
            reentryReverted = true;
        }
    }
}
