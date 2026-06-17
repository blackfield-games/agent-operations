// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Script, console2} from "forge-std/Script.sol";
import {ComputeMeter} from "../src/ComputeMeter.sol";
import {RenderReceipts} from "../src/RenderReceipts.sol";
import {RegionAuthority} from "../src/RegionAuthority.sol";
import {ArtifactTemplate} from "../src/ArtifactTemplate.sol";

/// @notice Production-shaped deploy for the four Blackfield contracts.
///
///         The on-chain logic lives in `deploy(...)` so it can be exercised from
///         tests with mocks (no RPC, no broadcast). `run()` only resolves config
///         from the environment and wraps `deploy(...)` in a broadcast.
contract Deploy is Script {
    /// @dev All deploy-time inputs, resolved from .env in `run()`.
    struct DeployConfig {
        address token; // $BLCKFLD — credited/burned by ComputeMeter, staked in RegionAuthority
        address eas; // canonical EAS attestation contract
        address schemaRegistry; // canonical EAS schema registry
        address owner; // Ownable owner across all four contracts
        address coordinator; // authorized render-job coordinator + minter
        uint256 stakeRequired; // $BLCKFLD required to claim a region
        string artifactBaseUri; // ERC-1155 base URI for artifact templates
        uint256 artifactMintFeeRate; // ArtifactTemplate per-unit full-rarity mint fee
    }

    /// @dev Deployed contract handles, returned to callers (tests + run()).
    struct Deployed {
        ComputeMeter computeMeter;
        RenderReceipts renderReceipts;
        RegionAuthority regionAuthority;
        ArtifactTemplate artifactTemplate;
    }

    function run() external returns (Deployed memory deployed) {
        DeployConfig memory cfg = _configFromEnv();

        // Under broadcast, every CREATE and owner-gated call is sent from the EOA, so
        // the EOA (msg.sender) — not the ephemeral script contract — is the deployer.
        vm.startBroadcast();
        deployed = deploy(cfg, msg.sender);
        vm.stopBroadcast();

        _log(cfg, deployed);
    }

    /// @notice Deploys all four contracts and performs post-deploy wiring.
    /// @dev Pure on-chain logic — no broadcast, no env. Callable from tests.
    ///
    ///      `deployer` is whoever actually sends the CREATE + wiring calls: the
    ///      broadcasting EOA (`msg.sender`) under `run()`, or the calling script
    ///      contract in tests. It is set as the initial owner so it can perform the
    ///      owner-gated wiring, then ownership is transferred to `cfg.owner`.
    ///      ComputeMeter / RenderReceipts / RegionAuthority are Ownable2Step, so
    ///      `cfg.owner` must call `acceptOwnership()` on each to finalize.
    ///      ArtifactTemplate transfer (also Ownable2Step) is identical.
    function deploy(DeployConfig memory cfg, address deployer) public returns (Deployed memory deployed) {
        // 1. Compute budget meter — burns $BLCKFLD, credits buyers.
        ComputeMeter computeMeter = new ComputeMeter(cfg.token, deployer);

        // 2. Render receipts — EAS attestations for validated render-jobs.
        RenderReceipts renderReceipts = new RenderReceipts(cfg.eas, deployer);

        // 3. Region authority — staked ERC-721 over world regions.
        RegionAuthority regionAuthority = new RegionAuthority(cfg.token, cfg.stakeRequired, deployer);

        // 4. Artifact templates — ERC-1155 player-authored artifacts; mints debit the
        //    rarity-scaled fee from the recipient's credit on this ComputeMeter.
        ArtifactTemplate artifactTemplate = new ArtifactTemplate(
            deployer, cfg.artifactBaseUri, address(computeMeter), cfg.artifactMintFeeRate
        );

        // --- post-deploy wiring (deployer is owner) ---

        // Register the canonical EAS schema and authorize the coordinator to issue receipts.
        renderReceipts.registerSchema(cfg.schemaRegistry);
        renderReceipts.setCoordinator(cfg.coordinator, true);

        // Coordinator is the authorized compute spender + artifact minter.
        computeMeter.setSpender(cfg.coordinator, true);
        artifactTemplate.setMinter(cfg.coordinator);

        // ArtifactTemplate debits the mint fee directly via ComputeMeter.spend, so it
        // must itself be an authorized spender or every fee-charging mint reverts.
        computeMeter.setSpender(address(artifactTemplate), true);

        // --- hand ownership to the configured owner (two-step; owner must accept) ---
        if (cfg.owner != deployer) {
            computeMeter.transferOwnership(cfg.owner);
            renderReceipts.transferOwnership(cfg.owner);
            regionAuthority.transferOwnership(cfg.owner);
            artifactTemplate.transferOwnership(cfg.owner);
        }

        deployed = Deployed({
            computeMeter: computeMeter,
            renderReceipts: renderReceipts,
            regionAuthority: regionAuthority,
            artifactTemplate: artifactTemplate
        });
    }

    /// @dev Resolves config from .env with sane defaults so `forge build` and the
    ///      simulation test work without any environment set.
    function _configFromEnv() internal view returns (DeployConfig memory cfg) {
        // EAS predeploy slots on Base; safe non-zero defaults.
        address defaultEas = 0x4200000000000000000000000000000000000021;
        address defaultRegistry = 0x4200000000000000000000000000000000000020;

        // Placeholder $BLCKFLD until the Clanker launch cast; overridden via env in prod.
        cfg.token = vm.envOr("BLCKFLD_ADDRESS", address(0xB1ACFc1D00000000000000000000000000000001));
        cfg.eas = vm.envOr("EAS_ADDRESS", defaultEas);
        cfg.schemaRegistry = vm.envOr("EAS_SCHEMA_REGISTRY", defaultRegistry);
        cfg.owner = vm.envOr("OWNER_ADDRESS", msg.sender);
        cfg.coordinator = vm.envOr("COORDINATOR_ADDRESS", msg.sender);
        cfg.stakeRequired = vm.envOr("REGION_STAKE_REQUIRED", uint256(100_000 ether));
        cfg.artifactBaseUri =
            vm.envOr("ARTIFACT_BASE_URI", string("https://artifacts.blackfield.xyz/{id}.json"));
        cfg.artifactMintFeeRate = vm.envOr("ARTIFACT_MINT_FEE_RATE", uint256(1 ether));
    }

    function _log(DeployConfig memory cfg, Deployed memory deployed) internal pure {
        console2.log("== Blackfield deploy ==");
        console2.log("owner:           ", cfg.owner);
        console2.log("coordinator:     ", cfg.coordinator);
        console2.log("token:           ", cfg.token);
        console2.log("ComputeMeter:    ", address(deployed.computeMeter));
        console2.log("RenderReceipts:  ", address(deployed.renderReceipts));
        console2.log("RegionAuthority: ", address(deployed.regionAuthority));
        console2.log("ArtifactTemplate:", address(deployed.artifactTemplate));
    }
}
