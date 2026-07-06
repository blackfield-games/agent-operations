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
use proto::{hello_digest, signing_digest, CoordinatorMsg, EarnerMsg, JobKind, JobResult, JobSpec};
use sha2::{Digest, Sha256};
use sha3::Keccak256;
use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
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

    /// Sign a 32-byte prehash with a recoverable ECDSA signature and hex-encode
    /// the 65-byte [r||s||v] the coordinator recovers from.
    fn sign_digest(&self, digest: &[u8; 32]) -> String {
        let (sig, recid): (Signature, RecoveryId) = self
            .signing_key
            .sign_prehash_recoverable(digest)
            .expect("signing a 32-byte prehash cannot fail");
        let mut out = Vec::with_capacity(65);
        out.extend_from_slice(&sig.to_bytes()); // 64 bytes r||s
        out.push(recid.to_byte()); // v (0/1)
        hex::encode(out)
    }

    /// Sign the canonical `signing_digest(job_id, output_hash)` — the result
    /// attestation the coordinator validates before settle.
    fn sign_result(&self, job_id: &Uuid, output_hash: &str) -> String {
        self.sign_digest(&signing_digest(job_id, output_hash))
    }

    /// Sign the canonical `hello_digest` over our advertised capabilities plus
    /// the coordinator-issued challenge `nonce`, proving possession of the key
    /// behind `self.address` at registration AND binding it to this connection
    /// (anti-replay). The HTTP path, whose replay is a benign upsert, passes an
    /// empty nonce.
    fn sign_hello(&self, gpu_model: &str, vram_gb: u32, supported: &[JobKind], nonce: &[u8]) -> String {
        self.sign_digest(&hello_digest(&self.address, gpu_model, vram_gb, supported, nonce))
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

/// Total deadline for a single earner→coordinator HTTP request — connect, send,
/// and the WHOLE response. `reqwest::Client::new()` sets NEITHER a request nor a
/// connect timeout, so a coordinator (or on-path device) that accepts the TCP
/// connection then stalls — never sending headers, or trickling the body — hangs
/// a `poll_once` forever, wedging the entire poll loop (it never returns to
/// sleep+retry). This bounds the whole exchange, not just connect, so a
/// post-connect slowloris stall still surfaces as an `Err` the poll loop logs +
/// backs off on. reqwest applies it PER-REQUEST, so the GET `/jobs/next` and the
/// POST `/{id}/submit` in one poll cycle each get their own deadline (the local
/// `runner::render` between them is not a network op and is unbounded here).
/// Generous (45s) so a slow-but-live coordinator under load isn't
/// sheared into a spurious error; the OS/edge TCP timeouts remain the primary
/// liveness defense and this is the app-layer backstop — the HTTP twin of the ws
/// path's `CHALLENGE_TIMEOUT_SECS`. A const, not a knob, to match the sibling
/// inbound caps (`MAX_INBOUND_FRAME_BYTES`/`MAX_RESPONSE_BODY_BYTES`); promoting
/// it to a `--request-timeout-secs` arg is a one-line follow-up if an operator
/// needs per-deployment tuning.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// Deadline for just the TCP connect — a subset of [`HTTP_REQUEST_TIMEOUT`].
/// Trips fast when the coordinator host is unreachable/black-holed so the loop
/// retries promptly instead of waiting out the full request timeout on a dead
/// host. Both are set deliberately: a connect timeout alone leaves the
/// slowloris-response hole (connect succeeds fast, then the body stalls forever).
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the earner's HTTP poll client with explicit connect + total request
/// timeouts. Factored out of [`run_http`] so a test can build a small-timeout
/// twin and prove a stalled coordinator surfaces as `Err` (not a hang); that test
/// pins the TOTAL `.timeout` (dropping it makes the stall hang the full delay).
/// The `.connect_timeout` is a production liveness guard for an unreachable host
/// and is NOT separately unit-pinned — a hermetic connect-timeout test isn't
/// cleanly constructible (a bound-but-unaccepted local socket still completes the
/// TCP handshake, so connect succeeds; a non-routable address black-holes vs
/// fast-rejects per the host's routing, so timing isn't deterministic). Only
/// `run_http` builds an HTTP client; the ws path uses `tokio_tungstenite` and
/// shares nothing here, so this timeout never bounds the persistent ws session.
fn http_client(request_timeout: Duration, connect_timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(request_timeout)
        .connect_timeout(connect_timeout)
        .build()
        .context("building earner HTTP client")
}

/// Legacy HTTP poll loop. Registers, then polls `/jobs/next` forever.
async fn run_http(args: &Args, session: &Session) -> Result<()> {
    let client = http_client(HTTP_REQUEST_TIMEOUT, HTTP_CONNECT_TIMEOUT)?;
    let supported = all_supported();

    let poll_token = match register(&client, args, session).await {
        Ok(token) => token,
        Err(e) => {
            // Non-fatal: the coordinator may not yet support /register, or be down.
            // We still fall through to polling for jobs (with no poll token).
            tracing::warn!(error = %e, "registration failed");
            None
        }
    };

    loop {
        match poll_once(&client, args, session, &supported, poll_token.as_deref()).await {
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

/// Inbound WS frame/message ceiling for the coordinator → earner direction. The
/// earner dials OUT to the coordinator, so without an explicit config it inherits
/// tungstenite's 64 MiB message / 16 MiB frame defaults for what it ACCEPTS — a
/// buggy, compromised, or on-path coordinator could make an operator's earner buffer
/// multi-MiB per frame before serde ever sees it. The largest LEGITIMATE inbound
/// `CoordinatorMsg` is a `JobOffer` carrying a `JobSpec` whose `inputs` is bounded to
/// the coordinator's `MAX_INPUTS_BYTES` (16 KiB); `Challenge`/`Accepted`/`Rejected`
/// are tiny. 64 KiB leaves generous headroom above a max `JobOffer` (4× the inputs
/// cap plus the `CoordinatorMsg`/`JobSpec` framing) while shedding anything an honest
/// coordinator would never send. In-process backstop, the client-side analogue of the
/// coordinator's own inbound cap (`ws_handler` max_message_size/max_frame_size) — set
/// deliberately LOOSER here (4× the inputs cap vs the coordinator's 2×, since extra
/// client-side headroom costs nothing and avoids shearing honest dispatch); the
/// primary defense is still edge/OS (an earner largely controls who it dials).
const MAX_INBOUND_FRAME_BYTES: usize = 64 * 1024;

/// WebSocket config for the coordinator connection: bound the inbound message AND
/// frame size to [`MAX_INBOUND_FRAME_BYTES`]. Both, deliberately — a per-frame cap
/// alone still lets a message split across many frames grow to the message cap.
/// tungstenite's `max_*_size` are RECEIVE limits, so the earner's own outbound
/// `Submit` is unaffected (it is already coordinator-bounded on the submit route).
fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(MAX_INBOUND_FRAME_BYTES),
        max_frame_size: Some(MAX_INBOUND_FRAME_BYTES),
        ..Default::default()
    }
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
        match tokio_tungstenite::connect_async_with_config(&url, Some(ws_config()), false).await {
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

/// How long to wait for the coordinator's opening `Challenge` before giving up
/// and reconnecting — generous, since an honest coordinator challenges on connect.
const CHALLENGE_TIMEOUT_SECS: u64 = 30;

/// Read the coordinator's opening `Challenge` frame and return its decoded nonce
/// bytes. Skips pre-handshake ping/pong/binary; errors (so `run_ws` reconnects)
/// on a timeout, a closed/ended stream, a non-`Challenge` first message, or a
/// non-hex nonce — failing closed rather than registering without a challenge.
async fn recv_challenge<S>(ws: &mut S) -> Result<Vec<u8>>
where
    S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let wait = Duration::from_secs(CHALLENGE_TIMEOUT_SECS);
    loop {
        let frame = tokio::time::timeout(wait, ws.next())
            .await
            .context("timed out awaiting coordinator challenge")?
            .context("connection closed before challenge")?
            .context("ws recv")?;
        let text = match frame {
            WsMessage::Text(t) => t,
            WsMessage::Close(_) => return Err(anyhow!("connection closed before challenge")),
            _ => continue, // ping/pong/binary before the challenge
        };
        return match serde_json::from_str::<CoordinatorMsg>(&text) {
            Ok(CoordinatorMsg::Challenge { nonce }) => {
                hex::decode(&nonce).context("challenge nonce is not valid hex")
            }
            Ok(other) => Err(anyhow!("expected Challenge as first frame, got {other:?}")),
            Err(e) => Err(anyhow!(e)).context("undecodable challenge frame"),
        };
    }
}

/// Run a single websocket connection: read the coordinator's `Challenge`, send a
/// `Hello` signed over it, then handle `JobOffer` → render → sign → `Accept` +
/// `Submit`, reading the `Accepted`/`Rejected` verdict, until the stream ends.
///
/// Returns `Ok(())` on a clean `Close` or stream end and `Err` on a transport
/// recv error or a missing/invalid challenge — either way [`run_ws`] reconnects.
/// In-session DECODE failures (one malformed frame) and `handle_offer` errors are
/// non-fatal: log a `warn` and keep the session alive, mirroring how the
/// coordinator tolerates undecodable earner messages.
async fn ws_session<S>(mut ws: S, args: &Args, session: &Session) -> Result<()>
where
    S: SinkExt<WsMessage>
        + StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    <S as futures_util::Sink<WsMessage>>::Error: std::error::Error + Send + Sync + 'static,
{
    // Advertised capability set — captured ONCE and used both for Hello and for
    // the per-offer self-guard in `handle_offer`, so the guard always checks
    // against exactly what we told the coordinator (no Hello/guard skew, and no
    // mid-session race: a single session never re-Hellos).
    let supported = all_supported();
    // The coordinator opens with a single-use Challenge; fold its nonce into the
    // signed Hello so a Hello captured off the wire and replayed on a fresh
    // connection — which receives a different challenge — fails signature
    // recovery. Bounded so a coordinator that upgrades the socket but never
    // challenges can't wedge us: on timeout/miss we return and `run_ws` reconnects.
    let nonce = recv_challenge(&mut ws).await?;
    let hello = EarnerMsg::Hello {
        earner_address: session.address.clone(),
        gpu_model: args.gpu_model.clone(),
        vram_gb: args.vram_gb,
        supported: supported.clone(),
        signature_hex: session.sign_hello(&args.gpu_model, args.vram_gb, &supported, &nonce),
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
            CoordinatorMsg::Challenge { .. } => {
                // The challenge is consumed once during the opening handshake; a
                // second one mid-session is a protocol anomaly — log and ignore
                // (re-registration is not supported on a live connection).
                tracing::warn!("unexpected mid-session challenge; ignoring");
            }
            CoordinatorMsg::JobOffer(job) => {
                if let Err(e) =
                    handle_offer(&mut ws, session, &supported, job, args.heartbeat_secs).await
                {
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

/// Estimate render progress as a percentage of the job's deadline elapsed.
///
/// The renderer is opaque — a single future with no work-progress channel — so the
/// only signal available for a mid-render heartbeat is wall-clock elapsed against the
/// job's `deadline_secs`, the same window the coordinator's deadline reaper measures.
/// Capped at 99: 100 means "done", which the `Submit` (not a heartbeat) signals, so a
/// render running past its deadline reports 99, never 100+. A zero-deadline job reports
/// 0 and never divides by zero.
fn estimate_progress(elapsed_secs: u64, deadline_secs: u32) -> u8 {
    if deadline_secs == 0 {
        return 0;
    }
    (elapsed_secs.saturating_mul(100) / deadline_secs as u64).min(99) as u8
}

/// The render time to charge for a completed job, in whole seconds.
///
/// Floored to 1: the coordinator's content gate rejects `render_seconds == 0`
/// (`ZeroRenderSeconds`) — a job that submitted a result did at least a second of
/// work, and both `/stats total_render_seconds` and the metered `rate * render_seconds`
/// charge would zero out otherwise. Saturated at `u32::MAX` so a pathologically long
/// render cannot wrap the `u32` the wire and the charge use; the coordinator's own
/// `validate_render_seconds` upper bound (`deadline_secs * slack`) rejects any value
/// that large anyway, so the saturation is belt-and-suspenders, not the real ceiling.
fn render_seconds_charged(elapsed_secs: u64) -> u32 {
    (elapsed_secs.min(u32::MAX as u64) as u32).max(1)
}

/// Drive `render_fut` to completion while emitting a heartbeat every `heartbeat_secs`
/// seconds (`.max(1)` guards against a zero-second interval panicking
/// `tokio::time::interval`). The coordinator bumps `started_at` on each heartbeat so the
/// deadline reaper measures the window from the last sign of life rather than from
/// dispatch, and each beat carries a live [`estimate_progress`] reading (elapsed vs
/// `deadline_secs`) so `/stats in_flight_progress_pct_avg` tracks real progress instead
/// of the constant it reported before. Returns the rendered bytes.
///
/// `started` is the caller's render clock, passed in rather than captured here so the
/// SAME instant backs both the progress estimate and the caller's `render_seconds`
/// charge (one source of truth for "how long has this render run").
///
/// The stub `runner::render` returns instantly, so for stub jobs no heartbeat fires
/// mid-render (the loop breaks on the first poll) — that is correct; this mechanism is
/// for slow real renders.
async fn render_with_heartbeats<S, F>(
    ws: &mut S,
    job_id: Uuid,
    started: tokio::time::Instant,
    deadline_secs: u32,
    heartbeat_secs: u64,
    render_fut: F,
) -> Result<Vec<u8>>
where
    S: SinkExt<WsMessage> + Unpin,
    <S as futures_util::Sink<WsMessage>>::Error: std::error::Error + Send + Sync + 'static,
    F: std::future::Future<Output = Result<Vec<u8>>>,
{
    tokio::pin!(render_fut);
    let mut hb = tokio::time::interval(Duration::from_secs(heartbeat_secs.max(1)));
    hb.tick().await; // consume the immediate first tick so the first beat is one interval in
    loop {
        tokio::select! {
            res = &mut render_fut => break res.context("render failed"),
            _ = hb.tick() => {
                let progress = estimate_progress(started.elapsed().as_secs(), deadline_secs);
                let beat = EarnerMsg::Heartbeat { job_id: Some(job_id), progress_pct: progress };
                ws.send(WsMessage::text(serde_json::to_string(&beat)?))
                    .await
                    .map_err(|e| anyhow!(e))
                    .context("sending Heartbeat")?;
                tracing::debug!(job_id = %job_id, progress_pct = progress, "heartbeat sent");
            }
        }
    }
}

/// Render a single offered job, then `Accept` + `Submit` it over the socket. The
/// render runs concurrently with a progress-carrying heartbeat (see
/// [`render_with_heartbeats`]) so a slow render keeps signalling life and is not
/// reaped mid-flight.
async fn handle_offer<S>(
    ws: &mut S,
    session: &Session,
    supported: &[JobKind],
    job: JobSpec,
    heartbeat_secs: u64,
) -> Result<()>
where
    S: SinkExt<WsMessage> + Unpin,
    <S as futures_util::Sink<WsMessage>>::Error: std::error::Error + Send + Sync + 'static,
{
    tracing::info!(job_id = %job.id, kind = ?job.kind, region = %job.region.region_id(), "job offered");

    // Capability self-guard: never render a kind we didn't advertise. The
    // coordinator's ws dispatch normally filters by our Hello `supported` set,
    // but a coordinator bug, a stale capability view, or a malicious coordinator
    // could still offer an unsupported kind — which the stub would "render" into
    // garbage (and a real per-kind runner would crash on), only to fail
    // validation and burn the job's budget. Decline it back over the socket
    // (a normal protocol message, NOT a disconnect) so the coordinator requeues
    // it for a capable earner; we render nothing and stay connected for the next
    // offer.
    if !supported.contains(&job.kind) {
        tracing::warn!(job_id = %job.id, kind = ?job.kind, "declining offer: unsupported job kind");
        ws.send(WsMessage::text(serde_json::to_string(&EarnerMsg::Decline {
            job_id: job.id,
            reason: format!("unsupported job kind: {}", job.kind.as_str()),
        })?))
        .await
        .map_err(|e| anyhow!(e))
        .context("sending Decline")?;
        return Ok(());
    }

    // Accept first so the coordinator marks it in-flight for us.
    ws.send(WsMessage::text(serde_json::to_string(&EarnerMsg::Accept {
        job_id: job.id,
    })?))
    .await
    .map_err(|e| anyhow!(e))
    .context("sending Accept")?;

    // Run the render concurrently with a progress-carrying heartbeat so a job making
    // progress is never reaped by the deadline reaper; a silent earner still hits the
    // original window. `started` clocks the render for both the heartbeat progress
    // estimate and the `render_seconds` charge below.
    let started = tokio::time::Instant::now();
    let output = render_with_heartbeats(
        ws,
        job.id,
        started,
        job.deadline_secs,
        heartbeat_secs,
        runner::render(&job),
    )
    .await?;

    let mut hasher = Sha256::new();
    hasher.update(&output);
    let output_hash = hex::encode(hasher.finalize());
    let signature_hex = session.sign_result(&job.id, &output_hash);

    let result = JobResult {
        job_id: job.id,
        earner_address: session.address.clone(),
        output_hash,
        output_url: format!("memory://{}", job.id),
        render_seconds: render_seconds_charged(started.elapsed().as_secs()),
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

/// Register with the coordinator and return the poll token it issued in the
/// `x-poll-token` response header — the credential [`poll_once`] echoes as
/// `Authorization: Bearer <token>` so the coordinator refreshes THIS earner's
/// liveness (an unauthenticated poll for our address can't). `None` if the header is
/// absent (an older coordinator that predates the poll-token gate), in which case the
/// earner still polls and is dispatched work — it just won't have its liveness
/// refreshed by polling, so it relies on the submit-path refresh.
async fn register(client: &reqwest::Client, args: &Args, session: &Session) -> Result<Option<String>> {
    let supported = all_supported();
    let hello = EarnerMsg::Hello {
        earner_address: session.address.clone(),
        gpu_model: args.gpu_model.clone(),
        vram_gb: args.vram_gb,
        signature_hex: session.sign_hello(&args.gpu_model, args.vram_gb, &supported, &[]),
        supported,
    };
    let url = format!("{}/register", args.coordinator);
    let resp = client.post(&url).json(&hello).send().await?.error_for_status()?;
    let poll_token = resp
        .headers()
        .get("x-poll-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    tracing::info!(status = %resp.status(), have_poll_token = poll_token.is_some(), "registered with coordinator");
    Ok(poll_token)
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

/// Hard cap on the coordinator's `GET /jobs/next` response body before it is
/// buffered and deserialized. The HTTP-transport twin of [`MAX_INBOUND_FRAME_BYTES`]:
/// the earner dials OUT, and `reqwest` has NO default response-size limit, so a
/// buggy, compromised, or on-path coordinator returning a giant body would OOM an
/// operator's earner inside `resp.json()` before serde ever runs. The largest
/// LEGITIMATE body is a single `JobSpec` whose `inputs` is bounded to the
/// coordinator's `MAX_INPUTS_BYTES` (16 KiB) — or JSON `null` (no job). 64 KiB is
/// the same ceiling the inbound WS frame gets (4× the inputs cap plus framing), a
/// generous backstop that never shears honest dispatch yet sheds anything an honest
/// coordinator would never send. Enforced on BYTES ACTUALLY READ (see [`poll_once`]),
/// not the advertised `Content-Length` — a chunked response can omit or lie about it.
const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;

async fn poll_once(
    client: &reqwest::Client,
    args: &Args,
    session: &Session,
    supported: &[JobKind],
    poll_token: Option<&str>,
) -> Result<bool> {
    let url = format!("{}/jobs/next", args.coordinator);
    // Name ourselves so the coordinator filters the hand-out to the kinds we
    // advertised at registration (capability match on the HTTP transport). The
    // self-guard below stays as defense-in-depth for a coordinator that doesn't.
    // Present the poll token issued at registration as `Authorization: Bearer` so the
    // coordinator refreshes OUR liveness — without it the refresh is declined (an
    // unauthenticated poll can't keep an address live), so an idle-but-polling earner
    // would otherwise age out of the registry between jobs.
    let mut req = client
        .get(&url)
        .query(&[("earner", session.address.as_str())]);
    if let Some(token) = poll_token {
        req = req.bearer_auth(token);
    }
    let mut resp = req.send().await?;
    // The coordinator stamps the dispatch_seq for this hand-out in a header; we
    // must echo it on submit so a job reaped+reassigned to another earner can't
    // be settled by us (the fence). Read it before consuming the body.
    let dispatch_seq = resp
        .headers()
        .get("x-dispatch-seq")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    // Bounded read, replacing an unbounded `resp.json()`: accumulate the body
    // chunk-by-chunk and error past the cap so the poll cycle logs + backs off
    // instead of OOMing on a hostile coordinator. The limit is on bytes ACTUALLY
    // READ — never the advertised Content-Length, which a chunked response can omit
    // or under-report. Peak transient memory is the cap plus one in-flight `chunk()`
    // (hyper sizes a chunk to its socket read, not to the sender's framing).
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > MAX_RESPONSE_BODY_BYTES {
            return Err(anyhow!(
                "coordinator /jobs/next body exceeded {MAX_RESPONSE_BODY_BYTES} bytes; refusing to buffer"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let job: Option<JobSpec> = serde_json::from_slice(&body)?;
    let Some(job) = job else { return Ok(false) };

    // Capability self-guard (mirrors the ws `handle_offer` guard). Unlike the ws
    // dispatcher, the stateless HTTP `/jobs/next` does NOT filter by our
    // advertised kinds, so it can hand us a kind we can't render. This legacy
    // transport has no per-poll decline message, so we just drop the job WITHOUT
    // rendering or submitting and back off (Ok(false)). Honest accounting: the
    // job was already marked in_flight with an attempt charged by /jobs/next, so
    // it stays in_flight until the deadline reaper requeues it — and that requeue
    // is attempt-CHARGING (unlike the ws Decline path, which refunds), so a job
    // only an incapable earner ever polls will burn attempts toward dead-letter.
    // That is the cost of the stateless transport; the primary goal (never render
    // garbage) still holds, and a real earner advertises every kind so this only
    // bites a buggy/malicious coordinator. Backing off rather than re-polling at
    // once keeps a run of unsupported jobs from draining the queue into in_flight.
    if !supported.contains(&job.kind) {
        tracing::warn!(job_id = %job.id, kind = ?job.kind, "dropping unsupported job kind (http, no decline on this transport)");
        return Ok(false);
    }

    let Some(dispatch_seq) = dispatch_seq else {
        tracing::warn!(job_id = %job.id, "offered job without a dispatch-seq header; skipping");
        return Ok(false);
    };

    tracing::info!(job_id = %job.id, kind = ?job.kind, region = %job.region.region_id(), "job accepted");
    // The http poll path has no heartbeat channel, so it clocks the render inline for
    // the `render_seconds` charge (the ws path shares its clock with the heartbeat).
    let started = tokio::time::Instant::now();
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
        render_seconds: render_seconds_charged(started.elapsed().as_secs()),
        signature_hex,
    };

    let submit_url = format!("{}/jobs/{}/submit", args.coordinator, job.id);
    let resp = client
        .post(&submit_url)
        .header("x-dispatch-seq", dispatch_seq.to_string())
        .json(&result)
        .send()
        .await?;
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
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::Poll;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    // ---- session key loading + attestation signing ----

    /// The lowercase Ethereum-style address derived from `DEV_SESSION_KEY`
    /// (no EIP-55 checksum — `address_from_verifying_key` hex-encodes lowercase).
    /// This is the address the coordinator recovers from the earner's signature.
    const DEV_ADDRESS: &str = "0x2c7536e3605d9c16a7a3d7b1898e529396a65c23";

    #[test]
    fn session_from_hex_derives_known_dev_address() {
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        assert_eq!(session.address, DEV_ADDRESS);
    }

    #[test]
    fn session_from_hex_accepts_0x_prefix() {
        // The 0x-prefixed and bare forms of the same key derive the same address.
        let prefixed = Session::from_hex(&format!("0x{DEV_SESSION_KEY}")).unwrap();
        let bare = Session::from_hex(DEV_SESSION_KEY).unwrap();
        assert_eq!(prefixed.address, bare.address);
        assert_eq!(prefixed.address, DEV_ADDRESS);
    }

    #[test]
    fn session_from_hex_rejects_invalid_hex() {
        assert!(Session::from_hex("nothex!!").is_err());
    }

    #[test]
    fn session_from_hex_rejects_wrong_length_key() {
        // Valid hex but not a 32-byte secp256k1 scalar.
        assert!(Session::from_hex("00").is_err());
        assert!(Session::from_hex("").is_err());
    }

    #[test]
    fn sign_result_is_recoverable_to_session_address() {
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let job_id = Uuid::new_v4();
        let output_hash = "deadbeef";

        let sig_hex = session.sign_result(&job_id, output_hash);
        let raw = hex::decode(&sig_hex).unwrap();
        assert_eq!(raw.len(), 65, "signature is 65 bytes r||s||v");

        // Recover exactly as the coordinator does (coordinator verify.rs): the
        // signer the coordinator recovers MUST be this earner's own address, or
        // every submission this earner makes would be rejected as a mismatch.
        let digest = signing_digest(&job_id, output_hash);
        let sig = Signature::from_slice(&raw[..64]).unwrap();
        let recid = RecoveryId::from_byte(raw[64]).unwrap();
        let vk = VerifyingKey::recover_from_prehash(&digest, &sig, recid).unwrap();
        assert_eq!(address_from_verifying_key(&vk), session.address);
    }

    // ---- ws send path: handle_offer over a mock Sink ----

    /// A `futures_util::Sink<WsMessage>` that records every sent frame instead of
    /// writing to a socket. The `handle_offer`/`ws_session` generic bounds were
    /// designed for exactly this — drive the earner's send path with no network.
    /// `Infallible` as the error satisfies the `Error + Send + Sync + 'static`
    /// bound and means a send never fails.
    #[derive(Default)]
    struct RecordingSink {
        sent: Vec<WsMessage>,
    }

    impl futures_util::Sink<WsMessage> for RecordingSink {
        type Error = std::convert::Infallible;

        fn poll_ready(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
            self.get_mut().sent.push(item);
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn decode_frame(frame: &WsMessage) -> EarnerMsg {
        match frame {
            WsMessage::Text(t) => {
                serde_json::from_str(t.as_str()).expect("sent frame is valid EarnerMsg JSON")
            }
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_offer_emits_accept_then_signed_submit() {
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let job = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Terrain,
            region: proto::RegionCoord { x: 42, y: -17, layer: 0 },
            deadline_secs: 120,
            max_payout_wei: "1000000000000000000".to_string(),
            inputs: serde_json::json!({}),
        };
        let job_id = job.id;

        // Large heartbeat interval: the stub render returns instantly, so the
        // render completes before any beat fires — exactly two frames are sent.
        let mut sink = RecordingSink::default();
        handle_offer(&mut sink, &session, &all_supported(), job, 3600).await.unwrap();

        assert_eq!(sink.sent.len(), 2, "expected Accept then Submit, got {:?}", sink.sent);

        // 1) Accept names this job so the coordinator marks it in-flight for us.
        match decode_frame(&sink.sent[0]) {
            EarnerMsg::Accept { job_id: accepted } => assert_eq!(accepted, job_id),
            other => panic!("first frame must be Accept, got {other:?}"),
        }

        // 2) Submit whose signature recovers to the earner's OWN address — the
        // property the whole payout model rests on (mirror of verify.rs recovery).
        match decode_frame(&sink.sent[1]) {
            EarnerMsg::Submit(result) => {
                assert_eq!(result.job_id, job_id);
                assert_eq!(result.earner_address, session.address);
                // The stub render returns instantly (0 elapsed) so the charge floors
                // to 1 — never 0 (the coordinator rejects ZeroRenderSeconds).
                assert_eq!(result.render_seconds, 1, "instant stub render charges the 1s floor");

                let raw = hex::decode(&result.signature_hex).unwrap();
                assert_eq!(raw.len(), 65, "signature is 65 bytes r||s||v");
                let digest = signing_digest(&result.job_id, &result.output_hash);
                let sig = Signature::from_slice(&raw[..64]).unwrap();
                let recid = RecoveryId::from_byte(raw[64]).unwrap();
                let vk = VerifyingKey::recover_from_prehash(&digest, &sig, recid).unwrap();
                assert_eq!(address_from_verifying_key(&vk), session.address);
            }
            other => panic!("second frame must be Submit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_offer_declines_an_unsupported_kind_without_rendering() {
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        // We advertise ONLY Terrain but are offered a DiffusionTile job — what a
        // buggy/malicious coordinator (or a stale capability view) could do.
        let supported = vec![JobKind::Terrain];
        let job = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::DiffusionTile,
            region: proto::RegionCoord { x: 1, y: 2, layer: 0 },
            deadline_secs: 120,
            max_payout_wei: "1000000000000000000".to_string(),
            inputs: serde_json::json!({}),
        };
        let job_id = job.id;

        let mut sink = RecordingSink::default();
        handle_offer(&mut sink, &session, &supported, job, 3600).await.unwrap();

        // Exactly one frame — a Decline naming the job + the offending kind. No
        // Accept, no Submit: nothing was rendered.
        assert_eq!(
            sink.sent.len(),
            1,
            "an unsupported offer must send only a Decline, got {:?}",
            sink.sent
        );
        match decode_frame(&sink.sent[0]) {
            EarnerMsg::Decline { job_id: declined, reason } => {
                assert_eq!(declined, job_id);
                assert!(reason.contains("diffusion_tile"), "reason should name the kind: {reason}");
            }
            other => panic!("the only frame must be Decline, got {other:?}"),
        }
        assert!(
            !sink.sent.iter().any(|f| matches!(
                decode_frame(f),
                EarnerMsg::Accept { .. } | EarnerMsg::Submit(_)
            )),
            "must never Accept or Submit an unsupported job",
        );
    }

    #[tokio::test]
    async fn handle_offer_accepts_a_supported_kind_from_a_restricted_set() {
        // Discriminates against a guard that declines whenever the advertised set
        // isn't the full ALL set: a restricted set that DOES contain the offered
        // kind must still accept + render unchanged.
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let supported = vec![JobKind::Terrain];
        let job = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Terrain,
            region: proto::RegionCoord { x: 0, y: 0, layer: 0 },
            deadline_secs: 120,
            max_payout_wei: "1000000000000000000".to_string(),
            inputs: serde_json::json!({}),
        };
        let job_id = job.id;

        let mut sink = RecordingSink::default();
        handle_offer(&mut sink, &session, &supported, job, 3600).await.unwrap();

        assert_eq!(
            sink.sent.len(),
            2,
            "a supported offer must Accept then Submit, got {:?}",
            sink.sent
        );
        assert!(matches!(decode_frame(&sink.sent[0]), EarnerMsg::Accept { job_id: a } if a == job_id));
        assert!(matches!(decode_frame(&sink.sent[1]), EarnerMsg::Submit(_)));
    }

    // ---- ws full session loop: ws_session over a dual Sink+Stream mock ----

    /// A mock websocket that is BOTH a `Sink<WsMessage>` (records every frame the
    /// earner sends, like [`RecordingSink`]) AND a `Stream` of pre-seeded incoming
    /// frames (what the coordinator "sends"). `ws_session`'s `S: SinkExt + StreamExt`
    /// bounds were designed for exactly this — drive the whole session loop with no
    /// network. `poll_next` drains the seeded queue in order, then yields `None`,
    /// which `ws_session` treats as a clean stream end → `Ok(())`.
    struct MockSocket {
        sent: Vec<WsMessage>,
        incoming: VecDeque<Result<WsMessage, tokio_tungstenite::tungstenite::Error>>,
    }

    impl MockSocket {
        fn new(incoming: VecDeque<Result<WsMessage, tokio_tungstenite::tungstenite::Error>>) -> Self {
            Self { sent: Vec::new(), incoming }
        }
    }

    impl futures_util::Sink<WsMessage> for MockSocket {
        type Error = std::convert::Infallible;

        fn poll_ready(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
            self.get_mut().sent.push(item);
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl futures_util::Stream for MockSocket {
        type Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
            // Drain the seeded queue in order; an empty queue ends the stream.
            Poll::Ready(self.get_mut().incoming.pop_front())
        }
    }

    /// A `JobOffer` text frame for the canonical dev test job, plus its job id
    /// (the offer moves the `JobSpec`, so the id is captured first).
    fn job_offer_frame() -> (WsMessage, Uuid) {
        let job = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Terrain,
            region: proto::RegionCoord { x: 42, y: -17, layer: 0 },
            deadline_secs: 120,
            max_payout_wei: "1000000000000000000".to_string(),
            inputs: serde_json::json!({}),
        };
        let job_id = job.id;
        let offer = CoordinatorMsg::JobOffer(job);
        (WsMessage::text(serde_json::to_string(&offer).unwrap()), job_id)
    }

    /// The coordinator's opening `Challenge` text frame carrying `nonce_hex`.
    /// Seeded (wrapped in `Ok`) at the front of every `ws_session` mock stream so
    /// the handshake can read it before the offer/close/error frames under test.
    fn challenge_frame(nonce_hex: &str) -> WsMessage {
        let msg = CoordinatorMsg::Challenge { nonce: nonce_hex.to_string() };
        WsMessage::text(serde_json::to_string(&msg).unwrap())
    }

    #[tokio::test]
    async fn ws_session_runs_hello_then_accept_then_signed_submit() {
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let args = Args::parse_from(["earner"]);

        // The coordinator offers exactly one job, then the stream ends.
        let (offer, job_id) = job_offer_frame();
        let mut incoming = VecDeque::new();
        incoming.push_back(Ok(challenge_frame("0a1b2c3d4e5f60718293a4b5c6d7e8f9")));
        incoming.push_back(Ok(offer));
        let mut mock = MockSocket::new(incoming);

        // `&mut mock` so we can read the recorded sends afterward; ws_session takes
        // `S` by value, and `&mut MockSocket` is itself a Sink+Stream.
        ws_session(&mut mock, &args, &session).await.unwrap();

        // Hello on connect, then Accept + signed Submit for the one offer.
        assert_eq!(mock.sent.len(), 3, "expected Hello, Accept, Submit, got {:?}", mock.sent);

        match decode_frame(&mock.sent[0]) {
            EarnerMsg::Hello { earner_address, supported, .. } => {
                assert_eq!(earner_address, session.address);
                assert_eq!(supported, all_supported());
            }
            other => panic!("first frame must be Hello, got {other:?}"),
        }
        match decode_frame(&mock.sent[1]) {
            EarnerMsg::Accept { job_id: accepted } => assert_eq!(accepted, job_id),
            other => panic!("second frame must be Accept, got {other:?}"),
        }
        // The Submit signature recovers to the earner's OWN address — the property
        // the coordinator's verify.rs submit gate enforces (mirror of that recovery).
        match decode_frame(&mock.sent[2]) {
            EarnerMsg::Submit(result) => {
                assert_eq!(result.job_id, job_id);
                assert_eq!(result.earner_address, session.address);
                let raw = hex::decode(&result.signature_hex).unwrap();
                assert_eq!(raw.len(), 65, "signature is 65 bytes r||s||v");
                let digest = signing_digest(&result.job_id, &result.output_hash);
                let sig = Signature::from_slice(&raw[..64]).unwrap();
                let recid = RecoveryId::from_byte(raw[64]).unwrap();
                let vk = VerifyingKey::recover_from_prehash(&digest, &sig, recid).unwrap();
                assert_eq!(address_from_verifying_key(&vk), session.address);
            }
            other => panic!("third frame must be Submit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ws_session_signs_hello_over_the_issued_challenge() {
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let args = Args::parse_from(["earner"]);
        let nonce_hex = "0a1b2c3d4e5f60718293a4b5c6d7e8f9";

        // Only a challenge, then the stream ends — ws_session just registers.
        let mut incoming = VecDeque::new();
        incoming.push_back(Ok(challenge_frame(nonce_hex)));
        let mut mock = MockSocket::new(incoming);
        ws_session(&mut mock, &args, &session).await.unwrap();

        let EarnerMsg::Hello { earner_address, gpu_model, vram_gb, supported, signature_hex } =
            decode_frame(&mock.sent[0])
        else {
            panic!("first frame must be Hello");
        };
        assert_eq!(earner_address, session.address);
        let raw = hex::decode(&signature_hex).unwrap();
        let sig = Signature::from_slice(&raw[..64]).unwrap();
        let recid = RecoveryId::from_byte(raw[64]).unwrap();
        // The Hello signature recovers to the earner's address over the digest
        // built with the ISSUED nonce — proving the earner folded in the challenge.
        let nonce = hex::decode(nonce_hex).unwrap();
        let digest = hello_digest(&earner_address, &gpu_model, vram_gb, &supported, &nonce);
        let vk = VerifyingKey::recover_from_prehash(&digest, &sig, recid).unwrap();
        assert_eq!(address_from_verifying_key(&vk), session.address);
        // Over a DIFFERENT challenge the same signature recovers a different key,
        // so a captured Hello can't be replayed against a fresh challenge.
        let other = hello_digest(&earner_address, &gpu_model, vram_gb, &supported, b"other-nonce");
        let vk2 = VerifyingKey::recover_from_prehash(&other, &sig, recid).unwrap();
        assert_ne!(
            address_from_verifying_key(&vk2),
            session.address,
            "the signature must not verify over a different challenge nonce"
        );
    }

    #[tokio::test]
    async fn ws_session_skips_an_undecodable_frame_then_processes_the_offer() {
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let args = Args::parse_from(["earner"]);

        // A malformed text frame precedes the real offer. ws_session must log and
        // skip it (as the coordinator does for undecodable earner messages) and
        // still process the following offer — one bad frame can't drop the session.
        let (offer, job_id) = job_offer_frame();
        let mut incoming = VecDeque::new();
        incoming.push_back(Ok(challenge_frame("0a1b2c3d4e5f60718293a4b5c6d7e8f9")));
        incoming.push_back(Ok(WsMessage::text("definitely not json {{{".to_string())));
        incoming.push_back(Ok(offer));
        let mut mock = MockSocket::new(incoming);

        ws_session(&mut mock, &args, &session).await.unwrap();

        // The garbage frame produced no send; the offer still yielded Accept+Submit.
        assert_eq!(mock.sent.len(), 3, "garbage frame must be skipped, offer still handled: {:?}", mock.sent);
        match decode_frame(&mock.sent[1]) {
            EarnerMsg::Accept { job_id: accepted } => assert_eq!(accepted, job_id),
            other => panic!("second frame must be Accept, got {other:?}"),
        }
        assert!(matches!(decode_frame(&mock.sent[2]), EarnerMsg::Submit(_)));
    }

    #[tokio::test]
    async fn ws_session_stops_on_a_close_frame() {
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let args = Args::parse_from(["earner"]);

        // A Close frame ends the session loop. An offer queued AFTER the Close must
        // never be processed — ws_session breaks on Close and returns Ok(()).
        let (offer, _job_id) = job_offer_frame();
        let mut incoming = VecDeque::new();
        incoming.push_back(Ok(challenge_frame("0a1b2c3d4e5f60718293a4b5c6d7e8f9")));
        incoming.push_back(Ok(WsMessage::Close(None)));
        incoming.push_back(Ok(offer));
        let mut mock = MockSocket::new(incoming);

        ws_session(&mut mock, &args, &session).await.unwrap();

        // Only the connect-time Hello was sent; the post-Close offer was ignored.
        assert_eq!(mock.sent.len(), 1, "Close must stop the loop before the queued offer: {:?}", mock.sent);
        assert!(matches!(decode_frame(&mock.sent[0]), EarnerMsg::Hello { .. }));
    }

    #[tokio::test]
    async fn ws_session_returns_err_on_a_recv_error() {
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let args = Args::parse_from(["earner"]);

        // A transport recv error must surface as Err so run_ws logs it, backs off,
        // and reconnects. The challenge is read and Hello sent before the error.
        let mut incoming = VecDeque::new();
        incoming.push_back(Ok(challenge_frame("0a1b2c3d4e5f60718293a4b5c6d7e8f9")));
        incoming.push_back(Err(tokio_tungstenite::tungstenite::Error::Io(std::io::Error::other(
            "simulated recv error",
        ))));
        let mut mock = MockSocket::new(incoming);

        let result = ws_session(&mut mock, &args, &session).await;
        assert!(result.is_err(), "a recv error must surface as Err for run_ws to reconnect");
        assert_eq!(mock.sent.len(), 1, "Hello is sent before the recv error surfaces: {:?}", mock.sent);
        assert!(matches!(decode_frame(&mock.sent[0]), EarnerMsg::Hello { .. }));
    }

    // ---- http poll path: poll_once over a wiremock coordinator ----

    /// The canonical dev test job used by the HTTP poll-path tests.
    fn dev_job() -> JobSpec {
        JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Terrain,
            region: proto::RegionCoord { x: 1, y: -2, layer: 0 },
            deadline_secs: 120,
            max_payout_wei: "1000000000000000000".to_string(),
            inputs: serde_json::json!({}),
        }
    }

    /// `Args` whose coordinator base URL points at the mock server. The earner
    /// derives both `/jobs/next` and `/jobs/{id}/submit` from this base.
    fn args_for(server: &MockServer) -> Args {
        let mut args = Args::parse_from(["earner"]);
        args.coordinator = server.uri();
        args
    }

    #[tokio::test]
    async fn poll_once_renders_signs_submits_and_keeps_polling() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let job = dev_job();
        let job_id = job.id;

        // GET /jobs/next dispenses the one job, stamping its dispatch_seq in the
        // fence header; POST /jobs/{id}/submit accepts it.
        Mock::given(method("GET"))
            .and(path("/jobs/next"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-dispatch-seq", "7")
                    .set_body_json(&job),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = reqwest::Client::new();

        // A submit accepted with 2xx means more work may be queued → keep polling.
        let keep = poll_once(&client, &args, &session, &all_supported(), None).await.unwrap();
        assert!(keep, "a successful submit (2xx) should keep polling");

        // The earner POSTed a JobResult to /jobs/{id}/submit whose signature
        // recovers to its OWN address — the property the coordinator's submit gate
        // enforces (mirror of verify.rs recovery), exercised here over real HTTP.
        let requests = server.received_requests().await.unwrap();
        let submit = requests
            .iter()
            .find(|r| r.url.path().ends_with("/submit"))
            .expect("earner POSTed the result to the submit endpoint");
        assert_eq!(submit.url.path(), format!("/jobs/{job_id}/submit").as_str());
        // The earner echoes the dispatch_seq it received from /jobs/next (the
        // fence the coordinator checks before settling).
        assert_eq!(
            submit.headers.get("x-dispatch-seq").map(|v| v.to_str().unwrap()),
            Some("7"),
            "earner must echo the dispatch-seq header on submit"
        );

        let result: JobResult = serde_json::from_slice(&submit.body).unwrap();
        assert_eq!(result.job_id, job_id);
        assert_eq!(result.earner_address, session.address);
        assert_eq!(result.render_seconds, 1, "instant stub render charges the 1s floor over http too");
        let raw = hex::decode(&result.signature_hex).unwrap();
        assert_eq!(raw.len(), 65, "signature is 65 bytes r||s||v");
        let digest = signing_digest(&result.job_id, &result.output_hash);
        let sig = Signature::from_slice(&raw[..64]).unwrap();
        let recid = RecoveryId::from_byte(raw[64]).unwrap();
        let vk = VerifyingKey::recover_from_prehash(&digest, &sig, recid).unwrap();
        assert_eq!(address_from_verifying_key(&vk), session.address);
    }

    /// The HTTP poll authenticates its liveness refresh: a poll token, when present, is
    /// sent as `Authorization: Bearer <token>` on `/jobs/next` (the credential the
    /// coordinator requires to refresh this earner's last_seen); with no token, no
    /// Authorization header is sent at all.
    #[tokio::test]
    async fn poll_once_sends_the_poll_token_as_bearer() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();

        // No job either way — the assertion is on the OUTBOUND request headers.
        Mock::given(method("GET"))
            .and(path("/jobs/next"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::Value::Null))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = reqwest::Client::new();

        poll_once(&client, &args, &session, &all_supported(), Some("poll-tok-xyz")).await.unwrap();
        poll_once(&client, &args, &session, &all_supported(), None).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let polls: Vec<_> = requests.iter().filter(|r| r.url.path() == "/jobs/next").collect();
        assert_eq!(polls.len(), 2, "both polls hit /jobs/next");
        assert_eq!(
            polls[0].headers.get("authorization").map(|v| v.to_str().unwrap()),
            Some("Bearer poll-tok-xyz"),
            "an authenticated poll sends the token as a Bearer credential"
        );
        assert_eq!(
            polls[1].headers.get("authorization"),
            None,
            "a poll with no token sends no Authorization header"
        );
    }

    #[tokio::test]
    async fn poll_once_returns_false_when_no_job_is_available() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();

        // /jobs/next yields JSON null → no job; poll_once backs off (false) and
        // never reaches the submit path.
        Mock::given(method("GET"))
            .and(path("/jobs/next"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::Value::Null))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = reqwest::Client::new();

        let keep = poll_once(&client, &args, &session, &all_supported(), None).await.unwrap();
        assert!(!keep, "no job available → back off rather than re-poll immediately");

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.iter().all(|r| !r.url.path().ends_with("/submit")),
            "no submit must be attempted when there is no job: {requests:?}"
        );
    }

    #[tokio::test]
    async fn poll_once_returns_false_when_the_submit_is_rejected() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let job = dev_job();

        // The coordinator gates the submit with 409 (not in-flight / already done).
        // poll_once must render, submit, then back off — NOT keep polling — so a
        // persistent rejection can't spin a tight render→submit→reject loop
        // (keep_polling_after_submit). The GET carries x-dispatch-seq so poll_once
        // proceeds PAST the header fail-close (main.rs:744) into the submit path this
        // test covers — without it poll_once returns early and never reaches the 409.
        Mock::given(method("GET"))
            .and(path("/jobs/next"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-dispatch-seq", "1")
                    .set_body_json(&job),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = reqwest::Client::new();

        let keep = poll_once(&client, &args, &session, &all_supported(), None).await.unwrap();
        assert!(!keep, "a 409-rejected submit must back off, not keep polling");

        // The back-off must be the POST-SUBMIT disposition, not the header fail-close:
        // assert the submit actually happened, so a regression that skips the submit or
        // flips keep_polling_after_submit to true on a non-2xx is caught here.
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.iter().any(|r| r.url.path().ends_with("/submit")),
            "poll_once must reach the submit before backing off on the 409: {requests:?}"
        );
    }

    #[tokio::test]
    async fn poll_once_drops_an_unsupported_job_without_submitting() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        // The stateless /jobs/next hands out a DiffusionTile job, but we only
        // support Terrain. The earner must drop it: no render, no submit, back off.
        let mut job = dev_job();
        job.kind = JobKind::DiffusionTile;

        Mock::given(method("GET"))
            .and(path("/jobs/next"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-dispatch-seq", "3")
                    .set_body_json(&job),
            )
            .mount(&server)
            .await;
        // A submit would be a bug; mount a catch-all POST so one would be recorded
        // (and fail the no-submit assertion) rather than 404-ing silently.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = reqwest::Client::new();

        let keep = poll_once(&client, &args, &session, &[JobKind::Terrain], None).await.unwrap();
        assert!(!keep, "an unsupported job must back off, not keep polling");

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.iter().all(|r| !r.url.path().ends_with("/submit")),
            "no submit must be attempted for an unsupported job: {requests:?}"
        );
    }

    /// A near-maximal legitimate JobSpec — `inputs` at the coordinator's 16 KiB
    /// `MAX_INPUTS_BYTES` cap — still deserializes and dispatches through the bounded
    /// read. Proves `MAX_RESPONSE_BODY_BYTES` is a backstop, not a functional limit:
    /// the honest worst-case body sits well under the 64 KiB ceiling (FM2).
    #[tokio::test]
    async fn poll_once_accepts_a_max_sized_legitimate_body() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let mut job = dev_job();
        // ~16 KiB inputs blob: the largest `inputs` the coordinator hands out.
        job.inputs = serde_json::json!({ "blob": "x".repeat(16 * 1024) });
        let body_len = serde_json::to_vec(&job).unwrap().len();
        assert!(
            body_len > 16 * 1024 && body_len <= MAX_RESPONSE_BODY_BYTES,
            "test must exercise a large-but-legal body (got {body_len} bytes)"
        );

        Mock::given(method("GET"))
            .and(path("/jobs/next"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-dispatch-seq", "11")
                    .set_body_json(&job),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = reqwest::Client::new();

        let keep = poll_once(&client, &args, &session, &all_supported(), None).await.unwrap();
        assert!(keep, "a max-sized legitimate body must dispatch and keep polling");

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.iter().any(|r| r.url.path().ends_with("/submit")),
            "the earner must submit a result for a max-sized legitimate job: {requests:?}"
        );
    }

    /// An oversized `/jobs/next` body is rejected by the bounded read (Err) BEFORE it
    /// reaches serde — nothing is rendered or submitted. The body is a VALID JobSpec
    /// JSON, just huge (>2× the cap): the discriminator against the prior unbounded
    /// `resp.json()`, which would have buffered + deserialized + dispatched it. The
    /// limit is enforced on bytes actually read, so an under-reported Content-Length
    /// can't slip the body past (FM1/FM3).
    #[tokio::test]
    async fn poll_once_rejects_an_oversized_response_body() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let mut job = dev_job();
        job.inputs = serde_json::json!({ "blob": "x".repeat(MAX_RESPONSE_BODY_BYTES * 2) });
        assert!(
            serde_json::to_vec(&job).unwrap().len() > MAX_RESPONSE_BODY_BYTES,
            "the test body must exceed the cap to be a valid discriminator"
        );

        Mock::given(method("GET"))
            .and(path("/jobs/next"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-dispatch-seq", "13")
                    .set_body_json(&job),
            )
            .mount(&server)
            .await;
        // A submit would be a bug; mount a catch-all POST so one would be recorded
        // (and fail the no-submit assertion) rather than 404-ing silently.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = reqwest::Client::new();

        let result = poll_once(&client, &args, &session, &all_supported(), None).await;
        assert!(
            result.is_err(),
            "an oversized response body must be rejected (Err), not deserialized: {result:?}"
        );

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.iter().all(|r| !r.url.path().ends_with("/submit")),
            "nothing must be submitted when the body is rejected: {requests:?}"
        );
    }

    /// A coordinator that ACCEPTS the connection then stalls the response makes
    /// `poll_once` return `Err` (the poll loop logs + backs off) instead of
    /// hanging forever. The connect is instant (local server) and the stall is on
    /// the RESPONSE — so this proves the TOTAL request timeout covers the
    /// post-connect exchange, not just connect (FM1): a `connect_timeout`-only
    /// client would sail past a fast connect and hang here. Deterministic, not a
    /// race (FM3): the client timeout (200ms) fires far before the server's 10s
    /// delay could elapse, so the assertion is the timeout FIRING, not two
    /// durations racing. Without `.timeout()` on the client this hangs until the
    /// 10s delay, then returns `Ok` — the discriminator against an untimed client.
    #[tokio::test]
    async fn poll_once_times_out_a_stalled_coordinator() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();

        // Connection accepted, but the response is withheld for 10s — far past the
        // 200ms client timeout below. A real slowloris coordinator, in miniature.
        Mock::given(method("GET"))
            .and(path("/jobs/next"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(dev_job())
                    .set_delay(Duration::from_secs(10)),
            )
            .mount(&server)
            .await;

        let args = args_for(&server);
        // A test-only small total timeout: the assertion is the timeout firing,
        // not the production 45s elapsing. Connect timeout is irrelevant here (the
        // local connect is instant); the TOTAL timeout is what trips on the stall.
        let client = http_client(Duration::from_millis(200), Duration::from_secs(10)).unwrap();

        let result = poll_once(&client, &args, &session, &all_supported(), None).await;
        assert!(
            result.is_err(),
            "a stalled coordinator response must time out as Err, not hang: {result:?}"
        );
    }

    /// A fast normal response dispatches byte-identically under the PRODUCTION
    /// timeouts — the cap is a liveness backstop, not a functional limit (FM2). If
    /// the timeout sheared honest traffic this would fail; it doesn't, because a
    /// prompt `/jobs/next` completes in milliseconds, orders of magnitude under
    /// the 45s ceiling. Exercises the real `HTTP_REQUEST_TIMEOUT`/`HTTP_CONNECT_TIMEOUT`
    /// consts (the `.unwrap()` also proves they build a valid client).
    #[tokio::test]
    async fn poll_once_dispatches_a_fast_response_under_the_production_timeout() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();
        let job = dev_job();
        let job_id = job.id;

        Mock::given(method("GET"))
            .and(path("/jobs/next"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-dispatch-seq", "5")
                    .set_body_json(&job),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = http_client(HTTP_REQUEST_TIMEOUT, HTTP_CONNECT_TIMEOUT).unwrap();

        let keep = poll_once(&client, &args, &session, &all_supported(), None).await.unwrap();
        assert!(keep, "a fast response under the production timeout must dispatch + keep polling");

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.iter().any(|r| r.url.path() == format!("/jobs/{job_id}/submit").as_str()),
            "the earner must submit the rendered result for a fast normal response: {requests:?}"
        );
    }

    // ---- http register path: register over a wiremock coordinator ----

    #[tokio::test]
    async fn register_posts_hello_and_succeeds_on_2xx() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();

        Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(ResponseTemplate::new(200).insert_header("x-poll-token", "issued-tok-123"))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = reqwest::Client::new();

        let poll_token = register(&client, &args, &session).await.unwrap();
        assert_eq!(
            poll_token.as_deref(),
            Some("issued-tok-123"),
            "register captures the issued poll token from the x-poll-token response header"
        );

        // The earner POSTed its Hello announcing address, GPU, and supported kinds.
        let requests = server.received_requests().await.unwrap();
        let reg = requests
            .iter()
            .find(|r| r.url.path() == "/register")
            .expect("earner POSTed a Hello to /register");
        match serde_json::from_slice::<EarnerMsg>(&reg.body).unwrap() {
            EarnerMsg::Hello { earner_address, gpu_model, vram_gb, supported, .. } => {
                assert_eq!(earner_address, session.address);
                assert_eq!(gpu_model, args.gpu_model);
                assert_eq!(vram_gb, args.vram_gb);
                assert_eq!(supported, all_supported());
            }
            other => panic!("register must POST a Hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_errors_on_non_2xx() {
        let server = MockServer::start().await;
        let session = Session::from_hex(DEV_SESSION_KEY).unwrap();

        // A 5xx from the coordinator surfaces as Err (error_for_status). run_http
        // treats a failed register as non-fatal: it logs and falls through to
        // polling, so this Err must not be silently swallowed at the source.
        Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let args = args_for(&server);
        let client = reqwest::Client::new();

        assert!(
            register(&client, &args, &session).await.is_err(),
            "a non-2xx register response must surface as Err"
        );
    }

    /// The inbound cap binds BOTH the message and the single-frame size to
    /// MAX_INBOUND_FRAME_BYTES. Pinning both is the point: a per-frame cap alone
    /// would still let a message split across many frames grow to the message cap.
    #[test]
    fn ws_config_caps_both_inbound_message_and_frame_size() {
        let cfg = ws_config();
        assert_eq!(cfg.max_message_size, Some(MAX_INBOUND_FRAME_BYTES));
        assert_eq!(cfg.max_frame_size, Some(MAX_INBOUND_FRAME_BYTES));
    }

    /// End-to-end proof that `ws_config()` ENFORCES the cap on the wire, not merely
    /// that its fields hold the right values: an ephemeral server sends one text frame
    /// just over MAX_INBOUND_FRAME_BYTES; a client connected with `ws_config()` errors
    /// on recv (frame too large), where a client on tungstenite's DEFAULT config (64
    /// MiB) decodes the SAME frame fine. The baseline is the discriminator — it proves
    /// the rejection is our cap, not a malformed or absolutely-oversized frame. Drop
    /// the `Some(ws_config())` (revert to the default) and the capped assertion fails.
    #[tokio::test]
    async fn ws_config_rejects_an_oversized_inbound_frame() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Each accepted connection gets one oversized text frame, in its own task so
        // the two client connections below are served concurrently (no head-of-line
        // block while a handler holds its socket open).
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    if let Ok(mut ws) = accept_async(stream).await {
                        let oversized = "x".repeat(MAX_INBOUND_FRAME_BYTES + 1);
                        let _ = ws.send(WsMessage::text(oversized)).await;
                        let _ = ws.flush().await;
                        let _ = ws.next().await; // hold open until the client reacts
                    }
                });
            }
        });
        let url = format!("ws://{addr}");

        let (mut capped, _) =
            tokio_tungstenite::connect_async_with_config(&url, Some(ws_config()), false)
                .await
                .unwrap();
        let capped_recv = capped.next().await.expect("a frame or error arrives");
        assert!(
            capped_recv.is_err(),
            "ws_config() must shed the oversized inbound frame, got {capped_recv:?}",
        );

        let (mut uncapped, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let msg = uncapped
            .next()
            .await
            .expect("a frame arrives")
            .expect("the default 64 MiB config accepts the same frame the cap rejected");
        assert_eq!(
            msg.len(),
            MAX_INBOUND_FRAME_BYTES + 1,
            "baseline decodes the exact frame the cap rejected — so the cap is the discriminator",
        );
    }

    #[test]
    fn estimate_progress_is_elapsed_over_deadline_capped_at_99() {
        // elapsed/deadline as a percent: 0 at the start, 50 at the half, CAPPED at 99
        // at/after the deadline — 100 is reserved for "done", which Submit signals, so a
        // heartbeat never claims completion.
        assert_eq!(estimate_progress(0, 100), 0, "no time elapsed -> 0");
        assert_eq!(estimate_progress(50, 100), 50, "half the deadline -> 50");
        assert_eq!(estimate_progress(99, 100), 99);
        assert_eq!(estimate_progress(100, 100), 99, "at the deadline caps at 99, not 100");
        assert_eq!(estimate_progress(10_000, 100), 99, "past the deadline stays capped at 99");
    }

    #[test]
    fn estimate_progress_guards_zero_deadline_and_overflow() {
        // A zero-deadline job reports 0 rather than dividing by zero, and a pathological
        // elapsed cannot overflow the *100 or wrap the u8 (saturating mul, then cap 99).
        assert_eq!(estimate_progress(30, 0), 0, "zero deadline -> 0, no divide-by-zero");
        assert_eq!(estimate_progress(u64::MAX, 1), 99, "saturating + capped, no overflow/panic");
    }

    #[test]
    fn estimate_progress_is_monotonic_non_decreasing_in_elapsed() {
        let mut prev = 0u8;
        for elapsed in [0u64, 5, 10, 25, 50, 75, 100, 200, 1_000] {
            let p = estimate_progress(elapsed, 100);
            assert!(p >= prev, "progress must not decrease as elapsed grows ({p} < {prev})");
            prev = p;
        }
    }

    #[test]
    fn render_seconds_charged_floors_at_one() {
        // A completed job that elapsed under a second still did work — the coordinator
        // rejects render_seconds == 0 (ZeroRenderSeconds), so the charge floors to 1.
        assert_eq!(render_seconds_charged(0), 1, "sub-second render still charges 1s");
        assert_eq!(render_seconds_charged(1), 1);
    }

    #[test]
    fn render_seconds_charged_passes_through_real_elapsed() {
        // Above the floor the charge is the elapsed seconds verbatim — in SECONDS, the
        // unit total_render_seconds and the per-second rate both denominate in.
        assert_eq!(render_seconds_charged(50), 50);
        assert_eq!(render_seconds_charged(3600), 3600);
    }

    #[test]
    fn render_seconds_charged_saturates_the_u32_cast() {
        // A pathologically long elapsed cannot wrap the u32 the wire and the charge use.
        assert_eq!(render_seconds_charged(u32::MAX as u64), u32::MAX);
        assert_eq!(render_seconds_charged(u32::MAX as u64 + 1), u32::MAX, "no wrap past u32::MAX");
        assert_eq!(render_seconds_charged(u64::MAX), u32::MAX);
    }

    #[tokio::test(start_paused = true)]
    async fn render_with_heartbeats_reports_progress_from_elapsed_over_deadline() {
        // The heartbeat beat branch — never exercised before this slice (every
        // handle_offer test pairs an instant render with a 3600s interval, so no beat
        // ever fired). A never-completing render lets beats fire under virtual time:
        // deadline 100s, 20s interval => beats at elapsed 20/40/60s => progress 20/40/60.
        // The old hardcoded `progress_pct: 0` would emit all zeros; a wrong denominator
        // would emit wrong values. A virtual sleep bounds the drive so the test ends.
        let mut sink = RecordingSink::default();
        let job_id = Uuid::new_v4();
        let started = tokio::time::Instant::now();
        let render = std::future::pending::<Result<Vec<u8>>>();
        tokio::select! {
            _ = render_with_heartbeats(&mut sink, job_id, started, 100, 20, render) => {
                unreachable!("a pending render never completes")
            }
            _ = tokio::time::sleep(Duration::from_secs(65)) => {}
        }

        let progresses: Vec<u8> = sink
            .sent
            .iter()
            .map(|f| match decode_frame(f) {
                EarnerMsg::Heartbeat { job_id: jid, progress_pct } => {
                    assert_eq!(jid, Some(job_id), "each beat names the rendering job");
                    progress_pct
                }
                other => panic!("expected only Heartbeat frames mid-render, got {other:?}"),
            })
            .collect();
        assert_eq!(
            progresses,
            vec![20, 40, 60],
            "each beat reports elapsed/deadline progress, not the old hardcoded 0",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn render_with_heartbeats_charges_real_elapsed_render_seconds() {
        // render_seconds is charged from the SAME `started` clock the heartbeat reads,
        // so a render that completes after virtual time elapses charges that elapsed —
        // not the old hardcoded 1 — and a slower render charges more. deadline 1000s /
        // interval 3600s means no beat fires (the render finishes first), isolating the
        // render_seconds measurement; both durations stay under the coordinator's
        // deadline*2 plausibility bound.
        async fn charge_after(secs: u64) -> u32 {
            let mut sink = RecordingSink::default();
            let started = tokio::time::Instant::now();
            let render = async move {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                Ok(vec![7u8; 8])
            };
            let out = render_with_heartbeats(&mut sink, Uuid::new_v4(), started, 1000, 3600, render)
                .await
                .unwrap();
            assert_eq!(out, vec![7u8; 8], "the rendered bytes flow through unchanged");
            render_seconds_charged(started.elapsed().as_secs())
        }

        assert_eq!(charge_after(50).await, 50, "a 50s render charges 50 render-seconds, not 1");
        assert_eq!(charge_after(120).await, 120, "a slower render charges more");
    }
}
