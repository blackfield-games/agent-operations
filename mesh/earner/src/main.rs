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

use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use futures_util::{SinkExt, StreamExt};
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use proto::{signing_digest, CoordinatorMsg, EarnerMsg, JobKind, JobResult, JobSpec};
use sha2::{Digest, Sha256};
use sha3::Keccak256;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

mod runner;

/// Transport the earner uses to talk to the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Legacy HTTP poll loop (`/jobs/next` + `/jobs/{id}/submit`).
    Http,
    /// Websocket job dispatch (`/ws`) — the v1 upgrade.
    Ws,
}

/// All supported job kinds, advertised in `Hello` for both transports.
fn all_supported() -> Vec<JobKind> {
    vec![
        JobKind::Terrain,
        JobKind::Foliage,
        JobKind::NpcTick,
        JobKind::DiffusionTile,
        JobKind::Optimization,
    ]
}

/// Dev-only default session key. This is a well-known test private key (not
/// secret); production earners pass `--session-key` / `SESSION_KEY` from the OS
/// keychain per research-earner-client.md (EIP-7702 scoped key).
const DEV_SESSION_KEY: &str =
    "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";

#[derive(Parser)]
struct Args {
    #[arg(long, env = "COORDINATOR_URL", default_value = "http://127.0.0.1:8787")]
    coordinator: String,
    /// secp256k1 session private key, hex (with or without 0x prefix). The
    /// earner address is *derived* from this key; the derived address is used
    /// in submissions so the coordinator's signature check passes.
    #[arg(long, env = "SESSION_KEY", default_value = DEV_SESSION_KEY)]
    session_key: String,
    #[arg(long, env = "POLL_INTERVAL_SECS", default_value_t = 5)]
    poll_secs: u64,
    #[arg(long, env = "GPU_MODEL", default_value = "unknown-gpu")]
    gpu_model: String,
    #[arg(long, env = "VRAM_GB", default_value_t = 24)]
    vram_gb: u32,
    /// Transport: `http` (default, legacy poll loop) or `ws` (websocket job
    /// dispatch). `--ws` is shorthand for `--mode ws`.
    #[arg(long, value_enum, env = "EARNER_MODE", default_value_t = Mode::Http)]
    mode: Mode,
    /// Shorthand for `--mode ws`. Overrides `--mode` when set.
    #[arg(long, default_value_t = false)]
    ws: bool,
}

impl Args {
    /// Effective transport, accounting for the `--ws` shorthand.
    fn effective_mode(&self) -> Mode {
        if self.ws {
            Mode::Ws
        } else {
            self.mode
        }
    }
}

/// Loaded session key + its derived Ethereum-style address.
struct Session {
    signing_key: SigningKey,
    address: String,
}

/// Derive an Ethereum-style address (0x-prefixed, lowercase) from a verifying
/// key: keccak256(uncompressed_pubkey[1..])[12..].
fn address_from_verifying_key(vk: &VerifyingKey) -> String {
    let point = vk.to_encoded_point(false);
    let bytes = point.as_bytes(); // 65 bytes: 0x04 || X || Y
    let hash = Keccak256::digest(&bytes[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

impl Session {
    fn from_hex(key_hex: &str) -> Result<Self> {
        let trimmed = key_hex.strip_prefix("0x").unwrap_or(key_hex);
        let key_bytes = hex::decode(trimmed).context("session key is not valid hex")?;
        let signing_key =
            SigningKey::from_slice(&key_bytes).context("invalid secp256k1 session key")?;
        let address = address_from_verifying_key(signing_key.verifying_key());
        Ok(Self { signing_key, address })
    }

    /// Sign the canonical `signing_digest(job_id, output_hash)` with a
    /// recoverable ECDSA signature and hex-encode the 65-byte [r||s||v].
    fn sign_result(&self, job_id: &Uuid, output_hash: &str) -> String {
        let digest = signing_digest(job_id, output_hash);
        let (sig, recid): (Signature, RecoveryId) = self
            .signing_key
            .sign_prehash_recoverable(&digest)
            .expect("signing a 32-byte prehash cannot fail");
        let mut out = Vec::with_capacity(65);
        out.extend_from_slice(&sig.to_bytes()); // 64 bytes r||s
        out.push(recid.to_byte()); // v (0/1)
        hex::encode(out)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("earner=info")
        .init();
    let args = Args::parse();
    let session = Session::from_hex(&args.session_key)?;

    tracing::info!(
        coordinator = %args.coordinator,
        address = %session.address,
        mode = ?args.effective_mode(),
        "earner online"
    );

    match args.effective_mode() {
        Mode::Http => run_http(&args, &session).await,
        Mode::Ws => run_ws(&args, &session).await,
    }
}

/// Legacy HTTP poll loop. Registers, then polls `/jobs/next` forever.
async fn run_http(args: &Args, session: &Session) -> Result<()> {
    let client = reqwest::Client::new();

    if let Err(e) = register(&client, args, session).await {
        // Non-fatal: the coordinator may not yet support /register, or be down.
        // We still fall through to polling for jobs.
        tracing::warn!(error = %e, "registration failed");
    }

    loop {
        match poll_once(&client, args, session).await {
            Ok(true) => {}
            Ok(false) => tokio::time::sleep(Duration::from_secs(args.poll_secs)).await,
            Err(e) => {
                tracing::warn!(error = %e, "poll cycle failed");
                tokio::time::sleep(Duration::from_secs(args.poll_secs)).await;
            }
        }
    }
}

/// Derive the websocket URL from the coordinator HTTP base URL.
/// `http://host:port` → `ws://host:port/ws`, `https` → `wss`.
fn ws_url(coordinator: &str) -> String {
    let base = coordinator.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{base}/ws")
}

/// Websocket job dispatch (v1). Connects, sends `Hello`, then handles
/// `JobOffer` → render → sign → `Accept` + `Submit`, reading the
/// `Accepted`/`Rejected` verdict.
async fn run_ws(args: &Args, session: &Session) -> Result<()> {
    let url = ws_url(&args.coordinator);
    tracing::info!(%url, "connecting websocket");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("ws connect to {url} failed"))?;

    let hello = EarnerMsg::Hello {
        earner_address: session.address.clone(),
        gpu_model: args.gpu_model.clone(),
        vram_gb: args.vram_gb,
        supported: all_supported(),
    };
    ws.send(WsMessage::text(serde_json::to_string(&hello)?))
        .await
        .context("sending Hello")?;
    tracing::info!("registered with coordinator (ws)");

    while let Some(frame) = ws.next().await {
        let frame = frame.context("ws recv")?;
        let text = match frame {
            WsMessage::Text(t) => t,
            WsMessage::Close(_) => {
                tracing::info!("coordinator closed the connection");
                break;
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Binary(_) | WsMessage::Frame(_) => {
                continue
            }
        };
        let msg: CoordinatorMsg =
            serde_json::from_str(&text).context("decoding coordinator message")?;
        match msg {
            CoordinatorMsg::JobOffer(job) => {
                if let Err(e) = handle_offer(&mut ws, session, job).await {
                    tracing::warn!(error = %e, "job offer handling failed");
                }
            }
            CoordinatorMsg::Accepted { job_id, attestation_uid } => {
                tracing::info!(%job_id, %attestation_uid, "result accepted");
            }
            CoordinatorMsg::Rejected { job_id, reason } => {
                tracing::warn!(%job_id, %reason, "result rejected");
            }
        }
    }
    Ok(())
}

/// Render a single offered job, then `Accept` + `Submit` it over the socket.
async fn handle_offer<S>(ws: &mut S, session: &Session, job: JobSpec) -> Result<()>
where
    S: SinkExt<WsMessage> + Unpin,
    <S as futures_util::Sink<WsMessage>>::Error: std::error::Error + Send + Sync + 'static,
{
    tracing::info!(job_id = %job.id, kind = ?job.kind, region = %job.region.region_id(), "job offered");

    // Accept first so the coordinator marks it in-flight for us.
    ws.send(WsMessage::text(serde_json::to_string(&EarnerMsg::Accept {
        job_id: job.id,
    })?))
    .await
    .map_err(|e| anyhow!(e))
    .context("sending Accept")?;

    let output = runner::render(&job).await.context("render failed")?;
    let mut hasher = Sha256::new();
    hasher.update(&output);
    let output_hash = hex::encode(hasher.finalize());
    let signature_hex = session.sign_result(&job.id, &output_hash);

    let result = JobResult {
        job_id: job.id,
        earner_address: session.address.clone(),
        output_hash,
        output_url: format!("memory://{}", job.id),
        render_seconds: 1,
        signature_hex,
    };
    ws.send(WsMessage::text(serde_json::to_string(&EarnerMsg::Submit(
        result,
    ))?))
    .await
    .map_err(|e| anyhow!(e))
    .context("sending Submit")?;
    Ok(())
}

async fn register(client: &reqwest::Client, args: &Args, session: &Session) -> Result<()> {
    let hello = EarnerMsg::Hello {
        earner_address: session.address.clone(),
        gpu_model: args.gpu_model.clone(),
        vram_gb: args.vram_gb,
        supported: all_supported(),
    };
    let url = format!("{}/register", args.coordinator);
    let resp = client.post(&url).json(&hello).send().await?.error_for_status()?;
    tracing::info!(status = %resp.status(), "registered with coordinator");
    Ok(())
}

async fn poll_once(client: &reqwest::Client, args: &Args, session: &Session) -> Result<bool> {
    let url = format!("{}/jobs/next", args.coordinator);
    let job: Option<JobSpec> = client.get(&url).send().await?.json().await?;
    let Some(job) = job else { return Ok(false) };

    tracing::info!(job_id = %job.id, kind = ?job.kind, region = %job.region.region_id(), "job accepted");
    let output = runner::render(&job).await.context("render failed")?;

    let mut hasher = Sha256::new();
    hasher.update(&output);
    let output_hash = hex::encode(hasher.finalize());

    let signature_hex = session.sign_result(&job.id, &output_hash);

    let result = JobResult {
        job_id: job.id,
        earner_address: session.address.clone(),
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
