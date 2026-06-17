// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {Deploy} from "../script/Deploy.s.sol";
import {IEAS, ISchemaRegistry} from "../src/RenderReceipts.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// @dev Minimal EAS mock — only `attest` is reachable in the deploy path.
contract MockEAS is IEAS {
    function attest(IEAS.AttestationRequest calldata request) external payable returns (bytes32) {
        return keccak256(abi.encode(request.schema, request.data.recipient, request.data.data));
    }

    function multiAttest(IEAS.MultiAttestationRequest[] calldata)
        external
        payable
        returns (bytes32[] memory)
    {
        revert("not implemented");
    }
}

contract MockSchemaRegistry is ISchemaRegistry {
    bytes32 public constant FIXED_UID = keccak256("blackfield.render.schema.v1");

    function register(string calldata, address, bool) external pure returns (bytes32) {
        return FIXED_UID;
    }
}

contract MockToken is ERC20 {
    constructor() ERC20("Mock", "MCK") {
        _mint(msg.sender, 1_000_000 ether);
    }
}

/// @notice Simulation test: runs the deploy logic against mocks with no RPC and no
///         broadcast, then asserts every contract deployed and the wiring landed.
contract DeployTest is Test {
    Deploy script;
    MockEAS eas;
    MockSchemaRegistry registry;
    MockToken token;

    address owner = address(0xA11CE);
    address coordinator = address(0xC0DE);

    Deploy.Deployed deployed;

    function setUp() public {
        script = new Deploy();
        eas = new MockEAS();
        registry = new MockSchemaRegistry();
        token = new MockToken();

        Deploy.DeployConfig memory cfg = Deploy.DeployConfig({
            token: address(token),
            eas: address(eas),
            schemaRegistry: address(registry),
            owner: owner,
            coordinator: coordinator,
            stakeRequired: 50_000 ether,
            artifactBaseUri: "https://artifacts.test/{id}.json",
            artifactMintFeeRate: 1 ether
        });

        // In the test path the script contract itself sends the CREATE + wiring calls,
        // so it is the deployer/initial owner (run() passes msg.sender instead).
        deployed = script.deploy(cfg, address(script));

        // Ownership is handed off two-step; the configured owner accepts on each.
        // (The pending-then-accepted handoff is asserted in test_ownershipHandoffIsTwoStep.)
        vm.startPrank(owner);
        deployed.computeMeter.acceptOwnership();
        deployed.renderReceipts.acceptOwnership();
        deployed.regionAuthority.acceptOwnership();
        deployed.artifactTemplate.acceptOwnership();
        vm.stopPrank();
    }

    function test_allFourContractsDeployed() public view {
        assertGt(address(deployed.computeMeter).code.length, 0, "ComputeMeter no code");
        assertGt(address(deployed.renderReceipts).code.length, 0, "RenderReceipts no code");
        assertGt(address(deployed.regionAuthority).code.length, 0, "RegionAuthority no code");
        assertGt(address(deployed.artifactTemplate).code.length, 0, "ArtifactTemplate no code");
    }

    function test_constructorArgsWired() public view {
        assertEq(address(deployed.computeMeter.TOKEN()), address(token), "meter token");
        assertEq(address(deployed.renderReceipts.EAS()), address(eas), "receipts eas");
        assertEq(address(deployed.regionAuthority.TOKEN()), address(token), "region token");
        assertEq(deployed.regionAuthority.stakeRequired(), 50_000 ether, "stake required");
    }

    function test_schemaRegistered() public view {
        assertEq(deployed.renderReceipts.schemaUid(), registry.FIXED_UID(), "schema uid");
    }

    function test_coordinatorAuthorized() public view {
        assertTrue(
            deployed.renderReceipts.authorizedCoordinators(coordinator),
            "coordinator not authorized for receipts"
        );
        assertTrue(deployed.computeMeter.authorizedSpenders(coordinator), "coordinator not spender");
        assertEq(deployed.artifactTemplate.minter(), coordinator, "coordinator not minter");
    }

    function test_artifactTemplateWiredForMintFee() public view {
        // The mint-fee debit (ArtifactTemplate.spend -> ComputeMeter) only clears if
        // ArtifactTemplate is itself an authorized spender; without this wiring every
        // fee-charging mint would revert NotAuthorized.
        assertTrue(
            deployed.computeMeter.authorizedSpenders(address(deployed.artifactTemplate)),
            "artifact template not an authorized spender"
        );
        assertEq(
            address(deployed.artifactTemplate.computeMeter()),
            address(deployed.computeMeter),
            "artifact template meter not wired"
        );
        assertEq(deployed.artifactTemplate.mintFeeRate(), 1 ether, "artifact mint fee rate not wired");
    }

    function test_ownershipHandoffIsTwoStep() public {
        // Fresh deploy so we observe the pre-acceptance pending state.
        Deploy.DeployConfig memory cfg = Deploy.DeployConfig({
            token: address(token),
            eas: address(eas),
            schemaRegistry: address(registry),
            owner: owner,
            coordinator: coordinator,
            stakeRequired: 1 ether,
            artifactBaseUri: "ipfs://{id}",
            artifactMintFeeRate: 1 ether
        });
        Deploy.Deployed memory fresh = script.deploy(cfg, address(script));

        // Pending, not yet owner: the deployer (the script) still owns.
        assertEq(fresh.computeMeter.pendingOwner(), owner, "meter pending");
        assertEq(fresh.renderReceipts.pendingOwner(), owner, "receipts pending");
        assertEq(fresh.regionAuthority.pendingOwner(), owner, "region pending");
        assertEq(fresh.artifactTemplate.pendingOwner(), owner, "artifact pending");
        assertEq(fresh.computeMeter.owner(), address(script), "meter still deployer");

        vm.startPrank(owner);
        fresh.computeMeter.acceptOwnership();
        fresh.renderReceipts.acceptOwnership();
        fresh.regionAuthority.acceptOwnership();
        fresh.artifactTemplate.acceptOwnership();
        vm.stopPrank();

        assertEq(fresh.computeMeter.owner(), owner, "meter owner after accept");
        assertEq(fresh.renderReceipts.owner(), owner, "receipts owner after accept");
    }

    function test_ownersSetCorrectly() public view {
        assertEq(deployed.computeMeter.owner(), owner, "meter owner");
        assertEq(deployed.renderReceipts.owner(), owner, "receipts owner");
        assertEq(deployed.regionAuthority.owner(), owner, "region owner");
        assertEq(deployed.artifactTemplate.owner(), owner, "artifact owner");
    }
}
