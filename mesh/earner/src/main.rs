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
    JobKind::ALL.to_vec()
}

/// Dev-only default session key. This is a well-known test private key (not
/// secret); production earners pass `--session-key` / `SESSION_KEY` from the OS
/// keychain per research-earner-client.md (EIP-7702 scoped key).
const DEV_SESSION_KEY: &str =
    "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318";

/// Base delay for the websocket reconnect backoff. The delay doubles per
/// consecutive failure (1s, 2s, 4s, …) up to `--reconnect-max-secs`.
const BACKOFF_BASE_SECS: u64 = 1;

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
    /// Ceiling for the websocket reconnect backoff, in seconds. The backoff
    /// starts at `BACKOFF_BASE_SECS` and doubles per consecutive failure up to
    /// this cap, so a coordinator that's down doesn't get hammered.
    #[arg(long, env = "RECONNECT_MAX_SECS", default_value_t = 30)]
    reconnect_max_secs: u64,
    /// Transport: `http` (default, legacy poll loop) or `ws` (websocket job
    /// dispatch). `--ws` is shorthand for `--mode ws`.
    #[arg(long, value_enum, env = "EARNER_MODE", default_value_t = Mode::Http)]
    mode: Mode,
    /// Shorthand for `--mode ws`. Overrides `--mode` when set.
    #[arg(long, default_value_t = false)]
    ws: bool,
    /// Seconds between liveness heartbeats sent to the coordinator while a job
    /// is in-flight (ws mode). The coordinator bumps the job's `started_at` on
    /// each heartbeat so the deadline reaper measures the window from the last
    /// sign of life rather than from dispatch time.
    #[arg(long, env = "HEARTBEAT_SECS", default_value_t = 10)]
    heartbeat_secs: u64,
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

/// Websocket job dispatch (v1), run as a durable daemon. Connects, runs one
/// [`ws_session`], and on session end *or* connect failure logs, sleeps an
/// exponential backoff, and reconnects — forever. The coordinator reclaims and
/// requeues this earner's in-flight job when it bounces, so reconnecting is
/// enough to resume work. Mirrors the resilience of [`run_http`]'s poll loop.
///
/// Backoff starts at [`BACKOFF_BASE_SECS`] and doubles per *consecutive*
/// failure up to `--reconnect-max-secs`; it RESETS to the base once a
/// connection is successfully established, so a transient drop reconnects fast
/// while a downed coordinator isn't hammered. Never returns `Ok(())`.
async fn run_ws(args: &Args, session: &Session) -> Result<()> {
    let url = ws_url(&args.coordinator);
    let mut consecutive_failures: u32 = 0;

    loop {
        tracing::info!(%url, "connecting websocket");
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _resp)) => {
                // A successful connect resets the backoff: a transient drop
                // reconnects fast.
                consecutive_failures = 0;
                match ws_session(ws, args, session).await {
                    Ok(()) => tracing::info!("websocket session ended; reconnecting"),
                    Err(e) => tracing::warn!(error = %e, "websocket session error; reconnecting"),
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, %url, "ws connect failed; retrying");
            }
        }

        consecutive_failures = consecutive_failures.saturating_add(1);
        let delay = backoff_delay(consecutive_failures, args.reconnect_max_secs);
        tracing::info!(secs = delay.as_secs(), "backing off before reconnect");
        tokio::time::sleep(delay).await;
    }
}

/// Run a single websocket connection: send `Hello`, then handle `JobOffer` →
/// render → sign → `Accept` + `Submit`, reading the `Accepted`/`Rejected`
/// verdict, until the stream ends.
///
/// Returns `Ok(())` on a clean `Close` or stream end and `Err` on a transport
/// recv error — either way [`run_ws`] reconnects. In-session DECODE failures
/// (one malformed frame) and `handle_offer` errors are non-fatal: log a `warn`
/// and keep the session alive, mirroring how the coordinator tolerates
/// undecodable earner messages.
async fn ws_session<S>(mut ws: S, args: &Args, session: &Session) -> Result<()>
where
    S: SinkExt<WsMessage>
        + StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    <S as futures_util::Sink<WsMessage>>::Error: std::error::Error + Send + Sync + 'static,
{
    let hello = EarnerMsg::Hello {
        earner_address: session.address.clone(),
        gpu_model: args.gpu_model.clone(),
        vram_gb: args.vram_gb,
        supported: all_supported(),
    };
    ws.send(WsMessage::text(serde_json::to_string(&hello)?))
        .await
        .map_err(|e| anyhow!(e))
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
        // One malformed frame must not drop the session: log and skip it, as
        // the coordinator does for undecodable earner messages.
        let msg: CoordinatorMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "undecodable coordinator message");
                continue;
            }
        };
        match msg {
            CoordinatorMsg::JobOffer(job) => {
                if let Err(e) = handle_offer(&mut ws, session, job, args.heartbeat_secs).await {
                    tracing::warn!(error = %e, "job offer handling failed");
                }
            }
            CoordinatorMsg::Accepted { job_id, attestation_uid } => {
                tracing::info!(%job_id, %attestation_uid, "result accepted");
            }
            CoordinatorMsg::Rejected { job_id, reason } => {
                // Dropped cleanly: we never recorded it as done, and the
                // coordinator requeues the job for another earner (or
                // dead-letters it after max attempts). There is nothing to
                // retry here — we simply await the next offer, so a rejection
                // can't busy-loop the earner.
                tracing::warn!(%job_id, %reason, "result rejected; dropping job");
            }
        }
    }
    Ok(())
}

/// Exponential reconnect backoff: `min(2^(failures - 1), max_secs)` seconds.
/// `consecutive_failures` is 1 on the first failure (→ `BACKOFF_BASE_SECS`).
/// The shift is guarded against overflow — large failure counts saturate to
/// `max_secs` rather than panicking.
fn backoff_delay(consecutive_failures: u32, max_secs: u64) -> Duration {
    let secs = if consecutive_failures == 0 {
        BACKOFF_BASE_SECS
    } else {
        // 1u64 << shift overflows for shift >= 64; saturate past that. Any
        // shift >= 6 already exceeds the default 30s cap anyway.
        let shift = consecutive_failures - 1;
        let scaled = 1u64
            .checked_shl(shift)
            .and_then(|f| f.checked_mul(BACKOFF_BASE_SECS))
            .unwrap_or(u64::MAX);
        scaled.min(max_secs)
    };
    Duration::from_secs(secs.min(max_secs))
}

/// Render a single offered job, then `Accept` + `Submit` it over the socket.
///
/// While the render is in progress, a periodic heartbeat is sent to the
/// coordinator every `heartbeat_secs` seconds (`.max(1)` guards against a
/// zero-second interval panicking `tokio::time::interval`). The coordinator
/// bumps `started_at` on each heartbeat so the deadline reaper measures the
/// window from the last sign of life rather than from dispatch. A job that
/// keeps heartbeating is making progress and won't be reaped; a silent earner
/// still hits the deadline.
///
/// The stub `runner::render` returns instantly, so for stub jobs no heartbeat
/// fires mid-render — that is correct; this mechanism is for slow real renders.
async fn handle_offer<S>(
    ws: &mut S,
    session: &Session,
    job: JobSpec,
    heartbeat_secs: u64,
) -> Result<()>
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

    // Run the render concurrently with a periodic heartbeat sender. The
    // coordinator bumps `started_at` on each beat, so a job making progress
    // is never reaped by the deadline reaper; a silent earner still hits the
    // original window.
    let render_fut = runner::render(&job);
    tokio::pin!(render_fut);
    let mut hb = tokio::time::interval(Duration::from_secs(heartbeat_secs.max(1)));
    hb.tick().await; // consume the immediate first tick so the first beat is one interval in
    let output = loop {
        tokio::select! {
            res = &mut render_fut => break res.context("render failed")?,
            _ = hb.tick() => {
                let beat = EarnerMsg::Heartbeat { job_id: Some(job.id), progress_pct: 0 };
                ws.send(WsMessage::text(serde_json::to_string(&beat)?))
                    .await
                    .map_err(|e| anyhow!(e))
                    .context("sending Heartbeat")?;
                tracing::debug!(job_id = %job.id, "heartbeat sent");
            }
        }
    };

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

/// Whether a finished poll cycle should poll again immediately. A successful
/// submit (2xx) means there may be more work, so re-poll promptly (`true`). A
/// coordinator-rejected submit (any non-2xx — 401 bad attestation, 404 unknown
/// job, 409 not in-flight / already done) is dropped cleanly and the caller
/// backs off (`false`), so a persistent rejection (e.g. a misconfigured session
/// key) can't spin a tight render→submit→reject loop.
fn keep_polling_after_submit(status: reqwest::StatusCode) -> bool {
    status.is_success()
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
    let status = resp.status();
    if status.is_success() {
        tracing::info!(%status, job_id = %job.id, "submitted");
    } else {
        // The coordinator gated the submit (401 bad attestation, 404 unknown
        // job, 409 not in-flight / already done). Drop the job cleanly — it is
        // NOT counted as done — and back off rather than re-polling at once.
        tracing::warn!(%status, job_id = %job.id, "submit rejected; dropping job");
    }
    Ok(keep_polling_after_submit(status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_maps_http_to_ws_and_https_to_wss() {
        assert_eq!(ws_url("http://h:1"), "ws://h:1/ws");
        assert_eq!(ws_url("https://h:1"), "wss://h:1/ws");
    }

    #[test]
    fn ws_url_trims_a_trailing_slash_on_the_base() {
        assert_eq!(ws_url("http://h:1/"), "ws://h:1/ws");
        assert_eq!(ws_url("https://h:1/"), "wss://h:1/ws");
    }

    #[test]
    fn ws_url_passes_through_other_schemes_then_appends_ws() {
        // No http(s) prefix to rewrite: the scheme is left as-is and `/ws` is
        // appended. A `ws://` base therefore stays `ws://`, and a bare host is
        // left bare.
        assert_eq!(ws_url("ws://h:1"), "ws://h:1/ws");
        assert_eq!(ws_url("wss://h:1"), "wss://h:1/ws");
        assert_eq!(ws_url("h:1"), "h:1/ws");
        // The trailing-slash trim happens before the scheme check.
        assert_eq!(ws_url("ws://h:1/"), "ws://h:1/ws");
    }

    #[test]
    fn backoff_delay_doubles_per_failure() {
        let max = 1000; // high enough not to clamp these.
        assert_eq!(backoff_delay(1, max), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, max), Duration::from_secs(2));
        assert_eq!(backoff_delay(3, max), Duration::from_secs(4));
        assert_eq!(backoff_delay(4, max), Duration::from_secs(8));
    }

    #[test]
    fn backoff_delay_caps_at_max_secs() {
        assert_eq!(backoff_delay(10, 30), Duration::from_secs(30));
        // Once the doubled value exceeds the cap it stays pinned.
        assert_eq!(backoff_delay(6, 30), Duration::from_secs(30)); // 2^5 = 32 > 30
        assert_eq!(backoff_delay(5, 30), Duration::from_secs(16)); // 2^4 = 16 < 30
    }

    #[test]
    fn backoff_delay_never_panics_on_large_failure_counts() {
        // The shift would overflow a naive `1 << (n - 1)`; we must saturate.
        assert_eq!(backoff_delay(100, 30), Duration::from_secs(30));
        assert_eq!(backoff_delay(u32::MAX, 30), Duration::from_secs(30));
    }

    #[test]
    fn keep_polling_only_after_a_successful_submit() {
        use reqwest::StatusCode;
        // A successful submit may be followed by more work: re-poll promptly.
        assert!(keep_polling_after_submit(StatusCode::OK));
        assert!(keep_polling_after_submit(StatusCode::CREATED));
        // Coordinator submit-gate rejections must back off, not re-poll, so a
        // persistent rejection can't spin a tight render→submit→reject loop.
        assert!(!keep_polling_after_submit(StatusCode::UNAUTHORIZED)); // 401 bad attestation
        assert!(!keep_polling_after_submit(StatusCode::NOT_FOUND)); // 404 unknown job
        assert!(!keep_polling_after_submit(StatusCode::CONFLICT)); // 409 not in-flight/done
    }
}
