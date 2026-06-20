// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {Deploy} from "../script/Deploy.s.sol";
import {IEAS, ISchemaRegistry} from "../src/RenderReceipts.sol";
import {MatchSettlement} from "../src/MatchSettlement.sol";
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

    function revoke(IEAS.RevocationRequest calldata) external payable {
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
    address attester = address(0xA77E57E5);

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
            renderFeeRate: 1e12,
            artifactBaseUri: "https://artifacts.test/{id}.json",
            artifactMintFeeRate: 1 ether,
            artifactRoyaltyRate: 1 ether,
            agentMinBond: 0,
            matchAttester: attester,
            matchReputationDelta: 10 ether
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
        deployed.agentRegistry.acceptOwnership();
        deployed.matchSettlement.acceptOwnership();
        vm.stopPrank();
    }

    function test_allSixContractsDeployed() public view {
        assertGt(address(deployed.computeMeter).code.length, 0, "ComputeMeter no code");
        assertGt(address(deployed.renderReceipts).code.length, 0, "RenderReceipts no code");
        assertGt(address(deployed.regionAuthority).code.length, 0, "RegionAuthority no code");
        assertGt(address(deployed.artifactTemplate).code.length, 0, "ArtifactTemplate no code");
        assertGt(address(deployed.agentRegistry).code.length, 0, "AgentRegistry no code");
        assertGt(address(deployed.matchSettlement).code.length, 0, "MatchSettlement no code");
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

    function test_artifactTemplateWiredForRegionRoyalty() public view {
        // The region royalty deposits into the RegionAuthority deployed alongside, and
        // pays in the token RegionAuthority pulls — ArtifactTemplate derives the royalty
        // token from RegionAuthority.TOKEN() so the two can never desync.
        assertEq(
            address(deployed.artifactTemplate.regionAuthority()),
            address(deployed.regionAuthority),
            "artifact template region authority not wired"
        );
        assertEq(
            address(deployed.artifactTemplate.royaltyToken()),
            address(token),
            "royalty token not bound to RegionAuthority.TOKEN()"
        );
        assertEq(deployed.artifactTemplate.royaltyRate(), 1 ether, "artifact royalty rate not wired");
    }

    function test_renderReceiptsWiredForRegionFee() public view {
        // RenderReceipts routes a per-receipt fee-share into a region on the RegionAuthority
        // deployed alongside, paying in the token RegionAuthority pulls — it derives the
        // fee token from RegionAuthority.TOKEN() so the two can never desync.
        assertEq(
            address(deployed.renderReceipts.regionAuthority()),
            address(deployed.regionAuthority),
            "render receipts region authority not wired"
        );
        assertEq(
            address(deployed.renderReceipts.feeToken()),
            address(token),
            "render fee token not bound to RegionAuthority.TOKEN()"
        );
        assertEq(deployed.renderReceipts.renderFeeRate(), 1e12, "render fee rate not wired");
    }

    function test_agentRegistryAndSettlementConstructorArgs() public view {
        // Both bond in / escrow the same $BLCKFLD the rest of the stack uses; the
        // settlement binds its token from the registry so the two can never desync.
        assertEq(address(deployed.agentRegistry.TOKEN()), address(token), "registry token");
        assertEq(deployed.agentRegistry.minBond(), 0, "registry min bond");
        assertEq(
            address(deployed.matchSettlement.registry()),
            address(deployed.agentRegistry),
            "settlement registry not the deployed registry"
        );
        assertEq(address(deployed.matchSettlement.token()), address(token), "settlement token");
        assertEq(deployed.matchSettlement.reputationDelta(), 10 ether, "reputation delta not wired");
    }

    function test_settlementAuthorizedAsReputationWriter() public view {
        // FM1: without this, every on-chain settle reverts NotReputationWriter and the
        // escrow loop is bricked at runtime. The settlement is the SOLE writer wired —
        // no stray EOA (deployer/owner/attester) may move standing.
        assertTrue(
            deployed.agentRegistry.reputationWriters(address(deployed.matchSettlement)),
            "settlement not an authorized reputation writer"
        );
        assertFalse(
            deployed.agentRegistry.reputationWriters(address(script)), "deployer is a reputation writer"
        );
        assertFalse(deployed.agentRegistry.reputationWriters(owner), "owner is a reputation writer");
        assertFalse(deployed.agentRegistry.reputationWriters(attester), "attester is a reputation writer");
    }

    function test_attesterAuthorizedAndNonAttesterReverts() public {
        // FM3: the configured match-service key is authorized; no stray EOA retains
        // attester rights, and a non-attester cannot open (or settle) a match.
        assertTrue(deployed.matchSettlement.resultAttesters(attester), "attester not authorized");
        assertFalse(
            deployed.matchSettlement.resultAttesters(address(script)), "deployer retains attester rights"
        );
        assertFalse(deployed.matchSettlement.resultAttesters(owner), "owner retains attester rights");

        vm.prank(address(0xBAD));
        vm.expectRevert(MatchSettlement.NotAttester.selector);
        deployed.matchSettlement.openMatch(bytes32(uint256(1)), address(0x1), address(0x2), 0);
    }

    function test_wiringPrecedesOwnershipTransfer() public view {
        // FM2: the `deployed` fixture handed ownership to `owner` (!= deployer) and the
        // owner accepted. The owner-gated wiring landing ANYWAY proves it ran while the
        // deployer still owned — had transfer come first, deploy() would have reverted.
        assertEq(deployed.agentRegistry.owner(), owner, "registry not owned by configured owner");
        assertEq(deployed.matchSettlement.owner(), owner, "settlement not owned by configured owner");
        assertTrue(
            deployed.agentRegistry.reputationWriters(address(deployed.matchSettlement)),
            "writer wiring lost across transfer"
        );
        assertTrue(deployed.matchSettlement.resultAttesters(attester), "attester wiring lost across transfer");
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
            renderFeeRate: 1e12,
            artifactBaseUri: "ipfs://{id}",
            artifactMintFeeRate: 1 ether,
            artifactRoyaltyRate: 1 ether,
            agentMinBond: 0,
            matchAttester: attester,
            matchReputationDelta: 10 ether
        });
        Deploy.Deployed memory fresh = script.deploy(cfg, address(script));

        // Pending, not yet owner: the deployer (the script) still owns.
        assertEq(fresh.computeMeter.pendingOwner(), owner, "meter pending");
        assertEq(fresh.renderReceipts.pendingOwner(), owner, "receipts pending");
        assertEq(fresh.regionAuthority.pendingOwner(), owner, "region pending");
        assertEq(fresh.artifactTemplate.pendingOwner(), owner, "artifact pending");
        assertEq(fresh.agentRegistry.pendingOwner(), owner, "registry pending");
        assertEq(fresh.matchSettlement.pendingOwner(), owner, "settlement pending");
        assertEq(fresh.computeMeter.owner(), address(script), "meter still deployer");
        assertEq(fresh.matchSettlement.owner(), address(script), "settlement still deployer");

        vm.startPrank(owner);
        fresh.computeMeter.acceptOwnership();
        fresh.renderReceipts.acceptOwnership();
        fresh.regionAuthority.acceptOwnership();
        fresh.artifactTemplate.acceptOwnership();
        fresh.agentRegistry.acceptOwnership();
        fresh.matchSettlement.acceptOwnership();
        vm.stopPrank();

        assertEq(fresh.computeMeter.owner(), owner, "meter owner after accept");
        assertEq(fresh.renderReceipts.owner(), owner, "receipts owner after accept");
        assertEq(fresh.agentRegistry.owner(), owner, "registry owner after accept");
        assertEq(fresh.matchSettlement.owner(), owner, "settlement owner after accept");
    }

    function test_ownersSetCorrectly() public view {
        assertEq(deployed.computeMeter.owner(), owner, "meter owner");
        assertEq(deployed.renderReceipts.owner(), owner, "receipts owner");
        assertEq(deployed.regionAuthority.owner(), owner, "region owner");
        assertEq(deployed.artifactTemplate.owner(), owner, "artifact owner");
        assertEq(deployed.agentRegistry.owner(), owner, "registry owner");
        assertEq(deployed.matchSettlement.owner(), owner, "settlement owner");
    }
}
