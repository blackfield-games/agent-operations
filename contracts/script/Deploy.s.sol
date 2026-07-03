// SPDX-License-Identifier: MIT
pragma solidity ^0.8.27;

import {Script, console2} from "forge-std/Script.sol";
import {ComputeMeter} from "../src/ComputeMeter.sol";
import {RenderReceipts} from "../src/RenderReceipts.sol";
import {RegionAuthority} from "../src/RegionAuthority.sol";
import {ArtifactTemplate} from "../src/ArtifactTemplate.sol";
import {AgentRegistry} from "../src/AgentRegistry.sol";
import {MatchSettlement} from "../src/MatchSettlement.sol";

/// @notice Production-shaped deploy for the six Blackfield contracts.
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
        uint256 renderFeeRate; // RenderReceipts per-render-second region fee-share
        string artifactBaseUri; // ERC-1155 base URI for artifact templates
        uint256 artifactMintFeeRate; // ArtifactTemplate per-unit full-rarity mint fee
        uint256 artifactRoyaltyRate; // ArtifactTemplate per-unit full-rarity region royalty
        uint256 agentMinBond; // AgentRegistry $BLCKFLD bond floor (may be 0)
        address matchAttester; // MatchSettlement result attester — the match-service key
        uint256 matchReputationDelta; // MatchSettlement per-match reputation magnitude (non-zero)
    }

    /// @dev Deployed contract handles, returned to callers (tests + run()).
    struct Deployed {
        ComputeMeter computeMeter;
        RenderReceipts renderReceipts;
        RegionAuthority regionAuthority;
        ArtifactTemplate artifactTemplate;
        AgentRegistry agentRegistry;
        MatchSettlement matchSettlement;
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

    /// @notice Deploys all six contracts and performs post-deploy wiring.
    /// @dev Pure on-chain logic — no broadcast, no env. Callable from tests.
    ///
    ///      `deployer` is whoever actually sends the CREATE + wiring calls: the
    ///      broadcasting EOA (`msg.sender`) under `run()`, or the calling script
    ///      contract in tests. It is set as the initial owner so it can perform the
    ///      owner-gated wiring, then ownership is transferred to `cfg.owner`.
    ///      All six contracts are Ownable2Step, so `cfg.owner` must call
    ///      `acceptOwnership()` on each to finalize the handoff.
    function deploy(DeployConfig memory cfg, address deployer) public returns (Deployed memory deployed) {
        // 1. Compute budget meter — burns $BLCKFLD, credits buyers.
        ComputeMeter computeMeter = new ComputeMeter(cfg.token, deployer);

        // 2. Region authority — staked ERC-721 over world regions. Deployed before
        //    RenderReceipts because the latter routes a per-receipt fee-share into a
        //    region via depositFees and reads its fee token from RegionAuthority.TOKEN().
        RegionAuthority regionAuthority = new RegionAuthority(cfg.token, cfg.stakeRequired, deployer);

        // 3. Render receipts — EAS attestations for validated render-jobs; each validated
        //    receipt routes a real-$BLCKFLD fee-share (renderFeeRate per render-second,
        //    pulled from the issuing coordinator) into the receipt's region on the
        //    RegionAuthority above. depositFees is permissionless, so no cross-contract
        //    authorization is wired here; the fee token is derived from
        //    RegionAuthority.TOKEN() (== cfg.token), never a separate arg that could desync.
        RenderReceipts renderReceipts =
            new RenderReceipts(cfg.eas, deployer, address(regionAuthority), cfg.renderFeeRate);

        // 4. Artifact templates — ERC-1155 player-authored artifacts; mints debit the
        //    rarity-scaled fee from the recipient's credit on this ComputeMeter and
        //    route a rarity-scaled real-$BLCKFLD royalty into the artifact's region fee
        //    pool on the RegionAuthority deployed above (it reads the royalty token from
        //    RegionAuthority.TOKEN(), which is cfg.token).
        ArtifactTemplate artifactTemplate = new ArtifactTemplate(
            deployer,
            cfg.artifactBaseUri,
            address(computeMeter),
            cfg.artifactMintFeeRate,
            address(regionAuthority),
            cfg.artifactRoyaltyRate
        );

        // 5. Agent registry — on-chain identity + reputation for ranked agents. Bonds
        //    in the same $BLCKFLD as everything else; reputation is moved only by an
        //    owner-authorized writer (the settlement contract below).
        AgentRegistry agentRegistry = new AgentRegistry(cfg.token, cfg.agentMinBond, deployer);

        // 6. Match settlement — A2A wager escrow + reputation write-back, and each settled
        //    match attested on the same EAS rail as render receipts. Binds its escrow token
        //    to AgentRegistry.TOKEN() internally (can't desync), so beyond the EAS address it
        //    takes the registry address + the owner-set reputation magnitude only.
        MatchSettlement matchSettlement =
            new MatchSettlement(cfg.eas, address(agentRegistry), deployer, cfg.matchReputationDelta);

        // --- post-deploy wiring (deployer is owner) ---

        // Register the canonical EAS schemas and authorize the coordinator to issue receipts.
        // MatchSettlement's schemas MUST land before its attester is authorized below — a
        // settle before registration reverts SchemaNotSet.
        renderReceipts.registerSchema(cfg.schemaRegistry);
        matchSettlement.registerSchema(cfg.schemaRegistry);
        renderReceipts.setCoordinator(cfg.coordinator, true);

        // Coordinator is the authorized compute spender + artifact minter.
        computeMeter.setSpender(cfg.coordinator, true);
        artifactTemplate.setMinter(cfg.coordinator);

        // ArtifactTemplate debits the mint fee directly via ComputeMeter.spend, so it
        // must itself be an authorized spender or every fee-charging mint reverts.
        computeMeter.setSpender(address(artifactTemplate), true);

        // Settlement is the sole reputation writer on the registry (an agent can never
        // move its own standing) — without this every on-chain settle reverts
        // NotReputationWriter. Authorize the match-service key to open/settle matches —
        // an unset/wrong attester means the service can never open or settle a match.
        // Both calls MUST land while the deployer still owns the contracts, i.e. before
        // the transfer below — mirrors the existing four-contract wiring-then-transfer.
        agentRegistry.setReputationWriter(address(matchSettlement), true);
        matchSettlement.setAttester(cfg.matchAttester, true);

        // --- hand ownership to the configured owner (two-step; owner must accept) ---
        if (cfg.owner != deployer) {
            computeMeter.transferOwnership(cfg.owner);
            renderReceipts.transferOwnership(cfg.owner);
            regionAuthority.transferOwnership(cfg.owner);
            artifactTemplate.transferOwnership(cfg.owner);
            agentRegistry.transferOwnership(cfg.owner);
            matchSettlement.transferOwnership(cfg.owner);
        }

        deployed = Deployed({
            computeMeter: computeMeter,
            renderReceipts: renderReceipts,
            regionAuthority: regionAuthority,
            artifactTemplate: artifactTemplate,
            agentRegistry: agentRegistry,
            matchSettlement: matchSettlement
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
        // Per-render-second region fee-share, in $BLCKFLD wei. A render job's fee is this
        // times its render-seconds, so the rate is a micro-amount; the operator sets the
        // real economic value via env before mainnet (placeholder default like the rates).
        cfg.renderFeeRate = vm.envOr("RENDER_FEE_RATE", uint256(1e12));
        cfg.artifactBaseUri =
            vm.envOr("ARTIFACT_BASE_URI", string("https://artifacts.blackfield.xyz/{id}.json"));
        cfg.artifactMintFeeRate = vm.envOr("ARTIFACT_MINT_FEE_RATE", uint256(1 ether));
        cfg.artifactRoyaltyRate = vm.envOr("ARTIFACT_ROYALTY_RATE", uint256(1 ether));
        // Optional anti-Sybil bond floor for a ranked registration; 0 = no floor.
        cfg.agentMinBond = vm.envOr("AGENT_MIN_BOND", uint256(0));
        // The match-service result attester. Defaults to the deployer EOA like owner/
        // coordinator; the operator sets the real high-trust match-service key via env.
        cfg.matchAttester = vm.envOr("MATCH_ATTESTER_ADDRESS", msg.sender);
        // Fixed per-match reputation magnitude (win +k / loss −k). Rejected at zero by
        // the constructor; placeholder default — operator sets the real value before mainnet.
        cfg.matchReputationDelta = vm.envOr("MATCH_REPUTATION_DELTA", uint256(10 ether));
    }

    function _log(DeployConfig memory cfg, Deployed memory deployed) internal pure {
        console2.log("== Blackfield deploy ==");
        console2.log("owner:           ", cfg.owner);
        console2.log("coordinator:     ", cfg.coordinator);
        console2.log("attester:        ", cfg.matchAttester);
        console2.log("token:           ", cfg.token);
        console2.log("ComputeMeter:    ", address(deployed.computeMeter));
        console2.log("RenderReceipts:  ", address(deployed.renderReceipts));
        console2.log("RegionAuthority: ", address(deployed.regionAuthority));
        console2.log("ArtifactTemplate:", address(deployed.artifactTemplate));
        console2.log("AgentRegistry:   ", address(deployed.agentRegistry));
        console2.log("MatchSettlement: ", address(deployed.matchSettlement));
    }
}
