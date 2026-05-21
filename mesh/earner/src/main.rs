//! BLACKFIELD earner — polls coordinator, runs render jobs locally, submits
//! results with a signed attestation envelope.
//!
//! Per research-earner-client.md:
//! - Rust core daemon (Tauri shell ships later, for the consumer install).
//! - Sandboxing via gVisor + nvproxy on Linux — TODO once we have a real
//!   Houdini/diffusion runtime to sandbox. Stubbed render lives in-process.
//! - Attestation: redundant execution + slashing is the mature path. zk
//!   proof-of-GPU-compute is research-grade in 2026.
//! - Session keys: EIP-7702 delegation + ZeroDev-style scoped key in OS
//!   keychain. Stubbed here as a hex-encoded private key for dev only.

use anyhow::{Context, Result};
use clap::Parser;
use proto::{EarnerMsg, JobKind, JobResult, JobSpec};
use sha2::{Digest, Sha256};
use std::time::Duration;

mod runner;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "COORDINATOR_URL", default_value = "http://127.0.0.1:8787")]
    coordinator: String,
    #[arg(long, env = "EARNER_ADDRESS", default_value = "0x0000000000000000000000000000000000000000")]
    address: String,
    #[arg(long, env = "POLL_INTERVAL_SECS", default_value_t = 5)]
    poll_secs: u64,
    #[arg(long, env = "GPU_MODEL", default_value = "unknown-gpu")]
    gpu_model: String,
    #[arg(long, env = "VRAM_GB", default_value_t = 24)]
    vram_gb: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("earner=info")
        .init();
    let args = Args::parse();
    let client = reqwest::Client::new();

    tracing::info!(coordinator = %args.coordinator, address = %args.address, "earner online");

    if let Err(e) = register(&client, &args).await {
        // Non-fatal: the coordinator may not yet support /register, or be down.
        // We still fall through to polling for jobs.
        tracing::warn!(error = %e, "registration failed");
    }

    loop {
        match poll_once(&client, &args).await {
            Ok(true) => {}
            Ok(false) => tokio::time::sleep(Duration::from_secs(args.poll_secs)).await,
            Err(e) => {
                tracing::warn!(error = %e, "poll cycle failed");
                tokio::time::sleep(Duration::from_secs(args.poll_secs)).await;
            }
        }
    }
}

async fn register(client: &reqwest::Client, args: &Args) -> Result<()> {
    let hello = EarnerMsg::Hello {
        earner_address: args.address.clone(),
        gpu_model: args.gpu_model.clone(),
        vram_gb: args.vram_gb,
        supported: vec![
            JobKind::Terrain,
            JobKind::Foliage,
            JobKind::NpcTick,
            JobKind::DiffusionTile,
            JobKind::Optimization,
        ],
    };
    let url = format!("{}/register", args.coordinator);
    let resp = client.post(&url).json(&hello).send().await?.error_for_status()?;
    tracing::info!(status = %resp.status(), "registered with coordinator");
    Ok(())
}

async fn poll_once(client: &reqwest::Client, args: &Args) -> Result<bool> {
    let url = format!("{}/jobs/next", args.coordinator);
    let job: Option<JobSpec> = client.get(&url).send().await?.json().await?;
    let Some(job) = job else { return Ok(false) };

    tracing::info!(job_id = %job.id, kind = ?job.kind, region = %job.region.region_id(), "job accepted");
    let output = runner::render(&job).await.context("render failed")?;

    let mut hasher = Sha256::new();
    hasher.update(&output);
    let output_hash = hex::encode(hasher.finalize());

    // TODO session-key signature over (job_id, output_hash)
    let signature_hex = "00".repeat(65);

    let result = JobResult {
        job_id: job.id,
        earner_address: args.address.clone(),
        output_hash,
        output_url: format!("memory://{}", job.id),
        render_seconds: 1,
        signature_hex,
    };

    let submit_url = format!("{}/jobs/{}/submit", args.coordinator, job.id);
    let resp = client.post(&submit_url).json(&result).send().await?;
    tracing::info!(status = %resp.status(), "submitted");
    Ok(true)
}
