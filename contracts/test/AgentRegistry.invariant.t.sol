// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {AgentRegistry} from "../src/AgentRegistry.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

/// @notice Stateful handler driving bounded register/deregister/recordMatchResult
///         against the registry while tracking, off to the side, the bonds locked
///         and the reputation deltas applied — so conservation can be checked over a
///         run. The handler is itself the authorized reputation writer, so every
///         reputation move during the run flows through it.
contract AgentRegistryHandler is Test {
    AgentRegistry public registry;
    MockToken public token;

    address[] public actors;

    mapping(address actor => bool) public registered;
    mapping(address actor => bool) public everRegistered;
    mapping(address actor => uint256) public bondOf;
    uint256 public ghost_totalBonded;
    int256 public ghost_reputationSum;

    constructor(AgentRegistry registry_, MockToken token_, address[] memory actors_) {
        registry = registry_;
        token = token_;
        actors = actors_;
        for (uint256 i = 0; i < actors.length; i++) {
            vm.prank(actors[i]);
            token.approve(address(registry), type(uint256).max);
        }
    }

    function register(uint256 actorSeed, uint256 bondSeed) external {
        address actor = actors[actorSeed % actors.length];
        if (registered[actor]) return;
        uint256 bond = bound(bondSeed, 0, token.balanceOf(actor));

        vm.prank(actor);
        registry.register(bytes32(actorSeed), bond);

        registered[actor] = true;
        everRegistered[actor] = true;
        bondOf[actor] = bond;
        ghost_totalBonded += bond;
    }

    function deregister(uint256 actorSeed) external {
        address actor = actors[actorSeed % actors.length];
        if (!registered[actor]) return;

        vm.prank(actor);
        registry.deregister();

        ghost_totalBonded -= bondOf[actor];
        registered[actor] = false;
        bondOf[actor] = 0;
    }

    function recordMatch(uint256 agentSeed, int256 delta) external {
        address actor = actors[agentSeed % actors.length];
        if (!everRegistered[actor]) return; // would revert NotRegistered
        delta = bound(delta, -1e24, 1e24);

        // The handler is the authorized writer, so this is msg.sender == writer.
        registry.recordMatchResult(actor, delta);
        ghost_reputationSum += delta;
    }

    /// @dev The registry's own books: Σ bonds over every actor (a deregistered or
    ///      never-registered actor reads 0). Reconciled against the real balance.
    function internalBondSum() external view returns (uint256 sum) {
        for (uint256 i = 0; i < actors.length; i++) {
            (,,, uint256 bond,,) = registry.agents(actors[i]);
            sum += bond;
        }
    }

    function reputationSum() external view returns (int256 sum) {
        for (uint256 i = 0; i < actors.length; i++) {
            sum += registry.reputationOf(actors[i]);
        }
    }
}

contract AgentRegistryInvariantTest is Test {
    AgentRegistry registry;
    MockToken token;
    AgentRegistryHandler handler;

    address owner = address(0xA11CE);
    address[] actors;

    function setUp() public {
        token = new MockToken();
        registry = new AgentRegistry(address(token), 0, owner);

        actors = new address[](3);
        actors[0] = address(0xA1);
        actors[1] = address(0xB0B);
        actors[2] = address(0xCA201);

        handler = new AgentRegistryHandler(registry, token, actors);
        for (uint256 i = 0; i < actors.length; i++) {
            assertTrue(token.transfer(actors[i], 10_000 ether));
        }
        // The handler is the one authorized reputation writer, so every reputation
        // move during the run is accounted for by its ghost.
        vm.prank(owner);
        registry.setReputationWriter(address(handler), true);

        targetContract(address(handler));
    }

    /// @dev Token conservation against an independent ghost: the registry holds
    ///      exactly the bonds of currently-registered agents — a deregister refunds
    ///      its bond in full, and reputation moves lock no tokens.
    function invariant_balanceEqualsBondedGhost() public view {
        assertEq(token.balanceOf(address(registry)), handler.ghost_totalBonded());
    }

    /// @dev The registry's internal bond books reconcile to its real balance,
    ///      independent of the ghost.
    function invariant_balanceReconcilesInternalBonds() public view {
        assertEq(token.balanceOf(address(registry)), handler.internalBondSum());
    }

    /// @dev Reputation only ever moves through `recordMatchResult`: the sum of every
    ///      agent's standing equals the sum of the deltas applied — register and
    ///      deregister never touch it (the anti-laundering property in aggregate).
    function invariant_reputationEqualsAppliedDeltas() public view {
        assertEq(handler.reputationSum(), handler.ghost_reputationSum());
    }
}

contract AgentRegistryFuzzTest is Test {
    AgentRegistry registry;
    MockToken token;

    address owner = address(0xA11CE);
    address alice = address(0xA1);

    bytes32 constant HANDLE = bytes32("alice-bot");

    function setUp() public {
        token = new MockToken();
        registry = new AgentRegistry(address(token), 0, owner);
        assertTrue(token.transfer(alice, 10_000 ether));
        vm.prank(alice);
        token.approve(address(registry), type(uint256).max);
    }

    function testFuzz_register_thenDeregister_returnsExactBond(uint256 bond) public {
        bond = bound(bond, 0, token.balanceOf(alice));
        uint256 before = token.balanceOf(alice);

        vm.prank(alice);
        registry.register(HANDLE, bond);
        assertEq(token.balanceOf(address(registry)), bond);

        vm.prank(alice);
        registry.deregister();
        assertEq(token.balanceOf(alice), before, "bond fully restored");
        assertEq(token.balanceOf(address(registry)), 0);
    }

    function testFuzz_recordMatchResult_sumsDeltas(int256 a, int256 b) public {
        a = bound(a, -1e30, 1e30);
        b = bound(b, -1e30, 1e30);

        vm.prank(alice);
        registry.register(HANDLE, 0);
        vm.startPrank(owner);
        registry.recordMatchResult(alice, a);
        registry.recordMatchResult(alice, b);
        vm.stopPrank();

        assertEq(registry.reputationOf(alice), a + b);
    }
}
