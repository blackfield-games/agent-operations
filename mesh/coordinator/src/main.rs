//! BLACKFIELD coordinator — accepts earner registrations, dispatches jobs,
//! collects results, relays validated receipts to RenderReceipts.sol on Base.
//!
//! v0: HTTP-only stub. v1: replace `/jobs/next` poll with a websocket fanout.
//! v2: federated coordinators (research-earner-client.md flagged libp2p
//!     gossipsub as experimental; revisit when sub-200ms dispatch matters).

use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use clap::Parser;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use proto::{CoordinatorMsg, EarnerMsg, JobKind, JobResult, JobSpec, RegionCoord};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tower::ServiceBuilder;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

/// Placeholder EAS attestation UID returned on acceptance until the real
/// RenderReceipts.sol relay lands (later task). 32 zero bytes, 0x-prefixed.
const PLACEHOLDER_ATTESTATION_UID: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000000";

/// HTTP-poll fence header. `GET /jobs/next` returns the dispatched job's
/// `dispatch_seq` here; the earner echoes it on `POST /jobs/{id}/submit` so the
/// coordinator rejects a submit whose lease was reaped and reassigned. This is
/// best-effort: a stateless submit can't be bound to a session, and the seq is a
/// small integer an adversary could guess — it closes the *accidental* race for
/// honest earners. The websocket path (coordinator-remembered seq) is the
/// authoritative fence; HTTP poll is the legacy/dev transport.
const DISPATCH_SEQ_HEADER: &str = "x-dispatch-seq";

mod eas;
mod meter;
mod relay;
mod store;
mod validate;
mod verify;

use meter::{SpendError, Spender};
use relay::{BatchRelayError, Relay, RelayError, ALREADY_ISSUED_UID};
use store::Store;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "COORDINATOR_BIND", default_value = "127.0.0.1:8787")]
    bind: String,
    /// Path to the SQLite database backing the job queue + results. Created on
    /// first run; a restart reloads queue/completed state from it.
    #[arg(long, env = "COORDINATOR_DB", default_value = "coordinator.db")]
    db: String,
    /// How often (seconds) the background reaper scans for in-flight jobs whose
    /// deadline has elapsed and requeues them.
    #[arg(long, env = "COORDINATOR_REAP_INTERVAL_SECS", default_value = "5")]
    reap_interval_secs: u64,
    /// Maximum number of dispatch attempts before a job is dead-lettered into
    /// the terminal `failed` state and removed from the active queue forever.
    #[arg(long, env = "COORDINATOR_MAX_ATTEMPTS", default_value = "5")]
    max_attempts: u32,
    /// Maximum number of EARNER-fault result rejects (bad signature, malformed or
    /// implausible content, submit-protocol violation) a single job tolerates
    /// before it is dead-lettered. Earner faults do NOT consume the `max_attempts`
    /// renderability budget (a faulty earner shouldn't burn a renderable job), so
    /// this separate, more generous cap is the backstop that still terminates a
    /// poison job (a spec no connected earner can satisfy).
    #[arg(long, env = "COORDINATOR_MAX_FAULTS", default_value = "10")]
    max_faults: u32,
    /// An earner is counted in `/stats` and kept in the in-memory registry only
    /// while it has been seen within this many seconds. Refreshed on Hello, on
    /// any websocket frame, on a periodic liveness tick while a ws earner is
    /// idle, and on an authenticated HTTP submit. Past it, the reaper prunes it.
    #[arg(long, env = "COORDINATOR_EARNER_TTL_SECS", default_value = "60")]
    earner_ttl_secs: u64,
    /// Absolute wall-clock TTL as a multiple of each job's own `deadline_secs`: the
    /// reaper dead-letters a non-terminal job older than `created_at` plus
    /// `deadline_secs × this`. The poison-job backstop for a job a single earner
    /// keeps faulting on (never reaching `max_faults`, which needs distinct
    /// earners). Default 1440 (~24h at the 60s default deadline) is ~100× the
    /// `max_attempts + max_faults` retry-churn budget, so a healthy job never trips
    /// it. Must be >= 1; a small value collapses the TTL toward the bare deadline
    /// (aggressive — a job a legitimate redispatch would finish may be reaped).
    #[arg(long, env = "COORDINATOR_TTL_DEADLINE_MULTIPLE", default_value_t = JOB_TTL_DEADLINE_MULTIPLE)]
    ttl_deadline_multiple: u32,
    /// Seconds a TERMINAL (done/failed) job's record is retained before the
    /// background sweep deletes it (with its result + already-relayed
    /// attestation/debit rows). The asymptotic bound on the otherwise-unbounded
    /// `jobs` history: the covering `/stats` indexes only shrank the constant
    /// factor, retention caps the row count itself. A job is pruned only once it is
    /// older than this AND its on-chain receipt/debit have been relayed (a still-
    /// pending obligation keeps the record; with no relayer configured, settled jobs
    /// are retained and only failed jobs prune). Generous default (~30 days) — a
    /// storage backstop, not a tuned limit; widen it if `/stats` must report a
    /// longer window. UNDER RETENTION the `/stats` lifetime aggregates (completed/
    /// failed counts, attempts/faults, render-seconds, payout) and the `/earners`
    /// economics reflect the RETAINED WINDOW, not all-time, and no longer only grow.
    /// Must be >= 1 (0 would delete every terminal record on the first sweep).
    #[arg(long, env = "COORDINATOR_RETENTION_SECS", default_value_t = DEFAULT_RETENTION_SECS)]
    retention_secs: u64,
    /// Seconds the coordinator waits for a connecting ws client to complete the
    /// registration handshake (read the challenge, send a valid Hello) before
    /// closing the socket. Bounds an unauthenticated slowloris that would
    /// otherwise hold a `ws_session` task + FD open forever. An honest earner
    /// replies in milliseconds, so the default is generous; must be >= 1.
    #[arg(long, env = "COORDINATOR_HANDSHAKE_TIMEOUT_SECS", default_value_t = DEFAULT_HANDSHAKE_TIMEOUT.as_secs())]
    handshake_timeout_secs: u64,
    /// Seconds of read-idle (no inbound ws frame — including the pong to the
    /// coordinator's keepalive ping) tolerated on an ESTABLISHED earner session
    /// before the socket is closed. The post-Hello twin of `--handshake-timeout-secs`:
    /// that bounds the pre-Hello handshake; this bounds a session that completed
    /// Hello then went silent (a half-open/vanished peer that would otherwise hold a
    /// `ws_session` task + FD until ~2h OS TCP keepalive or a max-connections
    /// eviction). The coordinator pings at half this bound, so a live earner — even
    /// idle between jobs, which sends no application frames — auto-pongs and is never
    /// closed; an in-flight job's heartbeats reset it too. Only a peer that stops
    /// responding trips it. Must be >= 1; generous by default.
    #[arg(long, env = "COORDINATOR_SESSION_IDLE_TIMEOUT_SECS", default_value_t = DEFAULT_SESSION_IDLE_TIMEOUT.as_secs())]
    session_idle_timeout_secs: u64,
    /// Seconds the coordinator waits for a connection to send its complete HTTP
    /// request headers before closing it. Bounds a pre-routing slow-headers
    /// slowloris that parks an FD before any handler runs (one layer below the ws
    /// handshake timeout). Applies to every endpoint; must be >= 1.
    #[arg(long, env = "COORDINATOR_HTTP_HEADER_TIMEOUT_SECS", default_value_t = DEFAULT_HTTP_HEADER_TIMEOUT.as_secs())]
    http_header_timeout_secs: u64,
    /// Seconds the coordinator waits for a body-bearing mutating request
    /// (`POST /register`, `POST /jobs/{id}/submit`) to deliver its complete body
    /// before responding `408 Request Timeout` and closing the connection. Bounds
    /// a post-headers slow-body slowloris (the header timeout disarms once headers
    /// parse). The `/ws` upgrade and the GET routes carry no request body and are
    /// unaffected. Must be >= 1.
    #[arg(long, env = "COORDINATOR_HTTP_BODY_TIMEOUT_SECS", default_value_t = DEFAULT_HTTP_BODY_TIMEOUT.as_secs())]
    http_body_timeout_secs: u64,
    /// Maximum number of concurrently served connections. A backstop against a
    /// connection flood that opens sockets faster than the header/body timeouts
    /// close them (which would otherwise grow the task/FD count without bound).
    /// When the cap is reached a newly accepted connection is closed immediately
    /// (the accept loop is never blocked). Set WELL above peak concurrent earners
    /// — a fan-in of many earners at once is the designed load, so a too-low cap
    /// throttles honest traffic. The primary flood defense is still edge / OS FD
    /// limits; this is a process-level backstop. Must be >= 1.
    #[arg(long, env = "COORDINATOR_MAX_CONNECTIONS", default_value_t = DEFAULT_MAX_CONNECTIONS)]
    max_connections: usize,
    /// How often (seconds) the attestation relayer drains pending EAS receipts to
    /// the chain. Only runs when a relay is configured (see `--relay-dev-mock`).
    #[arg(long, env = "COORDINATOR_RELAY_INTERVAL_SECS", default_value = "10")]
    relay_interval_secs: u64,
    /// LOCAL DEV ONLY: drain pending attestations to an in-process mock relay
    /// instead of the chain. Exercises the full drain path (claim → submit →
    /// mark) without an RPC, a signer, or gas. The live Base relayer (an RPC
    /// provider + an authorized coordinator EAS signer) is operator-gated; with
    /// this flag off (the default) pending receipts accumulate, surfaced at
    /// `/stats pending_attestations`.
    #[arg(long, env = "COORDINATOR_RELAY_DEV_MOCK", default_value = "false")]
    relay_dev_mock: bool,
    /// Max receipts the attestation relayer batches into one `issueReceipts`
    /// (`EAS.multiAttest`) call per drain step. `issueReceipts` is atomic — every
    /// element runs in one tx — so the cap keeps a batch under the block gas limit:
    /// each element costs the per-receipt work plus its region fee-route, so an
    /// unbounded batch eventually exceeds the block gas and always reverts. The
    /// drain claims at most this many, submits one batch, and on a revert falls
    /// back to per-receipt single submits; the remaining backlog drains over the
    /// next steps/ticks. 32 leaves wide headroom under Base's block gas; raise it to
    /// amortize a deeper backlog, lower it if per-element cost grows. Must be >= 1.
    #[arg(long, env = "COORDINATOR_RELAY_BATCH_SIZE", default_value_t = DEFAULT_RELAY_BATCH_SIZE)]
    relay_batch_size: usize,
    /// How often (seconds) the debit relayer drains pending ComputeMeter debits to
    /// the chain. Only runs when a spender is configured (see `--spender-dev-mock`).
    #[arg(long, env = "COORDINATOR_SPENDER_INTERVAL_SECS", default_value = "10")]
    spender_interval_secs: u64,
    /// LOCAL DEV ONLY: drain pending debits to an in-process mock spender instead
    /// of the chain. Exercises the full drain path (claim → spend → mark) without
    /// an RPC, a signer, or gas. The live Base spender (an RPC provider + an
    /// authorized ComputeMeter spender key) is operator-gated; it targets
    /// `ComputeMeter.spendOnce` (the idempotent entry point — see `meter.rs`). With
    /// this flag off (the default) pending debits accumulate, surfaced at
    /// `/stats pending_debits`.
    #[arg(long, env = "COORDINATOR_SPENDER_DEV_MOCK", default_value = "false")]
    spender_dev_mock: bool,
    /// Shared-secret bearer token gating `POST /jobs` ingestion. When SET, a
    /// job-creation request must carry `Authorization: Bearer <token>` (compared
    /// in constant time); a request that is absent, malformed, or carries the
    /// wrong token is rejected `401`. When UNSET the endpoint stays open (dev
    /// default) and `main` logs a loud startup warning, mirroring the
    /// `--relay-dev-mock` posture. A blank/whitespace value is rejected at startup
    /// (an empty secret would authenticate every caller). The real token is an
    /// operator credential supplied at deploy.
    #[arg(long, env = "COORDINATOR_INGEST_TOKEN")]
    ingest_token: Option<String>,
    /// Maximum number of `queued` jobs the runtime ingestion endpoint
    /// (`POST /jobs`) will admit. At the cap a new create is rejected `503` (a
    /// retryable backpressure signal — a dispatched or reaped job frees a slot),
    /// so a flood of cheap valid jobs can't grow the backlog without bound
    /// (disk-fill, slower `take_next` scans, honest-job FIFO starvation). The
    /// boot-time seed and crash-recovery requeue are exempt (they don't go through
    /// this path). Must be >= 1. See [`DEFAULT_MAX_QUEUED_JOBS`].
    #[arg(long, env = "COORDINATOR_MAX_QUEUED_JOBS", default_value_t = DEFAULT_MAX_QUEUED_JOBS)]
    max_queued_jobs: usize,
    /// Maximum number of earners kept in the in-memory registry. Registration is
    /// signature-gated for identity but a fresh keypair is free, so without a cap
    /// cheap distinct signed Hellos inflate the map without bound (O(n) `/earners` +
    /// `/stats` scans, an oversized `/earners` response). At the cap a NEW
    /// registration evicts the stalest earner already past its TTL to make room; if
    /// every entry is currently live (a genuinely full fleet) it is rejected `503`
    /// (HTTP) / the socket is closed (WS). Re-registration of an already-known earner
    /// is an in-place upsert and never counts against the cap. Set WELL above peak
    /// fleet size — it is a backstop, not a tuned limit. Must be >= 1.
    #[arg(long, env = "COORDINATOR_MAX_EARNERS", default_value_t = DEFAULT_MAX_EARNERS)]
    max_earners: usize,
    /// Maximum registrations a single source IP may make per
    /// `REGISTRATION_WINDOW_SECS` (a token bucket, so the value is also the burst
    /// ceiling). Checked BEFORE the signature verify on both the HTTP `/register` and
    /// WS `Hello` paths, so an over-limit source is shed (`429` / socket close) before
    /// any secp256k1 recovery — bounding the registry-cap sustained-lockout lever
    /// without spending the expensive op on a flood. Keyed on the source the
    /// connection arrives from — the peer IP, or the real client recovered from
    /// `X-Forwarded-For` when the peer is a `--trusted-proxies` entry; behind an
    /// untrusted hop it is the peer (edge/OS remains the primary flood defense).
    /// Set WELL above honest per-source burst; must be >= 1. See
    /// [`DEFAULT_MAX_REGISTRATIONS`].
    #[arg(long, env = "COORDINATOR_MAX_REGISTRATIONS", default_value_t = DEFAULT_MAX_REGISTRATIONS)]
    max_registrations: u32,
    /// Reverse-proxy IPs or CIDR ranges whose `X-Forwarded-For` header is trusted to
    /// name the real client for per-source registration rate-limiting. Each entry is a
    /// bare IP (`10.0.0.1`, `2001:db8::1`) or a CIDR (`10.0.0.0/8`, `2001:db8::/32`);
    /// comma-separated, or repeat the flag. EMPTY by default: with no trusted proxy,
    /// XFF is ignored and the limiter keys on the raw connection peer, so a direct
    /// (untrusted) client can never spoof its source via a forged XFF. List a proxy
    /// here ONLY if it appends the address it observed to XFF; the limiter then keys on
    /// the rightmost XFF hop that is not itself a listed proxy/range (the address the
    /// trust boundary saw). A malformed entry is rejected at startup. See
    /// [`DEFAULT_MAX_REGISTRATIONS`] for the edge/OS-vs-app-layer rate-limit layering.
    #[arg(long, env = "COORDINATOR_TRUSTED_PROXIES", value_delimiter = ',')]
    trusted_proxies: Vec<String>,
    /// Wei charged per render-second to a job's buyer at settle, recorded as a
    /// pending ComputeMeter debit for the (operator-gated) on-chain relayer to
    /// spend. `0` (the default) DISABLES metering — no debit row is written — so
    /// the feature is opt-in: a deploy charges real buyer credit only once this is
    /// set to the real economic rate. Only jobs ingested with a `buyer` are
    /// metered; an unattributed job is never charged. See [`DEFAULT_COMPUTE_RATE_WEI`].
    #[arg(long, env = "COORDINATOR_COMPUTE_RATE_WEI", default_value_t = DEFAULT_COMPUTE_RATE_WEI)]
    compute_rate_wei: u128,
}

/// A registered earner's capabilities, recorded on `EarnerMsg::Hello`.
#[derive(Debug, Clone)]
struct EarnerInfo {
    /// GPU model the earner advertised at registration; surfaced in `/earners`.
    gpu_model: String,
    vram_gb: u32,
    supported: Vec<JobKind>,
    /// Unix epoch seconds of the last sign of life from this earner.
    last_seen: i64,
}

impl EarnerInfo {
    /// True if this earner has been seen within `ttl_secs` of `now` (both epoch
    /// seconds). Saturating sub so a backward clock step can't underflow.
    fn is_live(&self, now: i64, ttl_secs: i64) -> bool {
        now.saturating_sub(self.last_seen) <= ttl_secs
    }
}

struct AppState {
    /// Job queue + completed results, persisted to SQLite so they survive a
    /// restart. Earner registrations stay in-memory by design.
    store: Mutex<Store>,
    earners: Mutex<HashMap<String, EarnerInfo>>,
    /// Maximum dispatch attempts before a job is dead-lettered to `failed`.
    /// Mirrors `--max-attempts` / `COORDINATOR_MAX_ATTEMPTS`.
    max_attempts: u32,
    /// Maximum earner-fault rejects a job tolerates before dead-lettering, on a
    /// budget separate from `max_attempts` (earner faults don't burn the
    /// renderability budget). Mirrors `--max-faults` / `COORDINATOR_MAX_FAULTS`.
    max_faults: u32,
    /// How long (seconds) an earner stays counted in `/stats` after its last
    /// sign of life. Mirrors `--earner-ttl-secs` / `COORDINATOR_EARNER_TTL_SECS`.
    earner_ttl_secs: i64,
    /// Absolute wall-clock TTL as a multiple of each job's own `deadline_secs`:
    /// the poison-job backstop dead-letters a non-terminal job older than
    /// `created_at + deadline_secs * ttl_deadline_multiple`. Mirrors
    /// `--ttl-deadline-multiple` / `COORDINATOR_TTL_DEADLINE_MULTIPLE`.
    ttl_deadline_multiple: u32,
    /// Seconds a terminal (done/failed) job's record is retained before the
    /// retention sweep deletes it and its dependents. Read by the reaper's
    /// `prune_terminal_history`. Validated >= 1 in `with_store`. Mirrors
    /// `--retention-secs` / `COORDINATOR_RETENTION_SECS`.
    retention_secs: i64,
    /// Wall-clock bound on the ws registration handshake (challenge→Hello). The
    /// gate that closes a slowloris connection; read by `recv_hello`. Mirrors
    /// `--handshake-timeout-secs` / `COORDINATOR_HANDSHAKE_TIMEOUT_SECS`.
    handshake_timeout: Duration,
    /// Read-idle bound on an ESTABLISHED post-Hello ws session: the socket is
    /// closed if no inbound frame (including the pong to the keepalive ping) lands
    /// within this window. The post-Hello twin of `handshake_timeout`; read by
    /// `ws_session`. Validated > 0 in `with_store`. Mirrors
    /// `--session-idle-timeout-secs` / `COORDINATOR_SESSION_IDLE_TIMEOUT_SECS`.
    session_idle_timeout: Duration,
    /// Optional shared-secret bearer token gating `POST /jobs`. `Some` → ingestion
    /// requires `Authorization: Bearer <token>` (constant-time compared in
    /// `create_job`); `None` → the endpoint is open (dev), warned at startup.
    /// Validated non-blank in `with_store`. Mirrors `--ingest-token` /
    /// `COORDINATOR_INGEST_TOKEN`.
    ingest_token: Option<String>,
    /// Cap on the `queued` backlog admitted via `POST /jobs`; a create at the cap
    /// is rejected `503`. Validated >= 1 in `with_store`. Mirrors
    /// `--max-queued-jobs` / `COORDINATOR_MAX_QUEUED_JOBS`.
    max_queued_jobs: usize,
    /// Cap on the in-memory earner registry size; a new registration at the cap
    /// evicts the stalest past-TTL earner or, if all are live, is rejected. Read by
    /// the registration seam (`admit_earner`). Validated >= 1 in `with_store`.
    /// Mirrors `--max-earners` / `COORDINATOR_MAX_EARNERS`.
    max_earners: usize,
    /// Per-source registration token buckets, keyed on the connection peer IP. The
    /// rate-limit seam (`check_registration_rate`) refills + consumes under this lock
    /// on both registration paths; bounded at [`MAX_REGISTRATION_BUCKETS`].
    registration_buckets: Mutex<HashMap<IpAddr, RateBucket>>,
    /// Per-source registrations allowed per [`REGISTRATION_WINDOW_SECS`]. Read by the
    /// registration rate-limit seam. Validated >= 1 in `with_store`. Mirrors
    /// `--max-registrations` / `COORDINATOR_MAX_REGISTRATIONS`.
    max_registrations: u32,
    /// Reverse-proxy IPs whose `X-Forwarded-For` the registration limiter trusts to
    /// name the real source (otherwise it keys on the connection peer). Empty (the
    /// default) => XFF is ignored everywhere. Read by `resolve_source_ip` on both
    /// registration paths. Mirrors `--trusted-proxies` / `COORDINATOR_TRUSTED_PROXIES`.
    trusted_proxies: TrustedProxies,
}

/// The non-store construction knobs for [`AppState::with_store`], named so a long
/// list of same-typed positional args (several `u32`/`usize`, plus an `i64`) can't
/// be transposed into a silently mis-wired limit that still compiles. Each field
/// mirrors the like-named [`AppState`] field and its `--flag` / `COORDINATOR_*` env
/// knob; see those for the full semantics.
struct StoreConfig {
    max_attempts: u32,
    max_faults: u32,
    earner_ttl_secs: i64,
    ttl_deadline_multiple: u32,
    retention_secs: i64,
    handshake_timeout: Duration,
    session_idle_timeout: Duration,
    ingest_token: Option<String>,
    max_queued_jobs: usize,
    max_earners: usize,
    max_registrations: u32,
    trusted_proxies: TrustedProxies,
}

impl StoreConfig {
    /// Reject the zero/blank knob values that each turn a safety bound into an
    /// outage: a 0 `ttl_deadline_multiple` makes every job's TTL `deadline * 0 == 0`,
    /// so the reaper dead-letters every non-terminal job on its first tick; a 0
    /// `handshake_timeout` fires `timeout(ZERO, …)` immediately, rejecting every
    /// connection before its Hello; a blank `ingest_token` matches `Authorization:
    /// Bearer ` and so authenticates every caller (leave it UNSET for the dev-open
    /// posture, not blank); a 0 `max_queued_jobs`/`max_earners`/`max_registrations`
    /// sheds all ingestion/registration the moment the first job/earner/bucket lands;
    /// a 0 `retention_secs` sets the cutoff to `now`, so the first retention sweep
    /// deletes every terminal job record.
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.ttl_deadline_multiple > 0,
            "ttl_deadline_multiple must be >= 1 (0 would dead-letter every job on the first reap tick)"
        );
        anyhow::ensure!(
            self.retention_secs > 0,
            "retention_secs must be >= 1 (0 would delete every terminal job record on the first retention sweep)"
        );
        anyhow::ensure!(
            !self.handshake_timeout.is_zero(),
            "handshake_timeout must be > 0 (0 would reject every registration before its Hello)"
        );
        anyhow::ensure!(
            !self.session_idle_timeout.is_zero(),
            "session_idle_timeout must be > 0 (0 would close every established session on its first idle check)"
        );
        if let Some(token) = self.ingest_token.as_deref() {
            anyhow::ensure!(
                !token.trim().is_empty(),
                "ingest_token must be non-blank if set (an empty/whitespace token authenticates every caller — leave --ingest-token unset for the dev-open posture instead)"
            );
        }
        anyhow::ensure!(
            self.max_queued_jobs > 0,
            "max_queued_jobs must be >= 1 (0 would reject every job ingestion)"
        );
        anyhow::ensure!(
            self.max_earners > 0,
            "max_earners must be >= 1 (0 would reject every earner registration)"
        );
        anyhow::ensure!(
            self.max_registrations > 0,
            "max_registrations must be >= 1 (0 would reject every earner registration)"
        );
        Ok(())
    }
}

impl AppState {
    /// Build state backed by `store`, configured by `cfg`. Seeds one job only when
    /// the DB has no jobs yet, so a fresh DB gives earners something to do while a
    /// restart with existing jobs does NOT double-seed; also reclaims jobs left
    /// `in_flight` by a previous crash before deciding whether to seed.
    fn with_store(store: Store, cfg: StoreConfig) -> Result<Arc<Self>> {
        cfg.validate()?;
        // Reclaim any jobs left `in_flight` by a previous crash before we decide
        // whether to seed: a recovered job means the queue is not empty.
        let recovered = store.recover_in_flight()?;
        if recovered > 0 {
            tracing::info!(recovered, "reclaimed in-flight jobs orphaned by a crash");
        }
        if store.jobs_empty()? {
            for job in seed_jobs() {
                store.enqueue(&job)?;
            }
        }
        Ok(Arc::new(AppState {
            store: Mutex::new(store),
            earners: Mutex::new(HashMap::new()),
            max_attempts: cfg.max_attempts,
            max_faults: cfg.max_faults,
            earner_ttl_secs: cfg.earner_ttl_secs,
            ttl_deadline_multiple: cfg.ttl_deadline_multiple,
            retention_secs: cfg.retention_secs,
            handshake_timeout: cfg.handshake_timeout,
            session_idle_timeout: cfg.session_idle_timeout,
            ingest_token: cfg.ingest_token,
            max_queued_jobs: cfg.max_queued_jobs,
            max_earners: cfg.max_earners,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: cfg.max_registrations,
            trusted_proxies: cfg.trusted_proxies,
        }))
    }
}

/// Lifecycle status of a single job, for the HUD + observability.
#[derive(Debug, Serialize)]
struct JobStatusResponse {
    id: Uuid,
    status: String,
}

/// One row of the `GET /jobs` listing: the HUD/ops recent-jobs view.
#[derive(Debug, Serialize)]
struct JobSummary {
    id: Uuid,
    kind: JobKind,
    status: String,
}

/// Full detail for `GET /jobs/{id}`: the job's `JobSpec` plus its recorded
/// `JobResult` once the job has completed (`null` until then).
#[derive(Debug, Serialize)]
struct JobDetail {
    spec: JobSpec,
    result: Option<JobResult>,
}

/// Query string for `GET /jobs`. `status`, when present, filters the listing to
/// a single lifecycle status; absent returns all statuses.
#[derive(Debug, Deserialize)]
struct ListJobsQuery {
    status: Option<String>,
}

/// Request body for `POST /jobs` — the runtime job-ingestion front door (the
/// agents pipeline / operator tooling turning a scene patch into a render job).
/// There is deliberately NO `id` field: the coordinator mints it. `enqueue`
/// upserts on `id` (`ON CONFLICT(id) DO UPDATE`), so a caller-supplied id could
/// overwrite an existing in_flight/completed job's spec and reset it to queued.
#[derive(Debug, Deserialize)]
struct CreateJobRequest {
    kind: JobKind,
    region: RegionCoord,
    deadline_secs: u32,
    /// Max $BLCKFLD payable on acceptance, decimal wei. Validated to parse as a
    /// bounded `u128` before enqueue (see [`validate::validate_job_spec`]).
    max_payout_wei: String,
    inputs: serde_json::Value,
    /// Optional EVM address charged for this job's validated compute via
    /// ComputeMeter. Absent → the job is ingested unattributed and simply isn't
    /// metered. Stored coordinator-side (never on the proto wire to the earner).
    #[serde(default)]
    buyer: Option<String>,
}

/// Response for a successful `POST /jobs`: the coordinator-assigned job id the
/// caller can then poll at `GET /jobs/{id}`.
#[derive(Debug, Serialize)]
struct CreateJobResponse {
    id: Uuid,
}

/// Response for `POST /debits/{id}/redrive`: whether the re-drive re-armed a
/// dead-lettered debit. `false` is a successful no-op — the row was not
/// dead-lettered, was already settled, or no such debit exists — so a re-drive is
/// idempotent and an operator can replay it safely.
#[derive(Debug, Serialize)]
struct RedriveResponse {
    rearmed: bool,
}

/// Response for `POST /debits/redrive-all`: how many dead-lettered debits the bulk
/// re-drive re-armed. `0` is a successful no-op (nothing was dead-lettered) — the bulk
/// re-drive is idempotent, so an operator can replay it safely.
#[derive(Debug, Serialize)]
struct BulkRedriveResponse {
    rearmed: usize,
}

/// One dead-lettered ComputeMeter debit in the operator listing (`GET
/// /debits/dead-lettered`). `job_id` is the UUID an operator passes to `POST
/// /debits/{id}/redrive`; `amount_wei` is the owed charge as the persisted decimal
/// string (a 1e18-scale value, never coerced to a number); `dead_lettered_at` is the
/// epoch-second stamp it was quarantined; `redrive_count` is how many times it has been
/// re-driven, so a charge that keeps re-dead-lettering into an unfixed cause is visible.
#[derive(Debug, Serialize)]
struct DeadLetteredDebit {
    job_id: Uuid,
    buyer: String,
    amount_wei: String,
    dead_lettered_at: i64,
    redrive_count: u32,
}

/// Response for `GET /debits/dead-lettered`: the dead-lettered debits (oldest-first,
/// capped at [`MAX_DEAD_LETTERED_LIST`]) plus the full `total` so a truncated list is
/// never mistaken for the whole set. `truncated` is `true` when `total` exceeds the
/// returned page — the operator's signal that more stuck charges exist beyond the cap.
#[derive(Debug, Serialize)]
struct DeadLetteredListing {
    debits: Vec<DeadLetteredDebit>,
    total: usize,
    truncated: bool,
}

/// One dead-lettered EAS render receipt in the operator listing (`GET
/// /receipts/dead-lettered`). `job_id` is the UUID an operator passes to `POST
/// /receipts/{id}/redrive`; `earner` is whose validated work the stuck attestation
/// proves and `render_seconds` the compute it attests (the render-fee scale a
/// `Permanent` revert is usually about), `job_kind` the numeric JobKind; `dead_lettered_at`
/// is the epoch-second stamp it was quarantined; `redrive_count` is how many times it has
/// been re-driven, so a receipt that keeps re-dead-lettering into an unfixed cause is
/// visible. The attestation twin of [`DeadLetteredDebit`].
#[derive(Debug, Serialize)]
struct DeadLetteredReceipt {
    job_id: Uuid,
    earner: String,
    render_seconds: u64,
    job_kind: u16,
    dead_lettered_at: i64,
    redrive_count: u32,
}

/// Response for `GET /receipts/dead-lettered`: the dead-lettered receipts (oldest-first,
/// capped at [`MAX_DEAD_LETTERED_LIST`]) plus the full `total` so a truncated list is
/// never mistaken for the whole set. `truncated` is `true` when `total` exceeds the
/// returned page — the operator's signal that more stuck receipts exist beyond the cap.
/// The attestation twin of [`DeadLetteredListing`].
#[derive(Debug, Serialize)]
struct DeadLetteredReceiptListing {
    receipts: Vec<DeadLetteredReceipt>,
    total: usize,
    truncated: bool,
}

/// Aggregate mesh stats. Backs the wedge requirement
/// "Mesh GPUs joined count exposed at /stats".
#[derive(Debug, Serialize)]
struct Stats {
    gpus_joined: usize,
    total_vram_gb: u64,
    jobs_queued: usize,
    jobs_in_flight: usize,
    jobs_completed: usize,
    /// Jobs dead-lettered after exhausting the configured `max_attempts`.
    jobs_failed: usize,
    /// How many registered earners advertise support for each job kind.
    supported_breakdown: HashMap<JobKind, usize>,
    /// Backlog composition: how many QUEUED jobs of each kind are waiting.
    queued_by_kind: HashMap<JobKind, usize>,
    /// In-flight composition: how many IN-FLIGHT jobs of each kind are running.
    in_flight_by_kind: HashMap<JobKind, usize>,
    /// Completed composition: how many DONE jobs of each kind have finished.
    /// Sums to `jobs_completed`.
    completed_by_kind: HashMap<JobKind, usize>,
    /// Failed composition: how many FAILED (dead-lettered) jobs of each kind.
    /// Sums to `jobs_failed`.
    failed_by_kind: HashMap<JobKind, usize>,
    /// Total render-seconds produced across all completed jobs — the mesh
    /// output metric for the HUD ("N render-seconds produced").
    total_render_seconds: u64,
    /// Total $BLCKFLD payable across all completed jobs, as a decimal wei string
    /// (1e18-scale). Sum of each done job's `max_payout_wei`. Serialized as a
    /// STRING because a 1e18-scale total overflows JSON's safe integer range and
    /// viem wants the decimal string anyway.
    total_payout_wei: String,
    /// Age in seconds of the longest-running in-flight job (`now - started_at` of
    /// the oldest `in_flight` dispatch), or `null` when nothing is in flight. A
    /// queue-health signal for the HUD: it climbs when an earner is slow or stuck
    /// and resets as jobs complete or get reaped. Needs no schema migration.
    oldest_in_flight_secs: Option<u64>,
    /// Mean progress (whole percent, `0..=100`) across the jobs currently in flight,
    /// from the latest `progress_pct` each earner reported on its heartbeats — or
    /// `null` when nothing is in flight. A working-vs-wedged signal for the HUD: it
    /// climbs as earners report progress and is reset to 0 for each job on dispatch,
    /// so it reflects only the work running now. Being a MEAN, a single stuck job is
    /// diluted by healthy ones — pair it with `oldest_in_flight_secs` (which surfaces
    /// a lone long-running dispatch the mean would hide). Additive and optional.
    in_flight_progress_pct_avg: Option<u8>,
    /// Count of jobs dispatched more than once (`attempts > 1`): the reaper
    /// requeued them on a missed deadline and they were handed out again. A
    /// cumulative reaper-churn signal for the HUD; 0 on a healthy mesh.
    jobs_redispatched: usize,
    /// Gross count of dispatch attempts across ALL jobs (Σ `attempts`): every
    /// hand-out to an earner, including redispatches after a missed deadline or
    /// disconnect, but NOT earner-fault rejects (a fault refunds the attempt and
    /// charges `total_faults` instead). Distinct from `jobs_redispatched`, which
    /// counts how many *jobs* were dispatched more than once: a single job
    /// dispatched 5 times adds 5 here but 1 there. 0 on a fresh mesh. Additive
    /// and optional — a strict client may ignore it.
    total_attempts: u64,
    /// Gross count of earner-fault result rejects across ALL jobs (Σ `faults`):
    /// bad/forged/replayed signatures, malformed or implausible content, and
    /// submit-protocol violations — the earner-quality signal, on a budget kept
    /// separate from `total_attempts`. Together they let an operator tell
    /// reaper/disconnect churn apart from earner-quality problems. 0 on a healthy
    /// mesh. Additive and optional.
    total_faults: u64,
    /// Settled jobs whose EAS render receipt has not yet been relayed on-chain —
    /// the attestation backlog depth. Each validated settle durably enqueues a
    /// pending receipt; this drains once the (operator-gated) on-chain relayer
    /// submits them. Until then it tracks `jobs_completed`.
    pending_attestations: usize,
    /// Receipts the relayer quarantined after a non-retryable (`Permanent`)
    /// issueReceipt error — e.g. a receipt naming a region whose render fee the
    /// coordinator cannot cover. The attestation is still owed proof of validated
    /// work: the row is retained (not dropped) but excluded from
    /// `pending_attestations` so one poison receipt cannot block the batch drain.
    /// Nonzero here means a stuck receipt needs operator attention. 0 on a healthy
    /// mesh. Additive and optional.
    dead_lettered_attestations: usize,
    /// Settled jobs whose ComputeMeter debit has not yet been spent on-chain AND is
    /// still drainable — the debit backlog depth (the metering twin of
    /// `pending_attestations`). A debit is enqueued only for a metered settle (a job
    /// with a buyer when `--compute-rate-wei` is set), so this is 0 whenever metering
    /// is disabled. Drains once the (operator-gated) on-chain relayer spends them.
    /// Excludes dead-lettered rows (see `dead_lettered_debits`). Additive and optional.
    pending_debits: usize,
    /// Debits the relayer quarantined after a non-retryable (`Permanent`) spend error
    /// — e.g. an underfunded buyer's `InsufficientCredit`. The charge is still owed:
    /// the row is retained (not dropped) but excluded from `pending_debits` so it
    /// cannot block the backlog. Nonzero here means a stuck charge needs operator
    /// attention (top-up + replay). 0 on a healthy mesh. Additive and optional.
    dead_lettered_debits: usize,
    /// Age in seconds of the OLDEST quarantined attestation (`now - MIN(dead_lettered_at)`
    /// over the dead-lettered, not-yet-attested rows), or `null` when none are stuck. The
    /// dead-letter-age twin of `oldest_in_flight_secs`: `dead_lettered_attestations` is the
    /// DEPTH, this is how LONG the oldest owed proof has been stuck — so an operator can
    /// alarm on a single long-quarantined receipt a low depth would hide. Additive and
    /// optional.
    oldest_dead_lettered_attestation_secs: Option<u64>,
    /// Age in seconds of the OLDEST quarantined debit (`now - MIN(dead_lettered_at)` over
    /// the dead-lettered, not-yet-settled rows), or `null` when none are stuck — the debit
    /// twin of `oldest_dead_lettered_attestation_secs`. Additive and optional.
    oldest_dead_lettered_debit_secs: Option<u64>,
}

/// One earner in the `GET /earners` live leaderboard: the capabilities it
/// advertised at registration plus its lifetime `completed` job count and
/// `render_seconds` total drawn from the `results` table.
#[derive(Debug, Serialize)]
struct EarnerEntry {
    address: String,
    gpu_model: String,
    vram_gb: u32,
    supported: Vec<JobKind>,
    /// Unix epoch seconds of the last sign of life (same clock as `is_live`).
    last_seen: i64,
    /// Recorded results for this earner (sums to its share of `jobs_completed`).
    completed: usize,
    /// Total render-seconds this earner has produced across its results.
    render_seconds: u64,
    /// Total $BLCKFLD payable to this earner across its DONE jobs, as a decimal
    /// wei string (the per-earner counterpart to `/stats` `total_payout_wei`).
    payout_wei: String,
    /// Genuine quality faults attributed to this earner (bad/forged signature,
    /// malformed/implausible content, submit-protocol violations) — the per-earner
    /// breakdown of `/stats` `total_faults`. Additive/optional field; an honest
    /// Decline of an unsupported kind is NOT counted, and a clean earner reports 0.
    /// Does not affect leaderboard order (still completed → render_seconds →
    /// address). ws-attributed only: an HTTP-submit fault is left for the reaper and
    /// is unattributed, so this can undercount a mesh polled heavily over HTTP.
    faults: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("coordinator=info,tower_http=info")
        .init();
    let args = Args::parse();
    anyhow::ensure!(
        args.relay_batch_size > 0,
        "relay_batch_size must be >= 1 (0 would claim an empty batch every tick and never drain the receipt backlog)"
    );
    let store = Store::open(&args.db)?.with_compute_rate_wei(args.compute_rate_wei);
    // Seeds a fresh DB only; a restart reloads existing jobs from the file.
    // `with_store` also reclaims jobs left in_flight by a previous crash.
    let state = AppState::with_store(
        store,
        StoreConfig {
            max_attempts: args.max_attempts,
            max_faults: args.max_faults,
            earner_ttl_secs: args.earner_ttl_secs as i64,
            ttl_deadline_multiple: args.ttl_deadline_multiple,
            retention_secs: args.retention_secs as i64,
            handshake_timeout: Duration::from_secs(args.handshake_timeout_secs),
            session_idle_timeout: Duration::from_secs(args.session_idle_timeout_secs),
            ingest_token: args.ingest_token,
            max_queued_jobs: args.max_queued_jobs,
            max_earners: args.max_earners,
            max_registrations: args.max_registrations,
            trusted_proxies: TrustedProxies::parse(&args.trusted_proxies)?,
        },
    )?;
    tracing::info!(db = %args.db, "store ready");

    // Metering posture: a nonzero rate charges real buyer credit at settle, so make
    // the ENABLED state explicit at startup. Disabled is the safe opt-in default
    // (info, not a warning — the inverse of the ingestion-auth posture below, where
    // OPEN is the risky state).
    if args.compute_rate_wei > 0 {
        tracing::info!(
            rate_wei = %args.compute_rate_wei,
            "ComputeMeter metering ENABLED — a settled job with a buyer is charged rate * render-seconds wei"
        );
    } else {
        tracing::info!("ComputeMeter metering disabled (--compute-rate-wei 0) — no buyer is charged");
    }

    // Ingestion auth posture (FM2): an open `POST /jobs` is fine for local dev but
    // a silent wide-open write surface in production is how a deploy ships
    // unauthenticated. Make the open case loud at startup, mirroring the
    // `--relay-dev-mock` warning; a configured token logs a quiet confirmation.
    if state.ingest_token.is_some() {
        tracing::info!("POST /jobs ingestion requires a bearer token (COORDINATOR_INGEST_TOKEN)");
    } else {
        tracing::warn!(
            "POST /jobs ingestion is UNAUTHENTICATED — anyone who can reach the coordinator can enqueue render jobs. Set --ingest-token / COORDINATOR_INGEST_TOKEN before production."
        );
    }

    // Background reaper: periodically requeue in-flight jobs past their
    // deadline so a stalled/vanished earner doesn't strand a job forever. The
    // router-based tests don't spawn this; it only runs in the real binary.
    spawn_reaper(state.clone(), args.reap_interval_secs);

    // Background attestation relayer. The live Base submitter (RPC + an
    // authorized coordinator EAS signer with gas) is operator-gated, so it is not
    // wired here; `--relay-dev-mock` drives the same drain loop against an
    // in-process mock for local end-to-end testing. With neither configured the
    // backlog accumulates (by design, pre-production) and is visible at `/stats`.
    if args.relay_dev_mock {
        tracing::warn!(
            "DEV: draining attestations to an in-process MOCK relay — receipts are NOT submitted on-chain. Never enable in production."
        );
        spawn_relayer(
            state.clone(),
            relay::MockRelay::succeeding(),
            args.relay_interval_secs,
            args.relay_batch_size,
        );
    } else {
        tracing::info!(
            "on-chain attestation relayer disabled (operator-gated: needs a Base RPC + an authorized coordinator EAS signer). Pending receipts accumulate; see /stats pending_attestations."
        );
    }

    // Background debit relayer — the metering twin of the attestation relayer
    // above. The live Base spender (RPC + an authorized ComputeMeter spender key
    // with gas) targets `ComputeMeter.spendOnce` (the idempotent entry point — see
    // meter.rs) and is operator-gated, so it is not wired here; `--spender-dev-mock`
    // drives the same drain loop against an in-process mock. With neither configured
    // the backlog accumulates (by design) and is visible at `/stats pending_debits`.
    if args.spender_dev_mock {
        tracing::warn!(
            "DEV: draining ComputeMeter debits to an in-process MOCK spender — nothing is spent on-chain. Never enable in production."
        );
        spawn_debit_relayer(
            state.clone(),
            meter::MockSpender::succeeding(),
            args.spender_interval_secs,
        );
    } else {
        tracing::info!(
            "on-chain debit relayer disabled (operator-gated: needs a Base RPC + an authorized ComputeMeter spender key; targets ComputeMeter.spendOnce). Pending debits accumulate; see /stats pending_debits."
        );
    }

    // A zero body timeout makes `TimeoutLayer` respond 408 to every POST before
    // its body can arrive — registration + submit over HTTP become a total
    // outage. Reject it at startup, mirroring the header-timeout zero guard.
    anyhow::ensure!(
        args.http_body_timeout_secs > 0,
        "http_body_timeout_secs must be > 0 (0 would 408 every POST before its body arrives)"
    );
    let app = router_with_body_timeout(state, Duration::from_secs(args.http_body_timeout_secs));

    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    tracing::info!(bind = %args.bind, "coordinator up");
    serve(
        listener,
        app,
        Duration::from_secs(args.http_header_timeout_secs),
        args.max_connections,
        shutdown_signal(),
    )
    .await
}

/// Serve `app` on `listener` until `shutdown` resolves, bounding the time a
/// connection may take to send its HTTP request headers (`header_read_timeout`).
/// We hand-roll the accept loop instead of `axum::serve` for two reasons:
/// 1. `axum::serve` exposes no header-read timeout, so a slow-headers slowloris
///    could park an FD at hyper's pre-routing read phase — before any handler,
///    and before the ws upgrade arms the per-connection handshake timeout — on
///    every endpoint. `header_read_timeout` (which needs an installed timer to
///    fire) closes it.
/// 2. `axum::serve` (like hyper-util's `auto::Builder`) sniffs the `PRI *
///    HTTP/2.0` preface and serves HTTP/2 cleartext, where the h1 header-read
///    bound does NOT apply and hyper installs no pre-SETTINGS read timeout — an
///    h2c slowloris would bypass the fix entirely. Every real client is h1 (the
///    ws upgrade is h1; the earner's `reqwest` over plaintext is h1), so we serve
///    h1-only: an h2c preface is parsed as a (rejected) h1 request, leaving no
///    unbounded protocol.
///
/// On `shutdown` we stop accepting and drain in-flight connections, so graceful
/// shutdown is preserved (`with_upgrades` keeps the ws 101 upgrade working).
///
/// `max_connections` caps concurrently served connections: a `try_acquire` (never
/// an await, so the accept/shutdown select is never blocked) gates each spawn, and
/// a connection accepted past the cap is closed immediately. The permit is moved
/// into the connection task, so it is released on every exit path (completion,
/// error, graceful drain) by RAII. See [`DEFAULT_MAX_CONNECTIONS`].
async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    header_read_timeout: Duration,
    max_connections: usize,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    anyhow::ensure!(
        !header_read_timeout.is_zero(),
        "http_header_timeout must be > 0 (0 would close every request before its headers complete)"
    );
    anyhow::ensure!(
        max_connections > 0,
        "max_connections must be > 0 (0 would reject every connection)"
    );
    let conn_limit = Arc::new(tokio::sync::Semaphore::new(max_connections));
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    // Graceful drain, by hand: hyper-util's `GracefulShutdown` doesn't implement
    // `GracefulConnection` for h1's *upgradeable* connection (the one ws needs),
    // so we track served connections in a `JoinSet` and broadcast a drain signal
    // over a `watch`. On shutdown each task calls `graceful_shutdown()` (disable
    // keep-alive) and runs its connection future to completion, so an in-flight
    // HTTP request finishes instead of being cut off. A ws connection's served
    // future completes at the 101 upgrade handoff (axum runs the session on a
    // detached task), so the drain returns promptly rather than awaiting the ws
    // to close — matching `axum::serve`.
    let (drain_tx, drain_rx) = tokio::sync::watch::channel(false);
    let mut conns = tokio::task::JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(conn) => conn,
                    // A transient accept error (e.g. fd exhaustion) shouldn't kill
                    // the listener; log and keep serving.
                    Err(e) => {
                        tracing::warn!(?e, "accept failed");
                        continue;
                    }
                };
                // Connection-flood backstop: `try_acquire_owned` never blocks the
                // accept loop (so the shutdown arm and existing connections are
                // unaffected at the cap); past the cap we drop `stream` here, which
                // closes the just-accepted socket immediately.
                let permit = match conn_limit.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!(max_connections, "connection cap reached; dropping new connection");
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                // Inject the connection's peer address so the registration handlers
                // can key the per-source rate limiter on it. Per-connection layer (the
                // peer is known only at accept), mirroring axum's connect-info plumbing
                // — cheap Arc clones over the shared router.
                let service =
                    TowerToHyperService::new(app.clone().layer(Extension(PeerAddr(peer))));
                let conn = builder.serve_connection(io, service).with_upgrades();
                let mut drain = drain_rx.clone();
                conns.spawn(async move {
                    // Held for the task's whole lifetime; dropped (released) on
                    // every exit path below — completion, error, or drain.
                    let _permit = permit;
                    let mut conn = std::pin::pin!(conn);
                    tokio::select! {
                        res = conn.as_mut() => {
                            if let Err(e) = res { tracing::debug!("connection ended: {e}"); }
                        }
                        // Wrapped so the borrowed `watch::Ref` is dropped here, not
                        // held across the drain await below (which would make the
                        // task non-`Send` and unspawnable).
                        _ = async { let _ = drain.wait_for(|d| *d).await; } => {
                            conn.as_mut().graceful_shutdown();
                            if let Err(e) = conn.await { tracing::debug!("connection drained: {e}"); }
                        }
                    }
                });
            }
            // Reap finished connections so the set doesn't grow while serving.
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received; draining in-flight connections");
                break;
            }
        }
    }
    drop(listener);
    let _ = drain_tx.send(true);
    while conns.join_next().await.is_some() {}
    Ok(())
}

/// Resolve when the process receives a shutdown signal — `SIGINT` (Ctrl-C)
/// on any platform, or `SIGTERM` on unix (Render sends SIGTERM on stop).
/// Awaited by [`serve`]'s graceful shutdown so in-flight requests and ws
/// sessions drain before exit.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received; draining in-flight work");
}

/// Current time as epoch seconds. Saturates to 0 if the clock is before the
/// epoch (it never is in practice).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Remove earners not seen within `ttl_secs` of `now` (epoch seconds), returning
/// the number removed. Keeps the in-memory earner registry bounded as ws sessions
/// end and HTTP earners go quiet.
fn prune_stale_earners(
    earners: &mut std::collections::HashMap<String, EarnerInfo>,
    now: i64,
    ttl_secs: i64,
) -> usize {
    let before = earners.len();
    earners.retain(|_, info| info.is_live(now, ttl_secs));
    before - earners.len()
}

/// Admit an earner registration into the `earners` registry under the size cap
/// `max_earners`, returning whether it was admitted. Shared by both registration
/// paths (HTTP `register`, WS `recv_hello_inner`) so the bound is enforced
/// identically; the caller runs it while holding the `earners` lock, so the
/// size check, eviction, and insert are atomic against a concurrent registration
/// (no TOCTOU over-fill).
///
/// Policy:
/// - An UPSERT of an already-registered address overwrites in place and always
///   succeeds — it doesn't grow the map, so a known earner (even one gone stale)
///   re-registering / changing capabilities is never blocked by the cap.
/// - A NEW address below the cap is inserted unconditionally.
/// - A NEW address AT the cap is admitted only by evicting the stalest entry that
///   is already past its TTL (`is_live == false`, smallest `last_seen`) — an entry
///   the reaper would prune and `/stats`/`/earners` already filter out. A LIVE
///   earner is never displaced; when every entry is live (a genuinely full fleet),
///   the registration is rejected (`false`). The at-cap scan is O(n) but only runs
///   when the backstop engages, and `n` is bounded by `max_earners`.
fn admit_earner(
    earners: &mut std::collections::HashMap<String, EarnerInfo>,
    address: String,
    info: EarnerInfo,
    max_earners: usize,
    now: i64,
    ttl_secs: i64,
) -> bool {
    if earners.contains_key(&address) || earners.len() < max_earners {
        earners.insert(address, info);
        return true;
    }
    let stalest = earners
        .iter()
        .filter(|(_, e)| !e.is_live(now, ttl_secs))
        .min_by_key(|(_, e)| e.last_seen)
        .map(|(k, _)| k.clone());
    match stalest {
        Some(key) => {
            earners.remove(&key);
            earners.insert(address, info);
            true
        }
        None => false,
    }
}

/// A per-source registration token bucket. `level` is the remaining allowance in
/// SCALED units (whole tokens × `window_secs`), so refill is exact integer
/// arithmetic with no division-floor loss: each elapsed second credits `capacity`
/// units, the level saturates at `capacity × window_secs` (one full bucket), and a
/// single registration costs `window_secs` units (one token). Scaling the level lets
/// a bucket polled more often than once per `window_secs / capacity` seconds still
/// accrue sub-token progress — a whole-token counter that reset its clock on every
/// poll would silently drop the remainder and never refill under steady churn.
#[derive(Debug, Clone, Copy)]
struct RateBucket {
    /// Remaining allowance, in `tokens × window_secs` units; in `[0, capacity × window_secs]`.
    level: i64,
    /// Epoch seconds of the last refill.
    last_refill: i64,
}

/// Credit `bucket` for the time elapsed since its last refill, saturating at a full
/// bucket (`cap_level`). A non-positive elapsed (the clock didn't advance, or stepped
/// backward) is a no-op — the level and clock are left untouched — so a backward
/// clock step can never fabricate or destroy allowance.
fn refill_bucket(bucket: &mut RateBucket, now: i64, capacity: u32, cap_level: i64) {
    let elapsed = now.saturating_sub(bucket.last_refill);
    if elapsed <= 0 {
        return;
    }
    let credit = elapsed.saturating_mul(capacity as i64);
    bucket.level = bucket.level.saturating_add(credit).min(cap_level);
    bucket.last_refill = now;
}

/// Per-source registration rate limit: refill `source`'s token bucket, consume one
/// token if available, and return whether the registration is admitted. The caller
/// holds the bucket-map lock, so refill + consume + eviction are atomic against a
/// concurrent registration (the in-memory analogue of [`admit_earner`]'s atomic
/// admission). Run BEFORE the secp256k1 verify on both registration paths
/// (cheap-reject-first): an over-limit source is shed before any curve recovery, so
/// the limiter mitigates the registration flood instead of amplifying it.
///
/// `capacity` (== `--max-registrations`, validated `> 0`) is the per-source allowance
/// per `window_secs` and the burst ceiling. A NEW source enters with a full bucket,
/// so a source's *first* registration is never blocked by the limiter; the bucket is
/// keyed per source so an honest fleet's fan-in is not throttled as one global pool.
///
/// The bucket map is itself bounded at `max_buckets`, so the limiter cannot be turned
/// into the memory-DoS it defends against (FM4): a new source at the cap first drops
/// buckets that have fully refilled — an idle/full bucket carries no state, so
/// forgetting it is lossless (a fresh source also starts full) — and, if the map is
/// still full of actively-limited sources, evicts the stalest (oldest `last_refill`).
/// Eviction only ever forgets a source's own *deficit* (it re-enters full, loosening
/// its own limit, never bypassing another source's), and triggering it costs a
/// distinct active source per slot, so `max_buckets` is sized far above any honest
/// source count.
fn check_registration_rate(
    buckets: &mut HashMap<IpAddr, RateBucket>,
    source: IpAddr,
    now: i64,
    capacity: u32,
    window_secs: i64,
    max_buckets: usize,
) -> bool {
    let cap_level = (capacity as i64).saturating_mul(window_secs);
    if !buckets.contains_key(&source) {
        if buckets.len() >= max_buckets {
            // Drop buckets that WOULD refill to full as of `now` — they carry no
            // state, so reclaiming them is lossless. Computed as a pure projection so
            // it does NOT advance a survivor's `last_refill`; rewriting it here would
            // tie every survivor's staleness and make the eviction below arbitrary.
            buckets.retain(|_, b| {
                let elapsed = now.saturating_sub(b.last_refill).max(0);
                let projected = b.level.saturating_add(elapsed.saturating_mul(capacity as i64));
                projected < cap_level
            });
        }
        if buckets.len() >= max_buckets {
            if let Some(stalest) =
                buckets.iter().min_by_key(|(_, b)| b.last_refill).map(|(k, _)| *k)
            {
                buckets.remove(&stalest);
            }
        }
        buckets.insert(source, RateBucket { level: cap_level, last_refill: now });
    }
    let bucket = buckets
        .get_mut(&source)
        .expect("source bucket present (pre-existing or just inserted)");
    refill_bucket(bucket, now, capacity, cap_level);
    if bucket.level < window_secs {
        return false;
    }
    bucket.level -= window_secs;
    true
}

/// The accepted connection's peer address, injected as a request extension by
/// [`serve`] (per connection) and read by the registration handlers to key the
/// per-source rate limiter. Behind a reverse proxy this is the proxy's address;
/// [`resolve_source_ip`] recovers the real client from `X-Forwarded-For` when the
/// peer is a trusted proxy — see [`DEFAULT_MAX_REGISTRATIONS`] for the layering.
#[derive(Debug, Clone, Copy)]
struct PeerAddr(SocketAddr);

/// One trusted-proxy allowlist entry: an exact IP (stored as a `/32` or `/128`) or a
/// CIDR network. [`contains`](TrustedCidr::contains) compares `(peer & mask) ==
/// (network & mask)` within the SAME address family, so the network's host bits are
/// irrelevant (masked off both sides) and a v4 entry never matches a v6 peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrustedCidr {
    network: IpAddr,
    prefix: u8,
}

impl TrustedCidr {
    fn contains(&self, peer: &IpAddr) -> bool {
        match (self.network, peer) {
            (IpAddr::V4(net), IpAddr::V4(peer)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                (u32::from(net) & mask) == (u32::from(*peer) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(peer)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                (u128::from(net) & mask) == (u128::from(*peer) & mask)
            }
            _ => false,
        }
    }
}

/// Parse one `--trusted-proxies` entry: a bare IP (`10.0.0.1`, `2001:db8::1`) as an
/// exact `/32`/`/128`, or a CIDR (`10.0.0.0/8`, `2001:db8::/32`). A malformed
/// address, an unparseable prefix, an out-of-family-range prefix, or a blank entry is
/// rejected — never silently widened to a `/0` catch-all that would trust every peer.
fn parse_trusted_entry(entry: &str) -> Result<TrustedCidr> {
    let entry = entry.trim();
    anyhow::ensure!(!entry.is_empty(), "blank trusted-proxy entry");
    if let Some((addr, prefix)) = entry.split_once('/') {
        let network: IpAddr =
            addr.parse().map_err(|_| anyhow::anyhow!("invalid trusted-proxy CIDR address: {entry}"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid trusted-proxy CIDR prefix: {entry}"))?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        anyhow::ensure!(prefix <= max, "trusted-proxy CIDR prefix /{prefix} exceeds /{max}: {entry}");
        Ok(TrustedCidr { network, prefix })
    } else {
        let network: IpAddr =
            entry.parse().map_err(|_| anyhow::anyhow!("invalid trusted-proxy IP: {entry}"))?;
        let prefix = if network.is_ipv4() { 32 } else { 128 };
        Ok(TrustedCidr { network, prefix })
    }
}

/// Reverse-proxy IPs/ranges whose `X-Forwarded-For` (or RFC-7239 `Forwarded`) the
/// registration limiter trusts to name the real client. Empty by default: with no
/// proxy trusted, forwarded headers are ignored and the limiter keys on the raw
/// connection peer (byte-identical to the pre-XFF behavior). Populated from
/// `--trusted-proxies` (exact IPs and/or CIDR ranges); read by [`resolve_source_ip`].
#[derive(Debug, Clone, Default)]
struct TrustedProxies(Vec<TrustedCidr>);

impl TrustedProxies {
    /// Blank/whitespace-only entries are skipped (an empty `COORDINATOR_TRUSTED_PROXIES`
    /// env var or a trailing comma yields trust-no-proxy, never a startup crash); a
    /// non-blank malformed entry is still rejected.
    fn parse(entries: &[String]) -> Result<Self> {
        entries
            .iter()
            .map(|e| e.trim())
            .filter(|e| !e.is_empty())
            .map(parse_trusted_entry)
            .collect::<Result<Vec<_>>>()
            .map(TrustedProxies)
    }

    fn contains(&self, peer: &IpAddr) -> bool {
        self.0.iter().any(|c| c.contains(peer))
    }
}

/// Parse one `X-Forwarded-For` node into the IP it names. Handles a bare IP
/// (`203.0.113.7`, `2001:db8::1`) and the `ip:port` / `[ipv6]:port` forms some
/// proxies append (via a `SocketAddr` fallback). Returns `None` for an obfuscated
/// or malformed token (`_hidden`, `unknown`, empty) — the caller treats an
/// indeterminate nearest hop as a reason to fall back to the connection peer.
fn parse_forwarded_node(token: &str) -> Option<IpAddr> {
    let t = token.trim();
    if let Ok(ip) = t.parse::<IpAddr>() {
        return Some(ip);
    }
    t.parse::<SocketAddr>().ok().map(|sa| sa.ip())
}

/// Parse one RFC-7239 forwarded-element — a `;`-separated list of `token=value`
/// pairs (`for=`/`by=`/`proto=`/`host=`) — into the client IP its `for=` names.
/// Only `for` identifies the client. The value may be a bare token or a
/// quoted-string; an IPv6 address is bracketed (`for="[2001:db8::1]:443"`) and may
/// carry a port. Returns `None` (an indeterminate hop, like `parse_forwarded_node`
/// for XFF) when the element has no `for=`, or its value is obfuscated (`_hidden`),
/// `unknown`, empty, or unparseable — the caller then falls back to the peer.
fn parse_forwarded_element(element: &str) -> Option<IpAddr> {
    let raw = element
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("for"))?
        .1
        .trim();
    // A value is a bare token or a quoted-string. Honor a MATCHED pair of quotes and
    // reject an embedded or unbalanced quote — a comma/semicolon-bearing quoted value
    // is torn apart by the header-line (`,`) and element (`;`) splits upstream, so the
    // fragments carry a stray quote; failing them to None makes such a value fall back
    // to the peer rather than yield a spurious IP.
    let v = match raw.strip_prefix('"') {
        Some(inner) => inner.strip_suffix('"')?.trim(),
        None => raw,
    };
    if v.contains('"') {
        return None;
    }
    if let Some(inner) = v.strip_prefix('[') {
        return inner.split(']').next()?.parse::<IpAddr>().ok();
    }
    parse_forwarded_node(v)
}

/// Parse the RFC-7239 `Forwarded` header(s) into the ordered `for=` client hops,
/// left-to-right (farthest→nearest, since a proxy APPENDS its element — the same
/// ordering as `X-Forwarded-For`). Multiple header lines are flattened in order,
/// then each comma-separated forwarded-element yields one entry: its parsed `for=`
/// client, or `None` for a missing/obfuscated/unparseable hop. Empty when no
/// `Forwarded` header is present; `resolve_source_ip` reaches this only when no
/// `X-Forwarded-For` is present, and an empty result then falls back to the peer.
fn parse_forwarded_for_nodes(headers: &HeaderMap) -> Vec<Option<IpAddr>> {
    headers
        .get_all("forwarded")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(','))
        .map(parse_forwarded_element)
        .collect()
}

/// Parse the `X-Forwarded-For` header(s) into the ordered client hops, left-to-right
/// (farthest→nearest). Flattens BOTH the comma-separated entries within a line and
/// multiple header lines (per HTTP, repeated field lines are equivalent to one
/// comma-joined value) — using `get_all`, so a proxy that appends the observed client
/// as a separate `X-Forwarded-For` line still places it as the nearest hop, and an
/// attacker cannot win the nearest slot by adding a second line.
fn parse_xff_nodes(headers: &HeaderMap) -> Vec<Option<IpAddr>> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(','))
        .map(parse_forwarded_node)
        .collect()
}

/// Walk a forwarded-hop chain NEAREST→FARTHEST (the order a proxy APPENDS in, so
/// right-to-left) and return the first address that is not itself a trusted proxy
/// — the IP the trust boundary actually observed the request arrive from. Trusted
/// hops (our own ingress chain) are skipped; an upstream attacker's forged entries
/// sit FARTHER along and are never reached past the real client.
///
/// Falls back to `peer` when the chain is empty, every hop is trusted (no observed
/// external client), or the nearest hop is indeterminate (`None`: obfuscated or
/// unparseable) — each collapses onto the conservative proxy bucket, never a looser
/// one. Shared by the `X-Forwarded-For` and RFC-7239 `Forwarded` paths so both have
/// byte-identical selection semantics.
fn select_untrusted_hop(
    hops_near_to_far: impl Iterator<Item = Option<IpAddr>>,
    peer: IpAddr,
    trusted: &TrustedProxies,
) -> IpAddr {
    for hop in hops_near_to_far {
        match hop {
            Some(ip) if trusted.contains(&ip) => continue,
            Some(ip) => return ip,
            None => return peer,
        }
    }
    peer
}

/// Resolve the client IP the per-source registration limiter should key on, from
/// the connection `peer` and the request's forwarded headers, trusting them ONLY
/// when `peer` is itself a configured trusted proxy.
///
/// A direct (untrusted) peer can write anything in either header, so they are
/// ignored and the limiter keys on the peer — a direct attacker cannot forge a
/// different source. When `peer` is trusted, walk the forwarded chain RIGHT-TO-LEFT
/// via [`select_untrusted_hop`]: a proxy APPENDS the address it observed, so the
/// rightmost entries are the nearest hops, and the first non-trusted one is the IP
/// the trust boundary saw.
///
/// PRECEDENCE: `X-Forwarded-For` takes precedence over RFC-7239 `Forwarded` when
/// both are present; `Forwarded` is consulted only when no `X-Forwarded-For` is
/// present at all, else the peer. XFF-first is the NO-REGRESSION order: before
/// `Forwarded` was parsed, an XFF-fronted deployment ignored any client-supplied
/// `Forwarded`, so keeping XFF authoritative whenever present means an attacker
/// behind such a proxy still cannot inject a `Forwarded` to override the real
/// XFF-attributed client (Forwarded-first WOULD let them — the common nginx config
/// emits XFF but passes an inbound `Forwarded` through verbatim). A present-but-junk
/// XFF collapses to the peer here, never falling through to `Forwarded`, so the
/// flip cannot reintroduce that cross-header bypass.
///
/// Residual (documented): a proxy that emits *only* `Forwarded` must itself
/// strip/overwrite any inbound `X-Forwarded-For`, or an attacker could supply one to
/// win precedence — symmetric to the long-standing XFF-strip discipline, and a
/// fresh-config requirement of the new capability, not a regression. The
/// right-to-left walk already neutralizes prepended entries within whichever header
/// is authoritative.
fn resolve_source_ip(peer: IpAddr, headers: &HeaderMap, trusted: &TrustedProxies) -> IpAddr {
    if !trusted.contains(&peer) {
        return peer;
    }
    if headers.get("x-forwarded-for").is_some() {
        let xff = parse_xff_nodes(headers);
        return select_untrusted_hop(xff.into_iter().rev(), peer, trusted);
    }
    let forwarded = parse_forwarded_for_nodes(headers);
    if forwarded.is_empty() {
        return peer;
    }
    select_untrusted_hop(forwarded.into_iter().rev(), peer, trusted)
}

/// Per-job absolute wall-clock TTL, as a multiple of the job's own
/// `deadline_secs`: a job alive (queued or in_flight) past
/// `created_at + deadline_secs * JOB_TTL_DEADLINE_MULTIPLE` is dead-lettered by
/// the reaper regardless of attempts/faults/earner count. ~24h at the 60s
/// default deadline, scaling up with longer-budget jobs. Chosen to dominate the
/// `max_attempts (5) + max_faults (10)` ≈ 15 retry-churn windows by ~100×, so
/// the existing budgets always terminate a churning job first and this backstop
/// only catches a job that is genuinely stuck — a queued poison job a single
/// connected earner keeps faulting on (which never accrues the *distinct*-earner
/// faults `max_faults` needs), or a silently wedged in-flight dispatch.
///
/// This is the DEFAULT only: it backs `Args::ttl_deadline_multiple`
/// (`--ttl-deadline-multiple` / `COORDINATOR_TTL_DEADLINE_MULTIPLE`), and the
/// reaper reads the configured `AppState::ttl_deadline_multiple` at runtime, not
/// this const directly. A dev can shrink it for fast-expiry testing; an operator
/// with long-deadline jobs can tighten the bound — without a rebuild.
const JOB_TTL_DEADLINE_MULTIPLE: u32 = 1440;

/// Default retention horizon for terminal-job history: ~30 days. Terminal
/// (done/failed) jobs older than this whose on-chain receipt/debit have been
/// relayed are deleted by the background sweep, bounding the otherwise-unbounded
/// `jobs` history (and its `results` / `pending_*` dependents). A generous
/// backstop, not a tuned limit: far longer than any job's wall-clock TTL
/// (`deadline_secs * ttl_deadline_multiple`), so only long-settled history ages
/// out and `/stats` reports an honest month-long window. Backs
/// `Args::retention_secs` (`--retention-secs` / `COORDINATOR_RETENTION_SECS`); the
/// reaper reads the configured `AppState::retention_secs` at runtime. An operator
/// who needs a longer lifetime window widens it — without a rebuild.
const DEFAULT_RETENTION_SECS: u64 = 30 * 24 * 60 * 60;

/// Default wall-clock bound on the ws registration handshake: the coordinator
/// closes a connection that hasn't completed the challenge→Hello exchange within
/// this window. An honest earner replies within milliseconds of reading the
/// challenge, so 10s is generous headroom for a laggy link while still denying a
/// slowloris (open a connection, read or ignore the challenge, never send a
/// Hello) the ability to park a live `ws_session` task + socket indefinitely.
/// Backs `Args::handshake_timeout_secs` (`--handshake-timeout-secs` /
/// `COORDINATOR_HANDSHAKE_TIMEOUT_SECS`); the gate reads the configured
/// `AppState::handshake_timeout` at runtime, so a test can shrink it to a
/// sub-second value without a slow suite.
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Default read-idle bound on an ESTABLISHED post-Hello ws session. The
/// coordinator pings at half this bound; a live earner — even idle between jobs,
/// which emits no application frames — auto-pongs in milliseconds, so 90s only
/// ever closes a genuinely silent/vanished session. Larger than the 60s
/// `earner_ttl` so the socket outlives a brief registry-liveness gap, and far
/// under the ~2h OS TCP keepalive it replaces. Backs `Args::session_idle_timeout_secs`
/// (`--session-idle-timeout-secs` / `COORDINATOR_SESSION_IDLE_TIMEOUT_SECS`);
/// `ws_session` reads the configured `AppState::session_idle_timeout` at runtime,
/// so a test can shrink it to a sub-second value without a slow suite.
const DEFAULT_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Default wall-clock bound on the pre-routing HTTP request-header read: the
/// coordinator closes a connection that hasn't sent its complete request headers
/// within this window. `axum::serve` installs no such bound, so a slow-headers
/// slowloris (open a connection, send the request line, then dribble headers
/// forever) would otherwise park an FD at hyper's read phase before any handler —
/// including before the ws upgrade arms the per-connection handshake timeout —
/// on EVERY endpoint. Honest clients send headers in well under a second, so 15s
/// is generous. Backs `Args::http_header_timeout_secs`
/// (`--http-header-timeout-secs` / `COORDINATOR_HTTP_HEADER_TIMEOUT_SECS`).
const DEFAULT_HTTP_HEADER_TIMEOUT: Duration = Duration::from_secs(15);

/// Default bound on how long a body-bearing mutating request (`POST /register`,
/// `POST /jobs/{id}/submit`) may take to deliver its complete body once its
/// headers have parsed. `header_read_timeout` disarms the moment headers parse,
/// so without this bound a slow-body slowloris (send full headers + a
/// `Content-Length`, then dribble or stall the body) parks a post-routing task
/// indefinitely. Applied as a total request `TimeoutLayer` (responds `408`) on
/// the two POST routes only. A total bound (rather than a body-stream bound) is
/// safe here because these handlers do no network I/O and no unbounded await: a
/// signature verify, a fast indexed SQLite write under the shared store lock, and
/// `validate::is_fetchable_url` (a string check, not a fetch). The store time is
/// under the clock too, but it is sub-millisecond on the local DB, so in practice
/// a slow body is the only thing that can approach the budget — the total bound
/// acts as a de-facto body-read bound with no realistic false-positive on a
/// legitimately slow handler. The `/ws` upgrade and the GET routes carry no
/// request body and are deliberately left unwrapped. Honest earners send their
/// small JSON body in well under a second, so 30s is generous. Backs
/// `Args::http_body_timeout_secs` (`--http-body-timeout-secs` /
/// `COORDINATOR_HTTP_BODY_TIMEOUT_SECS`).
const DEFAULT_HTTP_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on a request body before it is buffered and deserialized, applied to
/// the body-bearing POST routes. axum's built-in default is 2 MiB; this drops it
/// ~64x to just above the largest legitimate body — a `CreateJobRequest` whose
/// `inputs` is capped at [`validate::MAX_INPUTS_BYTES`] plus its small framing —
/// so an oversized body is refused with `413` before serde allocates the parse
/// tree, not after. The precise per-field caps still run in
/// [`validate::validate_job_spec`]; this is the coarse pre-parse backstop that
/// bounds transient request memory (the per-field cap only bounds the stored row).
const MAX_REQUEST_BODY_BYTES: usize = 2 * validate::MAX_INPUTS_BYTES;

/// Default cap on concurrently served connections — a process-level backstop
/// against a connection flood. The per-FD header/body timeouts bound how long
/// each connection lingers, but a client that opens and recycles connections
/// faster than they time out still grows the live task/FD count without a cap.
/// 4096 is ~an order of magnitude above any plausible early-fleet peak of
/// concurrent earners (each holds ~one ws connection plus transient HTTP polls),
/// so it never throttles honest GPU fan-in at pre-production / early scale — a
/// larger fleet raises it via the knob. The PRIMARY flood defense remains edge /
/// OS FD limits; this only prevents unbounded growth. Backs
/// `Args::max_connections` (`--max-connections` / `COORDINATOR_MAX_CONNECTIONS`).
const DEFAULT_MAX_CONNECTIONS: usize = 4096;

/// Default cap on receipts per `issueReceipts` batch. 32 keeps the atomic batch
/// well under Base's block gas limit (each element pays the per-receipt fence +
/// counters + attestation + region fee-route) while still amortizing the base tx
/// cost across a settle wave. Backs `Args::relay_batch_size`
/// (`--relay-batch-size` / `COORDINATOR_RELAY_BATCH_SIZE`); a deeper backlog can
/// raise it, a heavier per-element cost lower it.
const DEFAULT_RELAY_BATCH_SIZE: usize = 32;

/// Default cap on the number of `queued` jobs the runtime ingestion endpoint
/// (`POST /jobs`) will admit. Sized far above any legitimate early backlog (the
/// boot-time seed is a handful of jobs, exempt anyway) so it never bites honest
/// load; an operator running a larger backlog raises it. The backstop against an
/// unbounded queued backlog (disk-fill, slower dispatch scans, FIFO starvation).
const DEFAULT_MAX_QUEUED_JOBS: usize = 10_000;

/// Default wei charged per render-second to a job's buyer at settle (a pending
/// ComputeMeter debit). `0` DISABLES metering: no debit row is written, so the
/// feature is strictly opt-in — a deploy starts charging real buyer credit only
/// once the operator sets the real economic rate via `--compute-rate-wei` /
/// `COORDINATOR_COMPUTE_RATE_WEI`. Backs `Args::compute_rate_wei`.
const DEFAULT_COMPUTE_RATE_WEI: u128 = 0;

/// Default cap on the in-memory earner registry (`state.earners`). Registration is
/// signature-gated for identity but fresh keypairs are free, so cheap distinct
/// signed Hellos would otherwise inflate the map without bound — growing the O(n)
/// `/earners` + `/stats` scans and the `/earners` response past the OS send buffer
/// (the lever behind the OS/edge-gated slow-response-read residual). 65536 is ~16×
/// the 4096 concurrent-connection cap, so it never throttles an honest fleet at
/// pre-production / early scale (each earner is one registry entry, persisting past
/// an HTTP earner's request until its TTL); it is a backstop against unbounded
/// growth, not a tuned fleet size — an operator runs a larger mesh by raising it.
/// Backs `Args::max_earners` (`--max-earners` / `COORDINATOR_MAX_EARNERS`).
const DEFAULT_MAX_EARNERS: usize = 65_536;

/// Window (seconds) over which `--max-registrations` is allowed per source. A
/// natural one-minute rate window; the token bucket refills `max_registrations`
/// tokens across it (see [`check_registration_rate`]).
const REGISTRATION_WINDOW_SECS: i64 = 60;

/// Default per-source registration allowance per [`REGISTRATION_WINDOW_SECS`]. Bounds
/// how fast any one source IP can churn registrations — the lever behind the
/// earner-registry-cap's documented sustained-lockout residual (an attacker holding
/// `max_earners` live slots by re-Helloing, or inflating the map with fresh
/// keypairs). 4096/min (~68/s) is far above any honest source's burst — a single
/// host brings up a handful of earners, and even a fleet behind one ingress
/// reconnects in bursts well under this — while still capping a single direct source
/// far below the rate needed to churn the registry. It is a backstop, not a tuned
/// limit: the PRIMARY per-source defense is edge/OS (and, behind a reverse proxy,
/// where the peer IP is the proxy's, the trusted-proxy `X-Forwarded-For` attribution
/// in [`resolve_source_ip`], keyed on `--trusted-proxies`, recovers the real client).
/// An operator on a known-direct deployment tunes it DOWN to
/// throttle a single source harder; one fronting a large fleet behind one ingress
/// tunes it UP. Backs `Args::max_registrations`.
const DEFAULT_MAX_REGISTRATIONS: u32 = 4096;

/// Cap on the per-source registration-bucket map, so the rate limiter cannot itself
/// be turned into the memory-DoS it defends against (FM4). One entry per distinct
/// source IP seen recently; a new source past this many is admitted by pruning
/// fully-refilled (idle) buckets, then evicting the stalest (see
/// [`check_registration_rate`]). Sized at the earner-registry cap — an honest
/// deployment has at most ~one source per earner (far fewer behind a proxy), so an
/// honest fleet never reaches it; an attacker with a large real source range (e.g.
/// an IPv6 /64) can force eviction, but each evicted source merely restarts full, and
/// the map stays bounded. A const, not a knob: a backstop on a backstop.
const MAX_REGISTRATION_BUCKETS: usize = 65_536;

/// Retention sweep: max terminal jobs deleted per batch. The store lock is held
/// for just one bounded delete before the reaper releases it, so a large aged
/// backlog cannot stall dispatch/settle behind a single long `DELETE` (FM2).
const RETENTION_BATCH: usize = 256;

/// Retention sweep: max batches per reap tick. `RETENTION_BATCH * this` caps the
/// rows pruned per tick (the lock is released between each batch); a larger aged
/// backlog drains over several ticks rather than monopolizing one. A backstop on
/// a backstop — in steady state a tick prunes only what just aged out.
const RETENTION_MAX_BATCHES_PER_TICK: usize = 16;

/// Spawn the deadline reaper: every `interval_secs`, requeue any in-flight job
/// whose deadline has elapsed (or dead-letter it when it has exhausted all
/// attempts), then dead-letter any job — queued or in-flight — past its absolute
/// wall-clock TTL. Logs requeued, deadline-failed, and TTL-expired counts
/// separately; store errors are logged and the loop continues.
fn spawn_reaper(state: Arc<AppState>, interval_secs: u64) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            let max_attempts = state.max_attempts;
            // Snapshot the live-earner addresses BEFORE taking the store lock, so the
            // registry lock and the store lock are never held at once (same discipline
            // as drain_attestations). The liveness reaper reclaims an in_flight job
            // whose recorded WS holder is not in this set.
            let live: HashSet<String> = {
                let now = now_secs();
                let ttl = state.earner_ttl_secs;
                let earners = state.earners.lock().await;
                earners
                    .iter()
                    .filter(|(_, info)| info.is_live(now, ttl))
                    .map(|(addr, _)| addr.clone())
                    .collect()
            };
            {
                let store = state.store.lock().await;
                match store.reap_expired(now_secs(), max_attempts) {
                    Ok(outcome) => {
                        if !outcome.requeued.is_empty() {
                            tracing::info!(
                                count = outcome.requeued.len(),
                                "requeued expired in-flight jobs"
                            );
                        }
                        if !outcome.failed.is_empty() {
                            tracing::warn!(
                                count = outcome.failed.len(),
                                "dead-lettered jobs (max attempts exhausted)"
                            );
                        }
                    }
                    Err(e) => tracing::error!(?e, "reaper: reap_expired failed"),
                }
                // Absolute-TTL backstop, on the same lock + clock as the deadline
                // sweep. Catches the poison job the deadline reaper and the
                // attempt/fault budgets cannot: one parked in `queued` by a single
                // faulting earner, never re-dispatched, never reaching max_faults.
                match store.reap_ttl_expired(now_secs(), state.ttl_deadline_multiple) {
                    Ok(expired) if !expired.is_empty() => {
                        tracing::warn!(
                            count = expired.len(),
                            "dead-lettered jobs (wall-clock TTL exceeded)"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(?e, "reaper: reap_ttl_expired failed"),
                }
                // Liveness reap: reclaim an in_flight job whose recorded WS holder has
                // dropped out of the live set (silent past earner_ttl_secs), on the
                // earner-TTL timescale instead of the per-job deadline. Grace ==
                // earner_ttl_secs so a healthy earner's heartbeat (which refreshes both
                // last_seen and the job's started_at) never trips it. HTTP-dispatched
                // jobs (NULL holder) are skipped and stay on the deadline reaper.
                match store.reap_stale_holders(
                    &live,
                    now_secs(),
                    state.earner_ttl_secs,
                    max_attempts,
                ) {
                    Ok(outcome) => {
                        if !outcome.requeued.is_empty() {
                            tracing::info!(
                                count = outcome.requeued.len(),
                                "requeued in-flight jobs whose holder went stale"
                            );
                        }
                        if !outcome.failed.is_empty() {
                            tracing::warn!(
                                count = outcome.failed.len(),
                                "dead-lettered stale-holder jobs (max attempts exhausted)"
                            );
                        }
                    }
                    Err(e) => tracing::error!(?e, "reaper: reap_stale_holders failed"),
                }
            } // store lock dropped here, before we touch earners

            let removed = {
                let mut earners = state.earners.lock().await;
                prune_stale_earners(&mut earners, now_secs(), state.earner_ttl_secs)
            };
            if removed > 0 {
                tracing::info!(removed, "pruned stale earners");
            }

            // Retention: delete aged terminal-job history in bounded batches, with
            // the store lock released between batches so a large aged backlog never
            // stalls dispatch/settle. Runs on the same tick as the reaps; in steady
            // state it prunes only what just aged past `retention_secs`.
            let pruned = prune_terminal_history(&state).await;
            if pruned > 0 {
                tracing::info!(pruned, "pruned aged terminal jobs (retention)");
            }
        }
    });
}

/// One retention sweep: delete aged terminal jobs (past `retention_secs`, with no
/// still-pending on-chain obligation) and their dependent rows, in bounded batches
/// with the store lock RELEASED between batches so dispatch/settle never stall
/// behind a large delete (FM2). Prunes at most
/// `RETENTION_BATCH * RETENTION_MAX_BATCHES_PER_TICK` jobs per call; a larger aged
/// backlog finishes over subsequent ticks. Mirrors `drain_attestations`'
/// claim-release-reacquire rhythm. Returns the number of jobs pruned.
async fn prune_terminal_history(state: &Arc<AppState>) -> usize {
    let horizon = state.retention_secs;
    let mut total = 0;
    for _ in 0..RETENTION_MAX_BATCHES_PER_TICK {
        let pruned = {
            let mut store = state.store.lock().await;
            match store.prune_terminal_jobs(now_secs(), horizon, RETENTION_BATCH) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(?e, "retention: prune_terminal_jobs failed");
                    break;
                }
            }
        }; // store lock dropped here, between batches
        total += pruned;
        if pruned < RETENTION_BATCH {
            break; // backlog drained this tick
        }
    }
    total
}

/// Spawn the attestation relayer: every `interval_secs`, drain the pending-receipt
/// backlog through `relay` in `batch_size`-capped chunks. Mirrors `spawn_reaper`;
/// only the real binary spawns it.
fn spawn_relayer<R: Relay + 'static>(
    state: Arc<AppState>,
    relay: R,
    interval_secs: u64,
    batch_size: usize,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            drain_attestations(&state, &relay, batch_size).await;
        }
    });
}

/// Drain pending EAS receipts oldest-first through `relay`, settling each to its
/// on-chain attestation UID. Each step claims a `batch_size`-capped chunk and
/// submits it as ONE `issueReceipts` (`multiAttest`) call, marking each row with
/// its submission-order uid; the loop then claims the next chunk until the backlog
/// is drained.
///
/// `issueReceipts` is atomic — any element reverting (one already on-chain, or one
/// bad arg) rolls the WHOLE batch back. So a batch revert falls back to
/// per-receipt single `submit`s ([`drain_singly`]) to isolate the offender: the
/// already-issued/bad element self-identifies (`AlreadyIssued` → marked, a per-row
/// Permanent → DEAD-LETTERED + skipped) while the rest still drain. A whole-batch
/// Transient backs off to the next tick; a whole-batch Permanent stops loudly (a
/// global misconfig — an unauthorized signer — never folded into the per-row
/// dead-letter). A reverted/failed chunk leaves every un-dead-lettered row pending
/// (nothing partial-marked), so the atomic revert reconciles to all-pending and is
/// retried.
///
/// The store lock is NEVER held across a submit await: a chunk is claimed under
/// the lock, the lock is dropped for the (slow) on-chain call, and only
/// re-acquired to mark each result. So settles and `/stats` never stall behind
/// network latency.
async fn drain_attestations<R: Relay>(state: &Arc<AppState>, relay: &R, batch_size: usize) {
    loop {
        let claimed = {
            let store = state.store.lock().await;
            store.claim_oldest_pending_batch(batch_size)
        }; // lock dropped before the on-chain submit below
        let claimed = match claimed {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(?e, "relay: claim_oldest_pending_batch failed");
                return;
            }
        };
        if claimed.is_empty() {
            return; // backlog drained
        }

        let atts: Vec<crate::eas::PendingAttestation> =
            claimed.iter().map(|(_, att)| att.clone()).collect();
        match relay.submit_batch(&atts).await {
            Ok(uids) => {
                // The contract returns exactly `n` uids in submission order; a short
                // return would mis-map a job to the wrong/stale uid (the contract
                // itself reverts on an indexed read past the end). Mark nothing and
                // back off rather than corrupt the mapping.
                if uids.len() != claimed.len() {
                    tracing::error!(
                        got = uids.len(),
                        want = claimed.len(),
                        "relay: issueReceipts returned the wrong uid count; marking nothing"
                    );
                    return;
                }
                for ((job_id, _), uid) in claimed.iter().zip(uids.iter()) {
                    if !mark_relayed(state, job_id, uid).await {
                        return;
                    }
                }
            }
            Err(BatchRelayError::Reverted(msg)) => {
                tracing::warn!(%msg, n = claimed.len(), "relay: batch reverted; isolating via single submits");
                if !drain_singly(state, relay, &claimed).await {
                    return;
                }
            }
            Err(BatchRelayError::Transient(msg)) => {
                tracing::warn!(%msg, "relay: transient batch failure; retrying next tick");
                return; // back off; the whole chunk stays pending
            }
            Err(BatchRelayError::Permanent(msg)) => {
                tracing::error!(%msg, "relay: permanent batch failure; draining paused (check coordinator authorization)");
                return; // whole chunk stays pending
            }
        }
    }
}

/// Mark one relayed receipt under the lock. Returns `false` only on a store error
/// (the caller stops the drain); a no-op re-mark (the row was already marked by a
/// recovered/duplicate drain) is logged and treated as success so the rest of the
/// backlog keeps draining.
async fn mark_relayed(state: &Arc<AppState>, job_id: &uuid::Uuid, uid: &str) -> bool {
    let marked = {
        let store = state.store.lock().await;
        store.mark_submitted(job_id, uid, now_secs())
    };
    match marked {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(%job_id, "relay: receipt already marked submitted");
            true
        }
        Err(e) => {
            tracing::error!(%job_id, ?e, "relay: mark_submitted failed");
            false
        }
    }
}

/// Quarantine one receipt the relayer hit a non-retryable (`Permanent`) error on,
/// under the lock. Returns `false` only on a store error (the caller stops the
/// drain); a no-op mark (the row was already submitted or already dead-lettered by
/// a recovered/duplicate drain) is logged and treated as handled so the rest of the
/// chunk keeps draining. Mirrors [`mark_relayed`].
async fn dead_letter_attestation(state: &Arc<AppState>, job_id: &uuid::Uuid) -> bool {
    let marked = {
        let store = state.store.lock().await;
        store.mark_attestation_dead_lettered(job_id, now_secs())
    };
    match marked {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(%job_id, "relay: receipt already settled or dead-lettered; skipping");
            true
        }
        Err(e) => {
            tracing::error!(%job_id, ?e, "relay: mark_attestation_dead_lettered failed");
            false
        }
    }
}

/// Isolate a reverted batch: re-submit each claimed receipt singly so the one
/// offending element (already on-chain, or bad) self-identifies while the rest
/// still drain. Mirrors the single-submit semantics — `AlreadyIssued` is an
/// idempotent success (marked with the sentinel), `Transient` backs off, and a
/// per-row `Permanent` DEAD-LETTERS that one receipt (quarantined + retained +
/// surfaced at `/stats dead_lettered_attestations`) and CONTINUES, so one poison
/// receipt never blocks the rest. Returns `true` once every receipt was handled (so
/// the outer loop claims the next chunk); a `Transient` single — or a store error —
/// returns `false` to stop this tick, leaving the unhandled rows pending.
async fn drain_singly<R: Relay>(
    state: &Arc<AppState>,
    relay: &R,
    claimed: &[(uuid::Uuid, crate::eas::PendingAttestation)],
) -> bool {
    for (job_id, att) in claimed {
        let uid = match relay.submit(att).await {
            Ok(uid) => uid,
            Err(RelayError::AlreadyIssued) => {
                tracing::info!(%job_id, "relay: receipt already on-chain; marking submitted");
                ALREADY_ISSUED_UID.to_string()
            }
            Err(RelayError::Transient(msg)) => {
                tracing::warn!(%job_id, %msg, "relay: transient submit failure; retrying next tick");
                return false;
            }
            Err(RelayError::Permanent(msg)) => {
                // A per-row, non-retryable fault (e.g. a receipt naming a region
                // whose render fee the coordinator can't cover, so issueReceipt
                // reverts) — NOT a global config problem like a whole-batch
                // unauthorized-signer revert. Quarantine THIS receipt and keep
                // draining the rest of the chunk, so one poison receipt never blocks
                // the others. The row is retained (the attestation is the canonical
                // proof of validated work + stays auditable) and surfaced at
                // `/stats dead_lettered_attestations`. On a mark error the row stays
                // pending, so stop rather than hot-loop.
                tracing::error!(%job_id, %msg, "relay: permanent submit failure; dead-lettering this receipt and continuing");
                if !dead_letter_attestation(state, job_id).await {
                    return false;
                }
                continue;
            }
        };
        if !mark_relayed(state, job_id, &uid).await {
            return false;
        }
    }
    true
}

/// Marker `tx_hash` stored when the contract reports the debit is already spent
/// (`ComputeMeter.spendOnce`'s jobId fence): the debit landed but the relay didn't
/// capture its real tx hash (a crash recovered between a prior `spendOnce` and its
/// local mark). The row is settled — it just carries this sentinel, not a spend tx.
const ALREADY_SPENT_TX: &str = "already-spent";

/// Spawn the debit relayer: every `interval_secs`, drain the pending-debit backlog
/// through `spender`. Mirrors `spawn_relayer`; only the real binary spawns it.
fn spawn_debit_relayer<S: Spender + 'static>(state: Arc<AppState>, spender: S, interval_secs: u64) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            drain_debits(&state, &spender).await;
        }
    });
}

/// Drain pending ComputeMeter debits oldest-first through `spender`, settling each
/// to its on-chain spend tx hash — the metering twin of [`drain_attestations`].
///
/// The store lock is NEVER held across the spend await: each debit is claimed
/// under the lock, the lock is dropped for the (slow) on-chain call, and only
/// re-acquired to mark the result, so settles and `/stats` never stall behind RPC
/// latency. `AlreadySpent` is an idempotent success (the debit is on-chain after a
/// recovered crash) so it is marked and the drain continues; `NotAuthorized` is
/// surfaced loudly + distinctly and STOPS the batch (a global misconfig — the
/// spender key needs `ComputeMeter.setSpender` — so every debit would revert); a
/// transient error stops the batch (backs off to the next tick). A `Permanent`
/// error is per-row: that one debit is DEAD-LETTERED (quarantined + retained +
/// surfaced at `/stats dead_lettered_debits`) and the drain CONTINUES, so one poison
/// debit never blocks the rest of the backlog. No debit is dropped or double-spent.
async fn drain_debits<S: Spender>(state: &Arc<AppState>, spender: &S) {
    loop {
        let claimed = {
            let store = state.store.lock().await;
            store.claim_oldest_pending_debit()
        }; // lock dropped before the on-chain spend below
        let claimed = match claimed {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(?e, "spender: claim_oldest_pending_debit failed");
                return;
            }
        };
        let Some((job_id, debit)) = claimed else { return }; // backlog drained

        let tx_hash = match spender.spend(&debit).await {
            Ok(tx) => tx,
            Err(SpendError::AlreadySpent) => {
                tracing::info!(%job_id, "spender: debit already spent on-chain; marking submitted");
                ALREADY_SPENT_TX.to_string()
            }
            Err(SpendError::NotAuthorized) => {
                tracing::error!(%job_id, "spender: NotAuthorized — the spender key is not authorized on ComputeMeter (owner must call setSpender); draining paused");
                return;
            }
            Err(SpendError::Transient(msg)) => {
                tracing::warn!(%job_id, %msg, "spender: transient spend failure; retrying next tick");
                return; // back off to the next tick
            }
            Err(SpendError::Permanent(msg)) => {
                // A per-row, non-retryable fault (e.g. an underfunded buyer's
                // InsufficientCredit) — NOT a global config problem like NotAuthorized.
                // Quarantine THIS debit and keep draining the rest, so one poison row
                // never blocks the backlog. The row is retained (the charge is still
                // owed + auditable) and surfaced at `/stats dead_lettered_debits`. On a
                // mark error the row stays pending, so back off rather than hot-loop.
                tracing::error!(%job_id, %msg, "spender: permanent spend failure; dead-lettering and continuing (e.g. an underfunded buyer's InsufficientCredit)");
                let marked = {
                    let store = state.store.lock().await;
                    store.mark_debit_dead_lettered(&job_id, now_secs())
                };
                match marked {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(%job_id, "spender: debit already settled or dead-lettered; skipping")
                    }
                    Err(e) => {
                        tracing::error!(%job_id, ?e, "spender: mark_debit_dead_lettered failed");
                        return;
                    }
                }
                continue;
            }
        };

        let marked = {
            let store = state.store.lock().await;
            store.mark_debit_submitted(&job_id, &tx_hash, now_secs())
        };
        match marked {
            Ok(true) => {}
            // The row was already marked (a concurrent/duplicate drain) — not an
            // error; keep draining the rest of the backlog.
            Ok(false) => tracing::warn!(%job_id, "spender: debit already marked submitted"),
            Err(e) => {
                tracing::error!(%job_id, ?e, "spender: mark_debit_submitted failed");
                return;
            }
        }
    }
}

/// One synthetic queued job per `JobKind`, so a fresh coordinator presents the
/// wedge HUD a representative backlog — every kind shows up in `queued_by_kind`
/// — rather than a single Terrain job. Region/inputs are placeholders; only the
/// spread of kinds matters here. Enqueued only when the DB is empty (see
/// `with_store`), so a restart with existing jobs never double-seeds.
fn seed_jobs() -> Vec<JobSpec> {
    JobKind::ALL
        .into_iter()
        .enumerate()
        .map(|(i, kind)| JobSpec {
            id: Uuid::new_v4(),
            kind,
            region: RegionCoord {
                x: 42 + i as i32,
                y: -17,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({ "seed": i as u64 }),
        })
        .collect()
}

/// A single Terrain job. Only used by tests (as a generic `JobSpec` fixture);
/// the production seed path uses `seed_jobs`. `#[cfg(test)]` keeps it out of the
/// release build so it isn't flagged as dead code.
#[cfg(test)]
fn seed_job() -> JobSpec {
    JobSpec {
        id: Uuid::new_v4(),
        kind: JobKind::Terrain,
        region: RegionCoord {
            x: 42,
            y: -17,
            layer: 0,
        },
        deadline_secs: 60,
        max_payout_wei: "1000000000000000000".into(),
        inputs: serde_json::json!({"heightfield_seed": 0xb1acf1e1du64}),
    }
}

/// Default-timeout router. Production (`main`) builds the router with the
/// configured timeout via [`router_with_body_timeout`]; this thin wrapper backs
/// the test suite, which exercises the router at the default bound.
#[cfg(test)]
fn router(state: Arc<AppState>) -> Router {
    router_with_body_timeout(state, DEFAULT_HTTP_BODY_TIMEOUT)
}

/// [`router`] with a caller-chosen request-body read timeout on the body-bearing
/// mutating routes, so the slow-body test can shrink it to a sub-second value
/// without a slow suite. The bound is a total request [`TimeoutLayer`] (responds
/// `408`) applied per-route to `POST /register` and `POST /jobs/{id}/submit`
/// only — the `/ws` upgrade and the GET routes carry no request body, so wrapping
/// them would needlessly bound the (legitimately open-ended) ws session and the
/// poll handlers. These two handlers do no network I/O and only fast local DB
/// work, so a slow body is the only thing that realistically approaches the bound
/// — see [`DEFAULT_HTTP_BODY_TIMEOUT`].
fn router_with_body_timeout(state: Arc<AppState>, body_read_timeout: Duration) -> Router {
    // 408 Request Timeout (explicit, vs the deprecated `new`): a slow/stalled
    // body on these routes is closed with a legible status the earner's reqwest
    // path treats as a transient error to retry, not a hard fault.
    let body_timeout = TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, body_read_timeout);
    // The body-bearing POST routes carry two guards together: a body-read timeout
    // (408 on a stalled body) and a pre-parse size cap (413 before serde buffers /
    // allocates — see [`MAX_REQUEST_BODY_BYTES`]). Bundled in one `ServiceBuilder`
    // so each route takes a single `.layer()` (two chained `.layer()`s on a
    // `MethodRouter` leave the error type unbound); both inner layers are `Copy`,
    // so the closure mints a fresh stack per route.
    let body_guard =
        || ServiceBuilder::new().layer(body_timeout).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES));
    Router::new()
        .route("/health", get(health))
        .route("/register", post(register).layer(body_guard()))
        .route("/stats", get(stats))
        .route("/earners", get(earners))
        // POST carries the body guards (a mutating, body-bearing route like
        // /register and /submit); GET /jobs is a body-less listing left unwrapped,
        // so the layer applies to the post handler only, then the get added after.
        .route("/jobs", post(create_job).layer(body_guard()).get(list_jobs))
        .route("/jobs/{id}", get(job_detail))
        .route("/jobs/next", get(next_job))
        .route("/jobs/{id}/submit", post(submit).layer(body_guard()))
        .route("/jobs/{id}/status", get(job_status))
        // Operator recovery: re-arm a dead-lettered debit (body-less; the same
        // bearer-token gate as POST /jobs, enforced inside the handler). /redrive-all
        // is a one-segment sibling of /dead-lettered, distinct from /{id}/redrive.
        .route("/debits/dead-lettered", get(dead_lettered_debits))
        .route("/debits/{id}/redrive", post(redrive_debit))
        .route("/debits/redrive-all", post(redrive_all_debits))
        .route("/receipts/dead-lettered", get(dead_lettered_receipts))
        .route("/receipts/{id}/redrive", post(redrive_receipt))
        .route("/receipts/redrive-all", post(redrive_all_receipts))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// True if `s` is a `0x`-prefixed 40-hex-digit Ethereum-style address — the
/// exact shape an earner derives (`keccak(pubkey)[12..]`) and the settle-time
/// signature gate compares against case-insensitively (`verify::verify_signature`
/// uses `eq_ignore_ascii_case`), so anything that registers can still produce a
/// verifiable result. Mixed-case (EIP-55-checksummed) addresses are accepted.
fn is_evm_address(s: &str) -> bool {
    let Some(hex) = s.strip_prefix("0x") else {
        return false;
    };
    hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// True iff `headers` carries `Authorization: Bearer <token>` whose token equals
/// `expected`. The token bytes are compared in CONSTANT TIME (`subtle::ct_eq`): a
/// normal `==` would early-exit at the first differing byte, leaking — over many
/// timed requests — the secret one byte at a time. A missing header, a value that
/// isn't valid visible-ASCII (`to_str` returns `Err`, never panics — FM3), a
/// non-`Bearer` scheme, or a wrong token all return `false`, which the caller maps
/// to a single uniform `401` so an attacker can't distinguish the cases. `ct_eq`
/// on byte slices is constant-time for equal lengths and short-circuits only on a
/// length mismatch — the token's length is not the secret, its content is, so a
/// wrong token of the SAME length (the byte-recovery attack) is the constant-time
/// path (pinned by a test).
fn ingest_authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|presented| presented.as_bytes().ct_eq(expected.as_bytes()).into())
}

/// Upper bounds on self-reported `Hello` fields, enforced by `validate_hello` so a
/// malformed or oversized registration can't bloat the in-memory registry or
/// poison the `/stats` aggregates. Both are generous headroom — hygiene, not
/// exclusion (FM1), and documented policy like the `vram_gb > 0` floor:
/// * a real GPU model string is ~25 chars (`"NVIDIA GeForce RTX 4090"`); 128 is
///   ample slack for any official name without letting a client store kilobytes.
/// * the largest single accelerator today is ~192 GB (B200); 1024 is far above any
///   single GPU for the foreseeable future, so an honest card always passes.
const MAX_GPU_MODEL_LEN: usize = 128;
const MAX_VRAM_GB: u32 = 1024;

/// Reject a malformed `Hello` before it enters the registry so `/stats` and
/// `/earners` only ever reflect well-formed earners: a blank/short address would
/// surface as an unmatchable leaderboard row, an empty `supported` set is an
/// earner that can never be offered a job, and a zero `vram_gb` pollutes the
/// `total_vram_gb` total. Rejecting `vram_gb == 0` is a deliberate policy for a
/// GPU mesh — it also excludes a CPU/iGPU earner that honestly reports no
/// dedicated VRAM, which is acceptable here because the real earner defaults to
/// 24 and no GPU operator reports 0.
///
/// Beyond the structural checks it also bounds the self-reported sizes (see
/// [`MAX_GPU_MODEL_LEN`]/[`MAX_VRAM_GB`]) and rejects a `supported` set carrying
/// DUPLICATE kinds — duplicates are rejected rather than silently de-duplicated so
/// the stored set always equals the reported set (no surprise mutation), and the
/// honest earner sends a clean set anyway; a dup would otherwise double-count its
/// kind in `/stats supported_breakdown`. `Err` carries the reason for the reject
/// log. Shared by the HTTP `/register` and WS `Hello` paths so neither can
/// pollute the registry the other guards.
///
/// Finally it enforces key possession: `signature_hex` must recover to the
/// claimed `earner_address` over [`proto::hello_digest`] (built with `nonce` —
/// the WS per-connection challenge, or empty on the HTTP path), so a client can
/// only register an address it holds the key for (not spoof another earner's
/// identity onto the leaderboard / fault ledger), and on WS only against the
/// challenge issued for *this* connection (anti-replay). Checked last — after the
/// cheap structural gates — so a malformed Hello costs no curve recovery.
fn validate_hello(
    earner_address: &str,
    gpu_model: &str,
    vram_gb: u32,
    supported: &[JobKind],
    nonce: &[u8],
    signature_hex: &str,
) -> Result<(), &'static str> {
    if !is_evm_address(earner_address) {
        return Err("earner_address is not a 0x-prefixed 20-byte hex address");
    }
    if gpu_model.len() > MAX_GPU_MODEL_LEN {
        return Err("gpu_model exceeds the maximum length");
    }
    if supported.is_empty() {
        return Err("supported is empty: earner advertises no renderable kinds");
    }
    let mut seen = HashSet::new();
    if !supported.iter().all(|k| seen.insert(*k)) {
        return Err("supported contains duplicate kinds");
    }
    if vram_gb == 0 {
        return Err("vram_gb is zero");
    }
    if vram_gb > MAX_VRAM_GB {
        return Err("vram_gb exceeds the plausible maximum");
    }
    // Key-possession check last: the structural checks above are cheap, so a
    // malformed Hello is rejected before the keccak + secp256k1 recovery (cheap
    // DoS triage). A signature that doesn't recover to the claimed address means
    // the registrant doesn't hold the key, so the identity the capability filter,
    // fault attribution, and `/stats` totals key on would be unauthenticated.
    if let Err(e) = verify::verify_hello_signature(
        earner_address,
        gpu_model,
        vram_gb,
        supported,
        nonce,
        signature_hex,
    ) {
        return Err(match e {
            verify::VerifyError::BadSignatureEncoding => "hello signature is malformed",
            verify::VerifyError::NonCanonicalSignature => {
                "hello signature is non-canonical (high-S)"
            }
            verify::VerifyError::Unrecoverable => "hello signature is unrecoverable",
            verify::VerifyError::AddressMismatch => "hello signature does not match earner_address",
        });
    }
    Ok(())
}

/// Earner → coordinator registration. Accepts an `EarnerMsg::Hello` and
/// upserts the earner keyed by address. Other `EarnerMsg` variants are
/// rejected here (job dispatch lives on its own routes for now).
async fn register(
    State(state): State<Arc<AppState>>,
    Extension(PeerAddr(peer)): Extension<PeerAddr>,
    headers: HeaderMap,
    Json(msg): Json<EarnerMsg>,
) -> Result<&'static str, StatusCode> {
    // Per-source registration rate limit FIRST — ahead of the secp256k1 verify in
    // `validate_hello` (cheap-reject-first, FM2) — so an over-limit source is shed
    // with 429 before any curve recovery or registry lock. The source is the
    // connection peer, or the real client recovered from X-Forwarded-For when the
    // peer is a trusted proxy; the bucket lock is held only for the check + consume.
    let source = resolve_source_ip(peer.ip(), &headers, &state.trusted_proxies);
    {
        let now = now_secs();
        let mut buckets = state.registration_buckets.lock().await;
        if !check_registration_rate(
            &mut buckets,
            source,
            now,
            state.max_registrations,
            REGISTRATION_WINDOW_SECS,
            MAX_REGISTRATION_BUCKETS,
        ) {
            tracing::warn!(%source, "rejected registration: per-source rate limit exceeded");
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    }

    let EarnerMsg::Hello {
        earner_address,
        gpu_model,
        vram_gb,
        supported,
        signature_hex,
    } = msg
    else {
        return Err(StatusCode::BAD_REQUEST);
    };

    if let Err(reason) =
        validate_hello(&earner_address, &gpu_model, vram_gb, &supported, &[], &signature_hex)
    {
        tracing::warn!(address = %earner_address, reason, "rejected malformed registration");
        return Err(StatusCode::BAD_REQUEST);
    }
    // Identity is keyed on the address string, so fold it to canonical lowercase now
    // that the signature (which commits to the as-sent case) has verified — a client
    // varying case across boundaries must not split into two identities.
    let earner_address = verify::canonical_earner_address(&earner_address);

    let now = now_secs();
    let admitted = admit_earner(
        &mut *state.earners.lock().await,
        earner_address.clone(),
        EarnerInfo {
            gpu_model,
            vram_gb,
            supported,
            last_seen: now,
        },
        state.max_earners,
        now,
        state.earner_ttl_secs,
    );
    if !admitted {
        // Registry full of LIVE earners: shed with a retryable 503 (matches the
        // queue-cap backpressure). A stale earner aging past its TTL frees a slot.
        tracing::warn!(
            address = %earner_address,
            cap = state.max_earners,
            "rejected registration: earner registry at capacity (all earners live)"
        );
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    tracing::info!(address = %earner_address, vram_gb, "earner registered");
    Ok("registered")
}

/// `POST /jobs` — enqueue a new render job. Validates the request, mints a fresh
/// server-side id, and persists the spec `queued`. The id is assigned here (not
/// taken from the caller) so a client can never collide with — and via
/// `enqueue`'s `ON CONFLICT(id) DO UPDATE` overwrite — an existing job.
///
/// Returns `201 Created` with the assigned id; `422` on a spec the ingestion
/// gate rejects (bad deadline / payout / oversized inputs); `503` when the queued
/// backlog is already at `--max-queued-jobs` (retryable backpressure). A body that
/// fails to deserialize (unknown `kind`, wrong types) is rejected by the `Json`
/// extractor before this runs.
///
/// When the coordinator is started with an ingest token (`--ingest-token` /
/// `COORDINATOR_INGEST_TOKEN`), a request must carry `Authorization: Bearer
/// <token>` or it is rejected `401` before any store work — no job is enqueued,
/// no lock taken, for an unauthorized caller. With no token configured the
/// endpoint is open (dev posture, warned at startup). The request body is parsed
/// by the `Json` extractor before this gate runs (axum runs all extractors first),
/// but it is bounded by the pre-parse size cap + body-read timeout, so the gate
/// protects the queue, not the (already-capped) parse. `HeaderMap` precedes the
/// body-consuming `Json` extractor, which must come last.
async fn create_job(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateJobRequest>,
) -> Result<(StatusCode, Json<CreateJobResponse>), StatusCode> {
    if let Some(expected) = state.ingest_token.as_deref() {
        if !ingest_authorized(&headers, expected) {
            tracing::warn!("rejected: POST /jobs missing/invalid bearer ingest token");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    if let Err(e) = validate::validate_job_spec(req.deadline_secs, &req.max_payout_wei, &req.inputs)
    {
        tracing::warn!(reason = e.reason(), "rejected: malformed job spec");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    // An attributed buyer must be a well-formed EVM address — the SAME shape
    // is_evm_address gates the earner address by, so a buyer accepted here is one
    // ComputeMeter.spend can later debit. Rejected like any other malformed field
    // (422), before anything is enqueued. Absent buyer is valid (unmetered).
    let buyer = req.buyer;
    if let Some(b) = &buyer {
        if !is_evm_address(b) {
            tracing::warn!("rejected: malformed job buyer address");
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }
    let spec = JobSpec {
        id: Uuid::new_v4(),
        kind: req.kind,
        region: req.region,
        deadline_secs: req.deadline_secs,
        max_payout_wei: req.max_payout_wei,
        inputs: req.inputs,
    };
    let id = spec.id;
    {
        let store = state.store.lock().await;
        match store.enqueue_within_cap(&spec, state.max_queued_jobs, buyer.as_deref()) {
            Ok(true) => {}
            Ok(false) => {
                // Backlog full: shed with a retryable 503 (NOT a 500) so a
                // well-behaved producer backs off and retries rather than treating
                // it as a hard failure. A dispatched or reaped job frees a slot.
                tracing::warn!(cap = state.max_queued_jobs, "rejected: job queue at capacity");
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            Err(e) => {
                tracing::error!(?id, ?e, "create_job: enqueue failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
    tracing::info!(?id, kind = ?spec.kind, "job enqueued");
    Ok((StatusCode::CREATED, Json(CreateJobResponse { id })))
}

/// `POST /debits/{id}/redrive` — operator recovery: re-arm the dead-lettered
/// ComputeMeter debit for job `{id}` so the next drain re-attempts it, used after
/// the `Permanent` cause is fixed (e.g. an underfunded buyer tops up the credit the
/// `InsufficientCredit` revert was about). A dead-lettered debit is never
/// auto-re-claimed, so this is the only path back into the drainable backlog.
///
/// A privileged recovery action, so it carries the SAME bearer-token gate as
/// `POST /jobs` (`--ingest-token` / `COORDINATOR_INGEST_TOKEN`): when a token is
/// configured, a request without `Authorization: Bearer <token>` is rejected `401`
/// before any store work — nothing is re-armed for an unauthorized caller. With no
/// token configured the endpoint is open (the same dev posture as ingestion).
///
/// Returns `200` with `{"rearmed": bool}`: `true` when a dead-lettered, unsettled
/// debit was re-armed; `false` for the idempotent no-op cases (the row was not
/// dead-lettered, was already settled — re-arming would risk a double-spend, so the
/// store refuses it — or no such debit exists). A re-armed debit re-enters the
/// oldest-first drain and, if the buyer is still underfunded, simply re-dead-letters
/// on the next `Permanent` error — one attempt per re-drive, never an auto-retry.
async fn redrive_debit(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<RedriveResponse>, StatusCode> {
    if let Some(expected) = state.ingest_token.as_deref() {
        if !ingest_authorized(&headers, expected) {
            tracing::warn!(%id, "rejected: POST /debits/{{id}}/redrive missing/invalid bearer ingest token");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let store = state.store.lock().await;
    match store.redrive_dead_lettered_debit(&id) {
        Ok(rearmed) => {
            if rearmed {
                tracing::info!(%id, "re-drive: dead-lettered debit re-armed for the next drain");
            } else {
                tracing::info!(%id, "re-drive: no dead-lettered, unsettled debit to re-arm (no-op)");
            }
            Ok(Json(RedriveResponse { rearmed }))
        }
        Err(e) => {
            tracing::error!(%id, ?e, "re-drive: redrive_dead_lettered_debit failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `POST /debits/redrive-all` — operator bulk recovery: re-arm EVERY dead-lettered
/// ComputeMeter debit in one call, used after a BROAD `Permanent` cause is fixed (the
/// spender key re-authorized via `setSpender`, or a whole class of underfunded buyers
/// tops up) — the bulk twin of `POST /debits/{id}/redrive` that saves enumerating
/// `GET /debits/dead-lettered` and issuing N single re-drives.
///
/// A privileged MASS-recovery action, so it carries the SAME bearer-token gate as
/// `POST /jobs` and the single re-drive: when a token is configured, a request without
/// `Authorization: Bearer <token>` is rejected `401` before any store work — nothing is
/// re-armed for an unauthorized caller. Open when no token is configured (the dev
/// posture). The store clears only `dead_lettered_at IS NOT NULL AND tx_hash IS NULL`
/// rows, so a pending (drainable) or settled (paid) debit is never touched.
///
/// Returns `200` with `{"rearmed": count}` — the number re-armed, `0` for the
/// idempotent no-op (nothing dead-lettered). Each re-armed debit re-enters the
/// oldest-first drain and, if the broad cause is NOT actually fixed, simply
/// re-dead-letters on the next `Permanent` error — one attempt per re-drive, never an
/// auto-retry loop.
async fn redrive_all_debits(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<BulkRedriveResponse>, StatusCode> {
    if let Some(expected) = state.ingest_token.as_deref() {
        if !ingest_authorized(&headers, expected) {
            tracing::warn!("rejected: POST /debits/redrive-all missing/invalid bearer ingest token");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let store = state.store.lock().await;
    match store.redrive_all_dead_lettered_debits() {
        Ok(rearmed) => {
            tracing::info!(rearmed, "bulk re-drive: dead-lettered debits re-armed for the next drain");
            Ok(Json(BulkRedriveResponse { rearmed }))
        }
        Err(e) => {
            tracing::error!(?e, "bulk re-drive: redrive_all_dead_lettered_debits failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `POST /receipts/{id}/redrive` — operator recovery: re-arm the dead-lettered EAS
/// render receipt for job `{id}` so the next drain re-attempts it, used after the
/// `Permanent` cause is fixed (e.g. the coordinator funds the region fee whose
/// unpayability reverted `issueReceipt`, or the operator authorizes the signer). A
/// dead-lettered receipt is never auto-re-claimed (`claim_oldest_pending_batch` skips
/// it), so this is the only path back into the drainable backlog. The attestation
/// twin of [`redrive_debit`].
///
/// A privileged recovery action, so it carries the SAME bearer-token gate as
/// `POST /jobs` (`--ingest-token` / `COORDINATOR_INGEST_TOKEN`): when a token is
/// configured, a request without `Authorization: Bearer <token>` is rejected `401`
/// before any store work — nothing is re-armed for an unauthorized caller. With no
/// token configured the endpoint is open (the same dev posture as ingestion).
///
/// Returns `200` with `{"rearmed": bool}`: `true` when a dead-lettered, not-yet-attested
/// receipt was re-armed; `false` for the idempotent no-op cases (the row was not
/// dead-lettered, was already attested — re-arming would resurrect a landed attestation,
/// so the store refuses it — or no such receipt exists). A re-armed receipt re-enters
/// the oldest-first batch drain and, if the cause is still unfixed, simply re-dead-letters
/// on the next `Permanent` error — one attempt per re-drive, never an auto-retry.
async fn redrive_receipt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<RedriveResponse>, StatusCode> {
    if let Some(expected) = state.ingest_token.as_deref() {
        if !ingest_authorized(&headers, expected) {
            tracing::warn!(%id, "rejected: POST /receipts/{{id}}/redrive missing/invalid bearer ingest token");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let store = state.store.lock().await;
    match store.redrive_dead_lettered_attestation(&id) {
        Ok(rearmed) => {
            if rearmed {
                tracing::info!(%id, "re-drive: dead-lettered attestation re-armed for the next drain");
            } else {
                tracing::info!(%id, "re-drive: no dead-lettered, unattested receipt to re-arm (no-op)");
            }
            Ok(Json(RedriveResponse { rearmed }))
        }
        Err(e) => {
            tracing::error!(%id, ?e, "re-drive: redrive_dead_lettered_attestation failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `POST /receipts/redrive-all` — operator bulk recovery: re-arm EVERY dead-lettered
/// EAS render receipt in one call, used after a BROAD `Permanent` cause is fixed (the
/// coordinator signer re-authorized via `setCoordinator`, or a whole class of region
/// fees funded) — the bulk twin of `POST /receipts/{id}/redrive`, the attestation twin
/// of [`redrive_all_debits`].
///
/// A privileged MASS-recovery action, so it carries the SAME bearer-token gate as
/// `POST /jobs` and the single re-drive: when a token is configured, a request without
/// `Authorization: Bearer <token>` is rejected `401` before any store work — nothing is
/// re-armed for an unauthorized caller. Open when no token is configured (the dev
/// posture). The store clears only `dead_lettered_at IS NOT NULL AND uid IS NULL` rows,
/// so a pending (drainable) or attested (landed) receipt is never touched.
///
/// Returns `200` with `{"rearmed": count}` — the number re-armed, `0` for the
/// idempotent no-op (nothing dead-lettered). Each re-armed receipt re-enters the
/// oldest-first batch drain and, if the broad cause is NOT actually fixed, simply
/// re-dead-letters on the next `Permanent` error — one attempt per re-drive, never an
/// auto-retry loop.
async fn redrive_all_receipts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<BulkRedriveResponse>, StatusCode> {
    if let Some(expected) = state.ingest_token.as_deref() {
        if !ingest_authorized(&headers, expected) {
            tracing::warn!("rejected: POST /receipts/redrive-all missing/invalid bearer ingest token");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let store = state.store.lock().await;
    match store.redrive_all_dead_lettered_attestations() {
        Ok(rearmed) => {
            tracing::info!(rearmed, "bulk re-drive: dead-lettered attestations re-armed for the next drain");
            Ok(Json(BulkRedriveResponse { rearmed }))
        }
        Err(e) => {
            tracing::error!(?e, "bulk re-drive: redrive_all_dead_lettered_attestations failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET /debits/dead-lettered` — operator enumeration of quarantined ComputeMeter
/// debits, so a stuck charge can be located and re-driven by id (`POST
/// /debits/{id}/redrive`) after the buyer tops up. Returns each dead-lettered,
/// not-yet-settled debit (oldest-first, capped at [`MAX_DEAD_LETTERED_LIST`]) plus
/// the full `total` and a `truncated` flag, so a capped page is never mistaken for
/// the whole set.
///
/// The listing exposes buyer addresses + owed amounts (a privileged operational
/// view), so it carries the SAME bearer-token gate as `POST /jobs` and `POST
/// /debits/{id}/redrive`: a missing/malformed/wrong/blank token is `401` before any
/// store work; open when no token is configured (the dev posture). A listed row is
/// always genuinely re-armable — the store filters to `dead_lettered_at IS NOT NULL
/// AND tx_hash IS NULL`, so a still-pending (drainable) or already-settled (paid)
/// debit never appears.
async fn dead_lettered_debits(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<DeadLetteredListing>, StatusCode> {
    if let Some(expected) = state.ingest_token.as_deref() {
        if !ingest_authorized(&headers, expected) {
            tracing::warn!("rejected: GET /debits/dead-lettered missing/invalid bearer ingest token");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let store = state.store.lock().await;
    let rows = match store.list_dead_lettered_debits(MAX_DEAD_LETTERED_LIST) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(?e, "dead_lettered_debits: list query failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // `total` is the dead-letter count (the same value `/stats dead_lettered_debits`
    // shows). It equals the count of LISTABLE rows because no debit is ever both
    // dead-lettered and settled — `mark_debit_dead_lettered` requires `tx_hash IS
    // NULL` and `claim_oldest_pending_debit` skips dead-lettered rows, so the listing's
    // extra `tx_hash IS NULL` filter never excludes a counted row. Thus `truncated` is
    // exact today; if that state-machine invariant ever changes, count the listable
    // predicate here instead.
    let total = match store.dead_lettered_debit_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, "dead_lettered_debits: count query failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let debits = rows
        .into_iter()
        .map(|(job_id, buyer, amount_wei, dead_lettered_at, redrive_count)| DeadLetteredDebit {
            job_id,
            buyer,
            amount_wei,
            dead_lettered_at,
            redrive_count,
        })
        .collect::<Vec<_>>();
    let truncated = total > debits.len();
    Ok(Json(DeadLetteredListing { debits, total, truncated }))
}

/// `GET /receipts/dead-lettered` — operator enumeration of quarantined EAS render
/// receipts, so a stuck attestation can be located and re-driven by id (`POST
/// /receipts/{id}/redrive`) once its `Permanent` cause is fixed (the region fee funded,
/// or the signer re-authorized via `setCoordinator`). Returns each dead-lettered,
/// not-yet-attested receipt (oldest-first, capped at [`MAX_DEAD_LETTERED_LIST`]) plus the
/// full `total` and a `truncated` flag, so a capped page is never mistaken for the whole
/// set. The attestation twin of [`dead_lettered_debits`] — it closes the
/// `mesh-attestation-dead-letter-redrive` deferral (no way to DISCOVER the stuck ids the
/// by-id re-drive needs; `/stats dead_lettered_attestations` surfaced only the count).
///
/// The listing exposes earner addresses + the attested compute (a privileged operational
/// view), so it carries the SAME bearer-token gate as `POST /jobs` and the re-drive
/// endpoints: a missing/malformed/wrong/blank token is `401` before any store work; open
/// when no token is configured (the dev posture). A listed row is always genuinely
/// re-armable — the store filters to `dead_lettered_at IS NOT NULL AND uid IS NULL`, so a
/// still-pending (drainable) or already-attested (landed) receipt never appears.
async fn dead_lettered_receipts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<DeadLetteredReceiptListing>, StatusCode> {
    if let Some(expected) = state.ingest_token.as_deref() {
        if !ingest_authorized(&headers, expected) {
            tracing::warn!("rejected: GET /receipts/dead-lettered missing/invalid bearer ingest token");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let store = state.store.lock().await;
    let rows = match store.list_dead_lettered_attestations(MAX_DEAD_LETTERED_LIST) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(?e, "dead_lettered_receipts: list query failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // `total` is the dead-letter count (the same value `/stats dead_lettered_attestations`
    // shows). It equals the count of LISTABLE rows because no attestation is ever both
    // dead-lettered and attested — `mark_attestation_dead_lettered` requires `uid IS NULL`
    // and `claim_oldest_pending_batch` skips dead-lettered rows, so the listing's extra
    // `uid IS NULL` filter never excludes a counted row. Thus `truncated` is exact today;
    // if that state-machine invariant ever changes, count the listable predicate here instead.
    let total = match store.dead_lettered_attestation_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, "dead_lettered_receipts: count query failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let receipts = rows
        .into_iter()
        .map(
            |(job_id, earner, render_seconds, job_kind, dead_lettered_at, redrive_count)| {
                DeadLetteredReceipt {
                    job_id,
                    earner,
                    render_seconds,
                    job_kind,
                    dead_lettered_at,
                    redrive_count,
                }
            },
        )
        .collect::<Vec<_>>();
    let truncated = total > receipts.len();
    Ok(Json(DeadLetteredReceiptListing { receipts, total, truncated }))
}

async fn stats(State(state): State<Arc<AppState>>) -> Result<Json<Stats>, StatusCode> {
    let now = now_secs();
    let ttl = state.earner_ttl_secs;
    let earners = state.earners.lock().await;
    let mut gpus_joined: usize = 0;
    let mut total_vram_gb: u64 = 0;
    let mut supported_breakdown: HashMap<JobKind, usize> = HashMap::new();
    for info in earners.values() {
        if !info.is_live(now, ttl) {
            continue;
        }
        gpus_joined += 1;
        total_vram_gb += info.vram_gb as u64;
        for kind in &info.supported {
            *supported_breakdown.entry(*kind).or_insert(0) += 1;
        }
    }
    let store = state.store.lock().await;
    let (jobs_queued, jobs_in_flight, jobs_completed, jobs_failed) = match (
        store.queued_count(),
        store.in_flight_count(),
        store.completed_count(),
        store.failed_count(),
    ) {
        (Ok(q), Ok(f), Ok(c), Ok(d)) => (q, f, c, d),
        (q, f, c, d) => {
            tracing::error!(?q, ?f, ?c, ?d, "stats: store count failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let queued_by_kind = match store.queued_count_by_kind() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(?e, "stats: queued_count_by_kind failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // Computed under the same store lock (earners still in scope) so the
    // load-bearing `earners ⊃ store` lock order is preserved.
    let in_flight_by_kind = match store.in_flight_count_by_kind() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(?e, "stats: in_flight_count_by_kind failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let completed_by_kind = match store.done_count_by_kind() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(?e, "stats: done_count_by_kind failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let failed_by_kind = match store.failed_count_by_kind() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(?e, "stats: failed_count_by_kind failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let total_render_seconds = match store.total_render_seconds() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, "stats: total_render_seconds failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let total_payout_wei = match store.total_payout_wei() {
        Ok(w) => w.to_string(),
        Err(e) => {
            tracing::error!(?e, "stats: total_payout_wei failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // Age of the longest-running in-flight dispatch (`now - oldest started_at`),
    // or None when nothing is in flight. `max(0)` guards against clock skew where
    // a heartbeat-bumped `started_at` briefly leads `now`. Still under the held
    // store lock, so the load-bearing `earners ⊃ store` order is preserved.
    let oldest_in_flight_secs = match store.oldest_in_flight_started_at() {
        Ok(Some(ts)) => Some(now.saturating_sub(ts).max(0) as u64),
        Ok(None) => None,
        Err(e) => {
            tracing::error!(?e, "stats: oldest_in_flight_started_at failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // Mean in-flight progress (None when nothing is in flight). Still under the held
    // store lock, preserving the load-bearing `earners ⊃ store` order.
    let in_flight_progress_pct_avg = match store.in_flight_progress_pct_avg() {
        Ok(avg) => avg,
        Err(e) => {
            tracing::error!(?e, "stats: in_flight_progress_pct_avg failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let jobs_redispatched = match store.redispatched_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, "stats: redispatched_count failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // Gross attempt + fault volume in one scan (cheaper than total_payout_wei
    // above, which decodes every done spec); still under the held store lock so
    // the `earners ⊃ store` order is preserved.
    let (total_attempts, total_faults) = match store.attempt_fault_totals() {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(?e, "stats: attempt_fault_totals failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let pending_attestations = match store.pending_attestation_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, "stats: pending_attestation_count failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let dead_lettered_attestations = match store.dead_lettered_attestation_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, "stats: dead_lettered_attestation_count failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let pending_debits = match store.pending_debit_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, "stats: pending_debit_count failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let dead_lettered_debits = match store.dead_lettered_debit_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, "stats: dead_lettered_debit_count failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // Age (now - MIN stamp) of the oldest stuck row in each dead-letter backlog, the
    // age twin of `oldest_in_flight_secs`. `.max(0)` floors a future stamp (clock skew)
    // at 0 rather than wrapping to a huge u64; `None` when nothing is quarantined.
    let oldest_dead_lettered_attestation_secs = match store.oldest_dead_lettered_attestation_at() {
        Ok(Some(ts)) => Some(now.saturating_sub(ts).max(0) as u64),
        Ok(None) => None,
        Err(e) => {
            tracing::error!(?e, "stats: oldest_dead_lettered_attestation_at failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let oldest_dead_lettered_debit_secs = match store.oldest_dead_lettered_debit_at() {
        Ok(Some(ts)) => Some(now.saturating_sub(ts).max(0) as u64),
        Ok(None) => None,
        Err(e) => {
            tracing::error!(?e, "stats: oldest_dead_lettered_debit_at failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    Ok(Json(Stats {
        gpus_joined,
        total_vram_gb,
        jobs_queued,
        jobs_in_flight,
        jobs_completed,
        jobs_failed,
        supported_breakdown,
        queued_by_kind,
        in_flight_by_kind,
        completed_by_kind,
        failed_by_kind,
        total_render_seconds,
        total_payout_wei,
        oldest_in_flight_secs,
        in_flight_progress_pct_avg,
        jobs_redispatched,
        total_attempts,
        total_faults,
        pending_attestations,
        dead_lettered_attestations,
        pending_debits,
        dead_lettered_debits,
        oldest_dead_lettered_attestation_secs,
        oldest_dead_lettered_debit_secs,
    }))
}

/// `GET /earners` — live earner leaderboard for the HUD. Returns a JSON array of
/// the currently-live earners (same `is_live(now, ttl)` filter as `/stats`), each
/// with its advertised capabilities plus lifetime `completed` job count and
/// `render_seconds` total drawn from the `results` table. Stale earners are
/// excluded. Ordered by `completed` (then `render_seconds`, then `address`)
/// descending so the busiest earner leads and the order is deterministic.
///
/// Holds `earners ⊃ store` (the store aggregates are computed while the earners
/// guard is held), matching the `/stats` lock order to avoid a deadlock.
async fn earners(State(state): State<Arc<AppState>>) -> Result<Json<Vec<EarnerEntry>>, StatusCode> {
    let now = now_secs();
    let ttl = state.earner_ttl_secs;
    let earners = state.earners.lock().await;
    let store = state.store.lock().await;
    let completed = match store.completed_count_by_earner() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(?e, "earners: completed_count_by_earner failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let render_seconds = match store.render_seconds_by_earner() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(?e, "earners: render_seconds_by_earner failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let payout_wei = match store.payout_wei_by_earner() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(?e, "earners: payout_wei_by_earner failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let faults = match store.faults_by_earner() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(?e, "earners: faults_by_earner failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let mut out: Vec<EarnerEntry> = earners
        .iter()
        .filter(|(_, info)| info.is_live(now, ttl))
        .map(|(address, info)| EarnerEntry {
            address: address.clone(),
            gpu_model: info.gpu_model.clone(),
            vram_gb: info.vram_gb,
            supported: info.supported.clone(),
            last_seen: info.last_seen,
            completed: completed.get(address).copied().unwrap_or(0),
            render_seconds: render_seconds.get(address).copied().unwrap_or(0),
            payout_wei: payout_wei.get(address).copied().unwrap_or(0).to_string(),
            faults: faults.get(address).copied().unwrap_or(0),
        })
        .collect();
    // Leaderboard order, with address as a final tiebreak so the array is stable
    // across runs (HashMap iteration order is not).
    out.sort_by(|a, b| {
        b.completed
            .cmp(&a.completed)
            .then(b.render_seconds.cmp(&a.render_seconds))
            .then(a.address.cmp(&b.address))
    });
    Ok(Json(out))
}

/// Query string for `GET /jobs/next`. `earner`, when present and matching a
/// registered earner's address, restricts the hand-out to the kinds that earner
/// advertised in its `Hello`/`/register` — the capability match the websocket
/// dispatcher already enforces. Absent (a legacy poller) or unknown (an
/// unregistered/typo'd address) falls back to the unfiltered take, so a match
/// miss never starves a poller; it only risks being handed a kind it will
/// self-drop, exactly the pre-existing behavior.
#[derive(Debug, Deserialize)]
struct NextJobQuery {
    earner: Option<String>,
}

async fn next_job(State(state): State<Arc<AppState>>, Query(q): Query<NextJobQuery>) -> Response {
    // Capability match for the HTTP transport. The stateless `/jobs/next` used to
    // hand out any kind; an earner that advertised a SUBSET then self-dropped the
    // unsupported job (see the earner's poll_once guard) had already had an
    // attempt charged, so a job only incapable earners polled burned its budget
    // toward dead-letter. If the poll names a registered earner, hand out only
    // kinds it supports. The earners lock is taken and the supported set cloned
    // out HERE, before the store lock, so the two locks never overlap.
    let supported = match q.earner.as_deref() {
        Some(addr) => {
            // Resolve the poll to the canonical identity it registered under, so a
            // case-variant `earner=` param hits the same registry entry rather than
            // being treated as an unknown earner (which would skip the liveness
            // refresh and lapse to unfiltered dispatch).
            let addr = verify::canonical_earner_address(addr);
            // An identified poll is a sign of life: refresh last_seen (mirroring
            // the submit path) so an actively-polling HTTP earner stays live in
            // the registry — counted in /stats, and keeping THIS filter applicable
            // instead of lapsing to unfiltered once the reaper prunes it. Clones
            // the advertised kinds out; the earners lock drops at the block end,
            // before the store lock.
            let mut earners = state.earners.lock().await;
            earners.get_mut(&addr).map(|e| {
                e.last_seen = now_secs();
                e.supported.clone()
            })
        }
        None => None,
    };
    let store = state.store.lock().await;
    let taken = match &supported {
        Some(kinds) => store.take_next(|job| kinds.contains(&job.kind)),
        None => store.take_next(|_| true),
    };
    match taken {
        // Return the job and stamp its dispatch_seq in a header so the poller can
        // echo it on submit (the HTTP fence).
        Ok(Some((job, seq))) => {
            ([(DISPATCH_SEQ_HEADER, seq.to_string())], Json(Some(job))).into_response()
        }
        Ok(None) => Json(None::<JobSpec>).into_response(),
        Err(e) => {
            tracing::error!(?e, "next_job: take_next failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /jobs/{id}/status` — returns `{ "id": <uuid>, "status": ... }` for a
/// known job (status is one of queued/in_flight/done/failed), 404 for an
/// unknown id, 500 on a store error. Read-only; takes no write locks.
async fn job_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<JobStatusResponse>, StatusCode> {
    let store = state.store.lock().await;
    match store.job_status(&id) {
        Ok(Some(status)) => Ok(Json(JobStatusResponse { id, status })),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(?id, ?e, "job_status: lookup failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET /jobs/{id}` — full detail for a single job: its `JobSpec` plus the
/// recorded `JobResult` once the job has completed (`result` is `null` until
/// then). 404 for an unknown id, 500 on a store error. Read-only; takes a
/// single store lock. Distinct from the status-only `GET /jobs/{id}/status`.
async fn job_detail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<JobDetail>, StatusCode> {
    let store = state.store.lock().await;
    let spec = match store.get_job(&id) {
        Ok(Some(spec)) => spec,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(?id, ?e, "job_detail: get_job failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let result = match store.get_result(&id) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(?id, ?e, "job_detail: get_result failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    Ok(Json(JobDetail { spec, result }))
}

/// Upper bound on rows returned by `GET /jobs`, to keep the payload bounded.
const RECENT_JOBS_LIMIT: usize = 100;

/// Upper bound on rows returned by `GET /debits/dead-lettered`, so a pathological
/// dead-letter backlog can't return an unbounded body. Generous headroom: a
/// dead-lettered debit means a real stuck charge, so even hundreds is already an
/// alarm condition; beyond this cap the response's `truncated`/`total` fields tell
/// the operator more exist. The dead-letter depth is also on `/stats`.
const MAX_DEAD_LETTERED_LIST: usize = 1000;

/// `GET /jobs` — most-recent jobs (capped at `RECENT_JOBS_LIMIT`) as a JSON
/// array of `{ id, kind, status }`, newest first. An optional `?status=` query
/// param filters to one lifecycle status (`queued`/`in_flight`/`done`/`failed`);
/// an unrecognized value is a 400. Empty array if none match; 500 on a store
/// error. Read-only; takes a single store lock.
async fn list_jobs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListJobsQuery>,
) -> Result<Json<Vec<JobSummary>>, StatusCode> {
    // Validate the optional filter against the known statuses before querying,
    // so a bad value is a 400 rather than a silently-empty 200.
    if let Some(s) = &q.status {
        if !store::JOB_STATUSES.contains(&s.as_str()) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    let store = state.store.lock().await;
    match store.list_jobs(RECENT_JOBS_LIMIT, q.status.as_deref()) {
        Ok(rows) => Ok(Json(
            rows.into_iter()
                .map(|(id, kind, status)| JobSummary { id, kind, status })
                .collect(),
        )),
        Err(e) => {
            tracing::error!(?e, "list_jobs: store query failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn submit(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(mut result): Json<JobResult>,
) -> Result<&'static str, StatusCode> {
    if result.job_id != id {
        return Err(StatusCode::BAD_REQUEST);
    }
    // The poller must echo the dispatch_seq it got from /jobs/next (the fence).
    // A submit without it can't be tied to a dispatch, so reject it outright.
    let claimed_seq: i64 = match headers
        .get(DISPATCH_SEQ_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
    {
        Some(s) => s,
        None => {
            tracing::warn!(
                ?id,
                "rejected: missing/invalid {DISPATCH_SEQ_HEADER} header"
            );
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    // Verify the earner's recoverable secp256k1 attestation: the signer must be
    // the claimed earner_address. Rejected submissions never enter `completed`.
    if let Err(e) = verify::verify_signature(
        &result.job_id,
        &result.output_hash,
        &result.earner_address,
        &result.signature_hex,
    ) {
        tracing::warn!(?id, earner = %result.earner_address, ?e, "rejected: bad attestation");
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Fold the signer address to canonical lowercase post-verification so the liveness
    // map lookup, the attestation earner, and per-earner aggregates all key on one
    // identity regardless of the case this submit used.
    result.earner_address = verify::canonical_earner_address(&result.earner_address);
    // Content gate: the signature proves who produced the result; this proves the
    // result is well-formed enough to meter and attest. A malformed output hash,
    // unfetchable url, or zero render-seconds is unprocessable — reject it before
    // any state mutation. The job is left in_flight for the reaper, mirroring the
    // bad-attestation path above (which also leaves it for the reaper).
    if let Err(e) = validate::validate_result(&result) {
        tracing::warn!(?id, earner = %result.earner_address, reason = e.reason(), "rejected: result failed content gate");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    // Authenticated, live earner: refresh its liveness for /stats. Done before
    // locking the store so we never hold the earners lock under the store lock.
    {
        let now = now_secs();
        if let Some(info) = state.earners.lock().await.get_mut(&result.earner_address) {
            info.last_seen = now;
        }
    }
    // Gate on lifecycle: a result is only valid for a job the earner actually
    // took (which marked it in_flight). Checked after signature verification so
    // we don't reveal job state to unauthenticated callers.
    let mut store = state.store.lock().await;
    match store.job_status(&id) {
        Ok(Some(status)) if status == "in_flight" => {}
        Ok(Some(_)) => {
            tracing::warn!(?id, earner = %result.earner_address, "rejected: job not in_flight");
            return Err(StatusCode::CONFLICT);
        }
        Ok(None) => {
            tracing::warn!(?id, earner = %result.earner_address, "rejected: unknown job");
            return Err(StatusCode::NOT_FOUND);
        }
        Err(e) => {
            tracing::error!(?id, ?e, "submit: job_status lookup failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    // Fence on the dispatch seq, under the same lock as the in_flight check and
    // the settle: a job reaped and reassigned after this earner polled carries a
    // higher seq than the one it echoed, so its stale submit is refused — no
    // credit for a lease it no longer holds.
    match store.current_dispatch_seq(&id) {
        Ok(Some(seq)) if seq == claimed_seq => {}
        Ok(_) => {
            tracing::warn!(?id, earner = %result.earner_address, claimed_seq, "rejected: dispatch reassigned (stale lease)");
            return Err(StatusCode::CONFLICT);
        }
        Err(e) => {
            tracing::error!(?id, ?e, "submit: dispatch_seq lookup failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    // Bound render_seconds against the job's own deadline. The spec is read here,
    // under the same store lock as the lifecycle/fence checks above, so there is
    // no extra pre-lock read and no TOCTOU. An implausible value is a content
    // fault → 422, job left in_flight for the reaper (mirroring the pre-lock
    // content gate, which can't see the deadline before locking).
    match store.get_job(&id) {
        Ok(Some(spec)) => {
            if let Err(e) =
                validate::validate_render_seconds(result.render_seconds, spec.deadline_secs)
            {
                tracing::warn!(?id, earner = %result.earner_address, reason = e.reason(), "rejected: result failed content gate");
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
        }
        // job_status already confirmed an in_flight job under this same lock, so a
        // missing spec here is an internal inconsistency, not a client error.
        Ok(None) => {
            tracing::error!(?id, "submit: spec missing for an in_flight job");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        Err(e) => {
            tracing::error!(?id, ?e, "submit: get_job lookup failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    tracing::info!(?id, earner = %result.earner_address, "result received");
    // Content gate already cleared above. record_completed durably enqueues the
    // pending EAS receipt in the settle tx; the background relayer drains it to
    // RenderReceipts on-chain and stamps the real attestation uid.
    match store.record_completed(&result) {
        Ok(true) => Ok("accepted"),
        // The in_flight gate above ran under this same lock, so a false here
        // means the job changed underneath us — a conflict, not a success.
        Ok(false) => {
            tracing::warn!(?id, "submit: job no longer in_flight at settle");
            Err(StatusCode::CONFLICT)
        }
        Err(e) => {
            tracing::error!(?id, ?e, "submit: failed to persist result");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Websocket job dispatch (the v1 upgrade). Protocol, all JSON text frames:
///
///   0. coordinator → `CoordinatorMsg::Challenge { nonce }` (the FIRST frame): a
///      fresh single-use random nonce the earner must fold into its signed
///      `hello_digest`. Connection-scoped (held only for the handshake, never
///      stored), so a Hello captured off the wire and replayed on a new
///      connection — which gets a different challenge — fails recovery.
///   1. earner → `EarnerMsg::Hello` (required first earner message; registers
///      like `/register`, but its signature must additionally cover the
///      challenge). Any other first message, or a Hello whose signature doesn't
///      recover to the claimed address over the issued challenge, closes the
///      socket.
///   2. coordinator polls the queue; when a job whose `kind` the earner
///      advertised in `supported` is available, it pops it (stamping a fresh
///      `dispatch_seq`, which the session remembers) and sends
///      `CoordinatorMsg::JobOffer(job)`. Only one job is offered at a time.
///   3. earner → `EarnerMsg::Accept { job_id }` marks the offer in-flight; an
///      `Accept` for a different/unknown job is ignored.
///   4. earner → `EarnerMsg::Decline { job_id, reason }` (instead of Accept):
///      the earner refuses the offer without rendering it (its capability
///      self-guard caught a kind it can't serve). For the currently offered job
///      we requeue it for a capable earner with the dispatch attempt refunded
///      (`RequeueKind::EarnerFault`) and add it to this session's skip set, so it
///      is neither re-offered nor re-declined in a loop; a decline for any other
///      job id is ignored.
///   5. earner → `EarnerMsg::Submit(result)`: the signature + job_id are
///      verified and the job is settled only while it still holds the
///      `dispatch_seq` we remembered (the fence). Valid + current dispatch →
///      push to `completed` and reply `CoordinatorMsg::Accepted { job_id,
///      attestation_uid }`. Bad signature / wrong job → `Rejected` and the job
///      is requeued for another earner. Stale offer (the job was reaped and
///      reassigned out from under us, so its seq has advanced) → `Rejected`, and
///      the job is left untouched (not requeued, not settled). See
///      [`SubmitOutcome`].
///   6. earner → `EarnerMsg::Heartbeat { job_id, progress_pct }`: when
///      `job_id` matches the currently offered job, `store.touch` bumps
///      `started_at` so the reaper deadline slides from the last heartbeat
///      rather than from dispatch (liveness-aware reaping). A non-matching or
///      absent job_id is logged and ignored; the session is never broken on a
///      heartbeat.
///
/// The earner registration and queue/completed state are shared with the HTTP
/// endpoints, so `/stats` reflects ws activity identically.
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Extension(PeerAddr(peer)): Extension<PeerAddr>,
    headers: HeaderMap,
) -> Response {
    // Resolve the rate-limit source from the upgrade request's headers BEFORE the
    // upgrade consumes it: the real client behind a trusted proxy (via
    // X-Forwarded-For), else the connection peer — the same rule HTTP /register uses.
    let source = resolve_source_ip(peer.ip(), &headers, &state.trusted_proxies);
    // Bound inbound frames/messages to the same ceiling the HTTP paths enforce, so a
    // Hello — like any earner frame — is size-checked at the protocol layer BEFORE
    // serde parses it. Without this the ws path inherits tungstenite's 64 MiB default,
    // and the per-source registration rate check (which runs only once a Hello frame
    // is decoded) would sit AFTER an unbounded parse — letting the ws Hello amplify the
    // very flood the limiter exists to shed, where HTTP `/register` is already capped
    // by `DefaultBodyLimit`. The largest legitimate inbound message is an
    // `EarnerMsg::Submit`, already bounded to this ceiling on the HTTP `/jobs/{id}/submit`
    // path, so this is transport parity, not a new limit. Inbound-only: outbound
    // `JobOffer`s (a JobSpec, `inputs` bounded by `MAX_INPUTS_BYTES`) are unaffected.
    let ws = ws
        .max_message_size(MAX_REQUEST_BODY_BYTES)
        .max_frame_size(MAX_REQUEST_BODY_BYTES);
    ws.on_upgrade(move |socket| ws_session(socket, state, source))
}

/// Send a `CoordinatorMsg` as a JSON text frame. Returns `false` if the socket
/// is closed / the send failed (caller should end the session).
async fn send_msg(socket: &mut WebSocket, msg: &CoordinatorMsg) -> bool {
    match serde_json::to_string(msg) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(e) => {
            tracing::error!(?e, "failed to serialize coordinator message");
            false
        }
    }
}

async fn ws_session(mut socket: WebSocket, state: Arc<AppState>, source: IpAddr) {
    // 1. First message MUST be a Hello.
    let earner_address = match recv_hello(&mut socket, &state, source).await {
        Some(addr) => addr,
        None => return,
    };

    // The set of job kinds this earner advertised support for.
    let supported: Vec<JobKind> = state
        .earners
        .lock()
        .await
        .get(&earner_address)
        .map(|info| info.supported.clone())
        .unwrap_or_default();

    // The job we've currently offered, paired with the `dispatch_seq` stamped
    // when we took it. The seq is our fence: we only settle/requeue/touch this
    // job while it still holds this seq, so a reaped+reassigned job can't be
    // settled or preempted by us. Plus whether the earner accepted the offer.
    let mut offered: Option<(JobSpec, i64)> = None;
    let mut accepted = false;
    // Jobs this earner has faulted on in this session. We won't re-offer them to
    // the same earner (anti hot-loop, FM4); they stay queued for other earners.
    let mut faulted: HashSet<Uuid> = HashSet::new();
    // Poll the queue on this cadence when we have nothing offered.
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut last_liveness_bump = now_secs();

    // Read-idle keepalive. `handshake_timeout` bounds only the pre-Hello handshake;
    // once a session is established its `socket.recv()` has no deadline, so a peer
    // that completes Hello then goes silent (a half-open/vanished earner) would hold
    // this task + FD until ~2h OS TCP keepalive or a max-connections eviction — and
    // the `tick` arm below SELF-bumps the registry `last_seen` every second while
    // idle, so the `earner_ttl` prune can't even surface it as stale. We probe with a
    // ws Ping at half the idle bound: a live earner — even idle between jobs, which
    // sends no application frames — auto-pongs, refreshing `last_inbound` on the recv
    // arm, so only a peer that stops responding trips the deadline. An in-flight job's
    // heartbeats reset it the same way (never reap a live render).
    let idle_timeout = state.session_idle_timeout;
    let mut idle_probe = tokio::time::interval((idle_timeout / 2).max(Duration::from_millis(1)));
    idle_probe.tick().await; // consume the immediate first tick so the first probe is one interval in
    let mut last_inbound = tokio::time::Instant::now();

    loop {
        // If we have no outstanding offer, try to grab a supported job.
        if offered.is_none() {
            if let Some((job, seq)) =
                take_supported_job(&state, &earner_address, &supported, &faulted).await
            {
                if !send_msg(&mut socket, &CoordinatorMsg::JobOffer(job.clone())).await {
                    // Socket died delivering the offer — a disconnect, so charge.
                    requeue(&state, job, seq, RequeueKind::Charge, None).await;
                    return;
                }
                tracing::info!(earner = %earner_address, job_id = %job.id, "job offered");
                offered = Some((job, seq));
                accepted = false;
            }
        }

        tokio::select! {
            // Poll for new jobs while idle.
            _ = tick.tick(), if offered.is_none() => {
                let now = now_secs();
                if now - last_liveness_bump >= 1 {
                    if let Some(info) = state.earners.lock().await.get_mut(&earner_address) {
                        info.last_seen = now;
                    }
                    last_liveness_bump = now;
                }
                continue;
            }
            // Read-idle deadline + keepalive probe. Driven by last-inbound-frame time
            // (NOT the self-bumped `last_seen`, and NOT last-job-offer), so it fires
            // for a silent/vanished session even while the job-poll tick keeps running.
            _ = idle_probe.tick() => {
                if last_inbound.elapsed() >= idle_timeout {
                    tracing::info!(
                        earner = %earner_address,
                        idle_secs = idle_timeout.as_secs(),
                        "ws session idle past the read deadline (no inbound frame, incl. keepalive pong); closing",
                    );
                    break;
                }
                // A live earner — even idle between jobs — auto-pongs this, resetting
                // `last_inbound` on the recv arm; a vanished peer never pongs and trips
                // the deadline on a later tick. Best-effort: a failed send means the
                // socket is already gone, so the next `recv()` ends the session.
                let _ = socket.send(Message::Ping(Vec::new().into())).await;
            }
            incoming = socket.recv() => {
                let Some(frame) = incoming else { break }; // socket closed
                let frame = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(earner = %earner_address, ?e, "ws recv error");
                        break;
                    }
                };
                // Any received frame — Text/Binary/Ping/Pong (incl. the pong to our
                // keepalive ping) — is a sign the peer is still responding; reset the
                // read-idle deadline before we filter by frame type.
                last_inbound = tokio::time::Instant::now();
                let text = match frame {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    // Ping/Pong/Binary: ignore (axum auto-replies to pings).
                    _ => continue,
                };
                let msg: EarnerMsg = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(earner = %earner_address, ?e, "undecodable earner message");
                        continue;
                    }
                };
                // Any decoded frame is a sign of life — refresh liveness for /stats.
                {
                    let now = now_secs();
                    if let Some(info) = state.earners.lock().await.get_mut(&earner_address) {
                        info.last_seen = now;
                    }
                    last_liveness_bump = now;
                }
                match msg {
                    EarnerMsg::Hello { .. } => {
                        // Re-hello mid-session is unexpected; log and ignore.
                        tracing::warn!(earner = %earner_address, "unexpected Hello mid-session");
                    }
                    EarnerMsg::Accept { job_id } => {
                        match &offered {
                            Some((job, _)) if job.id == job_id => {
                                accepted = true;
                                tracing::info!(earner = %earner_address, %job_id, "offer accepted");
                            }
                            _ => tracing::warn!(earner = %earner_address, %job_id, "accept for unknown/stale job"),
                        }
                    }
                    EarnerMsg::Decline { job_id, reason } => {
                        // The earner refuses this offer without rendering it (its
                        // capability self-guard caught a kind it can't serve).
                        // Mirror the earner-fault disposition: the job is still
                        // renderable, so requeue it for a capable earner with the
                        // dispatch attempt refunded (RequeueKind::EarnerFault), and
                        // add it to this session's skip set so we don't re-offer —
                        // and it can't re-decline — the same job in a hot loop.
                        // Only act on the job we currently have offered (fenced by
                        // the remembered dispatch seq inside `requeue`); a decline
                        // for any other id is stale/unknown and ignored. A decline
                        // after the earner already Accepted is unusual but handled
                        // identically (fault-requeue + `accepted` reset) — still
                        // fenced and capped at one fault per session.
                        match &offered {
                            Some((job, _)) if job.id == job_id => {
                                tracing::info!(earner = %earner_address, %job_id, %reason, "offer declined; requeueing for a capable earner");
                                if let Some((job, seq)) = offered.take() {
                                    faulted.insert(job.id);
                                    requeue(&state, job, seq, RequeueKind::EarnerFault, None).await;
                                }
                                accepted = false;
                            }
                            _ => tracing::warn!(earner = %earner_address, %job_id, "decline for unknown/stale job"),
                        }
                    }
                    EarnerMsg::Submit(result) => {
                        let outcome = handle_submit(&state, &offered, accepted, result).await;
                        let sent = send_msg(&mut socket, outcome.reply()).await;
                        match outcome {
                            // Settled or stale: clear the offer, requeue nothing.
                            SubmitOutcome::Accepted(_) | SubmitOutcome::Drop(_) => {
                                offered = None;
                                accepted = false;
                            }
                            // Rejected but still ours: requeue for another earner.
                            SubmitOutcome::Requeue(_, kind) => {
                                if let Some((job, seq)) = offered.take() {
                                    // Don't re-offer a job this earner just faulted
                                    // on (anti hot-loop): another earner can still
                                    // take it. This intentionally caps THIS session's
                                    // contribution to the job's persistent fault
                                    // count at one, so a single connected earner
                                    // can't unilaterally drive a renderable job to
                                    // max_faults — dead-lettering needs faults from
                                    // multiple sessions/earners (see requeue_earner_fault).
                                    if kind == RequeueKind::EarnerFault {
                                        faulted.insert(job.id);
                                    }
                                    // Attribute a genuine quality fault to this
                                    // session's registered address — the dispatch
                                    // holder, so a faulting earner only ever smears
                                    // its OWN reputation, never a claimed victim's
                                    // result.earner_address. A Charge (transient)
                                    // is ignored by `requeue`.
                                    requeue(&state, job, seq, kind, Some(&earner_address)).await;
                                }
                                accepted = false;
                            }
                        }
                        // Verdict undeliverable (socket gone): disposition applied
                        // above, so just end the session.
                        if !sent {
                            return;
                        }
                    }
                    EarnerMsg::Heartbeat { job_id, progress_pct } => {
                        // When the heartbeat carries a job_id that matches the
                        // currently offered job, bump `started_at` (so the reaper's
                        // deadline window resets from "last sign of life" rather than
                        // from dispatch) and record the reported progress. `store.touch`
                        // is fenced on `*seq`: only the holder of THIS dispatch writes —
                        // a heartbeat for a job since reaped+reassigned (newer seq) finds
                        // no matching row and is a no-op, so it can't keep the new
                        // holder's lease alive nor overwrite its progress. The clamp to
                        // 0..=100 lives in the store.
                        match (job_id, &offered) {
                            (Some(jid), Some((job, seq))) if jid == job.id => {
                                let store = state.store.lock().await;
                                match store.touch(&jid, *seq, now_secs(), progress_pct) {
                                    Ok(true) => tracing::debug!(
                                        earner = %earner_address,
                                        %jid,
                                        progress_pct,
                                        "heartbeat: liveness + progress recorded",
                                    ),
                                    Ok(false) => tracing::debug!(
                                        earner = %earner_address,
                                        %jid,
                                        progress_pct,
                                        "heartbeat for a reassigned/stale/non-in-flight dispatch ignored",
                                    ),
                                    Err(e) => tracing::error!(
                                        earner = %earner_address,
                                        %jid,
                                        ?e,
                                        "heartbeat: touch failed",
                                    ),
                                }
                            }
                            _ => tracing::debug!(
                                earner = %earner_address,
                                ?job_id,
                                progress_pct,
                                "heartbeat with no matching offer; ignoring",
                            ),
                        }
                    }
                }
            }
        }
    }

    // Socket ended with an un-submitted offer in flight → requeue it (fenced on
    // our dispatch seq, so a job already reaped+reassigned isn't yanked back). A
    // dropped socket is a disconnect, not an earner fault, so it charges.
    if let Some((job, seq)) = offered.take() {
        requeue(&state, job, seq, RequeueKind::Charge, None).await;
    }
    tracing::info!(earner = %earner_address, "ws session ended");
}

/// Number of random bytes in the per-connection registration challenge (128 bit).
const HELLO_NONCE_BYTES: usize = 16;

/// Issue a single-use challenge, then block on the first `EarnerMsg::Hello` and
/// require its signature to cover that challenge. Registers the earner (shared
/// with `/register`) and returns its address, or `None` if the socket closed /
/// sent something other than a valid Hello.
///
/// The challenge is a fresh random nonce sent as the FIRST frame and held only
/// for this handshake (never stored), so a Hello captured off the wire and
/// replayed on a new connection — which receives a different challenge — fails
/// signature recovery, and there is no nonce store for an attacker to exhaust.
/// Run the registration handshake under a single wall-clock deadline. A client
/// that opens a connection, reads (or ignores) the challenge, and never sends a
/// valid Hello — or that drip-feeds pings to keep the pre-Hello loop alive —
/// would otherwise park a live `ws_session` task + socket forever (slowloris).
/// One `timeout` around the WHOLE inner exchange bounds the entire handshake
/// wall-clock, so frame-flooding can't extend it (the deadline is not reset per
/// frame). On elapse we fail closed: return `None` (no registry insert, no
/// eviction), the caller returns, and axum closes the socket.
async fn recv_hello(socket: &mut WebSocket, state: &Arc<AppState>, source: IpAddr) -> Option<String> {
    match tokio::time::timeout(state.handshake_timeout, recv_hello_inner(socket, state, source)).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!("ws: registration handshake timed out before a valid Hello; closing");
            None
        }
    }
}

async fn recv_hello_inner(
    socket: &mut WebSocket,
    state: &Arc<AppState>,
    source: IpAddr,
) -> Option<String> {
    let mut nonce = [0u8; HELLO_NONCE_BYTES];
    if getrandom::getrandom(&mut nonce).is_err() {
        tracing::error!("ws: failed to generate a registration challenge; closing");
        return None;
    }
    if !send_msg(socket, &CoordinatorMsg::Challenge { nonce: hex::encode(nonce) }).await {
        return None;
    }
    loop {
        let frame = socket.recv().await?.ok()?;
        let text = match frame {
            Message::Text(t) => t,
            Message::Close(_) => return None,
            _ => continue, // ignore pings/binary before Hello
        };
        let msg: EarnerMsg = serde_json::from_str(&text).ok()?;
        let EarnerMsg::Hello {
            earner_address,
            gpu_model,
            vram_gb,
            supported,
            signature_hex,
        } = msg
        else {
            tracing::warn!("ws: first message was not Hello; closing");
            return None;
        };
        // Per-source registration rate limit, once a Hello frame is in hand and
        // BEFORE its secp256k1 verify in validate_hello (cheap-reject-first, FM2) —
        // symmetric with HTTP /register. Over-limit → close the socket (return None),
        // no registry work. Keyed on the resolved source (peer, or the trusted-proxy
        // X-Forwarded-For client) computed in ws_handler.
        {
            let now = now_secs();
            let mut buckets = state.registration_buckets.lock().await;
            if !check_registration_rate(
                &mut buckets,
                source,
                now,
                state.max_registrations,
                REGISTRATION_WINDOW_SECS,
                MAX_REGISTRATION_BUCKETS,
            ) {
                tracing::warn!(
                    %source,
                    "ws: rejected registration: per-source rate limit exceeded; closing"
                );
                return None;
            }
        }
        if let Err(reason) = validate_hello(
            &earner_address,
            &gpu_model,
            vram_gb,
            &supported,
            &nonce,
            &signature_hex,
        ) {
            tracing::warn!(address = %earner_address, reason, "ws: rejected malformed Hello; closing");
            return None;
        }
        // Canonical-lowercase the session identity post-verification: the returned
        // address keys the registry, every fault attribution, and job offers for this
        // session, so they all derive from one case-invariant form.
        let earner_address = verify::canonical_earner_address(&earner_address);
        let now = now_secs();
        let admitted = admit_earner(
            &mut *state.earners.lock().await,
            earner_address.clone(),
            EarnerInfo {
                gpu_model,
                vram_gb,
                supported,
                last_seen: now,
            },
            state.max_earners,
            now,
            state.earner_ttl_secs,
        );
        if !admitted {
            // Same admission policy as HTTP /register (FM4 parity): a registry full
            // of live earners can't make room, so close the socket instead of
            // admitting (no live earner is ever displaced).
            tracing::warn!(
                address = %earner_address,
                cap = state.max_earners,
                "ws: rejected registration: earner registry at capacity (all earners live); closing"
            );
            return None;
        }
        tracing::info!(address = %earner_address, vram_gb, "earner registered (ws)");
        return Some(earner_address);
    }
}

/// Take the oldest-waiting queued job whose kind the earner supports and that it
/// has not already faulted on this session, marking it in-flight. Returns the job
/// with the `dispatch_seq` stamped on this hand-out, which the session remembers
/// to fence later settle/requeue/heartbeat actions. Leaves unsupported jobs — and
/// jobs in `skip` — queued for other earners. `skip` is what stops a faulting
/// earner from being re-offered the same job in a reject/re-offer hot loop.
async fn take_supported_job(
    state: &Arc<AppState>,
    earner_address: &str,
    supported: &[JobKind],
    skip: &HashSet<Uuid>,
) -> Option<(JobSpec, i64)> {
    let store = state.store.lock().await;
    // Record this earner as the job's holder (`take_next_for`) so the liveness reaper
    // can reclaim it promptly if this earner goes stale, instead of waiting for the
    // full deadline. The anonymous HTTP poll uses `take_next` (no holder).
    match store.take_next_for(earner_address, |job| {
        supported.contains(&job.kind) && !skip.contains(&job.id)
    }) {
        Ok(job) => job,
        Err(e) => {
            tracing::error!(?e, "take_supported_job: take_next failed");
            None
        }
    }
}

/// Why a rejected/returned job is going back on the queue, which decides whether
/// this dispatch counts against the job's renderability budget (`max_attempts`)
/// or the separate earner-fault budget (`max_faults`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequeueKind {
    /// The earner failed to deliver (missed deadline, dropped socket) or the
    /// requeue is a coordinator-internal transient. The dispatch was a genuine
    /// rendering attempt, so it charges `max_attempts` (the existing behavior).
    Charge,
    /// The earner returned a faulty result (bad signature, malformed/implausible
    /// content) or violated the submit protocol. The job is still renderable, so
    /// the dispatch attempt is refunded and a `max_faults` fault is charged
    /// instead — the faulting earner is penalized, never the job.
    EarnerFault,
}

/// Put a job back on the queue, or dead-letter it once it has exhausted the
/// relevant budget — but only while we still hold the dispatch identified by
/// `seq`. `kind` selects the budget: a `Charge` requeue (deadline/disconnect)
/// consumes `state.max_attempts`; an `EarnerFault` requeue refunds the dispatch
/// attempt and consumes `state.max_faults` instead (see
/// [`Store::requeue_earner_fault`]).
///
/// The fence read and the requeue run under the same store lock, so they are
/// atomic against any other dispatch. If the job was reaped and reassigned to a
/// new earner since we took it, its `dispatch_seq` has advanced past `seq` and we
/// skip: requeueing would preempt the new holder mid-render. The store method
/// then applies its own `in_flight` guard (a reaper-parked `queued` job is a
/// no-op). The fence lives here, in one place, for both kinds.
///
/// `attribute_to` names the earner to charge a `/earners` reputation fault when
/// `kind` is `EarnerFault` and the fault is a GENUINE quality fault (the ws
/// Submit-fault path passes `Some(session_address)`). An honest `Decline` routes
/// through `EarnerFault` too but passes `None` — it refunds and requeues like a
/// fault yet is never a reputation fault. `Charge` ignores it. The attribution is
/// applied INSIDE `requeue_earner_fault`, behind that method's own `in_flight`
/// guard — NOT here behind the seq-fence — because a reaper can park the job back
/// to `queued` at this same `seq` (reapers don't bump `dispatch_seq`), which the
/// fence can't see; gating attribution on the in_flight bump keeps the per-earner
/// tally in lockstep with the per-job `faults` budget.
async fn requeue(
    state: &Arc<AppState>,
    job: JobSpec,
    seq: i64,
    kind: RequeueKind,
    attribute_to: Option<&str>,
) {
    let store = state.store.lock().await;
    match store.current_dispatch_seq(&job.id) {
        Ok(Some(cur)) if cur == seq => {}
        Ok(_) => {
            tracing::debug!(job_id = %job.id, seq, "requeue skipped: dispatch reassigned or job gone");
            return;
        }
        Err(e) => {
            tracing::error!(job_id = %job.id, ?e, "requeue: dispatch_seq read failed");
            return;
        }
    }
    let (dead_lettered, why) = match kind {
        RequeueKind::Charge => (
            store.requeue(&job, state.max_attempts),
            "max attempts exhausted",
        ),
        RequeueKind::EarnerFault => (
            // `attribute_to` is recorded INSIDE requeue_earner_fault, gated by the
            // same in_flight check as the fault bump (a reaper can park the job to
            // `queued` at this same seq, which the fence above can't see) — so a
            // genuine quality fault attributes iff it actually charges. An honest
            // Decline passes None and is never attributed.
            store.requeue_earner_fault(&job, state.max_faults, attribute_to),
            "max earner faults exhausted",
        ),
    };
    match dead_lettered {
        Ok(true) => tracing::warn!(job_id = %job.id, ?kind, "job dead-lettered ({why})"),
        Ok(false) => {}
        Err(e) => tracing::error!(job_id = %job.id, ?kind, ?e, "requeue failed"),
    }
}

/// Disposition of a ws `Submit`: the verdict to send the earner, plus what
/// `ws_session` should do with the outstanding offer.
#[derive(Debug)]
enum SubmitOutcome {
    /// Verified + settled. Clear the offer; do not requeue.
    Accepted(CoordinatorMsg),
    /// Rejected, and the job is still this earner's in-flight work — put it back
    /// on the queue so another earner can try. The `RequeueKind` says whether the
    /// reject was an earner fault (refund the attempt, charge a fault) or a
    /// charging requeue (coordinator-internal transient).
    Requeue(CoordinatorMsg, RequeueKind),
    /// Rejected, and the offer is stale: the job is no longer in_flight under us
    /// (reaped, reassigned, or already terminal). Drop our in-memory offer but
    /// leave the job's store state untouched — requeueing would clobber a job
    /// another earner may now hold or resurrect a dead-lettered one.
    Drop(CoordinatorMsg),
}

impl SubmitOutcome {
    fn reply(&self) -> &CoordinatorMsg {
        match self {
            SubmitOutcome::Accepted(m) | SubmitOutcome::Requeue(m, _) | SubmitOutcome::Drop(m) => m,
        }
    }
}

/// Verify a ws `Submit` against the outstanding offer and settle it. Returns the
/// verdict to relay plus the offer disposition (see [`SubmitOutcome`]). A result
/// is settled only while its job is still in_flight under this connection, so a
/// stale offer cannot double-credit or resurrect a terminal job.
async fn handle_submit(
    state: &Arc<AppState>,
    offered: &Option<(JobSpec, i64)>,
    accepted: bool,
    mut result: JobResult,
) -> SubmitOutcome {
    let job_id = result.job_id;
    let rejected = |reason: &str| CoordinatorMsg::Rejected {
        job_id,
        reason: reason.to_string(),
    };

    let (deadline_secs, expected_seq) = match offered {
        Some((job, seq)) if job.id == job_id => (job.deadline_secs, *seq),
        // The offered job is still validly ours; don't settle it on a mismatched
        // submit, but free it for another earner rather than stranding it. A
        // submit for the wrong job_id is an earner protocol fault — the offered
        // job is untouched and renderable, so don't charge its attempt budget.
        Some(_) => {
            return SubmitOutcome::Requeue(
                rejected("submit job_id does not match the offered job"),
                RequeueKind::EarnerFault,
            )
        }
        None => return SubmitOutcome::Drop(rejected("no job was offered on this connection")),
    };
    if !accepted {
        // Submit before Accept is another earner protocol fault on a renderable job.
        return SubmitOutcome::Requeue(rejected("submit before accept"), RequeueKind::EarnerFault);
    }

    if let Err(e) = verify::verify_signature(
        &result.job_id,
        &result.output_hash,
        &result.earner_address,
        &result.signature_hex,
    ) {
        tracing::warn!(%job_id, earner = %result.earner_address, ?e, "ws rejected: bad attestation");
        return SubmitOutcome::Requeue(
            rejected("attestation signature verification failed"),
            RequeueKind::EarnerFault,
        );
    }
    // Canonical-lowercase the verified signer so the attestation (record_completed)
    // keys on the same identity the registry and fault ledger do — case-invariant.
    result.earner_address = verify::canonical_earner_address(&result.earner_address);

    // Content gate: reject a result that is not well-formed enough to meter or
    // attest before touching the store. Requeue (not Drop) so the job — which may
    // be perfectly renderable — gets another earner; the requeue helper is itself
    // dispatch_seq-fenced, so a content-reject for a since-reassigned lease can't
    // clobber the new holder. Mirrors the bad-attestation Requeue above.
    if let Err(e) = validate::validate_result(&result) {
        tracing::warn!(%job_id, earner = %result.earner_address, reason = e.reason(), "ws rejected: result failed content gate");
        return SubmitOutcome::Requeue(rejected(e.reason()), RequeueKind::EarnerFault);
    }
    // Bound render_seconds against this dispatch's deadline (the reaper's lease
    // budget, already in hand via `offered`). A result claiming far more compute
    // than the job allowed is fabricated — refuse it like any other content
    // fault. Requeue: the job may be renderable by an honest earner.
    if let Err(e) = validate::validate_render_seconds(result.render_seconds, deadline_secs) {
        tracing::warn!(%job_id, earner = %result.earner_address, reason = e.reason(), "ws rejected: result failed content gate");
        return SubmitOutcome::Requeue(rejected(e.reason()), RequeueKind::EarnerFault);
    }

    // Settle only if we still hold the current dispatch. The fence read and the
    // settle run under one store lock, so they are atomic against any other
    // dispatch: a job reaped and reassigned to a new earner carries a higher
    // `dispatch_seq` than ours, so we Drop (never Requeue) — we must not settle
    // (which would credit us for the new holder's lease) or touch the job.
    // `record_completed` keeps its own `in_flight` guard as a backstop.
    let settled = {
        let mut store = state.store.lock().await;
        match store.current_dispatch_seq(&job_id) {
            Ok(Some(cur)) if cur == expected_seq => {}
            Ok(_) => {
                tracing::warn!(%job_id, earner = %result.earner_address, expected_seq, "ws rejected: dispatch reassigned (stale lease)");
                return SubmitOutcome::Drop(rejected("job is no longer in flight"));
            }
            Err(e) => {
                // Coordinator-internal transient (DB read), not an earner fault:
                // keep the conservative charging requeue (existing behavior).
                tracing::error!(%job_id, ?e, "ws: dispatch_seq read failed");
                return SubmitOutcome::Requeue(
                    rejected("failed to read dispatch state"),
                    RequeueKind::Charge,
                );
            }
        }
        match store.record_completed(&result) {
            Ok(settled) => settled,
            Err(e) => {
                // Coordinator-internal transient (DB write), not an earner fault:
                // keep the conservative charging requeue (existing behavior).
                tracing::error!(%job_id, ?e, "ws: failed to persist result");
                return SubmitOutcome::Requeue(
                    rejected("failed to persist result"),
                    RequeueKind::Charge,
                );
            }
        }
    };
    if !settled {
        tracing::warn!(%job_id, earner = %result.earner_address, "ws rejected: job no longer in_flight");
        return SubmitOutcome::Drop(rejected("job is no longer in flight"));
    }

    tracing::info!(%job_id, earner = %result.earner_address, "ws result accepted");
    // Content gate already cleared above. The settle (record_completed) durably
    // enqueues the pending EAS receipt; the on-chain attestation is async (the
    // background relayer assigns the real uid), so the accept reply still carries
    // the placeholder uid — the receipt's real uid lands later in the relay.
    SubmitOutcome::Accepted(CoordinatorMsg::Accepted {
        job_id,
        attestation_uid: PLACEHOLDER_ATTESTATION_UID.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    /// The common [`StoreConfig`] the test helpers build on: the test-chosen
    /// attempt/fault/TTL knobs plus the production defaults. Tests that exercise one
    /// knob override just that field via struct-update (`StoreConfig { knob: x,
    /// ..test_config() }`), so a call site names only what it varies — no positional
    /// list to transpose.
    fn test_config() -> StoreConfig {
        StoreConfig {
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        }
    }

    /// FM1: the named-field refactor's point is that no knob can be silently
    /// transposed. Construct a StoreConfig whose every field holds a DISTINCT value
    /// and assert each lands on the matching AppState field — a swap of any two
    /// same-typed fields (the `u32`/`usize` knobs) would put a detectably-wrong value
    /// on at least one field and red this test.
    #[test]
    fn with_store_maps_each_config_field_to_state() {
        let probe_ip = IpAddr::from([10, 1, 2, 3]);
        let state = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig {
                max_attempts: 3,
                max_faults: 7,
                earner_ttl_secs: 99,
                ttl_deadline_multiple: 4,
                retention_secs: 222,
                handshake_timeout: Duration::from_secs(11),
                session_idle_timeout: Duration::from_secs(13),
                ingest_token: Some("map-probe-token".to_string()),
                max_queued_jobs: 123,
                max_earners: 456,
                max_registrations: 789,
                trusted_proxies: TrustedProxies::parse(&[probe_ip.to_string()]).unwrap(),
            },
        )
        .unwrap();
        assert_eq!(state.max_attempts, 3);
        assert_eq!(state.max_faults, 7);
        assert_eq!(state.earner_ttl_secs, 99);
        assert_eq!(state.ttl_deadline_multiple, 4);
        assert_eq!(state.retention_secs, 222);
        assert_eq!(state.handshake_timeout, Duration::from_secs(11));
        assert_eq!(state.session_idle_timeout, Duration::from_secs(13));
        assert_eq!(state.ingest_token.as_deref(), Some("map-probe-token"));
        assert_eq!(state.max_queued_jobs, 123);
        assert_eq!(state.max_earners, 456);
        assert_eq!(state.max_registrations, 789);
        assert!(state.trusted_proxies.contains(&probe_ip));
    }

    /// In-memory store-backed state (no disk) with a custom handshake timeout, for
    /// the anti-slowloris tests that need a sub-second bound.
    fn test_state_handshake(handshake_timeout: Duration) -> Arc<AppState> {
        AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { handshake_timeout, ..test_config() },
        )
        .unwrap()
    }

    /// In-memory store-backed state (no disk). `with_store` seeds one job per
    /// `JobKind` because the in-memory DB starts empty; tests that need an empty
    /// queue drain it first via `/jobs/next` or use `test_state_empty`.
    fn test_state() -> Arc<AppState> {
        test_state_handshake(DEFAULT_HANDSHAKE_TIMEOUT)
    }

    /// Drain every auto-seeded job from `state` so its queue starts empty.
    async fn drain_seeded_jobs(state: &Arc<AppState>) {
        let store = state.store.lock().await;
        while store.take_next(|_| true).unwrap().is_some() {}
    }

    /// In-memory state with every auto-seeded job removed, so the queue starts
    /// empty (matches the old `AppState::default()` behavior used by tests
    /// that assert `jobs_queued == 0`).
    async fn test_state_empty() -> Arc<AppState> {
        let state = test_state();
        drain_seeded_jobs(&state).await;
        state
    }

    /// Empty-queue state with a custom handshake timeout, for the ws handshake
    /// timeout tests (which assert against `/stats` with no seeded-job noise and
    /// need a sub-second bound so a fail-closed test can't slow the suite).
    async fn test_state_empty_handshake(handshake_timeout: Duration) -> Arc<AppState> {
        let state = test_state_handshake(handshake_timeout);
        drain_seeded_jobs(&state).await;
        state
    }

    /// Empty-queue state with a custom session read-idle timeout (handshake left at
    /// the generous default so the Hello completes), for the ws idle-deadline tests
    /// that need a sub-second bound and no seeded-job noise.
    async fn test_state_empty_idle(session_idle_timeout: Duration) -> Arc<AppState> {
        let state = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { session_idle_timeout, ..test_config() },
        )
        .unwrap();
        drain_seeded_jobs(&state).await;
        state
    }

    /// Enqueue a job directly via the store (test helper replacing the old
    /// `state.queue.lock().await.push(job)`).
    async fn enqueue(state: &Arc<AppState>, job: &JobSpec) {
        state.store.lock().await.enqueue(job).unwrap();
    }

    /// Build a `Hello` that *claims* `claimed`, signed over its
    /// `hello_digest(claimed, …, nonce)` by `sk`. Separating the claim from the
    /// signing key lets a negative test forge a signature (sign with the wrong
    /// key) or claim an address the key doesn't derive to; separating the nonce
    /// lets the ws handshake tests bind the issued challenge (or a stale one).
    fn signed_hello_with_nonce(
        sk: &SigningKey,
        claimed: &str,
        gpu_model: &str,
        vram_gb: u32,
        supported: Vec<JobKind>,
        nonce: &[u8],
    ) -> EarnerMsg {
        let signature_hex = verify::sign_digest_for_test(
            sk,
            &proto::hello_digest(claimed, gpu_model, vram_gb, &supported, nonce),
        );
        EarnerMsg::Hello {
            earner_address: claimed.into(),
            gpu_model: gpu_model.into(),
            vram_gb,
            supported,
            signature_hex,
        }
    }

    /// The HTTP-path `Hello` builder: signs over the empty-nonce digest (the HTTP
    /// `/register` path is not challenge-gated — its replay is a benign upsert).
    fn signed_hello(
        sk: &SigningKey,
        claimed: &str,
        gpu_model: &str,
        vram_gb: u32,
        supported: Vec<JobKind>,
    ) -> EarnerMsg {
        signed_hello_with_nonce(sk, claimed, gpu_model, vram_gb, supported, b"")
    }

    /// A valid self-signed `Hello` from `label`'s deterministic key
    /// ([`test_signing_key`]), claiming its own derived address with a custom
    /// `gpu_model` — the honest registration the key-possession gate accepts.
    fn hello_gpu(label: &str, gpu_model: &str, vram_gb: u32, supported: Vec<JobKind>) -> EarnerMsg {
        let sk = test_signing_key(label);
        signed_hello(&sk, &address_from_signing_key(&sk), gpu_model, vram_gb, supported)
    }

    /// A valid self-signed `Hello` from `label`'s key with the default GPU model.
    fn hello(label: &str, vram_gb: u32, supported: Vec<JobKind>) -> EarnerMsg {
        hello_gpu(label, "RTX 4090", vram_gb, supported)
    }

    /// A `Hello` *claiming* an arbitrary (possibly malformed) address string,
    /// signed by a throwaway key. Used by the structural-reject tests where the
    /// gate must reject on the claimed address's shape before key possession is
    /// ever checked, so the signature is well-formed but never reached.
    fn hello_claiming(claimed: &str, vram_gb: u32, supported: Vec<JobKind>) -> EarnerMsg {
        signed_hello(&test_signing_key("throwaway"), claimed, "RTX 4090", vram_gb, supported)
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Loopback peer for oneshot tests — the per-connection `PeerAddr` that `serve`
    /// injects in production, supplied here so `/register`'s extractor and the
    /// per-source rate limiter behave as on a real connection.
    fn test_peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 0))
    }

    async fn post_json(
        state: Arc<AppState>,
        uri: &str,
        value: &serde_json::Value,
    ) -> axum::response::Response {
        router(state)
            .layer(Extension(PeerAddr(test_peer())))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(value).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// POST a result to a submit URI with the `x-dispatch-seq` fence header. Most
    /// tests take a job once (dispatch seq 1) before submitting, so they pass
    /// `seq = 1`; the fence test passes a stale/current seq to exercise rejection.
    async fn post_submit(
        state: Arc<AppState>,
        uri: &str,
        value: &serde_json::Value,
        seq: i64,
    ) -> axum::response::Response {
        router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .header("x-dispatch-seq", seq.to_string())
                    .body(Body::from(serde_json::to_vec(value).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn get(state: Arc<AppState>, uri: &str) -> axum::response::Response {
        router(state)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn register_then_stats_reflects_earner() {
        let state = test_state_empty().await;
        let msg = hello("a", 24, vec![JobKind::Terrain, JobKind::Foliage]);
        let resp = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(&msg).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = get(state.clone(), "/stats").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["gpus_joined"], 1);
        assert_eq!(json["total_vram_gb"], 24);
        assert_eq!(json["jobs_queued"], 0);
        assert_eq!(json["jobs_completed"], 0);
        assert_eq!(json["supported_breakdown"]["terrain"], 1);
        assert_eq!(json["supported_breakdown"]["foliage"], 1);
    }

    #[tokio::test]
    async fn register_upserts_and_sums_vram() {
        let state = test_state();
        // same label → same key → same address twice → upsert (count stays 1, vram updated)
        let m1 = hello("abc", 24, vec![JobKind::Terrain]);
        let m2 = hello("abc", 48, vec![JobKind::Terrain, JobKind::DiffusionTile]);
        let m3 = hello("def", 16, vec![JobKind::NpcTick]);
        for m in [&m1, &m2, &m3] {
            let resp = post_json(
                state.clone(),
                "/register",
                &serde_json::to_value(m).unwrap(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
        }

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 2); // abc upserted, def new
        assert_eq!(json["total_vram_gb"], 48 + 16);
        assert_eq!(json["supported_breakdown"]["terrain"], 1);
        assert_eq!(json["supported_breakdown"]["diffusion_tile"], 1);
        assert_eq!(json["supported_breakdown"]["npc_tick"], 1);
    }

    #[tokio::test]
    async fn register_rejects_non_hello() {
        let state = test_state();
        let msg = EarnerMsg::Accept {
            job_id: Uuid::new_v4(),
        };
        let resp = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(&msg).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Every malformed `Hello` is rejected at `/register` with 400 and inserts
    /// nothing, so `/stats` and `/earners` stay empty. Covers each guarded field:
    /// the address shape (empty / short / non-hex / missing `0x`), an empty
    /// supported set, and a zero `vram_gb`.
    #[tokio::test]
    async fn register_rejects_malformed_fields_and_leaves_stats_clean() {
        let state = test_state_empty().await;
        let non_hex = format!("0x{}", "g".repeat(40));
        let no_prefix = "a".repeat(40);
        // The address-shape rejects claim a malformed address (no key derives to
        // it) so they use `hello_claiming`; the empty-supported / zero-vram cases
        // claim a valid, self-signed address ("good") and reject on the other field.
        let malformed = [
            hello_claiming("", 24, vec![JobKind::Terrain]),         // empty address
            hello_claiming("0xabc", 24, vec![JobKind::Terrain]),    // too short
            hello_claiming(&non_hex, 24, vec![JobKind::Terrain]),   // 40 chars but not hex
            hello_claiming(&no_prefix, 24, vec![JobKind::Terrain]), // 40 hex but no 0x
            hello("good", 24, vec![]),                              // advertises no kinds
            hello("good", 0, vec![JobKind::Terrain]),               // zero vram
        ];
        for m in &malformed {
            let resp = post_json(
                state.clone(),
                "/register",
                &serde_json::to_value(m).unwrap(),
            )
            .await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "expected 400 for {m:?}"
            );
        }

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["gpus_joined"], 0,
            "no malformed earner may enter the registry"
        );
        assert_eq!(json["total_vram_gb"], 0);
        let earners = body_json(get(state.clone(), "/earners").await).await;
        assert_eq!(
            earners.as_array().unwrap().len(),
            0,
            "leaderboard stays empty"
        );
    }

    /// The depth bounds reject at `/register` with 400 and insert nothing: an
    /// oversized `gpu_model` (bloats the registry), a `supported` set with
    /// duplicate kinds (would double-count in `supported_breakdown`), and a
    /// `vram_gb` above the plausible ceiling (poisons `total_vram_gb`). A normal
    /// earner — within every bound — still registers, proving the bounds are
    /// hygiene, not exclusion (FM1).
    #[tokio::test]
    async fn register_rejects_oversized_gpu_dup_kinds_and_huge_vram() {
        let state = test_state_empty().await;
        let long_gpu = "x".repeat(MAX_GPU_MODEL_LEN + 1);
        // All three claim the valid, self-signed "depth" address and reject on a
        // bounded field, not on the address shape or signature.
        let malformed = [
            hello_gpu("depth", &long_gpu, 24, vec![JobKind::Terrain]), // gpu_model too long
            hello("depth", 24, vec![JobKind::Terrain, JobKind::Terrain]), // duplicate kind
            hello("depth", MAX_VRAM_GB + 1, vec![JobKind::Terrain]),   // vram over ceiling
        ];
        for m in &malformed {
            let resp = post_json(state.clone(), "/register", &serde_json::to_value(m).unwrap()).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "expected 400 for {m:?}");
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 0, "no out-of-bounds earner may register");
        assert_eq!(json["total_vram_gb"], 0);

        // A normal earner — within every bound — still registers.
        let ok = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello("depth", 24, vec![JobKind::Terrain, JobKind::Foliage])).unwrap(),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 1);
        // FM2: the two DISTINCT advertised kinds each count once in the breakdown.
        assert_eq!(json["supported_breakdown"]["terrain"], 1);
        assert_eq!(json["supported_breakdown"]["foliage"], 1);
    }

    /// Boundary values pass: a `gpu_model` of exactly `MAX_GPU_MODEL_LEN` and a
    /// `vram_gb` of exactly `MAX_VRAM_GB` are accepted (the bounds are inclusive),
    /// so a future big-VRAM card / long official name isn't locked out (FM1).
    #[tokio::test]
    async fn register_accepts_boundary_gpu_len_and_vram() {
        let state = test_state_empty().await;
        let max_gpu = "x".repeat(MAX_GPU_MODEL_LEN);
        let resp = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello_gpu("boundary", &max_gpu, MAX_VRAM_GB, vec![JobKind::Terrain])).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 1);
        assert_eq!(json["total_vram_gb"], MAX_VRAM_GB as u64);
    }

    /// A mixed-case (EIP-55-checksummed) address must register: both
    /// `is_evm_address` and the key-possession comparison are case-insensitive, so
    /// an earner that claims and signs over the checksummed form of an address it
    /// controls still registers (and its result would settle, compared the same
    /// way) (FM1). Constructed from a real key so the signature genuinely recovers.
    #[tokio::test]
    async fn register_accepts_checksummed_mixed_case_address() {
        let state = test_state_empty().await;
        let sk = test_signing_key("mixedcase");
        let lower = address_from_signing_key(&sk);
        let mixed = format!("0x{}", lower[2..].to_uppercase());
        assert_ne!(mixed, lower, "claim must differ in case from the recovered address");
        let resp = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(signed_hello(&sk, &mixed, "RTX 4090", 24, vec![JobKind::Terrain]))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            1
        );
    }

    /// A malformed re-`Hello` for an already-registered address is rejected
    /// without disturbing the existing live entry: the reject is a no-op on the
    /// registry (no insert, no delete, no overwrite), so a transiently-empty
    /// re-Hello can't drop or corrupt a previously-valid earner mid-session
    /// (FM4). The re-Hello carries a DIFFERENT vram (99) so the assertions catch
    /// an insert-before-validate overwrite, not just a deletion.
    #[tokio::test]
    async fn malformed_re_register_does_not_evict_existing_earner() {
        let state = test_state_empty().await;
        let ok = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello("keep", 24, vec![JobKind::Terrain])).unwrap(),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            1
        );

        let bad = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello("keep", 99, vec![])).unwrap(),
        )
        .await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        // The live entry is untouched: not evicted (count 1), not overwritten
        // (vram stays 24 not 99, supported still advertises terrain not empty).
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["gpus_joined"], 1,
            "rejected re-Hello must not evict the live earner"
        );
        assert_eq!(
            json["total_vram_gb"], 24,
            "original vram preserved, not overwritten with 99"
        );
        assert_eq!(
            json["supported_breakdown"]["terrain"], 1,
            "original supported set preserved"
        );
    }

    /// A Hello that claims a victim's address but is signed by a DIFFERENT key is
    /// rejected at `/register` with 400 and inserts nothing — key possession, not
    /// just address shape, gates the registry. Structurally the Hello is perfect
    /// (valid address, vram, supported), so only the signature check can reject it.
    #[tokio::test]
    async fn register_rejects_forged_hello_signature() {
        let state = test_state_empty().await;
        let victim = test_address("victim");
        // Attacker signs over the victim's claimed Hello with its own key.
        let forged = signed_hello(
            &test_signing_key("attacker"),
            &victim,
            "RTX 4090",
            24,
            vec![JobKind::Terrain],
        );
        let resp = post_json(state.clone(), "/register", &serde_json::to_value(&forged).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "forged signature must 400");
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 0, "a forged Hello must not enter the registry");
        assert_eq!(
            body_json(get(state.clone(), "/earners").await).await.as_array().unwrap().len(),
            0,
            "leaderboard stays empty"
        );
    }

    /// A forged re-`Hello` for an already-registered address — structurally valid
    /// but signed by the wrong key — is rejected without disturbing the live
    /// entry, so an attacker can't overwrite (or evict) a victim's registration by
    /// replaying a forged profile. The re-Hello carries a different vram (99) so an
    /// insert-before-verify overwrite would show; the signature reject path is the
    /// only thing that can stop it (the structural gates all pass).
    #[tokio::test]
    async fn forged_re_register_does_not_evict_existing_earner() {
        let state = test_state_empty().await;
        let victim = test_address("victim");
        let ok = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello("victim", 24, vec![JobKind::Terrain])).unwrap(),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);

        let forged = signed_hello(
            &test_signing_key("attacker"),
            &victim,
            "RTX 4090",
            99,
            vec![JobKind::Terrain],
        );
        let bad = post_json(state.clone(), "/register", &serde_json::to_value(&forged).unwrap()).await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 1, "forged re-Hello must not evict the live earner");
        assert_eq!(
            json["total_vram_gb"], 24,
            "original vram preserved, not overwritten with the forged 99"
        );
    }

    #[tokio::test]
    async fn next_job_drains_all_seeds_then_none() {
        // `test_state` auto-seeds one job per JobKind; that many polls each
        // return a job, then the queue is empty and the next poll is null.
        let state = test_state();

        for _ in 0..JobKind::ALL.len() {
            let json = body_json(get(state.clone(), "/jobs/next").await).await;
            assert!(!json.is_null(), "each seeded job should poll non-null");
        }
        let json = body_json(get(state.clone(), "/jobs/next").await).await;
        assert!(json.is_null(), "queue is empty after draining every seed");
    }

    /// A queued `JobSpec` of a chosen kind, for the capability-filter tests.
    fn job_of(kind: JobKind) -> JobSpec {
        JobSpec {
            id: Uuid::new_v4(),
            kind,
            region: RegionCoord {
                x: 1,
                y: 2,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn next_job_filters_to_supported_kind_and_stamps_seq() {
        // An earner advertising only Terrain must be handed the Terrain job, NOT
        // the older DiffusionTile that FIFO dispatch examines first. Discriminating:
        // the filter skips the non-matching oldest job and hands out the supported
        // (newer) one, still stamping the dispatch_seq fence header.
        let state = test_state_empty().await;
        enqueue(&state, &job_of(JobKind::DiffusionTile)).await; // older, unsupported
        let terrain = job_of(JobKind::Terrain);
        enqueue(&state, &terrain).await; // newer, supported

        let addr = test_address("cap");
        let reg = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello("cap", 24, vec![JobKind::Terrain])).unwrap(),
        )
        .await;
        assert_eq!(reg.status(), StatusCode::OK);

        let resp = get(state.clone(), &format!("/jobs/next?earner={addr}")).await;
        let seq = resp
            .headers()
            .get(DISPATCH_SEQ_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let json = body_json(resp).await;
        assert_eq!(
            json["id"].as_str().unwrap(),
            terrain.id.to_string(),
            "filtered poll returns the supported Terrain job, skipping the newer DiffusionTile"
        );
        assert!(
            seq.is_some(),
            "a supported hand-out stamps the dispatch_seq header"
        );
    }

    #[tokio::test]
    async fn next_job_skips_when_earner_supports_no_queued_kind() {
        // An earner whose kinds match NO queued job gets null + no seq header, and
        // the job is left queued (no attempt charged) — an unfiltered re-poll still
        // returns it, proving take_next never took it.
        let state = test_state_empty().await;
        let diffusion = job_of(JobKind::DiffusionTile);
        enqueue(&state, &diffusion).await;

        let addr = test_address("cap");
        post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello("cap", 24, vec![JobKind::Terrain])).unwrap(),
        )
        .await;

        let resp = get(state.clone(), &format!("/jobs/next?earner={addr}")).await;
        let seq = resp
            .headers()
            .get(DISPATCH_SEQ_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let json = body_json(resp).await;
        assert!(json.is_null(), "no supported job → null");
        assert!(seq.is_none(), "no hand-out → no dispatch_seq header");

        let json = body_json(get(state.clone(), "/jobs/next").await).await;
        assert_eq!(
            json["id"].as_str().unwrap(),
            diffusion.id.to_string(),
            "the skipped job stayed queued (no attempt burned)"
        );
    }

    #[tokio::test]
    async fn next_job_poll_refreshes_earner_liveness() {
        // An identified poll is a sign of life: it refreshes last_seen (mirroring
        // submit) so an actively-polling HTTP earner isn't pruned and its filter
        // keeps applying. Stale the earner to last_seen=0, poll, assert it advanced.
        let state = test_state_empty().await;
        enqueue(&state, &job_of(JobKind::Terrain)).await;
        let addr = test_address("cap");
        post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello("cap", 24, vec![JobKind::Terrain])).unwrap(),
        )
        .await;
        state.earners.lock().await.get_mut(&addr).unwrap().last_seen = 0;

        let _ = get(state.clone(), &format!("/jobs/next?earner={addr}")).await;

        let last_seen = state.earners.lock().await.get(&addr).unwrap().last_seen;
        assert!(
            last_seen > 0,
            "an identified poll refreshes the earner's last_seen"
        );
    }

    #[tokio::test]
    async fn next_job_unfiltered_without_earner_param() {
        // A legacy poll with no earner param keeps the unfiltered behavior: it
        // returns the oldest queued job regardless of kind (FIFO dispatch).
        let state = test_state_empty().await;
        let terrain = job_of(JobKind::Terrain);
        enqueue(&state, &terrain).await; // oldest
        enqueue(&state, &job_of(JobKind::DiffusionTile)).await; // newer

        let json = body_json(get(state.clone(), "/jobs/next").await).await;
        assert_eq!(
            json["id"].as_str().unwrap(),
            terrain.id.to_string(),
            "no earner param → unfiltered, returns the oldest queued job"
        );
    }

    #[tokio::test]
    async fn next_job_unknown_earner_param_falls_back_to_unfiltered() {
        // A present-but-unregistered earner param must NOT starve the poller: it
        // falls back to the unfiltered take (the earner is unknown/broken; the
        // pre-existing self-drop covers any unsupported hand-out), not null.
        let state = test_state_empty().await;
        let diffusion = job_of(JobKind::DiffusionTile);
        enqueue(&state, &diffusion).await;

        let ghost = test_address("ghost"); // never registered
        let json = body_json(get(state.clone(), &format!("/jobs/next?earner={ghost}")).await).await;
        assert_eq!(
            json["id"].as_str().unwrap(),
            diffusion.id.to_string(),
            "unknown earner → unfiltered fallback, not starved"
        );
    }

    /// A fresh coordinator seeds exactly one queued job per JobKind, so the
    /// HUD's queued-by-kind backlog is representative from boot.
    #[tokio::test]
    async fn fresh_state_seeds_representative_backlog() {
        let state = test_state();
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_queued"], JobKind::ALL.len() as u64);
        for kind in JobKind::ALL {
            assert_eq!(
                json["queued_by_kind"][kind.as_str()],
                1,
                "expected exactly one queued {} job",
                kind.as_str()
            );
        }
    }

    use k256::ecdsa::SigningKey;
    use sha3::{Digest, Keccak256};

    fn dev_signing_key() -> SigningKey {
        let bytes = hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318")
            .unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    /// Derive the lowercase Ethereum-style address from a signing key — the same
    /// derivation the earner and `verify.rs` use, so a test address matches what
    /// the coordinator recovers from a signature produced by the same key.
    fn address_from_signing_key(sk: &SigningKey) -> String {
        let point = sk.verifying_key().to_encoded_point(false);
        format!("0x{}", hex::encode(&Keccak256::digest(&point.as_bytes()[1..])[12..]))
    }

    fn dev_address() -> String {
        address_from_signing_key(&dev_signing_key())
    }

    /// Deterministic secp256k1 signing key from a readable label — distinct
    /// labels yield distinct keys. Lets tests keep legible names while every
    /// registration is backed by a real key able to produce the key-possession
    /// `Hello` signature the coordinator verifies.
    fn test_signing_key(label: &str) -> SigningKey {
        SigningKey::from_slice(&Keccak256::digest(label.as_bytes()))
            .expect("keccak digest is a valid secp256k1 scalar")
    }

    /// Expand a short, readable label into the `0x`+40-hex address of its
    /// [`test_signing_key`] — the shape the registration gate requires (and the
    /// settle-time gate accepts, case-insensitively). Distinct labels map to
    /// distinct, real-key-backed addresses, so tests keep using legible names
    /// (`"live"`, `"busy"`) while every registered address can sign for itself.
    fn test_address(label: &str) -> String {
        address_from_signing_key(&test_signing_key(label))
    }

    /// Expand a short, readable test label into a valid 256-bit lowercase-hex
    /// digest — the shape an honest earner emits and the result-validation gate
    /// requires. Distinct labels map to distinct hashes, so call sites keep
    /// passing `"deadbeef"`/`"a"`/`"bbbb"` for legibility while every submitted
    /// result is well-formed. Keccak (already a dep) keeps this dependency-free.
    fn test_output_hash(label: &str) -> String {
        hex::encode(Keccak256::digest(label.as_bytes()))
    }

    /// A `JobResult` validly signed by the dev key for the given job/label. The
    /// label is expanded to a valid 256-bit-hex `output_hash` (see
    /// [`test_output_hash`]) and the signature is taken over that hash.
    fn signed_result(job_id: Uuid, label: &str) -> JobResult {
        let output_hash = test_output_hash(label);
        let sk = dev_signing_key();
        let sig = verify::sign_for_test(&sk, &job_id, &output_hash);
        JobResult {
            job_id,
            earner_address: dev_address(),
            output_hash,
            output_url: "memory://x".into(),
            render_seconds: 1,
            signature_hex: sig,
        }
    }

    /// A `JobResult` validly signed by the dev key over a verbatim (possibly
    /// malformed) `output_hash`. Unlike [`signed_result`], the hash is NOT
    /// expanded — the signature is taken over exactly what is sent — so the
    /// signature gate passes and the content gate is the only thing that can
    /// reject. Isolates the content gate from the signature gate.
    fn signed_result_raw_hash(job_id: Uuid, output_hash: &str) -> JobResult {
        let sk = dev_signing_key();
        let sig = verify::sign_for_test(&sk, &job_id, output_hash);
        JobResult {
            job_id,
            earner_address: dev_address(),
            output_hash: output_hash.into(),
            output_url: "memory://x".into(),
            render_seconds: 1,
            signature_hex: sig,
        }
    }

    #[tokio::test]
    async fn submit_matching_id_accepted_mismatch_rejected() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;
        // The submit gate accepts results only for in_flight jobs; move it there
        // as a real earner would by polling /jobs/next.
        state.store.lock().await.take_next(|_| true).unwrap();

        let good = signed_result(job_id, "deadbeef");
        let uri = format!("/jobs/{}/submit", job_id);
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // mismatched path id vs body job_id → rejected
        let other = Uuid::new_v4();
        let uri = format!("/jobs/{}/submit", other);
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // completed count reflects the one accepted submit
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 1);
    }

    // -- earner-address canonical identity: a case-variant claim must not split
    //    reputation across the registry, fault ledger, and leaderboard --

    /// An uppercased-hex variant of `addr` (same address, EIP-55-style different
    /// case) — what an inconsistent/adversarial client could present at one boundary.
    fn upper_case_variant(addr: &str) -> String {
        format!("0x{}", addr[2..].to_ascii_uppercase())
    }

    /// FM2: a registration presenting a mixed-case address is keyed on the canonical
    /// lowercase identity (the recovered signer), not the as-sent case — so the same
    /// earner can't occupy two registry slots / leaderboard rows by varying case.
    #[tokio::test]
    async fn register_folds_a_mixed_case_address_to_one_canonical_identity() {
        let state = test_state_empty().await;
        let sk = test_signing_key("case-earner");
        let canonical = address_from_signing_key(&sk);
        let upper = upper_case_variant(&canonical);
        assert_ne!(upper, canonical, "the variant differs only by case");

        let hello = signed_hello(&sk, &upper, "RTX 4090", 24, vec![JobKind::Terrain]);
        let resp = post_json(state.clone(), "/register", &serde_json::to_value(hello).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK, "a case-variant claim still verifies");

        let earners = state.earners.lock().await;
        assert_eq!(earners.len(), 1, "exactly one identity");
        assert!(earners.contains_key(&canonical), "keyed on the canonical lowercase identity");
        assert!(!earners.contains_key(&upper), "not the as-sent mixed case");
    }

    /// FM3 (cross-boundary): a result submitted under a DIFFERENT case than the
    /// signer's canonical address still verifies (case-insensitive gate) and is
    /// credited to the canonical identity — the liveness lookup hits the registered
    /// earner and the leaderboard/aggregate keys on one identity, not the submit's case.
    #[tokio::test]
    async fn submit_folds_a_mixed_case_address_to_the_canonical_identity() {
        let state = test_state_empty().await;
        let canonical = dev_address();
        let upper = upper_case_variant(&canonical);

        // Register canonical, then stale its liveness so a hitting lookup is observable.
        post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(signed_hello(&dev_signing_key(), &canonical, "RTX 4090", 24, vec![JobKind::Terrain])).unwrap(),
        )
        .await;
        state.earners.lock().await.get_mut(&canonical).unwrap().last_seen = 0;

        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;
        state.store.lock().await.take_next(|_| true).unwrap(); // in_flight

        // Submit claiming the UPPER-case variant (the signing digest omits the
        // address, so the signature is valid for either case).
        let mut result = signed_result(job_id, "deadbeef");
        result.earner_address = upper.clone();
        let uri = format!("/jobs/{}/submit", job_id);
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&result).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::OK, "the case-variant submit settles");

        // Liveness lookup hit the canonical registry entry (last_seen advanced past 0).
        assert!(
            state.earners.lock().await.get(&canonical).unwrap().last_seen > 0,
            "the case-variant submit refreshed the canonical earner's liveness"
        );
        // The completed credit (leaderboard aggregate) keys on the canonical identity.
        let by_earner = state.store.lock().await.completed_count_by_earner().unwrap();
        assert_eq!(by_earner.get(&canonical).copied(), Some(1), "credited to the canonical identity");
        assert!(!by_earner.contains_key(&upper), "not the mixed case");
    }

    /// FM3 (dispatch poll): an HTTP `/jobs/next?earner=` poll using a case variant of
    /// the registered address resolves to the canonical identity — it applies that
    /// earner's capability filter (and refreshes its liveness) instead of being
    /// treated as an unknown earner, which would lapse to unfiltered dispatch and skip
    /// the liveness refresh.
    #[tokio::test]
    async fn next_job_poll_folds_a_mixed_case_earner_to_the_registered_identity() {
        let state = test_state_empty().await;
        // Older unsupported job first, newer supported job second.
        enqueue(&state, &job_of(JobKind::DiffusionTile)).await;
        let terrain = job_of(JobKind::Terrain);
        enqueue(&state, &terrain).await;

        // Register canonical supporting only Terrain, then stale its liveness.
        let sk = test_signing_key("poll-earner");
        let canonical = address_from_signing_key(&sk);
        let upper = upper_case_variant(&canonical);
        post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(signed_hello(&sk, &canonical, "RTX 4090", 24, vec![JobKind::Terrain])).unwrap(),
        )
        .await;
        state.earners.lock().await.get_mut(&canonical).unwrap().last_seen = 0;

        // Poll with the UPPER-case variant: it must resolve to the registered earner.
        let resp = get(state.clone(), &format!("/jobs/next?earner={upper}")).await;
        let json = body_json(resp).await;
        assert_eq!(
            json["id"].as_str().unwrap(),
            terrain.id.to_string(),
            "the case-variant poll applied the registered Terrain filter, not unfiltered dispatch"
        );
        assert!(
            state.earners.lock().await.get(&canonical).unwrap().last_seen > 0,
            "the case-variant poll refreshed the canonical earner's liveness"
        );
    }

    #[tokio::test]
    async fn submit_with_tampered_signature_rejected() {
        let state = test_state();
        let job_id = Uuid::new_v4();
        let mut bad = signed_result(job_id, "deadbeef");
        // Corrupt the trailing recovery/s byte.
        bad.signature_hex.pop();
        bad.signature_hex.push('f');

        let uri = format!("/jobs/{}/submit", job_id);
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&bad).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 0);
    }

    #[tokio::test]
    async fn submit_with_mismatched_earner_address_rejected() {
        let state = test_state();
        let job_id = Uuid::new_v4();
        let mut bad = signed_result(job_id, "deadbeef");
        // Valid signature, but claim a different address than the signer.
        bad.earner_address = "0x000000000000000000000000000000000000dead".into();

        let uri = format!("/jobs/{}/submit", job_id);
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&bad).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 0);
    }

    // ---- result content gate (http submit) ----

    /// A validly-signed result whose `output_hash` is not a 256-bit digest is
    /// 422 (Unprocessable), distinct from the 401 a bad signature gets: the
    /// signature verifies, the content gate fails. The job is not settled and is
    /// left in_flight for the reaper (mirroring the bad-signature disposition).
    #[tokio::test]
    async fn submit_with_malformed_output_hash_rejected_unprocessable() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;
        state.store.lock().await.take_next(|_| true).unwrap(); // → in_flight, seq 1

        // "deadbeef" is a valid label but only 8 chars as a raw hash — signed
        // verbatim so the signature gate passes and the content gate is what bites.
        let bad = signed_result_raw_hash(job_id, "deadbeef");
        let uri = format!("/jobs/{job_id}/submit");
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&bad).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Not settled, and still in_flight (not requeued, not done) — the reaper
        // owns its fate, exactly as for a bad-signature submit.
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 0);
        let json = body_json(get(state.clone(), &format!("/jobs/{job_id}/status")).await).await;
        assert_eq!(json["status"], "in_flight");
    }

    /// A result claiming zero render-seconds (validly signed — render_seconds is
    /// not part of the signed digest) is rejected 422 and not metered.
    #[tokio::test]
    async fn submit_with_zero_render_seconds_rejected_unprocessable() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        let mut bad = signed_result(job_id, "deadbeef"); // valid hash + signature
        bad.render_seconds = 0; // the only defect; signature stays valid
        let uri = format!("/jobs/{job_id}/submit");
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&bad).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 0);
    }

    /// A result claiming more render-seconds than the job's deadline plausibly
    /// allowed (u32::MAX vs a 60s deadline → bound 120) is rejected 422 and not
    /// metered — the bound that stops /stats and payout poisoning.
    #[tokio::test]
    async fn submit_with_implausible_render_seconds_rejected_unprocessable() {
        let state = test_state_empty().await;
        let job = seed_job(); // deadline_secs = 60 → plausibility bound 120
        let job_id = job.id;
        enqueue(&state, &job).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        let mut bad = signed_result(job_id, "deadbeef"); // valid hash + signature
        bad.render_seconds = u32::MAX; // the only defect; signature stays valid
        let uri = format!("/jobs/{job_id}/submit");
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&bad).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Not settled, still in_flight for the reaper — like every content reject.
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 0);
        let json = body_json(get(state.clone(), &format!("/jobs/{job_id}/status")).await).await;
        assert_eq!(json["status"], "in_flight");
    }

    /// FM2: an honest earner that finished within the slack (render_seconds ==
    /// deadline * SLACK, the inclusive bound) still settles end-to-end — the
    /// bound refuses fabricated values, not legitimate near-deadline work.
    #[tokio::test]
    async fn submit_at_the_render_seconds_slack_bound_settles() {
        let state = test_state_empty().await;
        let job = seed_job(); // deadline_secs = 60 → bound 120
        let job_id = job.id;
        enqueue(&state, &job).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        let mut good = signed_result(job_id, "deadbeef");
        good.render_seconds = 120; // exactly deadline * SLACK
        let uri = format!("/jobs/{job_id}/submit");
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 1);
    }

    /// The content gate sits after the signature gate: a result that is BOTH
    /// badly signed and malformed surfaces the 401, so the gate ordering does not
    /// leak job state to an unauthenticated caller.
    #[tokio::test]
    async fn submit_bad_signature_takes_precedence_over_content_gate() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        let mut bad = signed_result_raw_hash(job_id, "deadbeef"); // malformed hash
        bad.signature_hex.pop();
        bad.signature_hex.push('f'); // ...and now also a broken signature
        let uri = format!("/jobs/{job_id}/submit");
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&bad).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ---- pending EAS attestations ----

    /// Settling a job durably records a pending attestation in the same step,
    /// with every field mapped from the job's spec + result per the contract's
    /// schema (Terrain→0, the region packed, the UUID right-aligned in bytes32).
    #[tokio::test]
    async fn settle_records_a_pending_attestation_mapped_from_spec() {
        let state = test_state_empty().await;
        let job = seed_job(); // Terrain, region { x: 42, y: -17, layer: 0 }
        let job_id = job.id;
        enqueue(&state, &job).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        let result = signed_result(job_id, "render-1");
        let mut store = state.store.lock().await;
        assert!(store.record_completed(&result).unwrap());
        assert_eq!(store.pending_attestation_count().unwrap(), 1);

        let stored = store
            .pending_attestation(&job_id)
            .unwrap()
            .expect("pending row exists");
        // Round-trips through the canonical builder...
        assert_eq!(
            stored,
            eas::PendingAttestation::build(&job, &result).unwrap()
        );
        // ...and the mapping is exactly the contract's.
        assert_eq!(stored.job_kind, 0); // Terrain
        assert_eq!(stored.earner, result.earner_address);
        assert_eq!(stored.output_hash, result.output_hash);
        assert_eq!(stored.render_seconds, 1);
        assert_eq!(stored.job_id, eas::job_id_hex(&job_id));
        assert_eq!(stored.region_id, eas::region_id_hex(&job.region));
    }

    /// A second settle on an already-done job is refused by the in_flight guard,
    /// so the attestation backlog never double-counts a job.
    #[tokio::test]
    async fn replayed_settle_keeps_one_pending_attestation() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        let result = signed_result(job_id, "render-1");
        let mut store = state.store.lock().await;
        assert!(store.record_completed(&result).unwrap());
        assert!(
            !store.record_completed(&result).unwrap(),
            "second settle refused (done)"
        );
        assert_eq!(store.pending_attestation_count().unwrap(), 1);
    }

    /// Settling is the only thing that records an attestation: a record_completed
    /// against a job that is not in_flight settles nothing and enqueues nothing.
    #[tokio::test]
    async fn non_in_flight_settle_records_no_pending_attestation() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await; // queued, never taken → not in_flight

        let mut store = state.store.lock().await;
        assert!(!store.record_completed(&signed_result(job_id, "x")).unwrap());
        assert_eq!(store.pending_attestation_count().unwrap(), 0);
    }

    /// Defensive degrade: a result whose output_hash is not a valid bytes32 digest
    /// still settles (record_completed does not gate content — the accept paths do)
    /// but records no attestation rather than persisting a malformed receipt.
    #[tokio::test]
    async fn settle_with_unmappable_result_settles_but_skips_attestation() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        // "deadbeef" is 8 chars — not a 256-bit digest, so the attestation can't build.
        let mut store = state.store.lock().await;
        assert!(store
            .record_completed(&signed_result_raw_hash(job_id, "deadbeef"))
            .unwrap());
        assert_eq!(store.pending_attestation_count().unwrap(), 0);
    }

    /// `/stats` exposes the attestation backlog: until the relayer drains them,
    /// every settled job stays pending, so it tracks `jobs_completed`.
    #[tokio::test]
    async fn stats_reports_pending_attestation_backlog() {
        let state = test_state_empty().await;
        let jobs = [seed_job(), seed_job()];
        for job in &jobs {
            enqueue(&state, job).await;
            state
                .store
                .lock()
                .await
                .take_next(|j| j.id == job.id)
                .unwrap();
            state
                .store
                .lock()
                .await
                .record_completed(&signed_result(job.id, "r"))
                .unwrap();
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 2);
        assert_eq!(json["pending_attestations"], 2);
    }

    // ---- pending ComputeMeter debits ----

    const TEST_BUYER: &str = "0x00000000000000000000000000000000000000b1";

    /// Enqueue `job` attributed to `buyer` through the same capped path POST /jobs
    /// uses, so a later settle reads the buyer back and builds the debit. (The plain
    /// `enqueue` helper is buyerless, like seed/recovery.)
    async fn enqueue_with_buyer(state: &Arc<AppState>, job: &JobSpec, buyer: &str) {
        assert!(state
            .store
            .lock()
            .await
            .enqueue_within_cap(job, state.max_queued_jobs, Some(buyer))
            .unwrap());
    }

    /// Settling a buyer-attributed job under a nonzero rate durably records a
    /// pending debit in the same step, with `amount = rate * render_seconds` and
    /// the buyer/job mapped per the contract's `spend(buyer, amount, jobId)`.
    #[tokio::test]
    async fn settle_with_buyer_records_a_pending_debit_mapped_from_result() {
        let rate = 1_000_000_000_000u128; // 1e12 wei / render-second
        let state = test_state_empty_with_compute_rate(rate).await;
        let job = seed_job();
        let job_id = job.id;
        enqueue_with_buyer(&state, &job, TEST_BUYER).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        let result = signed_result(job_id, "render-1"); // render_seconds = 1
        let mut store = state.store.lock().await;
        assert!(store.record_completed(&result).unwrap());
        assert_eq!(store.pending_debit_count().unwrap(), 1);

        let stored = store
            .pending_debit(&job_id)
            .unwrap()
            .expect("pending debit row exists");
        // Round-trips through the canonical builder...
        assert_eq!(
            stored,
            meter::PendingDebit::build(Some(TEST_BUYER), &result, rate).unwrap()
        );
        // ...and the mapping is exactly the contract's spend args.
        assert_eq!(stored.buyer, TEST_BUYER);
        assert_eq!(stored.amount_wei, "1000000000000"); // rate * 1 render-second
        assert_eq!(stored.job_id, eas::job_id_hex(&job_id));
    }

    /// A second settle on an already-done job is refused by the in_flight guard
    /// (belt-and-suspenders with the debit's ON CONFLICT), so the debit backlog
    /// never double-counts a job.
    #[tokio::test]
    async fn replayed_settle_keeps_one_pending_debit() {
        let state = test_state_empty_with_compute_rate(1_000_000_000_000).await;
        let job = seed_job();
        let job_id = job.id;
        enqueue_with_buyer(&state, &job, TEST_BUYER).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        let result = signed_result(job_id, "render-1");
        let mut store = state.store.lock().await;
        assert!(store.record_completed(&result).unwrap());
        assert!(
            !store.record_completed(&result).unwrap(),
            "second settle refused (done)"
        );
        assert_eq!(store.pending_debit_count().unwrap(), 1);
    }

    /// The key divergence from the attestation: an unattributed job (no buyer)
    /// settles cleanly and is still attested, but accrues NO debit — there is
    /// nobody to charge. A settle must never be bricked by the absence of a buyer.
    #[tokio::test]
    async fn settle_without_buyer_records_no_debit_but_still_attests() {
        let state = test_state_empty_with_compute_rate(1_000_000_000_000).await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await; // buyerless
        state.store.lock().await.take_next(|_| true).unwrap();

        let mut store = state.store.lock().await;
        assert!(store.record_completed(&signed_result(job_id, "r")).unwrap());
        assert_eq!(store.pending_debit_count().unwrap(), 0, "no buyer → no debit");
        assert_eq!(
            store.pending_attestation_count().unwrap(),
            1,
            "the attestation still lands — the debit skip must not gate it"
        );
    }

    /// Metering is opt-in: with the default rate (0), even a buyer-attributed job
    /// accrues no debit, so the slice can't silently start charging real credit
    /// before an operator sets the economic rate.
    #[tokio::test]
    async fn settle_with_metering_disabled_records_no_debit() {
        let state = test_state_empty().await; // default store: compute_rate_wei = 0
        let job = seed_job();
        let job_id = job.id;
        enqueue_with_buyer(&state, &job, TEST_BUYER).await;
        state.store.lock().await.take_next(|_| true).unwrap();

        let mut store = state.store.lock().await;
        assert!(store.record_completed(&signed_result(job_id, "r")).unwrap());
        assert_eq!(
            store.pending_debit_count().unwrap(),
            0,
            "rate 0 = metering disabled → no debit even with a buyer"
        );
    }

    /// `/stats` exposes the debit backlog (the metering twin of
    /// `pending_attestations`): until the operator-gated relayer drains them, each
    /// settled+metered job stays pending, so it tracks `jobs_completed`.
    #[tokio::test]
    async fn stats_reports_pending_debit_backlog() {
        let state = test_state_empty_with_compute_rate(1_000_000_000_000).await;
        let jobs = [seed_job(), seed_job()];
        for job in &jobs {
            enqueue_with_buyer(&state, job, TEST_BUYER).await;
            state
                .store
                .lock()
                .await
                .take_next(|j| j.id == job.id)
                .unwrap();
            state
                .store
                .lock()
                .await
                .record_completed(&signed_result(job.id, "r"))
                .unwrap();
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 2);
        assert_eq!(json["pending_debits"], 2);
    }

    // ---- attestation relayer (drain loop) ----

    use relay::MockRelay;
    use tokio::sync::Notify;

    /// Enqueue, dispatch, and settle `job`, leaving it with one pending
    /// attestation — the precondition for every drain test.
    async fn settle_one(state: &Arc<AppState>, job: &JobSpec) {
        enqueue(state, job).await;
        state
            .store
            .lock()
            .await
            .take_next(|j| j.id == job.id)
            .unwrap();
        assert!(state
            .store
            .lock()
            .await
            .record_completed(&signed_result(job.id, "r"))
            .unwrap());
    }

    async fn pending(state: &Arc<AppState>) -> usize {
        state
            .store
            .lock()
            .await
            .pending_attestation_count()
            .unwrap()
    }

    async fn dead_lettered_attestations(state: &Arc<AppState>) -> usize {
        state
            .store
            .lock()
            .await
            .dead_lettered_attestation_count()
            .unwrap()
    }

    /// Seed + settle `n` distinct jobs, returning them in insertion (oldest-first)
    /// order so a batch test can map each job to its claimed slot.
    async fn settle_n(state: &Arc<AppState>, n: usize) -> Vec<JobSpec> {
        let jobs: Vec<JobSpec> = (0..n).map(|_| seed_job()).collect();
        for job in &jobs {
            settle_one(state, job).await;
        }
        jobs
    }

    async fn stored_uid(state: &Arc<AppState>, job_id: &Uuid) -> Option<String> {
        state.store.lock().await.attestation_uid(job_id).unwrap()
    }

    /// A generous cap so most tests drain in one batch; the cap itself is pinned by
    /// `drain_caps_the_batch_at_relay_batch_size`.
    const TEST_BATCH: usize = 16;

    #[tokio::test]
    async fn drain_batches_all_pending_in_one_multiattest() {
        let state = test_state_empty().await;
        let jobs = settle_n(&state, 3).await;
        assert_eq!(pending(&state).await, 3);

        let relay = MockRelay::succeeding();
        drain_attestations(&state, &relay, TEST_BATCH).await;

        assert_eq!(pending(&state).await, 0, "all receipts drained");
        assert_eq!(relay.batch_calls(), 1, "one multiAttest, not N issueReceipt");
        assert_eq!(relay.calls(), 0, "no single submits on the happy path");
        let mut submitted = relay.batch_submitted();
        submitted.sort();
        let mut expected: Vec<String> = jobs.iter().map(|j| eas::job_id_hex(&j.id)).collect();
        expected.sort();
        assert_eq!(submitted, expected, "every pending job in the one batch");
    }

    #[tokio::test]
    async fn drain_a_second_pass_with_an_empty_backlog_is_a_noop() {
        let state = test_state_empty().await;
        settle_n(&state, 1).await;

        let relay = MockRelay::succeeding();
        drain_attestations(&state, &relay, TEST_BATCH).await;
        assert_eq!(pending(&state).await, 0);
        assert_eq!(relay.batch_calls(), 1);

        // Nothing pending → the relay is not called again.
        drain_attestations(&state, &relay, TEST_BATCH).await;
        assert_eq!(relay.batch_calls(), 1, "an empty backlog submits nothing");
    }

    /// FM3: each job is marked with ITS OWN submission-order uid. The mock's batch
    /// uid embeds the job id, so a mis-indexed zip would store the wrong job's uid.
    #[tokio::test]
    async fn drain_marks_each_with_its_own_submission_order_uid() {
        let state = test_state_empty().await;
        let jobs = settle_n(&state, 4).await;

        drain_attestations(&state, &MockRelay::succeeding(), TEST_BATCH).await;

        for j in &jobs {
            let jid = eas::job_id_hex(&j.id);
            assert_eq!(
                stored_uid(&state, &j.id).await,
                Some(format!("0xmockbatch-{jid}")),
                "job mapped to its own uid, not a neighbour's"
            );
        }
    }

    /// FM3 defensive: a misbehaving relay that returns fewer uids than receipts
    /// must mark NOTHING — a positional zip over a short return would map jobs to
    /// the wrong/stale uid. The contract itself reverts on this; the drain guards
    /// it too rather than trusting the count.
    #[tokio::test]
    async fn drain_marks_nothing_when_the_batch_returns_too_few_uids() {
        struct ShortBatchRelay;
        impl Relay for ShortBatchRelay {
            async fn submit(&self, _att: &eas::PendingAttestation) -> Result<String, RelayError> {
                Ok("0xunused".into())
            }
            async fn submit_batch(
                &self,
                atts: &[eas::PendingAttestation],
            ) -> Result<Vec<String>, BatchRelayError> {
                // One fewer uid than requested.
                Ok(atts.iter().skip(1).map(|_| "0xshort".to_string()).collect())
            }
        }

        let state = test_state_empty().await;
        settle_n(&state, 3).await;
        drain_attestations(&state, &ShortBatchRelay, TEST_BATCH).await;
        assert_eq!(
            pending(&state).await,
            3,
            "a short uid return marks nothing — no mis-mapped uids"
        );
    }

    /// FM1: a batch with ONE already-on-chain receipt reverts on the contract's
    /// per-element `DuplicateReceipt` fence; the fallback isolates it so the other
    /// N-1 still drain and the already-issued one is marked — never zero progress.
    #[tokio::test]
    async fn drain_isolates_an_already_issued_receipt_in_a_reverted_batch() {
        let state = test_state_empty().await;
        let jobs = settle_n(&state, 3).await;
        let already = eas::job_id_hex(&jobs[1].id);

        let relay = MockRelay::succeeding()
            .with_batch_reverts()
            .with_already_issued_job(already.clone());
        drain_attestations(&state, &relay, TEST_BATCH).await;

        assert_eq!(
            pending(&state).await,
            0,
            "the other N-1 drain and the already-issued row is marked"
        );
        assert_eq!(relay.batch_calls(), 1, "one batch attempt, then fallback");
        let mut singly = relay.submitted();
        singly.sort();
        let mut expected: Vec<String> = jobs
            .iter()
            .map(|j| eas::job_id_hex(&j.id))
            .filter(|h| h != &already)
            .collect();
        expected.sort();
        assert_eq!(singly, expected, "fallback single-submits every non-already receipt");
        assert_eq!(
            stored_uid(&state, &jobs[1].id).await.as_deref(),
            Some(ALREADY_ISSUED_UID),
            "the already-issued row carries the sentinel, not a re-attestation"
        );
    }

    /// FM2: an atomic batch revert the fallback can't isolate this tick (every
    /// single also faults) marks NOTHING — all rows stay pending and retry, never
    /// partial-marked.
    #[tokio::test]
    async fn drain_reverted_batch_marks_nothing_when_unisolatable() {
        let state = test_state_empty().await;
        settle_n(&state, 3).await;

        // Batch reverts and every single submit is transient (an RPC brownout).
        let relay = MockRelay::transient_then_ok(usize::MAX).with_batch_reverts();
        drain_attestations(&state, &relay, TEST_BATCH).await;

        assert_eq!(pending(&state).await, 3, "a reverted batch marks nothing");
        assert_eq!(relay.batch_calls(), 1);
        assert_eq!(relay.calls(), 1, "fallback stops at the first transient — no hot loop");
        assert!(relay.submitted().is_empty());
    }

    /// A whole-batch transient backs off without the single fallback (singles would
    /// hit the same fault) and drops nothing.
    #[tokio::test]
    async fn drain_transient_batch_leaves_all_pending() {
        let state = test_state_empty().await;
        settle_n(&state, 2).await;

        let relay = MockRelay::succeeding().with_batch_transient();
        drain_attestations(&state, &relay, TEST_BATCH).await;

        assert_eq!(pending(&state).await, 2, "nothing dropped");
        assert_eq!(relay.batch_calls(), 1, "stops at the batch error — no hot loop");
        assert_eq!(relay.calls(), 0, "no single fallback on a whole-batch transient");
    }

    /// A transient batch is not terminal: the rows survive and a later successful
    /// tick drains them.
    #[tokio::test]
    async fn drain_retries_a_transient_batch_on_the_next_tick() {
        let state = test_state_empty().await;
        settle_n(&state, 2).await;

        drain_attestations(&state, &MockRelay::succeeding().with_batch_transient(), TEST_BATCH).await;
        assert_eq!(pending(&state).await, 2, "transient is not terminal");

        drain_attestations(&state, &MockRelay::succeeding(), TEST_BATCH).await;
        assert_eq!(pending(&state).await, 0, "drains once the batch succeeds");
    }

    /// A whole-batch permanent (e.g. an unauthorized signer) neither drops a
    /// receipt nor hot-loops — the drain stops for the operator.
    #[tokio::test]
    async fn drain_permanent_batch_leaves_all_pending() {
        let state = test_state_empty().await;
        settle_n(&state, 2).await;

        let relay = MockRelay::succeeding().with_batch_permanent();
        drain_attestations(&state, &relay, TEST_BATCH).await;

        assert_eq!(pending(&state).await, 2, "permanent batch error drops nothing");
        assert_eq!(relay.batch_calls(), 1, "no hot loop");
        assert_eq!(relay.calls(), 0, "no fallback on a whole-batch permanent");
        // FM3: a whole-batch Permanent is a GLOBAL misconfig (unauthorized signer),
        // never folded into the per-row dead-letter — masking it would quarantine
        // every receipt and hide the misconfig.
        assert_eq!(
            dead_lettered_attestations(&state).await,
            0,
            "a whole-batch permanent is never per-row dead-lettered"
        );
    }

    /// FM1: a reverted batch whose ONE poison receipt faults `Permanent` (e.g. an
    /// unpayable region fee) dead-letters that receipt and DRAINS THE REST the same
    /// tick — the head-of-line is unblocked, the poison retained (never dropped or
    /// attested), surfaced as the dead-letter depth.
    #[tokio::test]
    async fn drain_dead_letters_the_poison_receipt_and_drains_the_rest() {
        let state = test_state_empty().await;
        let jobs = settle_n(&state, 3).await;
        let poison = &jobs[1]; // mid-chunk, so a downstream receipt must still drain

        let relay = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&poison.id));
        drain_attestations(&state, &relay, TEST_BATCH).await;

        assert_eq!(
            dead_lettered_attestations(&state).await,
            1,
            "the poison is quarantined, not dropped"
        );
        assert_eq!(pending(&state).await, 0, "the other two still drained this tick");
        assert_eq!(
            stored_uid(&state, &poison.id).await,
            None,
            "the poison is retained but never attested"
        );
        for good in [&jobs[0], &jobs[2]] {
            assert!(
                stored_uid(&state, &good.id).await.is_some(),
                "a non-poison receipt is attested despite the poison sibling"
            );
        }
    }

    /// FM2: a `Transient` single (an RPC hiccup) inside a reverted batch backs off
    /// and is retried next tick — NEVER dead-lettered (quarantining a relayable
    /// receipt would lose it).
    #[tokio::test]
    async fn drain_does_not_dead_letter_a_transient_single() {
        let state = test_state_empty().await;
        settle_n(&state, 2).await;

        let relay = MockRelay::transient_then_ok(1).with_batch_reverts();
        drain_attestations(&state, &relay, TEST_BATCH).await;

        assert_eq!(
            dead_lettered_attestations(&state).await,
            0,
            "a transient single is retried, never dead-lettered"
        );
        assert_eq!(pending(&state).await, 2, "both stay pending for the next tick");
    }

    /// FM4: a dead-lettered receipt is not re-claimed by a later drain (the claim
    /// skips dead-lettered rows) and the mark is idempotent — so an eventual
    /// operator re-drive cannot double-attest or double-mark.
    #[tokio::test]
    async fn drain_does_not_redrive_a_dead_lettered_receipt() {
        let state = test_state_empty().await;
        let jobs = settle_n(&state, 1).await;
        let job = &jobs[0];

        let poison = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&job.id));
        drain_attestations(&state, &poison, TEST_BATCH).await;
        assert_eq!(dead_lettered_attestations(&state).await, 1);

        // A later drain with a healthy relay neither re-claims nor re-submits it.
        let healthy = MockRelay::succeeding();
        drain_attestations(&state, &healthy, TEST_BATCH).await;
        assert_eq!(healthy.batch_calls(), 0, "the dead-lettered row is never re-claimed");
        assert_eq!(healthy.calls(), 0, "nor re-submitted singly");
        assert_eq!(dead_lettered_attestations(&state).await, 1, "still quarantined");
        assert_eq!(stored_uid(&state, &job.id).await, None, "never attested");

        // The mark is idempotent: a re-mark of an already-dead-lettered row is a no-op.
        assert!(
            !state
                .store
                .lock()
                .await
                .mark_attestation_dead_lettered(&job.id, 123)
                .unwrap(),
            "re-marking an already-dead-lettered receipt is a no-op"
        );
    }

    /// FM4: the batch is capped to the gas-safe N — a 5-receipt backlog drains in
    /// ceil(5/2) batches, proving `claim_oldest_pending_batch` honours the limit
    /// (an uncapped claim would be one batch).
    #[tokio::test]
    async fn drain_caps_the_batch_at_relay_batch_size() {
        let state = test_state_empty().await;
        settle_n(&state, 5).await;

        let relay = MockRelay::succeeding();
        drain_attestations(&state, &relay, 2).await;

        assert_eq!(pending(&state).await, 0, "the whole backlog drains across capped chunks");
        assert_eq!(relay.batch_calls(), 3, "5 receipts drain in ceil(5/2)=3 gas-bounded batches");
    }

    /// FM4: the drain must not hold the store mutex across the slow on-chain batch
    /// submit, or every settle/stats stalls behind RPC latency. The gated relay
    /// holds the multiAttest in-flight while we prove the store lock is acquirable.
    #[tokio::test]
    async fn drain_holds_no_store_lock_across_the_batch_submit() {
        let state = test_state_empty().await;
        settle_n(&state, 2).await;

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let relay = MockRelay::gated(started.clone(), release.clone());

        let drive = {
            let state = state.clone();
            tokio::spawn(async move { drain_attestations(&state, &relay, TEST_BATCH).await })
        };

        // The batch submit is now in-flight (claimed, lock dropped, awaiting release).
        started.notified().await;

        // If the drain held the lock across the await this deadlocks; the timeout
        // turns that regression into a failure instead of a hang.
        tokio::time::timeout(Duration::from_secs(5), async {
            assert_eq!(pending(&state).await, 2, "claimed but not yet marked");
        })
        .await
        .expect("store lock free during the in-flight batch submit");

        release.notify_one();
        drive.await.unwrap();
        assert_eq!(
            pending(&state).await,
            0,
            "receipts marked once the batch returns"
        );
    }

    #[tokio::test]
    async fn stats_pending_attestations_drains_after_relay() {
        let state = test_state_empty().await;
        settle_n(&state, 2).await;
        let before = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(before["pending_attestations"], 2);

        drain_attestations(&state, &MockRelay::succeeding(), TEST_BATCH).await;

        let after = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            after["pending_attestations"], 0,
            "the backlog drains as receipts land"
        );
    }

    // -- POST /receipts/{id}/redrive + /receipts/redrive-all — operator re-drive of a
    //    dead-lettered attestation --

    /// Settle one job and drain it through a batch-reverting, per-job-permanent relay
    /// so the receipt is dead-lettered — the precondition for every attestation
    /// re-drive test. The batch reverts (→ single-submit fallback) and the single
    /// submit faults `Permanent`, so the row is quarantined and nothing is drainable.
    async fn dead_letter_one_attestation(state: &Arc<AppState>, job: &JobSpec) {
        settle_one(state, job).await;
        let poison = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&job.id));
        drain_attestations(state, &poison, TEST_BATCH).await;
        assert_eq!(dead_lettered_attestations(state).await, 1, "precondition: one dead-lettered attestation");
        assert_eq!(pending(state).await, 0, "precondition: nothing drainable");
    }

    /// Empty-queue state with an ingest token configured (so the re-drive endpoints are
    /// gated), for the endpoint auth tests. No compute rate — an attestation accrues on
    /// every settle, unlike a debit.
    async fn test_state_with_token(token: &str) -> Arc<AppState> {
        let state = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { ingest_token: Some(token.to_string()), ..test_config() },
        )
        .unwrap();
        drain_seeded_jobs(&state).await;
        state
    }

    /// `POST /receipts/{id}/redrive` with an optional `Authorization` header (body-less).
    async fn post_receipt_redrive(
        state: Arc<AppState>,
        id: Uuid,
        authorization: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder()
            .method("POST")
            .uri(format!("/receipts/{id}/redrive"));
        if let Some(auth) = authorization {
            builder = builder.header("authorization", auth);
        }
        router(state).oneshot(builder.body(Body::empty()).unwrap()).await.unwrap()
    }

    /// `POST /receipts/redrive-all` with an optional `Authorization` header (body-less).
    async fn post_receipts_redrive_all(state: Arc<AppState>, authorization: Option<&str>) -> axum::response::Response {
        let mut builder = Request::builder().method("POST").uri("/receipts/redrive-all");
        if let Some(auth) = authorization {
            builder = builder.header("authorization", auth);
        }
        router(state).oneshot(builder.body(Body::empty()).unwrap()).await.unwrap()
    }

    /// FM1: re-drive re-arms ONLY a dead-lettered, not-yet-attested receipt. A
    /// dead-lettered row is re-armed (→ drainable again); an already-attested row
    /// (re-arming would resurrect a landed attestation), an unknown job, and a
    /// still-pending row (already drainable) are all refused — the store method returns
    /// false and nothing changes.
    #[tokio::test]
    async fn redrive_rearms_only_a_dead_lettered_unattested_receipt() {
        let state = test_state_empty().await;
        let poison = seed_job();
        let good = seed_job();
        settle_one(&state, &poison).await; // settled first → claimed first (the head)
        settle_one(&state, &good).await;

        // Dead-letter the poison head; the good receipt attests in the same pass (the
        // batch reverts, falls to singles, the poison faults Permanent, the good lands).
        let relay = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&poison.id));
        drain_attestations(&state, &relay, TEST_BATCH).await;
        assert_eq!(dead_lettered_attestations(&state).await, 1);
        assert_eq!(pending(&state).await, 0, "good attested, poison quarantined");
        assert!(stored_uid(&state, &good.id).await.is_some(), "good landed its uid");

        {
            let store = state.store.lock().await;
            // Already-attested (good): uid set AND not dead-lettered, so the re-arm's
            // `dead_lettered_at IS NOT NULL` clause refuses it (no resurrecting a landed
            // attestation). The `uid IS NULL` clause is additional defense-in-depth — no
            // reachable row is both attested and dead-lettered, so it can't be exercised here.
            assert!(!store.redrive_dead_lettered_attestation(&good.id).unwrap(), "an attested receipt is not re-armed");
            // Unknown job: nothing to re-arm.
            assert!(!store.redrive_dead_lettered_attestation(&Uuid::new_v4()).unwrap(), "an unknown receipt is not re-armed");
            // Dead-lettered (poison): re-armed.
            assert!(store.redrive_dead_lettered_attestation(&poison.id).unwrap(), "a dead-lettered receipt is re-armed");
        }
        assert_eq!(dead_lettered_attestations(&state).await, 0, "poison left the dead-letter set");
        assert_eq!(pending(&state).await, 1, "poison re-entered the drainable backlog");

        // Now-pending poison: a second re-drive is a no-op (already drainable).
        let store = state.store.lock().await;
        assert!(!store.redrive_dead_lettered_attestation(&poison.id).unwrap(), "a still-pending receipt is not re-armed");
    }

    /// FM4 (over-reach): the bulk re-drive re-arms ONLY dead-lettered, unattested rows —
    /// never a still-pending (already drainable) row and never an attested (landed) one.
    /// With two dead-lettered, one attested, and one pending row present, it re-arms
    /// exactly the two, leaves the pending row pending, and never resurrects the landed
    /// attestation.
    #[tokio::test]
    async fn redrive_all_rearms_only_dead_lettered_unattested_receipts() {
        let state = test_state_empty().await;
        // Two poison heads → dead-lettered.
        let poison_a = seed_job();
        let poison_b = seed_job();
        settle_one(&state, &poison_a).await;
        settle_one(&state, &poison_b).await;
        let relay = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&poison_a.id))
            .with_permanent_job(eas::job_id_hex(&poison_b.id));
        drain_attestations(&state, &relay, TEST_BATCH).await;
        assert_eq!(dead_lettered_attestations(&state).await, 2);

        // One good receipt → attested (the poisons are skipped, stay quarantined).
        let good = seed_job();
        settle_one(&state, &good).await;
        drain_attestations(&state, &MockRelay::succeeding(), TEST_BATCH).await;
        assert_eq!(dead_lettered_attestations(&state).await, 2, "the poisons stay quarantined");
        let good_uid = stored_uid(&state, &good.id).await.expect("good attested");

        // One fresh receipt → still pending (never drained).
        let fresh = seed_job();
        settle_one(&state, &fresh).await;
        assert_eq!(pending(&state).await, 1, "fresh is drainable");

        // Bulk re-drive: only the two dead-lettered rows match.
        let rearmed = state.store.lock().await.redrive_all_dead_lettered_attestations().unwrap();
        assert_eq!(rearmed, 2, "exactly the two dead-lettered rows, not the pending or attested one");
        assert_eq!(dead_lettered_attestations(&state).await, 0, "both poisons re-armed");
        assert_eq!(pending(&state).await, 3, "the two re-armed poisons joined the fresh pending row");

        // The attested `good` was untouched: its uid is unchanged and a succeeding drain
        // attests exactly the 3 pending rows without resurrecting the landed one.
        drain_attestations(&state, &MockRelay::succeeding(), TEST_BATCH).await;
        assert_eq!(stored_uid(&state, &good.id).await.as_deref(), Some(good_uid.as_str()), "the landed attestation is never re-driven");
        assert_eq!(pending(&state).await, 0, "the 3 re-armed/fresh rows drained");
    }

    /// FM4: the re-drive endpoint requires the same bearer token as `POST /jobs`. A
    /// missing-header and a wrong-token call are both `401` and re-arm NOTHING (the
    /// receipt stays dead-lettered); the correct token re-arms it (`200 {"rearmed": true}`).
    #[tokio::test]
    async fn receipt_redrive_endpoint_rejects_unauthenticated_and_accepts_the_token() {
        let state = test_state_with_token(TEST_INGEST_TOKEN).await;
        let job = seed_job();
        dead_letter_one_attestation(&state, &job).await;

        // No Authorization header → 401, nothing re-armed.
        let resp = post_receipt_redrive(state.clone(), job.id, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(dead_lettered_attestations(&state).await, 1, "an unauthenticated re-drive re-arms nothing");
        assert_eq!(pending(&state).await, 0);

        // Wrong token → 401, still nothing re-armed.
        let resp = post_receipt_redrive(state.clone(), job.id, Some("Bearer wrong-token")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(dead_lettered_attestations(&state).await, 1);

        // Correct token → 200, re-armed.
        let auth = format!("Bearer {TEST_INGEST_TOKEN}");
        let resp = post_receipt_redrive(state.clone(), job.id, Some(&auth)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], true);
        assert_eq!(dead_lettered_attestations(&state).await, 0, "the authenticated re-drive re-armed it");
        assert_eq!(pending(&state).await, 1);
    }

    /// The endpoint mirrors `POST /jobs`' unconfigured-open posture: with no token a
    /// re-drive needs no auth. Also pins that the no-op path (the row no longer
    /// dead-lettered) is a successful `200 {"rearmed": false}`, not an error — so an
    /// operator can replay a re-drive idempotently.
    #[tokio::test]
    async fn receipt_redrive_endpoint_open_and_idempotent_without_token() {
        let state = test_state_empty().await; // no token configured
        assert!(state.ingest_token.is_none());
        let job = seed_job();
        dead_letter_one_attestation(&state, &job).await;

        // Open: re-drive with no auth re-arms the dead-lettered receipt.
        let resp = post_receipt_redrive(state.clone(), job.id, None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], true);
        assert_eq!(pending(&state).await, 1);

        // Idempotent no-op: a second re-drive (now pending, not dead-lettered) is a
        // successful 200 false.
        let resp = post_receipt_redrive(state.clone(), job.id, None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], false);
    }

    /// FM4: the bulk re-drive is a privileged mass-recovery action — a missing, blank, or
    /// wrong bearer token is `401` and re-arms NOTHING; the correct token re-arms the
    /// whole set (`200 {"rearmed": 2}`).
    #[tokio::test]
    async fn receipts_redrive_all_endpoint_rejects_unauthenticated_and_accepts_the_token() {
        let state = test_state_with_token(TEST_INGEST_TOKEN).await;
        let a = seed_job();
        let b = seed_job();
        settle_one(&state, &a).await;
        settle_one(&state, &b).await;
        // Both poison the single-submit fallback, so one drain dead-letters the pair.
        let relay = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&a.id))
            .with_permanent_job(eas::job_id_hex(&b.id));
        drain_attestations(&state, &relay, TEST_BATCH).await;
        assert_eq!(dead_lettered_attestations(&state).await, 2);

        // Missing / blank / wrong token → 401, nothing re-armed.
        assert_eq!(post_receipts_redrive_all(state.clone(), None).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(post_receipts_redrive_all(state.clone(), Some("Bearer ")).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(post_receipts_redrive_all(state.clone(), Some("Bearer wrong-token")).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(dead_lettered_attestations(&state).await, 2, "every unauthenticated bulk re-drive re-armed nothing");

        // Correct token → 200 {"rearmed": 2}.
        let auth = format!("Bearer {TEST_INGEST_TOKEN}");
        let resp = post_receipts_redrive_all(state.clone(), Some(&auth)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], 2);
        assert_eq!(dead_lettered_attestations(&state).await, 0, "the authenticated bulk re-drive re-armed both");
        assert_eq!(pending(&state).await, 2);
    }

    /// FM2: a re-armed receipt attests EXACTLY ONCE on the next drain, and a repeat
    /// re-drive after it has attested is a no-op — no double-attest. The off-chain double
    /// guard (`uid IS NULL AND dead_lettered_at IS NULL`) plus the on-chain
    /// `DuplicateReceipt` fence keep a re-driven receipt to at most one attestation; the
    /// single drain task is the only uid writer.
    #[tokio::test]
    async fn receipt_redrive_then_drain_attests_the_re_armed_receipt_exactly_once() {
        let state = test_state_empty().await;
        let job = seed_job();
        dead_letter_one_attestation(&state, &job).await;

        // Operator re-drives after the region fee is funded.
        assert!(state.store.lock().await.redrive_dead_lettered_attestation(&job.id).unwrap());
        assert_eq!(pending(&state).await, 1, "re-armed back into the backlog");

        // Next drain attests it once.
        let relay = MockRelay::succeeding();
        drain_attestations(&state, &relay, TEST_BATCH).await;
        assert_eq!(relay.batch_calls(), 1);
        assert!(stored_uid(&state, &job.id).await.is_some(), "the re-armed receipt attests exactly once");
        assert_eq!(pending(&state).await, 0);
        assert_eq!(dead_lettered_attestations(&state).await, 0);

        // A repeat re-drive once attested is a no-op (uid set), and a further drain claims
        // nothing — no double-attest on an operator double-call.
        assert!(
            !state.store.lock().await.redrive_dead_lettered_attestation(&job.id).unwrap(),
            "an attested receipt can't be re-armed"
        );
        let relay2 = MockRelay::succeeding();
        drain_attestations(&state, &relay2, TEST_BATCH).await;
        assert_eq!(relay2.batch_calls(), 0, "nothing left to claim — no second attest");
    }

    /// FM3: if the cause is STILL unfixed (the region fee still unpayable), re-driving
    /// then re-draining simply re-dead-letters the row — one more attempt per re-drive,
    /// never an infinite auto-retry and never a panic. It stays quarantined until the next
    /// EXPLICIT re-drive (a dead-lettered row is never auto-re-claimed).
    #[tokio::test]
    async fn receipt_redrive_re_dead_letters_on_a_repeat_permanent_error() {
        let state = test_state_empty().await;
        let job = seed_job();
        dead_letter_one_attestation(&state, &job).await;

        // Re-drive, then drain while the cause is still unfixed (permanent again).
        assert!(state.store.lock().await.redrive_dead_lettered_attestation(&job.id).unwrap());
        let poison = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&job.id));
        drain_attestations(&state, &poison, TEST_BATCH).await;
        assert_eq!(poison.batch_calls(), 1, "one more attempt — not a hot loop");
        assert_eq!(dead_lettered_attestations(&state).await, 1, "re-dead-lettered, not dropped");
        assert_eq!(pending(&state).await, 0);
        assert_eq!(stored_uid(&state, &job.id).await, None, "still unattested");

        // NOT auto-re-driven: a succeeding drain claims nothing until the next explicit re-drive.
        let healthy = MockRelay::succeeding();
        drain_attestations(&state, &healthy, TEST_BATCH).await;
        assert_eq!(healthy.batch_calls(), 0, "a re-dead-lettered receipt is not auto-re-claimed");
        assert_eq!(dead_lettered_attestations(&state).await, 1, "still quarantined");
    }

    /// FM3 (bulk): a bulk re-drive into a still-unfixed BROAD cause re-quarantines the
    /// whole set in ONE bounded drain wave — one attempt per row, no hot loop — rather
    /// than looping. Each re-armed row gets exactly one re-attempt and re-dead-letters.
    #[tokio::test]
    async fn receipts_redrive_all_re_dead_letters_into_a_still_unfixed_cause() {
        let state = test_state_empty().await;
        let a = seed_job();
        let b = seed_job();
        settle_one(&state, &a).await;
        settle_one(&state, &b).await;
        let relay = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&a.id))
            .with_permanent_job(eas::job_id_hex(&b.id));
        drain_attestations(&state, &relay, TEST_BATCH).await;
        assert_eq!(dead_lettered_attestations(&state).await, 2);

        // Bulk re-drive while the broad cause is still unfixed.
        assert_eq!(state.store.lock().await.redrive_all_dead_lettered_attestations().unwrap(), 2);
        assert_eq!(pending(&state).await, 2, "both re-armed back into the backlog");

        // One bounded drain wave re-quarantines the whole set — one attempt per row.
        let relay2 = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&a.id))
            .with_permanent_job(eas::job_id_hex(&b.id));
        drain_attestations(&state, &relay2, TEST_BATCH).await;
        assert_eq!(relay2.batch_calls(), 1, "one batch attempt — not a hot loop");
        assert_eq!(dead_lettered_attestations(&state).await, 2, "re-dead-lettered, not dropped");
        assert_eq!(pending(&state).await, 0);
    }

    // ---- dead-lettered attestation listing (GET /receipts/dead-lettered) ----

    /// FM1: the listing returns ONLY dead-lettered, not-yet-attested receipts — a
    /// still-pending row (drainable, dead_lettered_at IS NULL) and an already-attested
    /// row (uid set, a landed receipt) are both excluded, so every listed receipt is
    /// genuinely re-armable by id. FM5: the new one-segment GET /receipts/dead-lettered
    /// route resolves to this listing handler (not shadowed by /receipts/{id}/redrive).
    #[tokio::test]
    async fn dead_lettered_receipt_listing_returns_only_dead_lettered_unattested_receipts() {
        let state = test_state_empty().await;

        // attested: settle + a clean drain → uid set (a landed receipt, never listed).
        let attested = seed_job();
        settle_one(&state, &attested).await;
        drain_attestations(&state, &MockRelay::succeeding(), TEST_BATCH).await;
        assert!(stored_uid(&state, &attested.id).await.is_some(), "precondition: attested");

        // dead-lettered: the one stuck receipt the operator needs to discover.
        let deadletter = seed_job();
        dead_letter_one_attestation(&state, &deadletter).await;

        // pending: settled but never drained → drainable, not dead-lettered.
        let pending_job = seed_job();
        settle_one(&state, &pending_job).await;
        assert_eq!(pending(&state).await, 1, "precondition: one drainable receipt");

        let listing = body_json(get(state.clone(), "/receipts/dead-lettered").await).await;
        let receipts = listing["receipts"].as_array().unwrap();
        assert_eq!(receipts.len(), 1, "only the dead-lettered receipt is listed");
        assert_eq!(
            receipts[0]["job_id"], deadletter.id.to_string(),
            "the dead-lettered one (not attested/pending)"
        );
        assert_eq!(listing["total"], 1);
        assert_eq!(listing["truncated"], false);
    }

    /// FM4: each listed row carries the EXACT persisted fields (job_id, earner,
    /// render_seconds, job_kind, dead_lettered_at), never re-derived or coerced — an
    /// operator sees which earner's proof of how much compute is stuck, and when.
    #[tokio::test]
    async fn dead_lettered_receipt_listing_carries_the_exact_persisted_fields() {
        let state = test_state_empty().await;
        let job = seed_job(); // JobKind::Terrain (0); signed_result render_seconds = 1
        dead_letter_one_attestation(&state, &job).await;

        let listing = body_json(get(state.clone(), "/receipts/dead-lettered").await).await;
        let r = &listing["receipts"][0];
        assert_eq!(r["job_id"], job.id.to_string());
        assert_eq!(r["earner"], dev_address(), "the settling earner, verbatim");
        assert_eq!(r["render_seconds"], 1, "the attested compute, exact");
        assert_eq!(r["job_kind"], 0, "JobKind::Terrain numeric");
        assert!(r["dead_lettered_at"].as_i64().unwrap() > 0, "the quarantine stamp is present");
    }

    /// FM2: the store listing is capped and oldest-first — a limit smaller than the
    /// dead-letter backlog returns the OLDEST `limit` rows in insertion order, and the
    /// full count exceeds the page (the `total > len` the endpoint reports as
    /// `truncated`), so a capped page is never mistaken for the whole set.
    #[tokio::test]
    async fn list_dead_lettered_attestations_caps_and_orders_oldest_first() {
        let state = test_state_empty().await;
        let jobs = [seed_job(), seed_job(), seed_job()];
        for job in &jobs {
            settle_one(&state, job).await;
        }
        // One drain dead-letters all three: the batch reverts → single-submit fallback →
        // each single submit is Permanent for its job.
        let poison = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&jobs[0].id))
            .with_permanent_job(eas::job_id_hex(&jobs[1].id))
            .with_permanent_job(eas::job_id_hex(&jobs[2].id));
        drain_attestations(&state, &poison, TEST_BATCH).await;
        assert_eq!(dead_lettered_attestations(&state).await, 3, "precondition: all three quarantined");

        let store = state.store.lock().await;
        // Capped: a limit of 2 over 3 dead-lettered returns the 2 OLDEST, in order.
        let page = store.list_dead_lettered_attestations(2).unwrap();
        assert_eq!(
            page.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![jobs[0].id, jobs[1].id],
            "oldest two, in order"
        );
        // total exceeds the page → the endpoint's `truncated` signal (total > len).
        assert_eq!(store.dead_lettered_attestation_count().unwrap(), 3);
        // Uncapped (limit >= total) returns all three, oldest-first.
        let all = store.list_dead_lettered_attestations(10).unwrap();
        assert_eq!(
            all.iter().map(|r| r.0).collect::<Vec<_>>(),
            jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
            "all three, oldest-first"
        );
    }

    /// FM3: the listing exposes earner addresses + the attested compute, so it requires
    /// the same bearer token as POST /jobs — a missing or wrong token is 401 and lists
    /// nothing; the correct token returns the dead-lettered receipt. FM5: the by-id
    /// re-drive sibling still resolves under the same router (no route collision).
    #[tokio::test]
    async fn dead_lettered_receipt_listing_requires_the_ingest_token() {
        let state = test_state_with_token(TEST_INGEST_TOKEN).await;
        let job = seed_job();
        dead_letter_one_attestation(&state, &job).await;

        assert_eq!(
            get(state.clone(), "/receipts/dead-lettered").await.status(),
            StatusCode::UNAUTHORIZED,
            "no Authorization header → 401"
        );
        assert_eq!(
            get_auth(state.clone(), "/receipts/dead-lettered", Some("Bearer wrong-token"))
                .await
                .status(),
            StatusCode::UNAUTHORIZED,
            "wrong token → 401"
        );

        let auth = format!("Bearer {TEST_INGEST_TOKEN}");
        let resp = get_auth(state.clone(), "/receipts/dead-lettered", Some(&auth)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let listing = body_json(resp).await;
        assert_eq!(
            listing["receipts"].as_array().unwrap().len(),
            1,
            "the authenticated listing shows the receipt"
        );
        assert_eq!(listing["receipts"][0]["job_id"], job.id.to_string());

        // FM5: the by-id re-drive sibling still routes (not shadowed by the new
        // one-segment /receipts/dead-lettered) — it returns its own 200 under the token.
        let redrive = post_receipt_redrive(state.clone(), job.id, Some(&auth)).await;
        assert_eq!(redrive.status(), StatusCode::OK, "POST /receipts/{{id}}/redrive still routes");
    }

    // ---- debit relayer (drain loop) ----

    use meter::MockSpender;

    const DRAIN_RATE: u128 = 1_000_000_000_000; // 1e12 wei / render-second

    /// Enqueue (attributed to `TEST_BUYER`), dispatch, and settle `job` under a
    /// nonzero compute rate, leaving it with one pending debit — the precondition
    /// for every debit-drain test.
    async fn settle_one_metered(state: &Arc<AppState>, job: &JobSpec) {
        enqueue_with_buyer(state, job, TEST_BUYER).await;
        state
            .store
            .lock()
            .await
            .take_next(|j| j.id == job.id)
            .unwrap();
        assert!(state
            .store
            .lock()
            .await
            .record_completed(&signed_result(job.id, "r"))
            .unwrap());
    }

    async fn pending_debits(state: &Arc<AppState>) -> usize {
        state.store.lock().await.pending_debit_count().unwrap()
    }

    async fn dead_lettered_debits(state: &Arc<AppState>) -> usize {
        state.store.lock().await.dead_lettered_debit_count().unwrap()
    }

    #[tokio::test]
    async fn drain_debits_spends_each_pending_once_and_marks_it() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let jobs = [seed_job(), seed_job()];
        for job in &jobs {
            settle_one_metered(&state, job).await;
        }
        assert_eq!(pending_debits(&state).await, 2);

        let spender = MockSpender::succeeding();
        drain_debits(&state, &spender).await;

        assert_eq!(pending_debits(&state).await, 0, "both debits drained");
        assert_eq!(spender.calls(), 2);
        let mut spent = spender.spent();
        spent.sort();
        let mut expected: Vec<String> = jobs.iter().map(|j| eas::job_id_hex(&j.id)).collect();
        expected.sort();
        assert_eq!(spent, expected, "each pending debit spent exactly once");
    }

    #[tokio::test]
    async fn drain_debits_a_second_pass_with_an_empty_backlog_is_a_noop() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        settle_one_metered(&state, &job).await;

        let spender = MockSpender::succeeding();
        drain_debits(&state, &spender).await;
        assert_eq!(pending_debits(&state).await, 0);
        assert_eq!(spender.calls(), 1);

        // Nothing pending → the spender is not called again.
        drain_debits(&state, &spender).await;
        assert_eq!(spender.calls(), 1, "an empty backlog spends nothing");
    }

    /// FM4: the drain submits the buyer + amount EXACTLY as persisted at settle
    /// (read from the row), never re-derived from the rate at drain time.
    #[tokio::test]
    async fn drain_debits_submits_the_amount_persisted_at_settle() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        settle_one_metered(&state, &job).await;

        let spender = MockSpender::succeeding();
        drain_debits(&state, &spender).await;

        let spent = spender.spent_debits();
        assert_eq!(spent.len(), 1);
        assert_eq!(spent[0].buyer, TEST_BUYER);
        assert_eq!(spent[0].amount_wei, "1000000000000"); // DRAIN_RATE * 1, as persisted
        assert_eq!(spent[0].job_id, eas::job_id_hex(&job.id));
    }

    /// Crash recovery: a prior `spendOnce` landed on-chain but the process died
    /// before the local mark, so the row is still pending. The re-submit hits
    /// `ComputeMeter.spendOnce`'s per-job fence (`AlreadySpent`) and the drain marks
    /// the row rather than double-debiting the buyer.
    #[tokio::test]
    async fn drain_debits_marks_already_spent_without_respending() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        settle_one_metered(&state, &job).await;

        let spender = MockSpender::already_spent();
        drain_debits(&state, &spender).await;

        assert_eq!(pending_debits(&state).await, 0, "already-on-chain debit is marked");
        assert_eq!(spender.calls(), 1);
        assert!(spender.spent().is_empty(), "AlreadySpent spends nothing new");
    }

    #[tokio::test]
    async fn drain_debits_retries_a_transient_failure_on_the_next_tick() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        settle_one_metered(&state, &job).await;

        let spender = MockSpender::transient_then_ok(1);
        // First tick: the transient failure leaves the debit pending, not dropped.
        drain_debits(&state, &spender).await;
        assert_eq!(pending_debits(&state).await, 1, "transient failure is not terminal");
        assert_eq!(dead_lettered_debits(&state).await, 0, "a transient failure is retried, never dead-lettered");
        assert_eq!(spender.calls(), 1);
        assert!(spender.spent().is_empty());

        // Next tick: it succeeds and drains.
        drain_debits(&state, &spender).await;
        assert_eq!(pending_debits(&state).await, 0);
        assert_eq!(spender.calls(), 2);
        assert_eq!(spender.spent(), vec![eas::job_id_hex(&job.id)]);
    }

    /// A transient error stops the batch (so a flaky RPC backs off to the next
    /// tick) without dropping any debit or hot-looping.
    #[tokio::test]
    async fn drain_debits_stops_the_batch_at_a_transient_error() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        for job in [seed_job(), seed_job()] {
            settle_one_metered(&state, &job).await;
        }

        let spender = MockSpender::transient_then_ok(usize::MAX); // never reaches ok
        drain_debits(&state, &spender).await;

        assert_eq!(pending_debits(&state).await, 2, "nothing dropped");
        assert_eq!(spender.calls(), 1, "batch stops at the first error — no hot loop");
    }

    /// A permanent error quarantines that one debit (dead-lettered, NOT dropped) and
    /// the drain finishes: it leaves the drainable backlog (pending → 0), is retained
    /// + surfaced as a dead-letter, and nothing is spent on-chain. The single
    /// dead-lettered row is not re-claimed, so there is no hot loop.
    #[tokio::test]
    async fn drain_debits_dead_letters_a_permanent_error_and_retains_it() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        settle_one_metered(&state, &job).await;

        let spender = MockSpender::permanent();
        drain_debits(&state, &spender).await;

        assert_eq!(pending_debits(&state).await, 0, "the poison debit leaves the drainable backlog");
        assert_eq!(dead_lettered_debits(&state).await, 1, "it is quarantined, not dropped");
        assert_eq!(spender.calls(), 1, "no hot loop — the dead-lettered row is not re-claimed");
        assert!(spender.spent().is_empty(), "nothing spent on-chain");
    }

    /// FM1: a poison debit at the HEAD of the backlog is dead-lettered AND the debit
    /// behind it still drains in the SAME pass — head-of-line unblocking — while the
    /// poison row is retained (surfaced as a dead-letter), never dropped.
    #[tokio::test]
    async fn drain_debits_dead_letters_the_head_and_drains_the_rest() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let poison = seed_job();
        let good = seed_job();
        settle_one_metered(&state, &poison).await; // settled first → claimed first (the head)
        settle_one_metered(&state, &good).await;
        assert_eq!(pending_debits(&state).await, 2);

        let spender = MockSpender::permanent_for(eas::job_id_hex(&poison.id));
        drain_debits(&state, &spender).await;

        assert_eq!(pending_debits(&state).await, 0, "the backlog drains past the poison head");
        assert_eq!(dead_lettered_debits(&state).await, 1, "the poison is quarantined, not dropped");
        assert_eq!(
            spender.spent(),
            vec![eas::job_id_hex(&good.id)],
            "the debit behind the poison head still settles"
        );
        assert_eq!(spender.calls(), 2, "one Permanent on the head, one success on the rest");
    }

    /// FM3: a dead-lettered debit is never re-driven — a later drain pass does NOT
    /// re-claim it (no double-submit) and re-marking it is a no-op (no double-mark).
    /// Paired with `ComputeMeter.spendOnce`'s per-`jobId` fence, an operator replay
    /// can never double-spend a quarantined charge.
    #[tokio::test]
    async fn drain_debits_does_not_redrive_a_dead_lettered_debit() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        settle_one_metered(&state, &job).await;

        // First pass quarantines the poison.
        drain_debits(&state, &MockSpender::permanent()).await;
        assert_eq!(dead_lettered_debits(&state).await, 1);
        assert_eq!(pending_debits(&state).await, 0);

        // Second pass with a spender that WOULD settle anything claimed: the
        // dead-lettered row is not re-claimed, so nothing is spent and the quarantine
        // is unchanged — no double-submit.
        let spender = MockSpender::succeeding();
        drain_debits(&state, &spender).await;
        assert_eq!(spender.calls(), 0, "a dead-lettered debit is never re-claimed");
        assert!(spender.spent().is_empty(), "no double-submit on re-drive");
        assert_eq!(dead_lettered_debits(&state).await, 1, "still quarantined");
        assert_eq!(pending_debits(&state).await, 0);

        // Re-marking the same row is a no-op (the double guard), so an operator replay
        // never double-marks.
        let store = state.store.lock().await;
        assert!(
            !store.mark_debit_dead_lettered(&job.id, 123).unwrap(),
            "re-marking an already-dead-lettered debit is a no-op"
        );
    }

    /// FM3 (original): a `NotAuthorized` revert (the spender key isn't on
    /// `ComputeMeter.authorizedSpenders`) is a distinct, loud, non-retrying error —
    /// the backlog stalls visibly rather than silently looping like progress. It is a
    /// GLOBAL misconfig, so it is NEVER folded into the per-row dead-letter path
    /// (quarantining it would silently dead-letter every debit on a one-call fix).
    #[tokio::test]
    async fn drain_debits_surfaces_not_authorized_without_dropping() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        settle_one_metered(&state, &job).await;

        let spender = MockSpender::not_authorized();
        drain_debits(&state, &spender).await;

        assert_eq!(pending_debits(&state).await, 1, "unauthorized spender drops nothing");
        assert_eq!(dead_lettered_debits(&state).await, 0, "a global misconfig is never per-row dead-lettered");
        assert_eq!(spender.calls(), 1, "no hot loop");
        assert!(spender.spent().is_empty());
    }

    // -- POST /debits/{id}/redrive — operator re-drive of a dead-lettered debit --

    /// Settle one metered job and drain it through a permanent-failing spender so it
    /// is dead-lettered — the precondition for every re-drive test.
    async fn dead_letter_one(state: &Arc<AppState>, job: &JobSpec) {
        settle_one_metered(state, job).await;
        drain_debits(state, &MockSpender::permanent()).await;
        assert_eq!(dead_lettered_debits(state).await, 1, "precondition: one dead-lettered debit");
        assert_eq!(pending_debits(state).await, 0, "precondition: nothing drainable");
    }

    /// FM1: re-drive re-arms ONLY a dead-lettered, not-yet-settled debit. A
    /// dead-lettered row is re-armed (→ drainable again); an already-settled row
    /// (re-arming would resurrect a paid charge), an unknown job, and a still-pending
    /// row (already drainable) are all refused — the store method returns false and
    /// nothing changes.
    #[tokio::test]
    async fn redrive_rearms_only_a_dead_lettered_unsettled_debit() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let poison = seed_job();
        let good = seed_job();
        settle_one_metered(&state, &poison).await; // settled first → claimed first (the head)
        settle_one_metered(&state, &good).await;

        // Dead-letter the poison head; the good debit settles in the same pass.
        drain_debits(&state, &MockSpender::permanent_for(eas::job_id_hex(&poison.id))).await;
        assert_eq!(dead_lettered_debits(&state).await, 1);
        assert_eq!(pending_debits(&state).await, 0, "good settled, poison quarantined");

        {
            let store = state.store.lock().await;
            // Already-settled (good): tx_hash set AND not dead-lettered, so the re-arm's
            // `dead_lettered_at IS NOT NULL` clause refuses it (no resurrecting a paid
            // charge). The `tx_hash IS NULL` clause is additional defense-in-depth — no
            // reachable row is both settled and dead-lettered, so it can't be exercised here.
            assert!(!store.redrive_dead_lettered_debit(&good.id).unwrap(), "a settled debit is not re-armed");
            // Unknown job: nothing to re-arm.
            assert!(!store.redrive_dead_lettered_debit(&Uuid::new_v4()).unwrap(), "an unknown debit is not re-armed");
            // Dead-lettered (poison): re-armed.
            assert!(store.redrive_dead_lettered_debit(&poison.id).unwrap(), "a dead-lettered debit is re-armed");
        }
        assert_eq!(dead_lettered_debits(&state).await, 0, "poison left the dead-letter set");
        assert_eq!(pending_debits(&state).await, 1, "poison re-entered the drainable backlog");

        // Now-pending poison: a second re-drive is a no-op (already drainable).
        let store = state.store.lock().await;
        assert!(!store.redrive_dead_lettered_debit(&poison.id).unwrap(), "a still-pending debit is not re-armed");
    }

    /// FM2: a re-armed debit settles EXACTLY ONCE on the next drain, and a repeat
    /// re-drive after it has settled is a no-op — no double-charge. The off-chain
    /// double guard (`tx_hash IS NULL AND dead_lettered_at IS NULL`) plus the on-chain
    /// `spendOnce` fence keep a re-driven charge to at most one settle.
    #[tokio::test]
    async fn redrive_then_drain_settles_the_re_armed_debit_exactly_once() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        dead_letter_one(&state, &job).await;

        // Operator re-drives after the buyer tops up.
        assert!(state.store.lock().await.redrive_dead_lettered_debit(&job.id).unwrap());
        assert_eq!(pending_debits(&state).await, 1, "re-armed back into the backlog");

        // Next drain settles it once.
        let spender = MockSpender::succeeding();
        drain_debits(&state, &spender).await;
        assert_eq!(spender.calls(), 1);
        assert_eq!(spender.spent(), vec![eas::job_id_hex(&job.id)], "the re-armed debit settles exactly once");
        assert_eq!(pending_debits(&state).await, 0);
        assert_eq!(dead_lettered_debits(&state).await, 0);

        // A repeat re-drive once settled is a no-op (tx_hash set), and a further drain
        // claims nothing — no double-charge on an operator double-call.
        assert!(
            !state.store.lock().await.redrive_dead_lettered_debit(&job.id).unwrap(),
            "a settled debit can't be re-armed"
        );
        let spender2 = MockSpender::succeeding();
        drain_debits(&state, &spender2).await;
        assert_eq!(spender2.calls(), 0, "nothing left to claim — no second charge");
    }

    /// FM3: if the buyer is STILL underfunded, re-driving then re-draining simply
    /// re-dead-letters the row — one more attempt per re-drive, never an infinite
    /// auto-retry and never a panic. It stays quarantined until the next EXPLICIT
    /// re-drive (a dead-lettered row is never auto-re-claimed).
    #[tokio::test]
    async fn redrive_re_dead_letters_on_a_repeat_permanent_error() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        dead_letter_one(&state, &job).await;

        // Re-drive, then drain while the buyer is still underfunded (permanent again).
        assert!(state.store.lock().await.redrive_dead_lettered_debit(&job.id).unwrap());
        let spender = MockSpender::permanent();
        drain_debits(&state, &spender).await;
        assert_eq!(spender.calls(), 1, "one more attempt — not a hot loop");
        assert_eq!(dead_lettered_debits(&state).await, 1, "re-dead-lettered, not dropped");
        assert_eq!(pending_debits(&state).await, 0);

        // NOT auto-re-driven: a succeeding drain claims nothing until the next explicit re-drive.
        let spender2 = MockSpender::succeeding();
        drain_debits(&state, &spender2).await;
        assert_eq!(spender2.calls(), 0, "a re-dead-lettered debit is not auto-re-claimed");
        assert_eq!(dead_lettered_debits(&state).await, 1, "still quarantined");
    }

    /// Empty-queue state with BOTH a compute rate (so debits accrue) and an ingest
    /// token (so the re-drive endpoint is gated) — for the endpoint auth tests.
    async fn test_state_metered_with_token(token: &str) -> Arc<AppState> {
        let state = AppState::with_store(
            Store::open_in_memory().unwrap().with_compute_rate_wei(DRAIN_RATE),
            StoreConfig { ingest_token: Some(token.to_string()), ..test_config() },
        )
        .unwrap();
        drain_seeded_jobs(&state).await;
        state
    }

    /// `POST /debits/{id}/redrive` with an optional `Authorization` header (body-less).
    async fn post_redrive(
        state: Arc<AppState>,
        id: Uuid,
        authorization: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder()
            .method("POST")
            .uri(format!("/debits/{id}/redrive"));
        if let Some(auth) = authorization {
            builder = builder.header("authorization", auth);
        }
        router(state)
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// FM4: the re-drive endpoint requires the same bearer token as `POST /jobs`. A
    /// missing-header and a wrong-token call are both `401` and re-arm NOTHING (the
    /// debit stays dead-lettered); the correct token re-arms it (`200 {"rearmed": true}`).
    #[tokio::test]
    async fn redrive_endpoint_rejects_unauthenticated_and_accepts_the_token() {
        let state = test_state_metered_with_token(TEST_INGEST_TOKEN).await;
        let job = seed_job();
        dead_letter_one(&state, &job).await;

        // No Authorization header → 401, nothing re-armed.
        let resp = post_redrive(state.clone(), job.id, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(dead_lettered_debits(&state).await, 1, "an unauthenticated re-drive re-arms nothing");
        assert_eq!(pending_debits(&state).await, 0);

        // Wrong token → 401, still nothing re-armed.
        let resp = post_redrive(state.clone(), job.id, Some("Bearer wrong-token")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(dead_lettered_debits(&state).await, 1);

        // Correct token → 200, re-armed.
        let auth = format!("Bearer {TEST_INGEST_TOKEN}");
        let resp = post_redrive(state.clone(), job.id, Some(&auth)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], true);
        assert_eq!(dead_lettered_debits(&state).await, 0, "the authenticated re-drive re-armed it");
        assert_eq!(pending_debits(&state).await, 1);
    }

    /// The endpoint mirrors `POST /jobs`' unconfigured-open posture: with no token a
    /// re-drive needs no auth. Also pins that the no-op path (the row no longer
    /// dead-lettered) is a successful `200 {"rearmed": false}`, not an error — so an
    /// operator can replay a re-drive idempotently.
    #[tokio::test]
    async fn redrive_endpoint_open_and_idempotent_without_token() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await; // no token configured
        assert!(state.ingest_token.is_none());
        let job = seed_job();
        dead_letter_one(&state, &job).await;

        // Open: re-drive with no auth re-arms the dead-lettered debit.
        let resp = post_redrive(state.clone(), job.id, None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], true);
        assert_eq!(pending_debits(&state).await, 1);

        // Idempotent no-op: a second re-drive (now pending, not dead-lettered) is a
        // successful 200 false.
        let resp = post_redrive(state.clone(), job.id, None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], false);
    }

    // -- POST /debits/redrive-all — operator bulk re-drive of all dead-lettered debits --

    /// `POST /debits/redrive-all` with an optional `Authorization` header (body-less).
    async fn post_redrive_all(state: Arc<AppState>, authorization: Option<&str>) -> axum::response::Response {
        let mut builder = Request::builder().method("POST").uri("/debits/redrive-all");
        if let Some(auth) = authorization {
            builder = builder.header("authorization", auth);
        }
        router(state).oneshot(builder.body(Body::empty()).unwrap()).await.unwrap()
    }

    /// FM1 (over-reach): the bulk re-drive re-arms ONLY dead-lettered, unsettled rows —
    /// never a still-pending (already drainable) row and never a settled (paid) one. With
    /// two dead-lettered, one settled, and one pending row present, it re-arms exactly the
    /// two, leaves the pending row pending, and never resurrects the paid charge.
    #[tokio::test]
    async fn redrive_all_rearms_only_dead_lettered_unsettled_debits() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        // Two poison heads → dead-lettered.
        let poison_a = seed_job();
        let poison_b = seed_job();
        settle_one_metered(&state, &poison_a).await;
        settle_one_metered(&state, &poison_b).await;
        drain_debits(&state, &MockSpender::permanent()).await;
        assert_eq!(dead_lettered_debits(&state).await, 2);

        // One good debit → settled (the poisons are skipped, stay quarantined).
        let good = seed_job();
        settle_one_metered(&state, &good).await;
        drain_debits(&state, &MockSpender::succeeding()).await;
        assert_eq!(dead_lettered_debits(&state).await, 2, "the poisons stay quarantined");

        // One fresh debit → still pending (never drained).
        let fresh = seed_job();
        settle_one_metered(&state, &fresh).await;
        assert_eq!(pending_debits(&state).await, 1, "fresh is drainable");

        // Bulk re-drive: only the two dead-lettered rows match.
        let rearmed = state.store.lock().await.redrive_all_dead_lettered_debits().unwrap();
        assert_eq!(rearmed, 2, "exactly the two dead-lettered rows, not the pending or settled one");
        assert_eq!(dead_lettered_debits(&state).await, 0, "both poisons re-armed");
        assert_eq!(pending_debits(&state).await, 3, "the two re-armed poisons joined the fresh pending row");

        // The settled `good` was untouched: a succeeding drain settles exactly the 3
        // pending rows and never re-charges the paid one.
        let good_hex = eas::job_id_hex(&good.id);
        let spender = MockSpender::succeeding();
        drain_debits(&state, &spender).await;
        assert_eq!(spender.calls(), 3, "the 3 pending rows, not the settled good");
        assert!(!spender.spent().contains(&good_hex), "the paid charge is never re-driven");
    }

    /// FM2 (auth): the bulk re-drive is a privileged mass-recovery action — a missing,
    /// blank, or wrong bearer token is `401` and re-arms NOTHING; the correct token
    /// re-arms the whole set (`200 {"rearmed": 2}`).
    #[tokio::test]
    async fn redrive_all_endpoint_rejects_unauthenticated_and_accepts_the_token() {
        let state = test_state_metered_with_token(TEST_INGEST_TOKEN).await;
        let a = seed_job();
        let b = seed_job();
        settle_one_metered(&state, &a).await;
        settle_one_metered(&state, &b).await;
        drain_debits(&state, &MockSpender::permanent()).await;
        assert_eq!(dead_lettered_debits(&state).await, 2);

        // Missing / blank / wrong token → 401, nothing re-armed.
        assert_eq!(post_redrive_all(state.clone(), None).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(post_redrive_all(state.clone(), Some("Bearer ")).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(post_redrive_all(state.clone(), Some("Bearer wrong-token")).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(dead_lettered_debits(&state).await, 2, "every unauthenticated bulk re-drive re-armed nothing");

        // Correct token → 200 {"rearmed": 2}.
        let auth = format!("Bearer {TEST_INGEST_TOKEN}");
        let resp = post_redrive_all(state.clone(), Some(&auth)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], 2);
        assert_eq!(dead_lettered_debits(&state).await, 0, "the authenticated bulk re-drive re-armed both");
        assert_eq!(pending_debits(&state).await, 2);
    }

    /// FM3 (no double-charge): every bulk-re-armed row settles AT MOST ONCE under the
    /// next drain, and a second bulk re-drive after they settle charges nothing — the
    /// off-chain `tx_hash IS NULL` guard plus the on-chain `spendOnce` fence hold across
    /// the whole re-armed set.
    #[tokio::test]
    async fn redrive_all_then_drain_settles_each_re_armed_debit_exactly_once() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let jobs: Vec<JobSpec> = (0..3).map(|_| seed_job()).collect();
        for j in &jobs {
            settle_one_metered(&state, j).await;
        }
        drain_debits(&state, &MockSpender::permanent()).await;
        assert_eq!(dead_lettered_debits(&state).await, 3);

        // Bulk re-drive after the broad cause is fixed, then drain once.
        assert_eq!(state.store.lock().await.redrive_all_dead_lettered_debits().unwrap(), 3);
        assert_eq!(pending_debits(&state).await, 3);
        let spender = MockSpender::succeeding();
        drain_debits(&state, &spender).await;
        assert_eq!(spender.calls(), 3);
        let mut spent = spender.spent();
        spent.sort();
        let mut want: Vec<String> = jobs.iter().map(|j| eas::job_id_hex(&j.id)).collect();
        want.sort();
        assert_eq!(spent, want, "each re-armed debit settles exactly once");
        assert_eq!(pending_debits(&state).await, 0);
        assert_eq!(dead_lettered_debits(&state).await, 0);

        // A second bulk re-drive once settled re-arms nothing, and a further drain charges nothing.
        assert_eq!(state.store.lock().await.redrive_all_dead_lettered_debits().unwrap(), 0, "all settled — nothing to re-arm");
        let spender2 = MockSpender::succeeding();
        drain_debits(&state, &spender2).await;
        assert_eq!(spender2.calls(), 0, "no second charge on a double bulk call");
    }

    /// FM4 (zero / still-failing): a bulk re-drive with no dead-lettered rows is a clean
    /// `rearmed: 0` (not an error); and if the broad cause is NOT actually fixed the
    /// re-armed rows simply re-dead-letter on the next drain — one attempt each, never a
    /// hot loop, and not auto-re-claimed afterward.
    #[tokio::test]
    async fn redrive_all_is_zero_when_empty_and_re_dead_letters_if_still_failing() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;

        // Zero case: nothing dead-lettered → rearmed 0, no error.
        assert_eq!(state.store.lock().await.redrive_all_dead_lettered_debits().unwrap(), 0);

        // Two dead-lettered rows.
        let a = seed_job();
        let b = seed_job();
        settle_one_metered(&state, &a).await;
        settle_one_metered(&state, &b).await;
        drain_debits(&state, &MockSpender::permanent()).await;
        assert_eq!(dead_lettered_debits(&state).await, 2);

        // Bulk re-drive, but the cause is still present → both re-dead-letter (one attempt each).
        assert_eq!(state.store.lock().await.redrive_all_dead_lettered_debits().unwrap(), 2);
        let spender = MockSpender::permanent();
        drain_debits(&state, &spender).await;
        assert_eq!(spender.calls(), 2, "one more attempt per row — not a hot loop");
        assert_eq!(dead_lettered_debits(&state).await, 2, "re-dead-lettered, not dropped");
        assert_eq!(pending_debits(&state).await, 0);

        // Not auto-re-claimed: a succeeding drain claims nothing until the next explicit bulk re-drive.
        let spender2 = MockSpender::succeeding();
        drain_debits(&state, &spender2).await;
        assert_eq!(spender2.calls(), 0, "a re-dead-lettered set is not auto-re-claimed");
        assert_eq!(dead_lettered_debits(&state).await, 2);
    }

    /// The bulk endpoint mirrors `POST /jobs`' unconfigured-open posture: with no token a
    /// bulk re-drive needs no auth, and the empty no-op is a clean `200 {"rearmed": 0}`.
    #[tokio::test]
    async fn redrive_all_endpoint_open_and_idempotent_without_token() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await; // no token configured
        assert!(state.ingest_token.is_none());
        let job = seed_job();
        dead_letter_one(&state, &job).await;

        // Open: a bulk re-drive with no auth re-arms the dead-lettered debit.
        let resp = post_redrive_all(state.clone(), None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], 1);
        assert_eq!(pending_debits(&state).await, 1);

        // Idempotent: a second bulk re-drive (nothing dead-lettered now) is 200 {"rearmed": 0}.
        let resp = post_redrive_all(state.clone(), None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["rearmed"], 0);
    }

    // -- GET /debits/dead-lettered — operator listing of quarantined debits --------

    /// A GET with an optional `Authorization` header.
    async fn get_auth(
        state: Arc<AppState>,
        uri: &str,
        authorization: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder().uri(uri);
        if let Some(auth) = authorization {
            builder = builder.header("authorization", auth);
        }
        router(state).oneshot(builder.body(Body::empty()).unwrap()).await.unwrap()
    }

    /// FM1: the listing returns ONLY dead-lettered, not-yet-settled debits — a
    /// still-pending row (drainable) and an already-settled row (paid, tx_hash set)
    /// are both excluded, so every listed debit is genuinely re-armable.
    #[tokio::test]
    async fn dead_lettered_listing_returns_only_dead_lettered_unsettled_debits() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let poison = seed_job();
        let good = seed_job();
        let pending = seed_job();
        settle_one_metered(&state, &poison).await; // settled first → claimed first (the head)
        settle_one_metered(&state, &good).await;
        // poison dead-letters, good settles in the same pass.
        drain_debits(&state, &MockSpender::permanent_for(eas::job_id_hex(&poison.id))).await;
        // A third debit left pending (settled-metered, never drained).
        settle_one_metered(&state, &pending).await;

        let listing = body_json(get(state.clone(), "/debits/dead-lettered").await).await;
        let debits = listing["debits"].as_array().unwrap();
        assert_eq!(debits.len(), 1, "only the dead-lettered debit is listed");
        assert_eq!(debits[0]["job_id"], poison.id.to_string(), "the dead-lettered one (not good/pending)");
        assert_eq!(listing["total"], 1);
        assert_eq!(listing["truncated"], false);
    }

    /// FM2: the listing exposes buyer addresses + owed amounts, so it requires the same
    /// bearer token as POST /jobs — a missing or wrong token is 401 and lists nothing;
    /// the correct token returns the dead-lettered debit.
    #[tokio::test]
    async fn dead_lettered_listing_requires_the_ingest_token() {
        let state = test_state_metered_with_token(TEST_INGEST_TOKEN).await;
        let job = seed_job();
        dead_letter_one(&state, &job).await;

        assert_eq!(
            get(state.clone(), "/debits/dead-lettered").await.status(),
            StatusCode::UNAUTHORIZED,
            "no Authorization header → 401"
        );
        assert_eq!(
            get_auth(state.clone(), "/debits/dead-lettered", Some("Bearer wrong-token")).await.status(),
            StatusCode::UNAUTHORIZED,
            "wrong token → 401"
        );

        let auth = format!("Bearer {TEST_INGEST_TOKEN}");
        let resp = get_auth(state.clone(), "/debits/dead-lettered", Some(&auth)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let listing = body_json(resp).await;
        assert_eq!(listing["debits"].as_array().unwrap().len(), 1, "the authenticated listing shows the debit");
        assert_eq!(listing["debits"][0]["job_id"], job.id.to_string());
    }

    /// FM3: the store listing is capped and oldest-first — a limit smaller than the
    /// dead-letter backlog returns the OLDEST `limit` rows in insertion order, and the
    /// full count exceeds the page (the `total > len` the endpoint reports as
    /// `truncated`, so a capped page is never mistaken for the whole set).
    #[tokio::test]
    async fn list_dead_lettered_debits_caps_and_orders_oldest_first() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let jobs = [seed_job(), seed_job(), seed_job()];
        for job in &jobs {
            settle_one_metered(&state, job).await;
            drain_debits(&state, &MockSpender::permanent()).await; // dead-letter each in turn
        }
        let store = state.store.lock().await;
        // Capped: a limit of 2 over 3 dead-lettered returns the 2 OLDEST, in order.
        let page = store.list_dead_lettered_debits(2).unwrap();
        assert_eq!(page.iter().map(|d| d.0).collect::<Vec<_>>(), vec![jobs[0].id, jobs[1].id], "oldest two, in order");
        // total exceeds the page → the endpoint's `truncated` signal (total > len).
        assert_eq!(store.dead_lettered_debit_count().unwrap(), 3);
        // Uncapped (limit >= total) returns all three, oldest-first.
        let all = store.list_dead_lettered_debits(10).unwrap();
        assert_eq!(
            all.iter().map(|d| d.0).collect::<Vec<_>>(),
            jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
            "all three, oldest-first"
        );
    }

    /// FM4: the listed `amount_wei` is the exact decimal-wei string persisted at settle
    /// (never re-derived or coerced to a number), alongside the buyer and a present
    /// quarantine stamp — so an operator sees exactly what is owed.
    #[tokio::test]
    async fn dead_lettered_listing_carries_the_exact_persisted_fields() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        dead_letter_one(&state, &job).await;

        let listing = body_json(get(state.clone(), "/debits/dead-lettered").await).await;
        let d = &listing["debits"][0];
        assert_eq!(d["job_id"], job.id.to_string());
        assert_eq!(d["amount_wei"], "1000000000000", "DRAIN_RATE * 1, the persisted decimal string, exact");
        assert_eq!(d["buyer"], TEST_BUYER);
        assert!(d["dead_lettered_at"].as_i64().unwrap() > 0, "the quarantine stamp is present");
    }

    /// FM2: the drain must not hold the store mutex across the slow on-chain spend,
    /// or every settle/stats stalls behind RPC latency. The gated spender holds the
    /// spend in-flight while we prove the store lock is still acquirable.
    #[tokio::test]
    async fn drain_debits_holds_no_store_lock_across_the_spend() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let job = seed_job();
        settle_one_metered(&state, &job).await;

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let spender = MockSpender::gated(started.clone(), release.clone());

        let drive = {
            let state = state.clone();
            tokio::spawn(async move { drain_debits(&state, &spender).await })
        };

        // The spend is now in-flight (claimed, lock dropped, awaiting release).
        started.notified().await;

        // If the drain held the lock across the await this deadlocks; the timeout
        // turns that regression into a failure instead of a hang.
        tokio::time::timeout(Duration::from_secs(5), async {
            assert_eq!(pending_debits(&state).await, 1, "claimed but not yet marked");
        })
        .await
        .expect("store lock free during the in-flight spend");

        release.notify_one();
        drive.await.unwrap();
        assert_eq!(pending_debits(&state).await, 0, "debit marked once the spend returns");
    }

    #[tokio::test]
    async fn stats_pending_debits_drains_after_spender() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        for job in [seed_job(), seed_job()] {
            settle_one_metered(&state, &job).await;
        }
        let before = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(before["pending_debits"], 2);

        drain_debits(&state, &MockSpender::succeeding()).await;

        let after = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(after["pending_debits"], 0, "the backlog drains as debits land");
    }

    // ---- websocket dispatch integration tests ----

    use futures_util::{SinkExt, StreamExt};
    use proto::CoordinatorMsg;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// Bind the router on an ephemeral port and serve it on a spawned task.
    /// Returns the bound `host:port` so tests can build ws/http URLs. Routes
    /// through the real `serve()` (not `axum::serve`) so every ws integration test
    /// exercises the h1 accept loop + the 101-upgrade path; a generous header
    /// timeout + a never-resolving shutdown match production-idle serving.
    async fn serve_ephemeral(state: Arc<AppState>) -> String {
        serve_ephemeral_cfg(
            state,
            DEFAULT_HTTP_HEADER_TIMEOUT,
            DEFAULT_HTTP_BODY_TIMEOUT,
            DEFAULT_MAX_CONNECTIONS,
        )
        .await
    }

    /// `serve_ephemeral` with a caller-chosen header-read timeout, for the
    /// slow-headers test that needs a sub-second bound.
    async fn serve_ephemeral_with_header_timeout(
        state: Arc<AppState>,
        header_read_timeout: Duration,
    ) -> String {
        serve_ephemeral_cfg(
            state,
            header_read_timeout,
            DEFAULT_HTTP_BODY_TIMEOUT,
            DEFAULT_MAX_CONNECTIONS,
        )
        .await
    }

    /// `serve_ephemeral` with a caller-chosen request-body timeout, for the
    /// slow-body test that needs a sub-second bound on the POST routes.
    async fn serve_ephemeral_with_body_timeout(
        state: Arc<AppState>,
        body_read_timeout: Duration,
    ) -> String {
        serve_ephemeral_cfg(
            state,
            DEFAULT_HTTP_HEADER_TIMEOUT,
            body_read_timeout,
            DEFAULT_MAX_CONNECTIONS,
        )
        .await
    }

    /// `serve_ephemeral` with a caller-chosen connection cap, for the
    /// connection-flood test that needs a tiny cap.
    async fn serve_ephemeral_with_max_connections(
        state: Arc<AppState>,
        max_connections: usize,
    ) -> String {
        serve_ephemeral_cfg(
            state,
            DEFAULT_HTTP_HEADER_TIMEOUT,
            DEFAULT_HTTP_BODY_TIMEOUT,
            max_connections,
        )
        .await
    }

    /// Bind an ephemeral port, build the router at `body_read_timeout`, and serve
    /// it through the real `serve()` accept loop at `header_read_timeout` /
    /// `max_connections` until the test drops. The shared backing for the slowloris
    /// and connection-cap tests, which drive the bound under test down to a small
    /// value.
    async fn serve_ephemeral_cfg(
        state: Arc<AppState>,
        header_read_timeout: Duration,
        body_read_timeout: Duration,
        max_connections: usize,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router_with_body_timeout(state, body_read_timeout);
        tokio::spawn(async move {
            serve(listener, app, header_read_timeout, max_connections, std::future::pending::<()>())
                .await
                .unwrap();
        });
        addr.to_string()
    }

    fn ws_hello(nonce: &[u8]) -> EarnerMsg {
        let sk = dev_signing_key();
        signed_hello_with_nonce(
            &sk,
            &address_from_signing_key(&sk),
            "RTX 4090",
            24,
            vec![
                JobKind::Terrain,
                JobKind::Foliage,
                JobKind::NpcTick,
                JobKind::DiffusionTile,
                JobKind::Optimization,
            ],
            nonce,
        )
    }

    /// A ws read stream item — `next()` yields this from either the whole socket
    /// or a split read-half.
    type WsItem = Result<WsMessage, tokio_tungstenite::tungstenite::Error>;

    /// Read the coordinator's opening `Challenge` frame and return its nonce
    /// bytes. The coordinator now challenges on connect, so every ws test reads
    /// this before sending its Hello (and folds the nonce into the signature).
    /// Generic over the read stream so the handshake-timeout tests can drive a
    /// `split()` socket's read-half (pinging on the other half) through it.
    async fn recv_challenge<S: futures_util::Stream<Item = WsItem> + Unpin>(
        ws: &mut S,
    ) -> Vec<u8> {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(t))) => {
                    match serde_json::from_str::<CoordinatorMsg>(&t).unwrap() {
                        CoordinatorMsg::Challenge { nonce } => return hex::decode(nonce).unwrap(),
                        other => panic!("expected Challenge as first frame, got {other:?}"),
                    }
                }
                Some(Ok(_)) => continue, // ping/pong before the challenge
                other => panic!("expected a Challenge frame, got {other:?}"),
            }
        }
    }

    /// Assert the server closed the ws without first sending a frame — what a
    /// rejected `Hello` (or an elapsed handshake timeout) produces (`recv_hello`
    /// returns `None`, the handler returns, axum closes the socket).
    async fn expect_ws_closed<S: futures_util::Stream<Item = WsItem> + Unpin>(ws: &mut S) {
        loop {
            match ws.next().await {
                None | Some(Ok(WsMessage::Close(_))) | Some(Err(_)) => return,
                Some(Ok(WsMessage::Text(t))) => panic!("expected ws close, got frame: {t}"),
                Some(Ok(_)) => continue, // ping/pong before the close
            }
        }
    }

    /// Each malformed `Hello` on the ws path closes the socket and registers
    /// nothing — the same gate as `/register`, applied before the offer loop, so
    /// the ws transport can't pollute the registry the http path guards (FM2).
    #[tokio::test]
    async fn ws_rejects_malformed_hello_and_registers_nothing() {
        let state = test_state_empty().await;
        let addr = serve_ephemeral(state.clone()).await;
        let malformed = [
            hello_claiming("0xabc", 24, vec![JobKind::Terrain]), // short address
            hello("wsgood", 24, vec![]),                         // advertises no kinds
            hello("wsgood", 0, vec![JobKind::Terrain]),          // zero vram
        ];
        for m in &malformed {
            let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();
            recv_challenge(&mut ws).await; // structural reject fires before the nonce check
            ws.send(WsMessage::text(serde_json::to_string(m).unwrap()))
                .await
                .unwrap();
            expect_ws_closed(&mut ws).await;
        }
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            0,
            "no malformed Hello may register via ws",
        );
    }

    /// An inbound ws frame past `MAX_REQUEST_BODY_BYTES` is refused at the protocol
    /// layer (tungstenite capacity error → closed socket) BEFORE `recv_hello_inner` can
    /// UTF-8-validate, JSON-parse, or rate-check it. Without the `ws_handler` size cap
    /// the path inherits tungstenite's 64 MiB default, so the per-source rate check
    /// (which only fires once a Hello is decoded) would sit after an unbounded parse —
    /// the ws Hello amplifying the flood the limiter sheds, where HTTP `/register` is
    /// already `DefaultBodyLimit`-bounded.
    ///
    /// Discriminating: the frame is an OTHERWISE-VALID signed Hello, padded past the cap
    /// with an unknown field (`EarnerMsg` is `tag = "type"` without `deny_unknown_fields`,
    /// so serde ignores the pad, and the signature commits only to the real fields — an
    /// unpadded copy registers). With the cap the frame never parses, so `gpus_joined`
    /// stays 0; remove the cap and the 33 KiB frame is buffered, parsed, and REGISTERS
    /// (the server then holds the session open rather than closing) — so the close
    /// itself, time-bounded here, fails. Either signal flips on regression.
    #[tokio::test]
    async fn ws_oversized_frame_closes_before_parse_and_registers_nothing() {
        let state = test_state_empty().await;
        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let nonce = recv_challenge(&mut ws).await;
        let sk = test_signing_key("wsbig");
        let hello = signed_hello_with_nonce(
            &sk,
            &address_from_signing_key(&sk),
            "RTX 4090",
            24,
            vec![JobKind::Terrain],
            &nonce,
        );
        let mut obj = serde_json::to_value(&hello).unwrap();
        obj.as_object_mut().unwrap().insert(
            "pad".into(),
            serde_json::Value::String("x".repeat(MAX_REQUEST_BODY_BYTES)),
        );
        let wire = serde_json::to_string(&obj).unwrap();
        assert!(wire.len() > MAX_REQUEST_BODY_BYTES, "frame must exceed the inbound cap");
        ws.send(WsMessage::text(wire)).await.unwrap();
        // Time-bound the close: without the cap the Hello registers and the server keeps
        // the socket open, so this would hang — turn that regression into a clean fail.
        tokio::time::timeout(Duration::from_secs(5), expect_ws_closed(&mut ws))
            .await
            .expect("oversized frame must close the socket, not register");
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            0,
            "an oversized ws frame registered nothing",
        );
    }

    /// The same depth bounds (oversized gpu_model, duplicate kinds, over-ceiling
    /// vram) close the ws socket and register nothing — the shared validator gates
    /// both transports, so ws can't pollute the registry http guards.
    #[tokio::test]
    async fn ws_rejects_oversized_gpu_dup_kinds_and_huge_vram() {
        let state = test_state_empty().await;
        let addr = serve_ephemeral(state.clone()).await;
        let long_gpu = "x".repeat(MAX_GPU_MODEL_LEN + 1);
        let malformed = [
            hello_gpu("wsdepth", &long_gpu, 24, vec![JobKind::Terrain]),
            hello("wsdepth", 24, vec![JobKind::Terrain, JobKind::Terrain]),
            hello("wsdepth", MAX_VRAM_GB + 1, vec![JobKind::Terrain]),
        ];
        for m in &malformed {
            let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();
            recv_challenge(&mut ws).await; // structural reject fires before the nonce check
            ws.send(WsMessage::text(serde_json::to_string(m).unwrap()))
                .await
                .unwrap();
            expect_ws_closed(&mut ws).await;
        }
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            0,
            "no out-of-bounds Hello may register via ws",
        );
    }

    /// A forged Hello (claims a victim address, signed by another key) closes the
    /// ws socket and registers nothing — the same key-possession gate as
    /// `/register`, applied on the ws path before the offer loop, so neither
    /// transport can spoof an identity onto the registry (FM2).
    #[tokio::test]
    async fn ws_rejects_forged_hello_signature() {
        let state = test_state_empty().await;
        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let nonce = recv_challenge(&mut ws).await;
        // Forge over the issued challenge with the wrong key — rejects on signature.
        let forged = signed_hello_with_nonce(
            &test_signing_key("attacker"),
            &test_address("victim"),
            "RTX 4090",
            24,
            vec![JobKind::Terrain],
            &nonce,
        );
        ws.send(WsMessage::text(serde_json::to_string(&forged).unwrap()))
            .await
            .unwrap();
        expect_ws_closed(&mut ws).await;
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            0,
            "a forged Hello must not register via ws",
        );
    }

    /// Poll `/stats` until `gpus_joined` reaches `want`. Serializes the ws rate-limit
    /// test: the rate check precedes admission, so a registered earner means its token
    /// is already consumed — making the next connection deterministically the N+1-th
    /// to draw on the shared source bucket (the three ws connections race otherwise).
    async fn wait_for_gpus_joined(state: &Arc<AppState>, want: u64) {
        for _ in 0..200 {
            if body_json(get(state.clone(), "/stats").await).await["gpus_joined"] == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("gpus_joined never reached {want}");
    }

    /// WS Hello path parity with HTTP `/register`: a source over its per-source
    /// allowance is shed (socket closed) BEFORE the signature verify. All loopback
    /// connections share one source bucket; registrations are serialized via `/stats`
    /// so the 3rd Hello at a cap of 2 is deterministically the rejected one, and it
    /// registers nothing.
    #[tokio::test]
    async fn ws_over_rate_limit_closes_and_registers_nothing() {
        let state = test_state_empty_with_registrations(2).await;
        let addr = serve_ephemeral(state.clone()).await;
        let mut keep_open = Vec::new();
        for (i, label) in ["wsra", "wsrb"].iter().enumerate() {
            let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();
            let nonce = recv_challenge(&mut ws).await;
            let sk = test_signing_key(label);
            let hello = signed_hello_with_nonce(
                &sk,
                &address_from_signing_key(&sk),
                "RTX 4090",
                24,
                vec![JobKind::Terrain],
                &nonce,
            );
            ws.send(WsMessage::text(serde_json::to_string(&hello).unwrap()))
                .await
                .unwrap();
            // Block until this earner is registered → its token is consumed before the
            // next connection sends, so the bucket is provably empty by the 3rd Hello.
            wait_for_gpus_joined(&state, (i + 1) as u64).await;
            keep_open.push(ws); // hold the live session so the earner stays in /stats
        }
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let nonce = recv_challenge(&mut ws).await;
        let sk = test_signing_key("wsrc");
        let hello = signed_hello_with_nonce(
            &sk,
            &address_from_signing_key(&sk),
            "RTX 4090",
            24,
            vec![JobKind::Terrain],
            &nonce,
        );
        ws.send(WsMessage::text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();
        expect_ws_closed(&mut ws).await;
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            2,
            "the rate-limited ws Hello registered nothing",
        );
    }

    /// FM2 (cheap-reject-first) on the WS path, pinned DISCRIMINATINGLY: the rate
    /// check precedes `validate_hello`, so a structurally-valid Hello whose signature
    /// fails recovery still SPENDS its token when it is under the limit — the check ran
    /// and consumed before the verify rejected it. With a cap of 1, that malformed
    /// Hello drains the loopback source's only token, so a following VALID Hello from
    /// the same source is rate-shed and registers nothing (`gpus_joined == 0`). Move the
    /// ws rate-check block AFTER `validate_hello` and the malformed Hello is rejected at
    /// recovery BEFORE the check, leaving the token intact — the valid Hello then
    /// registers (`gpus_joined == 1`) and the server holds its session open, so the
    /// second `expect_ws_closed` also hangs. Either signal flips on the reorder, where
    /// `ws_over_rate_limit_*` (valid Hellos only) survives it. The HTTP analogue
    /// (`register_rate_limit_precedes_validation`) discriminates via 429-vs-400; WS has
    /// no status code, so the spent-token side effect is the discriminator.
    #[tokio::test]
    async fn ws_rate_limit_precedes_validation() {
        let state = test_state_empty_with_registrations(1).await;
        let addr = serve_ephemeral(state.clone()).await;
        // A real key + its own valid address, so every structural gate in
        // `validate_hello` passes and the Hello REACHES the secp256k1 recovery; signing
        // over a nonce that is NOT this connection's challenge makes that recovery fail
        // (address mismatch), so the socket closes either way — the only observable
        // difference is whether the rate check spent the token first.
        let sk = test_signing_key("wsorder");
        let claimed = address_from_signing_key(&sk);
        {
            let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();
            let _challenge = recv_challenge(&mut ws).await; // deliberately not signed over
            let malformed = signed_hello_with_nonce(
                &sk,
                &claimed,
                "RTX 4090",
                24,
                vec![JobKind::Terrain],
                b"not-this-connections-challenge",
            );
            ws.send(WsMessage::text(serde_json::to_string(&malformed).unwrap()))
                .await
                .unwrap();
            // The close is observable only after recv_hello_inner returns None, which
            // (cheap-reject-first) is AFTER the rate check consumed the token — so this
            // barrier guarantees the bucket is drained before the valid Hello below.
            expect_ws_closed(&mut ws).await;
        }
        {
            let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();
            let nonce = recv_challenge(&mut ws).await;
            let valid = signed_hello_with_nonce(
                &sk,
                &claimed,
                "RTX 4090",
                24,
                vec![JobKind::Terrain],
                &nonce,
            );
            ws.send(WsMessage::text(serde_json::to_string(&valid).unwrap()))
                .await
                .unwrap();
            // Correct order: shed → prompt close. A reorder regression registers this
            // Hello and holds the session open, so this would hang — time-bound it to
            // turn that regression into a clean failure instead of a stuck test.
            tokio::time::timeout(Duration::from_secs(5), expect_ws_closed(&mut ws))
                .await
                .expect("the valid Hello must be rate-shed: the malformed under-limit Hello spent the token");
        }
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            0,
            "cheap-reject-first holds on WS: the malformed under-limit Hello spent the source's token, so the valid Hello was shed",
        );
    }

    /// Connect a ws client carrying `xff` as `X-Forwarded-For`, complete the
    /// challenge, and send a valid Hello (signed by `seed`'s key) advertising
    /// `kind`. Returns true if the earner registered — observed by the coordinator
    /// offering it a job — and false if the Hello was rate-shed (the socket closes
    /// with no offer). The `X-Forwarded-For` header is exactly what `ws_handler`
    /// feeds `resolve_source_ip`, so this drives the ws source resolution end-to-end.
    async fn ws_register_xff(addr: &str, xff: &str, seed: &str, kind: JobKind) -> bool {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
        req.headers_mut().insert("x-forwarded-for", xff.parse().unwrap());
        let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        let nonce = recv_challenge(&mut ws).await;
        let sk = test_signing_key(seed);
        let hello = signed_hello_with_nonce(
            &sk,
            &address_from_signing_key(&sk),
            "RTX 4090",
            24,
            vec![kind],
            &nonce,
        );
        ws.send(WsMessage::text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(t))) => {
                    match serde_json::from_str::<CoordinatorMsg>(&t).unwrap() {
                        CoordinatorMsg::JobOffer(_) => return true,
                        other => panic!("expected a JobOffer or close after Hello, got {other:?}"),
                    }
                }
                None | Some(Ok(WsMessage::Close(_))) | Some(Err(_)) => return false,
                Some(Ok(_)) => continue, // ping/pong before the offer
            }
        }
    }

    /// The WS Hello path keys the per-source registration limiter on the SAME
    /// trusted-proxy-resolved source the HTTP `/register` path does. This pins the
    /// `ws_handler` wiring (`peer + headers -> resolve_source_ip -> ws_session ->
    /// recv_hello`) that neither the `resolve_source_ip` unit tests nor the
    /// `ws_session`/`recv_hello` helpers (which take an already-resolved `source`)
    /// can reach: every other ws test connects over loopback with the default empty
    /// allowlist, so the ws resolve only ever hits the untrusted-peer early return.
    /// Here the loopback peer IS the trusted proxy, and two clients arrive behind it
    /// under distinct `X-Forwarded-For` values. With a per-source allowance of 1, the
    /// first client spends its bucket and its second attempt is shed — but a client
    /// with a different XFF draws on its OWN bucket and registers. Were `ws_handler`
    /// keying on the raw peer (`127.0.0.1`) instead of the resolved source, all three
    /// would share one bucket and the third would be shed too. The three connections
    /// are awaited in sequence so each outcome is observed before the next begins
    /// (no inter-connection race on the shared bucket).
    #[tokio::test]
    async fn ws_trusted_proxy_separates_xff_clients() {
        let loopback = IpAddr::from([127, 0, 0, 1]);
        let state = test_state_registrations_trusted(1, trusted(&[loopback])).await;
        // One job per client's advertised kind, so each registered earner is offered
        // a job (the "registered" signal) without contending for the same queue entry.
        enqueue(&state, &job_of(JobKind::Terrain)).await;
        enqueue(&state, &job_of(JobKind::Foliage)).await;
        let addr = serve_ephemeral(state.clone()).await;

        assert!(
            ws_register_xff(&addr, "203.0.113.10", "wsxff-a", JobKind::Terrain).await,
            "first client behind the trusted proxy must register",
        );
        assert!(
            !ws_register_xff(&addr, "203.0.113.10", "wsxff-a2", JobKind::Terrain).await,
            "the same XFF client's second registration must be rate-shed",
        );
        assert!(
            ws_register_xff(&addr, "203.0.113.20", "wsxff-b", JobKind::Foliage).await,
            "a distinct XFF client gets its own bucket and registers — a raw-peer key would shed it",
        );
    }

    /// THE anti-replay property: a valid Hello captured off the wire and replayed
    /// on a FRESH connection is rejected. The new connection issues a different
    /// challenge, so the captured signature (bound to the first nonce) fails
    /// recovery and the socket closes — the replayer never bootstraps a session
    /// under the victim's address. Discriminating: the only thing wrong with the
    /// replayed Hello is the challenge it was signed over (re-signing over the
    /// second challenge would register — that is the happy path other ws tests
    /// already cover).
    #[tokio::test]
    async fn ws_replayed_hello_with_stale_challenge_is_rejected() {
        let state = test_state_empty().await;
        let addr = serve_ephemeral(state.clone()).await;

        // Connection #1: capture the challenge + the valid Hello bound to it, then
        // abandon the connection without completing registration.
        let (mut ws1, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let nonce1 = recv_challenge(&mut ws1).await;
        let captured = ws_hello(&nonce1);
        drop(ws1);

        // Connection #2 gets a fresh, different challenge; replay the captured Hello.
        let (mut ws2, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let nonce2 = recv_challenge(&mut ws2).await;
        assert_ne!(nonce1, nonce2, "each connection must get a fresh challenge");
        ws2.send(WsMessage::text(serde_json::to_string(&captured).unwrap()))
            .await
            .unwrap();
        expect_ws_closed(&mut ws2).await;
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            0,
            "a Hello replayed against a fresh challenge must not register",
        );
    }

    /// A Hello signed over a nonce the coordinator never issued is rejected: the
    /// coordinator verifies the signature against the challenge IT chose, never a
    /// nonce supplied on the wire, so an attacker can't pick its own freshness.
    #[tokio::test]
    async fn ws_hello_over_unissued_nonce_is_rejected() {
        let state = test_state_empty().await;
        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let issued = recv_challenge(&mut ws).await;
        // Sign over a nonce of our own invention, distinct from the issued one.
        let invented = b"a-nonce-the-coordinator-never-issued".to_vec();
        assert_ne!(invented, issued);
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&invented)).unwrap()))
            .await
            .unwrap();
        expect_ws_closed(&mut ws).await;
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            0,
            "a Hello over an unissued nonce must not register",
        );
    }

    /// THE slowloris bound: a client that upgrades, reads the challenge, and never
    /// sends a Hello is closed by the coordinator within the handshake timeout — it
    /// can't park a live `ws_session` task + FD forever. Bounded by an outer test
    /// timeout so a regression (the old unbounded `recv_hello` that blocks on
    /// `socket.recv()`) fails the suite instead of hanging it. Discriminating:
    /// without the fix this connection never closes.
    #[tokio::test]
    async fn ws_handshake_times_out_when_no_hello_sent() {
        let state = test_state_empty_handshake(Duration::from_millis(200)).await;
        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        recv_challenge(&mut ws).await; // read the challenge, then send nothing
        tokio::time::timeout(Duration::from_secs(5), expect_ws_closed(&mut ws))
            .await
            .expect("coordinator closed the silent handshake within the timeout");
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            0,
            "a timed-out handshake registers nothing",
        );
    }

    /// FM2: the bound is a SINGLE wall-clock deadline over the whole handshake, not
    /// reset per frame — a client that drip-feeds pings (each landing on the
    /// pre-Hello `_ => continue` arm) faster than the timeout cannot keep the
    /// handshake loop alive indefinitely. Pinging continuously every 50ms (< the
    /// 200ms bound), the connection must still close within a generous multiple of
    /// the bound; a per-frame-reset implementation would never close while the
    /// pings keep coming, so the outer assert-timeout fires and fails the test.
    #[tokio::test]
    async fn ws_handshake_deadline_is_not_reset_by_frames() {
        let timeout = Duration::from_millis(200);
        let state = test_state_empty_handshake(timeout).await;
        let addr = serve_ephemeral(state.clone()).await;
        let (ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let (mut tx, mut rx) = ws.split();
        recv_challenge(&mut rx).await;
        let pinger = tokio::spawn(async move {
            loop {
                if tx.send(WsMessage::Ping(Vec::new())).await.is_err() {
                    break; // coordinator closed the socket — stop pinging
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        tokio::time::timeout(timeout * 10, expect_ws_closed(&mut rx))
            .await
            .expect("the single handshake deadline fired despite the ping flood");
        pinger.abort();
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            0,
            "the ping-flooded handshake registers nothing",
        );
    }

    /// FM4: the timeout fail-path is connection-scoped — it has no global side
    /// effect on the registry, so a separately-registered earner survives another
    /// connection's timed-out handshake. Catches a regression test #1 cannot (a
    /// handler that clobbered the whole map would still leave an *empty* registry
    /// empty there, but would evict `liveearner` here). The cancel-safety of the
    /// sole insert itself is structural, not exercised here: a silent client never
    /// reaches the insert (it dies at `socket.recv()`), and the only mutation is a
    /// single await-free statement after the lock acquire, so the timeout can't
    /// fire between lock-acquire and insert-complete.
    #[tokio::test]
    async fn ws_handshake_timeout_does_not_evict_a_registered_earner() {
        let state = test_state_empty_handshake(Duration::from_millis(200)).await;
        state.earners.lock().await.insert(
            test_address("liveearner"),
            EarnerInfo {
                gpu_model: "RTX 4090".into(),
                vram_gb: 24,
                supported: vec![JobKind::Terrain],
                last_seen: now_secs(),
            },
        );
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            1,
            "the live earner is registered up front",
        );
        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        recv_challenge(&mut ws).await; // read the challenge, send nothing -> times out
        tokio::time::timeout(Duration::from_secs(5), expect_ws_closed(&mut ws))
            .await
            .expect("the silent handshake closed");
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            1,
            "a timed-out handshake must not evict the live earner",
        );
    }

    /// THE post-Hello slowloris bound — the established-session twin of
    /// `ws_handshake_times_out_when_no_hello_sent`. An earner that completes Hello
    /// then goes silent (sends no application frame AND never answers the
    /// coordinator's keepalive ping — a vanished/half-open peer) is closed within
    /// the read-idle deadline, so it can't park a `ws_session` task + FD until OS TCP
    /// keepalive. We deliberately do NOT poll the socket during the idle window, so
    /// tungstenite never auto-pongs the probe (simulating the dead peer); then we
    /// drain to observe the close. Discriminating: the old deadline-less recv loop
    /// never closes this, so the outer timeout fails the suite instead of hanging it.
    #[tokio::test]
    async fn ws_established_idle_session_closes_when_silent() {
        let idle = Duration::from_millis(200);
        let state = test_state_empty_idle(idle).await;
        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();
        // Established. Stay silent and DON'T poll (no auto-pong) past the bound so the
        // coordinator's read-idle deadline fires server-side, then drain: the buffered
        // keepalive ping(s) are skipped and the close is observed.
        tokio::time::sleep(idle * 3).await;
        tokio::time::timeout(idle * 25, expect_ws_closed(&mut ws))
            .await
            .expect("coordinator closed the idle established session within the bound");
    }

    /// FM3: an established session is NOT closed while the peer stays responsive — the
    /// read-idle deadline RESETS on any inbound frame (here a client ping every
    /// quarter-bound), unlike the single pre-Hello handshake deadline. A client that
    /// keeps sending frames faster than the bound must still be connected well past
    /// it; an implementation that ignored inbound frames (closing at the bound
    /// regardless) would drop this responsive earner and red the test.
    #[tokio::test]
    async fn ws_established_idle_deadline_resets_on_inbound_frame() {
        let idle = Duration::from_millis(200);
        let state = test_state_empty_idle(idle).await;
        let addr = serve_ephemeral(state.clone()).await;
        let (ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let (mut tx, mut rx) = ws.split();
        let nonce = recv_challenge(&mut rx).await;
        tx.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();
        // Keep the session responsive: an inbound ping every quarter-bound keeps the
        // server's last-inbound deadline fresh.
        let pinger = tokio::spawn(async move {
            loop {
                if tx.send(WsMessage::Ping(Vec::new())).await.is_err() {
                    break; // coordinator closed the socket — stop pinging
                }
                tokio::time::sleep(idle / 4).await;
            }
        });
        // Over 3x the bound the session must NOT close (expect_ws_closed times out).
        let closed = tokio::time::timeout(idle * 3, expect_ws_closed(&mut rx)).await;
        pinger.abort();
        assert!(
            closed.is_err(),
            "a responsive (frame-sending) session must not be idle-closed",
        );
    }

    /// FM4: a long-running render that keeps heartbeating is never idle-closed. The
    /// deadline is driven by last-inbound-frame, NOT last-job-offer — an offered job
    /// disables the idle poll tick, but the earner's periodic heartbeats still arrive
    /// and reset the deadline. We accept the offer, heartbeat every quarter-bound for
    /// 3x the bound, then submit a valid result and expect `Accepted`: a session that
    /// was wrongly idle-reaped would have requeued the job and closed the socket, so
    /// the surviving submit proves the in-flight job was never reaped.
    #[tokio::test]
    async fn ws_idle_does_not_reap_an_in_flight_heartbeating_job() {
        let idle = Duration::from_millis(200);
        let state = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { session_idle_timeout: idle, ..test_config() },
        )
        .unwrap();
        drain_seeded_jobs(&state).await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();

        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Accept { job_id }).unwrap(),
        ))
        .await
        .unwrap();

        // Heartbeat past the bound. The coordinator sends only keepalive pings while
        // the job is in flight, so we needn't read until the final submit (the pings
        // buffer and are skipped then).
        for _ in 0..12 {
            ws.send(WsMessage::text(
                serde_json::to_string(&EarnerMsg::Heartbeat {
                    job_id: Some(job_id),
                    progress_pct: 0,
                })
                .unwrap(),
            ))
            .await
            .unwrap();
            tokio::time::sleep(idle / 4).await;
        }

        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Submit(signed_result(job_id, "deadbeef"))).unwrap(),
        ))
        .await
        .unwrap();
        match next_coordinator_msg(&mut ws).await {
            CoordinatorMsg::Accepted { job_id: jid, .. } => assert_eq!(jid, job_id),
            other => panic!("expected Accepted (session survived the heartbeats), got {other:?}"),
        }
    }

    /// The pre-routing slow-headers slowloris is bounded: a connection that sends
    /// the request line + a header then stalls (never the terminating blank line)
    /// is closed within the header-read timeout. Discriminating: without the bound
    /// (`axum::serve`) the server waits for the rest of the headers forever, so
    /// `read_to_end` hangs and the outer test timeout fails the suite. The whole
    /// ws integration suite already proves a COMPLETE request (the 101 upgrade)
    /// still serves through this same loop.
    #[tokio::test]
    async fn http_header_read_times_out_on_dribbled_headers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let state = test_state_empty().await;
        let addr = serve_ephemeral_with_header_timeout(state, Duration::from_millis(300)).await;
        let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(b"GET /stats HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .unwrap();
        // The server must close the stalled connection (read_to_end resolves on the
        // close — whether via a 408 then EOF or a reset). A regression that never
        // bounds the read hangs here until the outer 5s timeout fails the test.
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
            .await
            .expect("server closed the stalled-headers connection within the header-read timeout")
            .ok();
    }

    /// An HTTP/2 cleartext (h2c) preface must not park the server unboundedly.
    /// `auto::Builder` sniffs the `PRI * HTTP/2.0` preface and switches to h2,
    /// where the h1 `header_read_timeout` does NOT apply and hyper installs no
    /// pre-SETTINGS read bound — so an h2c slowloris (send the preface, never the
    /// SETTINGS frame) would park an FD forever: FM3's protocol bypass. Serving
    /// h1-only closes it — the h1 parser reads the preface as a (rejected) request
    /// and the connection ends promptly. Discriminating: under `auto::Builder`
    /// this hangs in h2 until the outer timeout fails the suite.
    #[tokio::test]
    async fn h2c_preface_does_not_park_the_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let state = test_state_empty().await;
        let addr = serve_ephemeral_with_header_timeout(state, Duration::from_millis(300)).await;
        let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
            .await
            .expect("server closed the h2c-preface connection instead of serving unbounded h2")
            .ok();
    }

    /// The post-headers slow-body slowloris is bounded on a mutating POST route: a
    /// client that completes the headers + a `Content-Length` then dribbles a few
    /// body bytes and stalls is closed within the body-read timeout. The header
    /// timeout disarms the moment headers parse, so without the body `TimeoutLayer`
    /// hyper waits for the full promised body forever — `read_to_end` hangs until
    /// the outer 5s timeout fails the suite. Discriminating against no bound.
    #[tokio::test]
    async fn http_body_read_times_out_on_dribbled_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let state = test_state_empty().await;
        let addr = serve_ephemeral_with_body_timeout(state, Duration::from_millis(300)).await;
        let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
        // Full request head promising a 1000-byte body, then 4 body bytes + stall.
        stream
            .write_all(
                b"POST /register HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"ki",
            )
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
            .await
            .expect("server closed the stalled-body connection within the body-read timeout")
            .ok();
        // FM4: the bound surfaces as a legible 408 the earner's reqwest path treats
        // as retryable, flushed before the close — not a silent reset.
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.contains("408 Request Timeout"),
            "expected a 408 before the close, got: {text:?}"
        );
    }

    /// A complete request body is served even under a sub-second body timeout: the
    /// bound fires only on a SLOW body, never on a fast complete one, so an honest
    /// registration/submit is never falsely 408'd (FM2). We send a complete body
    /// (semantically rejected, but delivered in full) and assert a normal HTTP
    /// status comes back promptly — not a timeout.
    #[tokio::test]
    async fn http_complete_body_served_under_tiny_body_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let state = test_state_empty().await;
        let addr = serve_ephemeral_with_body_timeout(state, Duration::from_millis(300)).await;
        let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let body = b"{}";
        let req = format!(
            "POST /register HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
            .await
            .expect("a complete-body POST returns promptly, not a timeout-close")
            .ok();
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.starts_with("HTTP/1.1") && !text.contains("408"),
            "the complete body reached the handler (a fast non-408 response), got: {text:?}"
        );
    }

    // -- POST /jobs runtime ingestion -------------------------------------

    /// A valid `POST /jobs` body. Mutate one field per negative test.
    fn create_job_body() -> serde_json::Value {
        serde_json::json!({
            "kind": "terrain",
            "region": { "x": 7, "y": -3, "layer": 1 },
            "deadline_secs": 120,
            "max_payout_wei": "1000000000000000000",
            "inputs": { "asset_url": "https://cdn.example.com/tile.usd" }
        })
    }

    #[tokio::test]
    async fn create_job_enqueues_and_returns_id() {
        let state = test_state_empty().await;
        let resp = post_json(state.clone(), "/jobs", &create_job_body()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = body_json(resp).await["id"].as_str().unwrap().to_string();

        // The job is queryable under the assigned id, queued, with the spec we sent.
        let detail = body_json(get(state.clone(), &format!("/jobs/{id}")).await).await;
        assert_eq!(detail["spec"]["kind"], "terrain");
        assert_eq!(detail["spec"]["deadline_secs"], 120);
        assert_eq!(detail["spec"]["region"]["x"], 7);
        assert!(detail["result"].is_null());

        let status = body_json(get(state.clone(), &format!("/jobs/{id}/status")).await).await;
        assert_eq!(status["status"], "queued");
    }

    /// Boundary region coords (i32 extremes, layer u8::MAX) are accepted and
    /// round-trip intact. `RegionCoord::region_id` — which the dispatch and
    /// attestation paths later feed — is pure formatting with no arithmetic, so
    /// extreme coords can't overflow it; the ingestion front door must take them
    /// without panicking rather than rejecting valid frontier tiles.
    #[tokio::test]
    async fn create_job_accepts_boundary_region_coords() {
        let state = test_state_empty().await;
        let mut body = create_job_body();
        body["region"] = serde_json::json!({ "x": i32::MAX, "y": i32::MIN, "layer": u8::MAX });
        let resp = post_json(state.clone(), "/jobs", &body).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = body_json(resp).await["id"].as_str().unwrap().to_string();

        let detail = body_json(get(state.clone(), &format!("/jobs/{id}")).await).await;
        assert_eq!(detail["spec"]["region"]["x"], i32::MAX);
        assert_eq!(detail["spec"]["region"]["y"], i32::MIN);
        assert_eq!(detail["spec"]["region"]["layer"], u8::MAX);
        // The id the downstream paths derive from these coords formats, no overflow.
        let region = RegionCoord { x: i32::MAX, y: i32::MIN, layer: u8::MAX };
        assert!(region.region_id().starts_with('r'));
    }

    /// The id is minted server-side: a body that smuggles an `id` of an EXISTING
    /// job must NOT overwrite it (enqueue upserts on id, `ON CONFLICT DO UPDATE`),
    /// nor reset its lifecycle. We dispatch the existing job to `in_flight`, then
    /// POST a create body carrying that id + a different kind, and assert the
    /// existing job is untouched and a fresh queued job is created instead.
    #[tokio::test]
    async fn create_job_ignores_caller_id_and_never_overwrites() {
        let state = test_state_empty().await;
        let mut existing = seed_job(); // kind = Terrain
        existing.id = Uuid::new_v4();
        let victim = existing.id;
        enqueue(&state, &existing).await;
        // Move it in_flight so an overwrite-to-queued would be visible.
        state.store.lock().await.take_next(|_| true).unwrap().unwrap();

        let mut body = create_job_body();
        body["id"] = serde_json::json!(victim.to_string());
        body["kind"] = serde_json::json!("foliage");
        let resp = post_json(state.clone(), "/jobs", &body).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let new_id = body_json(resp).await["id"].as_str().unwrap().to_string();

        assert_ne!(new_id, victim.to_string(), "the assigned id must be freshly minted");
        // The victim is untouched: still in_flight, still Terrain (not Foliage).
        let v_status = body_json(get(state.clone(), &format!("/jobs/{victim}/status")).await).await;
        assert_eq!(v_status["status"], "in_flight", "caller id must not reset lifecycle");
        let v_detail = body_json(get(state.clone(), &format!("/jobs/{victim}")).await).await;
        assert_eq!(v_detail["spec"]["kind"], "terrain", "caller id must not overwrite spec");
        // The new job exists and is queued.
        let n_status = body_json(get(state.clone(), &format!("/jobs/{new_id}/status")).await).await;
        assert_eq!(n_status["status"], "queued");
    }

    #[tokio::test]
    async fn create_job_rejects_malformed_payout_and_leaves_stats_clean() {
        let state = test_state_empty().await;
        let mut body = create_job_body();
        body["max_payout_wei"] = serde_json::json!("not-a-number");
        let resp = post_json(state.clone(), "/jobs", &body).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        // Nothing was enqueued, so /stats stays well-formed (the deferred-poison FM).
        let stats = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(stats["jobs_queued"], 0);
        assert_eq!(stats["total_payout_wei"], "0");
    }

    /// A valid buyer on POST /jobs is validated and stored coordinator-side, so the
    /// settle path can later debit it via ComputeMeter. The buyer is read back
    /// through the same job_buyer accessor the metering seam uses.
    #[tokio::test]
    async fn create_job_attributes_a_valid_buyer() {
        let state = test_state_empty().await;
        let buyer = "0x00000000000000000000000000000000000000b1";
        let mut body = create_job_body();
        body["buyer"] = serde_json::json!(buyer);

        let resp = post_json(state.clone(), "/jobs", &body).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = Uuid::parse_str(body_json(resp).await["id"].as_str().unwrap()).unwrap();

        let stored = state.store.lock().await.job_buyer(&id).unwrap();
        assert_eq!(stored.as_deref(), Some(buyer), "the validated buyer must be stored for metering");
    }

    /// A malformed buyer (right length, non-hex — exercises the hex predicate, not
    /// just length) is rejected 422 like any other bad field, before anything is
    /// enqueued, so /stats stays clean.
    #[tokio::test]
    async fn create_job_rejects_a_malformed_buyer() {
        let state = test_state_empty().await;
        let mut body = create_job_body();
        body["buyer"] = serde_json::json!("0x00000000000000000000000000000000000000zz");

        let resp = post_json(state.clone(), "/jobs", &body).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 0);
    }

    /// An absent buyer is valid: the job enqueues unattributed (NULL), so the
    /// existing job sources (the agents poster, seed) keep working unchanged. The
    /// metering seam skips a NULL buyer (mirrors the unknown-region fee skip).
    #[tokio::test]
    async fn create_job_without_a_buyer_enqueues_unattributed() {
        let state = test_state_empty().await;

        let resp = post_json(state.clone(), "/jobs", &create_job_body()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = Uuid::parse_str(body_json(resp).await["id"].as_str().unwrap()).unwrap();

        let stored = state.store.lock().await.job_buyer(&id).unwrap();
        assert_eq!(stored, None, "an unattributed job stores no buyer (NULL → not metered)");
    }

    /// A stored buyer survives a coordinator restart: written through
    /// enqueue_within_cap, it is still readable after the SAME db file is reopened
    /// (the idempotent buyer ALTER on the second open must not error or wipe it).
    #[test]
    fn buyer_persists_across_reopen() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let job = job_with_deadline(60);
        let buyer = "0x00000000000000000000000000000000000000b1";
        {
            let store = Store::open(&db_path).unwrap();
            assert!(store.enqueue_within_cap(&job, 10, Some(buyer)).unwrap());
            assert_eq!(store.job_buyer(&job.id).unwrap().as_deref(), Some(buyer));
        } // close the first "process" before reopening
        let store = Store::open(&db_path).unwrap();
        assert_eq!(
            store.job_buyer(&job.id).unwrap().as_deref(),
            Some(buyer),
            "buyer must survive a restart (idempotent migration, no data loss)"
        );
    }

    #[tokio::test]
    async fn create_job_rejects_zero_deadline() {
        let state = test_state_empty().await;
        let mut body = create_job_body();
        body["deadline_secs"] = serde_json::json!(0);
        let resp = post_json(state.clone(), "/jobs", &body).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 0);
    }

    #[tokio::test]
    async fn create_job_rejects_oversized_inputs() {
        let state = test_state_empty().await;
        let mut body = create_job_body();
        // ~17 KiB: over MAX_INPUTS_BYTES (16 KiB) but under MAX_REQUEST_BODY_BYTES
        // (32 KiB), so it clears the pre-parse body cap and is rejected by the
        // precise inputs gate (422), not the coarse body cap (413) — see below.
        body["inputs"] = serde_json::json!({ "blob": "x".repeat(17 * 1024) });
        let resp = post_json(state.clone(), "/jobs", &body).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 0);
    }

    /// A body past MAX_REQUEST_BODY_BYTES is refused 413 by the DefaultBodyLimit
    /// layer BEFORE serde buffers and parses it — the pre-parse backstop that
    /// bounds transient request memory (FM4), distinct from the post-parse inputs
    /// gate above. Discriminating: drop `.layer(body_guard())`'s size cap and this
    /// 40 KiB body is buffered + parsed into a `Value` and only then 422'd by the
    /// inputs gate, so the status flips 413 -> 422.
    #[tokio::test]
    async fn create_job_rejects_body_over_the_pre_parse_cap() {
        let state = test_state_empty().await;
        let mut body = create_job_body();
        body["inputs"] = serde_json::json!({ "blob": "x".repeat(40 * 1024) });
        let resp = post_json(state.clone(), "/jobs", &body).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 0);
    }

    /// An unknown `kind` is rejected by the `Json` extractor (valid JSON, invalid
    /// enum) before the handler runs — a clean client error, not a panic or 500.
    #[tokio::test]
    async fn create_job_rejects_unknown_kind() {
        let state = test_state_empty().await;
        let mut body = create_job_body();
        body["kind"] = serde_json::json!("not_a_kind");
        let resp = post_json(state.clone(), "/jobs", &body).await;
        assert!(resp.status().is_client_error(), "got {}", resp.status());
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 0);
    }

    /// POST /jobs carries the body-read timeout (it is a body-bearing mutating
    /// route like /register and /submit): a dribbled body is closed with a 408,
    /// not parked forever. Discriminating — drop the `.layer(body_timeout)` on the
    /// route and `read_to_end` hangs past the outer 5s, failing the suite.
    #[tokio::test]
    async fn create_job_body_read_times_out_on_dribbled_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let state = test_state_empty().await;
        let addr = serve_ephemeral_with_body_timeout(state, Duration::from_millis(300)).await;
        let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(
                b"POST /jobs HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"ki",
            )
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
            .await
            .expect("server closed the stalled-body POST /jobs within the body-read timeout")
            .ok();
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.contains("408 Request Timeout"),
            "expected a 408 before the close, got: {text:?}"
        );
    }

    // -- POST /jobs ingestion auth (--ingest-token) -----------------------

    const TEST_INGEST_TOKEN: &str = "s3cr3t-ingest-token-abcdef0123456789";

    /// Empty-queue state configured with an ingest token, for the auth tests.
    async fn test_state_empty_with_token(token: &str) -> Arc<AppState> {
        let state = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { ingest_token: Some(token.to_string()), ..test_config() },
        )
        .unwrap();
        drain_seeded_jobs(&state).await;
        state
    }

    /// POST a JSON body with an optional `Authorization` header. `None` omits the
    /// header entirely (the missing-header case); `Some(v)` sets it verbatim.
    async fn post_json_auth(
        state: Arc<AppState>,
        uri: &str,
        value: &serde_json::Value,
        authorization: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(auth) = authorization {
            builder = builder.header("authorization", auth);
        }
        router(state)
            .oneshot(builder.body(Body::from(serde_json::to_vec(value).unwrap())).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_job_with_correct_token_returns_201() {
        let state = test_state_empty_with_token(TEST_INGEST_TOKEN).await;
        let auth = format!("Bearer {TEST_INGEST_TOKEN}");
        let resp = post_json_auth(state.clone(), "/jobs", &create_job_body(), Some(&auth)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 1);
    }

    /// FM1: a wrong token of the SAME LENGTH is rejected. This is the shape of the
    /// byte-by-byte recovery attack — a length-only check would accept it, and it is
    /// the case the constant-time `subtle::ct_eq` compare must reject on its
    /// constant-time path. The timing property itself isn't asserted (it's
    /// guaranteed by the primitive, not observable in a unit test); the functional
    /// rejection of an equal-length wrong token is what's pinned here.
    #[tokio::test]
    async fn create_job_with_wrong_same_length_token_returns_401() {
        let state = test_state_empty_with_token(TEST_INGEST_TOKEN).await;
        let mut wrong = TEST_INGEST_TOKEN.to_string();
        wrong.pop();
        wrong.push('X'); // flip the last byte, preserve length
        assert_eq!(wrong.len(), TEST_INGEST_TOKEN.len());
        let auth = format!("Bearer {wrong}");
        let resp = post_json_auth(state.clone(), "/jobs", &create_job_body(), Some(&auth)).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // The gate ran before any store work — nothing was enqueued.
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 0);
    }

    #[tokio::test]
    async fn create_job_missing_auth_header_returns_401() {
        let state = test_state_empty_with_token(TEST_INGEST_TOKEN).await;
        let resp = post_json_auth(state.clone(), "/jobs", &create_job_body(), None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 0);
    }

    /// FM3: a non-`Bearer` scheme AND a non-ASCII (opaque-byte) header value are both
    /// a uniform 401 with no panic — the wrong scheme fails `strip_prefix`, and
    /// `to_str()` returns `Err` on the non-ASCII value rather than unwinding.
    #[tokio::test]
    async fn create_job_malformed_auth_header_returns_401() {
        let state = test_state_empty_with_token(TEST_INGEST_TOKEN).await;
        // Wrong scheme: the correct token under `Basic`, not `Bearer`.
        let resp = post_json_auth(
            state.clone(),
            "/jobs",
            &create_job_body(),
            Some(&format!("Basic {TEST_INGEST_TOKEN}")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Non-ASCII (0xFF) value: HeaderValue holds it as opaque bytes, but the
        // handler's `to_str()` returns Err → 401, no panic. Built from raw bytes
        // because `Request::builder().header(_, &str)` rejects non-ASCII at build
        // time, so this handler path is only reachable with a byte-built value.
        let mut bytes = b"Bearer ".to_vec();
        bytes.push(0xFF);
        let value = axum::http::HeaderValue::from_bytes(&bytes).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/jobs")
            .header("content-type", "application/json")
            .header("authorization", value)
            .body(Body::from(serde_json::to_vec(&create_job_body()).unwrap()))
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// FM2 (open branch): with NO token configured the endpoint stays open — a
    /// create with no Authorization header succeeds. The loud startup warning lives
    /// in `main`; the open-vs-closed behavior is what a router test can pin.
    #[tokio::test]
    async fn create_job_unconfigured_allows_without_auth() {
        let state = test_state_empty().await; // no ingest token configured
        assert!(state.ingest_token.is_none());
        let resp = post_json_auth(state.clone(), "/jobs", &create_job_body(), None).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 1);
    }

    /// FM4: a blank/whitespace-only configured token is rejected at construction —
    /// an empty secret would authenticate `Authorization: Bearer ` (every caller),
    /// so it must fail like the other zero-knobs, not silently ship wide-open. A
    /// real token constructs fine.
    #[test]
    fn with_store_rejects_blank_ingest_token() {
        for blank in ["", "   ", "\t"] {
            let r = AppState::with_store(
                Store::open_in_memory().unwrap(),
                StoreConfig { ingest_token: Some(blank.to_string()), ..test_config() },
            );
            assert!(r.is_err(), "blank token {blank:?} must be rejected at construction");
        }
        assert!(AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { ingest_token: Some("real-token".to_string()), ..test_config() },
        )
        .is_ok());
    }

    // -- POST /jobs queued-backlog cap (--max-queued-jobs) ----------------

    /// Empty-queue state with a small queued-job cap, for the backlog-cap tests.
    async fn test_state_empty_with_cap(max_queued: usize) -> Arc<AppState> {
        let state = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { max_queued_jobs: max_queued, ..test_config() },
        )
        .unwrap();
        drain_seeded_jobs(&state).await;
        state
    }

    /// Empty-queue state whose store charges `rate` wei per render-second at settle,
    /// backing the pending-debit persistence tests (a settled job with a buyer
    /// accrues a debit; with `rate` 0 — the default state — none does).
    async fn test_state_empty_with_compute_rate(rate_wei: u128) -> Arc<AppState> {
        let state = AppState::with_store(
            Store::open_in_memory().unwrap().with_compute_rate_wei(rate_wei),
            test_config(),
        )
        .unwrap();
        drain_seeded_jobs(&state).await;
        state
    }

    /// FM5: at the cap, a create is shed with a retryable 503 and nothing is
    /// enqueued past the cap. Mutation-proven discriminating: drop the cap (swap
    /// `enqueue_within_cap` for `enqueue`) and the 3rd POST returns 201 with
    /// `jobs_queued == 3`, failing both assertions.
    #[tokio::test]
    async fn create_job_rejects_when_queue_at_cap() {
        let state = test_state_empty_with_cap(2).await;
        for _ in 0..2 {
            let resp = post_json(state.clone(), "/jobs", &create_job_body()).await;
            assert_eq!(resp.status(), StatusCode::CREATED);
        }
        let resp = post_json(state.clone(), "/jobs", &create_job_body()).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 2);
    }

    /// FM3: the cap reads the TRUE queued depth — a live COUNT over the queued rows,
    /// not a side counter that could drift — so a dispatch (queued→in_flight) frees
    /// a slot and a create that was just shed is admitted again. The count is a
    /// single `WHERE status = queued` query, so it tracks every transition both ways
    /// (a requeue back INTO queued is counted identically); the uncapped paths that
    /// can push the backlog over the cap are covered by `queue_cap_exempts_the_boot_seed`.
    #[tokio::test]
    async fn queue_cap_frees_a_slot_when_a_job_is_dispatched() {
        let state = test_state_empty_with_cap(2).await;
        for _ in 0..2 {
            assert_eq!(
                post_json(state.clone(), "/jobs", &create_job_body()).await.status(),
                StatusCode::CREATED
            );
        }
        // At the cap → shed.
        assert_eq!(
            post_json(state.clone(), "/jobs", &create_job_body()).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // Dispatch one (queued→in_flight): the cap counts only queued, so a slot frees.
        state.store.lock().await.take_next(|_| true).unwrap().unwrap();
        assert_eq!(
            post_json(state.clone(), "/jobs", &create_job_body()).await.status(),
            StatusCode::CREATED,
            "a dispatched job frees a queued slot"
        );
        // queued is back at the cap (one dispatched out, one fresh in).
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 2);
    }

    /// FM4: the boot-time seed enqueues via the uncapped path, so it must NOT be
    /// silently dropped by a low cap. Built with cap=1 and NOT drained: the seeded
    /// backlog exceeds the cap.
    #[tokio::test]
    async fn queue_cap_exempts_the_boot_seed() {
        let state = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { max_queued_jobs: 1, ..test_config() },
        )
        .unwrap();
        let queued = body_json(get(state, "/stats").await).await["jobs_queued"].as_u64().unwrap();
        assert!(queued > 1, "the boot seed must bypass the cap, got {queued} queued");
    }

    /// FM2: concurrent creators against a cap of 3 admit EXACTLY 3 — no overshoot.
    /// The atomic count-and-insert (one SQL statement under the store mutex) is what
    /// makes this hold; a check-then-insert with the lock released between would let
    /// several creators all observe `cap-1` and overshoot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queue_cap_no_overshoot_under_concurrent_creates() {
        let state = test_state_empty_with_cap(3).await;
        let futs: Vec<_> = (0..12)
            .map(|_| {
                let s = state.clone();
                async move { post_json(s, "/jobs", &create_job_body()).await.status() }
            })
            .collect();
        let statuses = futures_util::future::join_all(futs).await;
        let created = statuses.iter().filter(|s| **s == StatusCode::CREATED).count();
        let shed = statuses.iter().filter(|s| **s == StatusCode::SERVICE_UNAVAILABLE).count();
        assert_eq!(created, 3, "exactly cap jobs admitted");
        assert_eq!(shed, 9, "the rest are shed, none lost to a 500");
        assert_eq!(body_json(get(state, "/stats").await).await["jobs_queued"], 3);
    }

    #[test]
    fn with_store_rejects_zero_max_queued_jobs() {
        let r = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { max_queued_jobs: 0, ..test_config() },
        );
        assert!(r.is_err(), "zero max_queued_jobs must be rejected at construction");
    }

    // -- earner registry size cap (--max-earners) -------------------------

    /// An `EarnerInfo` with a chosen `last_seen` and otherwise fixed fields, so the
    /// `admit_earner` unit tests vary only the staleness the cap policy keys on.
    fn earner_info(last_seen: i64) -> EarnerInfo {
        EarnerInfo {
            gpu_model: "RTX 4090".into(),
            vram_gb: 24,
            supported: vec![JobKind::Terrain],
            last_seen,
        }
    }

    /// Below the cap a NEW address is admitted unconditionally and the map grows.
    #[test]
    fn admit_earner_inserts_below_cap() {
        let mut earners = HashMap::new();
        assert!(admit_earner(&mut earners, "a".into(), earner_info(100), 3, 100, 60));
        assert!(admit_earner(&mut earners, "b".into(), earner_info(100), 3, 100, 60));
        assert_eq!(earners.len(), 2);
    }

    /// FM2: re-registration of an already-known address is an in-place upsert — it
    /// never counts against the cap (the map doesn't grow) and refreshes the entry,
    /// even when the registry is otherwise full of live earners.
    #[test]
    fn admit_earner_upsert_never_blocked_at_cap() {
        let mut earners = HashMap::new();
        earners.insert("a".to_string(), earner_info(100));
        earners.insert("b".to_string(), earner_info(100));
        // At cap (2), all live: re-Hello from "a" still succeeds and updates it.
        assert!(admit_earner(&mut earners, "a".into(), earner_info(150), 2, 200, 60));
        assert_eq!(earners.len(), 2);
        assert_eq!(earners["a"].last_seen, 150, "the upsert refreshed the existing entry");
    }

    /// FM1/FM2: at the cap with every entry currently LIVE, a new registration is
    /// rejected and no live earner is displaced — a genuinely full fleet sheds the
    /// newcomer rather than evicting working earners.
    #[test]
    fn admit_earner_at_cap_all_live_rejects() {
        let mut earners = HashMap::new();
        earners.insert("a".to_string(), earner_info(100));
        earners.insert("b".to_string(), earner_info(100));
        assert!(!admit_earner(&mut earners, "c".into(), earner_info(100), 2, 100, 60));
        assert_eq!(earners.len(), 2);
        assert!(earners.contains_key("a") && earners.contains_key("b"));
        assert!(!earners.contains_key("c"), "no live earner displaced for the newcomer");
    }

    /// FM1: at the cap a new registration is admitted by evicting the stalest entry
    /// already past its TTL — the live earner is kept, the stale one reclaimed.
    #[test]
    fn admit_earner_at_cap_evicts_stalest_past_ttl() {
        let mut earners = HashMap::new();
        earners.insert("stale".to_string(), earner_info(10)); // now-10=90 > ttl 60 → not live
        earners.insert("live".to_string(), earner_info(100)); // now-100=0 → live
        assert!(admit_earner(&mut earners, "new".into(), earner_info(100), 2, 100, 60));
        assert_eq!(earners.len(), 2);
        assert!(!earners.contains_key("stale"), "the past-TTL entry was evicted");
        assert!(earners.contains_key("live"), "the live entry was kept");
        assert!(earners.contains_key("new"));
    }

    /// The eviction picks the STALEST (smallest `last_seen`) among several past-TTL
    /// entries, not an arbitrary one.
    #[test]
    fn admit_earner_evicts_the_stalest_among_several_stale() {
        let mut earners = HashMap::new();
        earners.insert("a".to_string(), earner_info(10));
        earners.insert("b".to_string(), earner_info(5)); // the stalest
        earners.insert("c".to_string(), earner_info(20));
        // now=100, ttl=60 → all three are past TTL; cap=3 is full.
        assert!(admit_earner(&mut earners, "d".into(), earner_info(100), 3, 100, 60));
        assert_eq!(earners.len(), 3);
        assert!(!earners.contains_key("b"), "the stalest (last_seen=5) was evicted");
        assert!(earners.contains_key("a") && earners.contains_key("c") && earners.contains_key("d"));
    }

    /// Distinct loopback source IP for the rate-limit unit tests (`10.0.0.<n>`).
    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    /// A fresh source may register up to `capacity` times in one window; the next
    /// attempt in the same window is shed.
    #[test]
    fn rate_under_limit_admits_over_limit_sheds() {
        let mut buckets = HashMap::new();
        for _ in 0..3 {
            assert!(check_registration_rate(&mut buckets, ip(1), 0, 3, 60, 16));
        }
        assert!(
            !check_registration_rate(&mut buckets, ip(1), 0, 3, 60, 16),
            "the 4th registration in the same window is over the cap of 3"
        );
    }

    /// The bucket refills at `capacity` tokens per window: a source drained at t=0 is
    /// re-admitted one token per `window/capacity` seconds, and a full idle window
    /// restores the entire burst.
    #[test]
    fn rate_refills_over_time() {
        let mut buckets = HashMap::new();
        // capacity 2 / 60s → one token every 30s.
        assert!(check_registration_rate(&mut buckets, ip(1), 0, 2, 60, 16));
        assert!(check_registration_rate(&mut buckets, ip(1), 0, 2, 60, 16));
        assert!(!check_registration_rate(&mut buckets, ip(1), 0, 2, 60, 16), "drained at t=0");
        assert!(!check_registration_rate(&mut buckets, ip(1), 29, 2, 60, 16), "29s < one token");
        assert!(check_registration_rate(&mut buckets, ip(1), 30, 2, 60, 16), "30s → one token back");
        assert!(!check_registration_rate(&mut buckets, ip(1), 30, 2, 60, 16), "and immediately drained");
        // A full window idle restores the whole burst (capped at capacity, not more).
        assert!(check_registration_rate(&mut buckets, ip(1), 90, 2, 60, 16));
        assert!(check_registration_rate(&mut buckets, ip(1), 90, 2, 60, 16));
        assert!(!check_registration_rate(&mut buckets, ip(1), 90, 2, 60, 16), "burst capped at capacity");
    }

    /// The limit is PER SOURCE: one source exhausting its bucket does not throttle a
    /// different source (FM3 — a global pool would shed an honest fleet's fan-in).
    #[test]
    fn rate_is_per_source() {
        let mut buckets = HashMap::new();
        assert!(check_registration_rate(&mut buckets, ip(1), 0, 1, 60, 16));
        assert!(!check_registration_rate(&mut buckets, ip(1), 0, 1, 60, 16), "source 1 exhausted");
        assert!(
            check_registration_rate(&mut buckets, ip(2), 0, 1, 60, 16),
            "source 2 has its own bucket"
        );
    }

    /// An idle bucket never accrues beyond `capacity`, even after an arbitrarily long
    /// gap — the burst ceiling holds.
    #[test]
    fn rate_saturates_at_capacity() {
        let mut buckets = HashMap::new();
        assert!(check_registration_rate(&mut buckets, ip(1), 0, 2, 60, 16));
        assert!(check_registration_rate(&mut buckets, ip(1), 0, 2, 60, 16));
        // Idle for ~1e6s: refill credit is enormous but saturates at capacity (2).
        assert!(check_registration_rate(&mut buckets, ip(1), 1_000_000, 2, 60, 16));
        assert!(check_registration_rate(&mut buckets, ip(1), 1_000_000, 2, 60, 16));
        assert!(
            !check_registration_rate(&mut buckets, ip(1), 1_000_000, 2, 60, 16),
            "no more than capacity even after a long idle"
        );
    }

    /// A backward clock step neither grants nor destroys allowance: a drained bucket
    /// stays drained when `now` jumps backward.
    #[test]
    fn rate_backward_clock_is_inert() {
        let mut buckets = HashMap::new();
        assert!(check_registration_rate(&mut buckets, ip(1), 100, 1, 60, 16));
        assert!(!check_registration_rate(&mut buckets, ip(1), 100, 1, 60, 16), "drained at t=100");
        assert!(
            !check_registration_rate(&mut buckets, ip(1), 50, 1, 60, 16),
            "clock stepped back → no refill, still drained"
        );
    }

    /// FM4: the bucket map is bounded — a new source at the cap evicts the stalest, so
    /// the map can never grow past `max_buckets` no matter how many distinct sources
    /// register. The evicted source simply starts fresh (full) on its next attempt.
    #[test]
    fn rate_bucket_map_bounded_evicts_stalest() {
        let mut buckets = HashMap::new();
        // Two actively-limited sources (each drained, so neither is idle/full).
        assert!(check_registration_rate(&mut buckets, ip(1), 0, 1, 60, 2));
        assert!(check_registration_rate(&mut buckets, ip(2), 1, 1, 60, 2));
        assert_eq!(buckets.len(), 2);
        // A third distinct source at the cap evicts the stalest (ip(1), last_refill 0).
        assert!(check_registration_rate(&mut buckets, ip(3), 2, 1, 60, 2));
        assert_eq!(buckets.len(), 2, "map stays bounded at max_buckets");
        assert!(!buckets.contains_key(&ip(1)), "the stalest source was evicted");
        assert!(buckets.contains_key(&ip(2)) && buckets.contains_key(&ip(3)));
    }

    /// FM4: a new source at the cap drops fully-refilled (idle) buckets BEFORE
    /// evicting the stalest — idle buckets carry no state, so reclaiming them is
    /// lossless and preferable to evicting an actively-limited source. Both prior
    /// sources have idled a full window back to full, so both are pruned and the map
    /// shrinks rather than holding the stalest.
    #[test]
    fn rate_bucket_map_prunes_idle_before_evicting() {
        let mut buckets = HashMap::new();
        assert!(check_registration_rate(&mut buckets, ip(1), 0, 1, 10, 2));
        assert!(check_registration_rate(&mut buckets, ip(2), 0, 1, 10, 2));
        assert_eq!(buckets.len(), 2);
        // At t=100 (≫ window 10), both prior buckets have refilled to full and are
        // pruned; only the newcomer remains. Evict-only would have kept one (len 2).
        assert!(check_registration_rate(&mut buckets, ip(3), 100, 1, 10, 2));
        assert_eq!(buckets.len(), 1, "idle buckets pruned before any eviction");
        assert!(buckets.contains_key(&ip(3)));
    }

    fn trusted(ips: &[IpAddr]) -> TrustedProxies {
        TrustedProxies::parse(&ips.iter().map(|ip| ip.to_string()).collect::<Vec<_>>()).unwrap()
    }

    /// parse_trusted_entry accepts a bare IP (as an exact /32 or /128) and an
    /// ip/prefix CIDR, and REJECTS a malformed address, an unparseable or
    /// out-of-range prefix, and a blank entry — never silently widening to /0.
    #[test]
    fn parse_trusted_entry_forms() {
        assert_eq!(
            parse_trusted_entry("10.0.0.1").unwrap(),
            TrustedCidr { network: IpAddr::from([10, 0, 0, 1]), prefix: 32 },
            "bare v4 -> /32"
        );
        assert_eq!(
            parse_trusted_entry("  10.0.0.0/8 ").unwrap(),
            TrustedCidr { network: IpAddr::from([10, 0, 0, 0]), prefix: 8 },
            "v4 cidr, trimmed"
        );
        assert_eq!(parse_trusted_entry("2001:db8::1").unwrap().prefix, 128, "bare v6 -> /128");
        assert_eq!(parse_trusted_entry("2001:db8::/32").unwrap().prefix, 32, "v6 cidr");
        assert!(parse_trusted_entry("").is_err(), "blank");
        assert!(parse_trusted_entry("   ").is_err(), "whitespace-only");
        assert!(parse_trusted_entry("nope").is_err(), "garbage ip");
        assert!(parse_trusted_entry("10.0.0.0/x").is_err(), "non-numeric prefix");
        assert!(parse_trusted_entry("10.0.0.0/33").is_err(), "v4 prefix > 32");
        assert!(parse_trusted_entry("2001:db8::/129").is_err(), "v6 prefix > 128");
    }

    /// parse() skips blank/whitespace entries (an empty env var or a trailing comma
    /// yields trust-no-proxy, never a startup crash) but still rejects a non-blank
    /// malformed entry — a blank is never widened to a /0 catch-all.
    #[test]
    fn trusted_proxies_parse_skips_blank_entries() {
        let t = TrustedProxies::parse(&["".into(), "  ".into(), "10.0.0.1".into()]).unwrap();
        assert!(t.contains(&IpAddr::from([10, 0, 0, 1])), "non-blank entry honored");
        assert!(!t.contains(&IpAddr::from([8, 8, 8, 8])), "blank did not become a catch-all");
        assert!(TrustedProxies::parse(&["".into(), "   ".into()]).unwrap().0.is_empty(), "all-blank -> empty");
        assert!(TrustedProxies::parse(&["10.0.0.1".into(), "garbage".into()]).is_err(), "garbage rejected");
    }

    /// A CIDR entry trusts every peer inside the range and none outside it (mask, not
    /// string-prefix); an exact IP entry still matches only itself; the two unify and
    /// an empty allowlist trusts nobody.
    #[test]
    fn trusted_cidr_containment_and_union() {
        let t = TrustedProxies::parse(&["10.0.0.0/8".into(), "192.168.1.5".into()]).unwrap();
        assert!(t.contains(&IpAddr::from([10, 0, 0, 0])), "network address in range");
        assert!(t.contains(&IpAddr::from([10, 0, 0, 1])), "in-range low");
        assert!(t.contains(&IpAddr::from([10, 255, 255, 254])), "in-range high");
        assert!(!t.contains(&IpAddr::from([11, 0, 0, 1])), "just outside the range");
        assert!(!t.contains(&IpAddr::from([9, 255, 255, 255])), "just below the range");
        assert!(t.contains(&IpAddr::from([192, 168, 1, 5])), "exact IP still trusted");
        assert!(!t.contains(&IpAddr::from([192, 168, 1, 6])), "neighbor of exact IP untrusted");
        assert!(!TrustedProxies::parse(&[]).unwrap().contains(&IpAddr::from([10, 0, 0, 1])), "empty");
    }

    /// CIDR containment is family-correct: a v4 range never contains a v6 (or
    /// v4-mapped-v6) peer, and a v6 range never matches a bare v4.
    #[test]
    fn trusted_cidr_cross_family_never_matches() {
        let v4 = TrustedProxies::parse(&["10.0.0.0/8".into()]).unwrap();
        assert!(!v4.contains(&"::ffff:10.0.0.1".parse().unwrap()), "v4 range vs v4-mapped-v6 peer");
        assert!(!v4.contains(&"2001:db8::1".parse().unwrap()), "v4 range vs v6 peer");
        let v6 = TrustedProxies::parse(&["2001:db8::/32".into()]).unwrap();
        assert!(v6.contains(&"2001:db8:dead::1".parse().unwrap()), "v6 in range");
        assert!(!v6.contains(&"2001:dead::1".parse().unwrap()), "v6 out of range");
        assert!(!v6.contains(&IpAddr::from([10, 0, 0, 1])), "v6 range vs v4 peer");
    }

    /// A /0 catch-all is honored only when EXPLICITLY entered (a blank/malformed entry
    /// is rejected, never widened to /0), and a v4 /0 still does not span v6.
    #[test]
    fn trusted_cidr_prefix_zero_is_explicit_catch_all() {
        let t = TrustedProxies::parse(&["0.0.0.0/0".into()]).unwrap();
        assert!(t.contains(&IpAddr::from([8, 8, 8, 8])), "/0 trusts any v4");
        assert!(t.contains(&IpAddr::from([10, 0, 0, 1])));
        assert!(!t.contains(&"2001:db8::1".parse().unwrap()), "v4 /0 does not span v6");
    }

    /// SECURITY end-to-end: a peer INSIDE a trusted CIDR honors XFF exactly as a listed
    /// exact IP does, while a peer OUTSIDE stays untrusted and keys on itself — so the
    /// CIDR only widens the trust gate, it does not let an out-of-range peer spoof.
    #[test]
    fn resolve_cidr_trusted_peer_honors_xff() {
        let t = TrustedProxies::parse(&["10.0.0.0/8".into()]).unwrap();
        let inside = IpAddr::from([10, 9, 9, 9]);
        assert_eq!(resolve_source_ip(inside, &xff("203.0.113.7"), &t), ip4(203, 0, 113, 7));
        let outside = IpAddr::from([11, 0, 0, 1]);
        assert_eq!(resolve_source_ip(outside, &xff("203.0.113.7"), &t), outside);
    }

    fn xff(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", value.parse().unwrap());
        h
    }

    fn fwd(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("forwarded", value.parse().unwrap());
        h
    }

    /// parse_forwarded_node accepts a bare IP and the ip:port / [ipv6]:port forms a
    /// proxy may append, and rejects obfuscated/garbage tokens.
    #[test]
    fn parse_forwarded_node_forms() {
        assert_eq!(parse_forwarded_node("203.0.113.7"), Some(ip4(203, 0, 113, 7)));
        assert_eq!(parse_forwarded_node("  203.0.113.7 "), Some(ip4(203, 0, 113, 7)), "trims");
        assert_eq!(parse_forwarded_node("203.0.113.7:54321"), Some(ip4(203, 0, 113, 7)), "v4:port");
        assert_eq!(parse_forwarded_node("2001:db8::1"), "2001:db8::1".parse().ok(), "bare v6");
        assert_eq!(
            parse_forwarded_node("[2001:db8::1]:443"),
            "2001:db8::1".parse().ok(),
            "[v6]:port"
        );
        assert_eq!(parse_forwarded_node("_hidden"), None);
        assert_eq!(parse_forwarded_node("unknown"), None);
        assert_eq!(parse_forwarded_node(""), None);
    }

    /// parse_forwarded_element pulls the `for=` client out of an RFC-7239
    /// forwarded-element across the legal shapes: bare/quoted IPv4, quoted
    /// bracketed IPv6 (with and without a port), a case-insensitive key, `for=`
    /// alongside other params — and maps obfuscated/`unknown`/empty/missing `for=`
    /// to None (the indeterminate-hop signal that falls back to the peer).
    #[test]
    fn parse_forwarded_element_forms() {
        assert_eq!(parse_forwarded_element("for=203.0.113.7"), Some(ip4(203, 0, 113, 7)));
        assert_eq!(parse_forwarded_element("for=\"203.0.113.7\""), Some(ip4(203, 0, 113, 7)), "quoted");
        assert_eq!(parse_forwarded_element("For=203.0.113.7"), Some(ip4(203, 0, 113, 7)), "case-insensitive key");
        assert_eq!(
            parse_forwarded_element("for=203.0.113.7;proto=https"),
            Some(ip4(203, 0, 113, 7)),
            "trailing params"
        );
        assert_eq!(
            parse_forwarded_element("by=198.51.100.1;for=203.0.113.7;proto=http"),
            Some(ip4(203, 0, 113, 7)),
            "for among by/proto"
        );
        assert_eq!(
            parse_forwarded_element("for=\"203.0.113.7:51000\""),
            Some(ip4(203, 0, 113, 7)),
            "quoted v4:port"
        );
        assert_eq!(
            parse_forwarded_element("for=\"[2001:db8::1]:443\""),
            "2001:db8::1".parse().ok(),
            "quoted [v6]:port"
        );
        assert_eq!(
            parse_forwarded_element("for=\"[2001:db8::1]\""),
            "2001:db8::1".parse().ok(),
            "quoted [v6] no port"
        );
        assert_eq!(parse_forwarded_element("for=_hidden"), None, "obfuscated");
        assert_eq!(parse_forwarded_element("for=unknown"), None, "unknown token");
        assert_eq!(parse_forwarded_element("for=\"\""), None, "empty quoted");
        assert_eq!(parse_forwarded_element("proto=https;by=198.51.100.1"), None, "no for=");
        assert_eq!(parse_forwarded_element(""), None, "empty element");
        assert_eq!(parse_forwarded_element("for=\"203.0.113.7"), None, "unbalanced leading quote");
        assert_eq!(parse_forwarded_element("for=203.0.113.7\""), None, "stray trailing quote");
        assert_eq!(parse_forwarded_element("for=\"a\"b\""), None, "embedded quote");
    }

    /// parse_forwarded_for_nodes flattens the header's comma-separated elements —
    /// and multiple `Forwarded` lines — into the ordered for= hops left-to-right
    /// (farthest→nearest), and is empty with no header so resolve falls to XFF.
    #[test]
    fn parse_forwarded_for_nodes_orders_and_flattens() {
        assert_eq!(
            parse_forwarded_for_nodes(&fwd("for=6.6.6.6, for=203.0.113.7")),
            vec![Some(ip4(6, 6, 6, 6)), Some(ip4(203, 0, 113, 7))],
            "one line, two elements, left-to-right"
        );
        let mut multi = HeaderMap::new();
        multi.append("forwarded", "for=6.6.6.6".parse().unwrap());
        multi.append("forwarded", "for=203.0.113.7".parse().unwrap());
        assert_eq!(
            parse_forwarded_for_nodes(&multi),
            vec![Some(ip4(6, 6, 6, 6)), Some(ip4(203, 0, 113, 7))],
            "multiple header lines flatten in order"
        );
        assert!(parse_forwarded_for_nodes(&HeaderMap::new()).is_empty(), "no header -> empty");
    }

    /// The default (empty) allowlist trusts no proxy, so XFF is ignored and the key
    /// is the peer — byte-identical to the pre-XFF behavior on every deployment.
    #[test]
    fn resolve_empty_allowlist_always_peer() {
        let t = TrustedProxies::default();
        assert_eq!(resolve_source_ip(ip(9), &xff("203.0.113.7"), &t), ip(9));
    }

    /// An untrusted peer can write anything in XFF; it is ignored and the peer is the
    /// key, so a direct attacker cannot forge a different source.
    #[test]
    fn resolve_untrusted_peer_ignores_xff() {
        let t = trusted(&[ip(1)]);
        assert_eq!(resolve_source_ip(ip(2), &xff("203.0.113.7"), &t), ip(2));
    }

    /// Behind one trusted proxy, the limiter keys on the client the proxy appended.
    #[test]
    fn resolve_trusted_proxy_single_client() {
        let t = trusted(&[ip(1)]);
        assert_eq!(resolve_source_ip(ip(1), &xff("203.0.113.7"), &t), ip4(203, 0, 113, 7));
    }

    /// Through a chain of two trusted proxies, the trusted hops are skipped and the
    /// external client (leftmost untrusted) is returned.
    #[test]
    fn resolve_trusted_chain_returns_external_client() {
        let t = trusted(&[ip(1), ip(2)]);
        // client -> ip(2) -> ip(1)=peer: XFF = "client, ip(2)" (ip(1) appended ip(2)).
        let h = xff(&format!("203.0.113.7, {}", ip(2)));
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip4(203, 0, 113, 7));
    }

    /// A forged XFF prepend sits LEFT of the real client the trusted proxy appended,
    /// so the rightmost-untrusted pick ignores it.
    #[test]
    fn resolve_spoofed_prepend_is_ignored() {
        let t = trusted(&[ip(1)]);
        let h = xff("6.6.6.6, 203.0.113.7"); // attacker prepended 6.6.6.6; proxy appended the client
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip4(203, 0, 113, 7));
    }

    /// A trusted peer with no XFF (the proxy didn't forward one) falls back to the peer.
    #[test]
    fn resolve_trusted_peer_no_xff_falls_back() {
        let t = trusted(&[ip(1)]);
        assert_eq!(resolve_source_ip(ip(1), &HeaderMap::new(), &t), ip(1));
        assert_eq!(resolve_source_ip(ip(1), &xff("   "), &t), ip(1), "blank XFF");
    }

    /// Every hop in the chain is a trusted proxy (no observed external client), so
    /// there is nothing untrusted to key on — fall back to the peer.
    #[test]
    fn resolve_all_trusted_chain_falls_back() {
        let t = trusted(&[ip(1), ip(2)]);
        let h = xff(&format!("{}, {}", ip(2), ip(2)));
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip(1));
    }

    /// The nearest (rightmost) hop is unparseable, so the source is indeterminate;
    /// fall back to the peer rather than reach further-left attacker-controlled data.
    #[test]
    fn resolve_unparseable_nearest_hop_falls_back() {
        let t = trusted(&[ip(1)]);
        let h = xff("203.0.113.7, _obfuscated");
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip(1));
    }

    /// An ip:port node from the proxy resolves to the bare client IP.
    #[test]
    fn resolve_strips_port_from_node() {
        let t = trusted(&[ip(1)]);
        assert_eq!(resolve_source_ip(ip(1), &xff("203.0.113.7:51000"), &t), ip4(203, 0, 113, 7));
    }

    /// SECURITY: X-Forwarded-For is flattened across MULTIPLE header lines, not just
    /// the first — a proxy that appends the observed client as a separate XFF line
    /// still places it as the nearest hop, so an attacker's earlier line cannot win
    /// the key. (With the old first-line-only read this returned 6.6.6.6.)
    #[test]
    fn resolve_xff_multiline_right_to_left() {
        let t = trusted(&[ip(1)]);
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", "6.6.6.6".parse().unwrap()); // attacker line, farthest
        h.append("x-forwarded-for", "203.0.113.7".parse().unwrap()); // proxy-appended, nearest
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip4(203, 0, 113, 7));
    }

    /// Behind one trusted proxy emitting RFC-7239, the limiter keys on the quoted
    /// `for=` client the proxy appended — the Forwarded twin of the XFF single-client
    /// case.
    #[test]
    fn resolve_forwarded_single_client() {
        let t = trusted(&[ip(1)]);
        let h = fwd("for=\"203.0.113.7\";proto=https");
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip4(203, 0, 113, 7));
    }

    /// A bracketed IPv6 `for=` with a port resolves to the bare client address.
    #[test]
    fn resolve_forwarded_bracketed_ipv6() {
        let t = trusted(&[ip(1)]);
        let h = fwd("for=\"[2001:db8::1]:443\"");
        assert_eq!(resolve_source_ip(ip(1), &h, &t), "2001:db8::1".parse::<IpAddr>().unwrap());
    }

    /// SECURITY: a quoted `for=` value bearing a comma is torn by the element split,
    /// but strict matched-quote handling fails both fragments to None, so no spurious
    /// victim hop is injected — resolution falls back to the conservative peer instead
    /// of keying on the attacker-named `9.9.9.9`.
    #[test]
    fn resolve_forwarded_quoted_comma_injection_falls_back() {
        let t = trusted(&[ip(1)]);
        assert_eq!(resolve_source_ip(ip(1), &fwd("for=\"a, for=9.9.9.9\""), &t), ip(1));
    }

    /// An obfuscated nearest `for=` is indeterminate, so the source falls back to the
    /// peer rather than reaching a farther-left attacker-controlled hop.
    #[test]
    fn resolve_forwarded_obfuscated_falls_back() {
        let t = trusted(&[ip(1)]);
        assert_eq!(resolve_source_ip(ip(1), &fwd("for=_hidden"), &t), ip(1));
    }

    /// Through a chain of two trusted proxies, the trusted nearest hop is skipped
    /// right-to-left and the external `for=` client is returned.
    #[test]
    fn resolve_forwarded_chain_skips_trusted() {
        let t = trusted(&[ip(1), ip(2)]);
        // client -> ip(2) -> ip(1)=peer: Forwarded = "for=client, for=ip(2)".
        let h = fwd(&format!("for=203.0.113.7, for=\"{}\"", ip(2)));
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip4(203, 0, 113, 7));
    }

    /// A forged `for=` prepend sits LEFT of the real client the trusted proxy
    /// appended, so the rightmost-untrusted pick ignores it.
    #[test]
    fn resolve_forwarded_spoofed_prepend_ignored() {
        let t = trusted(&[ip(1)]);
        let h = fwd("for=6.6.6.6, for=203.0.113.7");
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip4(203, 0, 113, 7));
    }

    /// The right-to-left walk spans multiple `Forwarded` header LINES (each a hop),
    /// not just comma-separated elements within one line.
    #[test]
    fn resolve_forwarded_multiline_right_to_left() {
        let t = trusted(&[ip(1)]);
        let mut h = HeaderMap::new();
        h.append("forwarded", "for=6.6.6.6".parse().unwrap());
        h.append("forwarded", "for=203.0.113.7".parse().unwrap());
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip4(203, 0, 113, 7));
    }

    /// An untrusted peer can write anything in Forwarded; it is ignored and the peer
    /// is the key, mirroring the XFF untrusted-peer guard.
    #[test]
    fn resolve_untrusted_peer_ignores_forwarded() {
        let t = trusted(&[ip(1)]);
        assert_eq!(resolve_source_ip(ip(2), &fwd("for=203.0.113.7"), &t), ip(2));
    }

    /// SECURITY/precedence: with BOTH headers present, X-Forwarded-For wins. This is
    /// the NO-REGRESSION order — an XFF-fronted deployment behaves exactly as it did
    /// before Forwarded was parsed, so an attacker behind it cannot inject a
    /// `Forwarded: for=...` to override the proxy's real XFF-attributed client
    /// (Forwarded-first would have allowed that bypass).
    #[test]
    fn resolve_xff_takes_precedence_over_forwarded() {
        let t = trusted(&[ip(1)]);
        let mut h = fwd("for=6.6.6.6"); // attacker-injected, passed through by the proxy
        h.insert("x-forwarded-for", "203.0.113.7".parse().unwrap()); // proxy's real XFF
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip4(203, 0, 113, 7));
    }

    /// SECURITY: a PRESENT-but-junk XFF collapses to the peer and does NOT fall
    /// through to a `Forwarded` header — so once XFF is authoritative an attacker
    /// cannot pair an unparseable XFF with a forged Forwarded to win the looser key.
    #[test]
    fn resolve_junk_xff_does_not_fall_through_to_forwarded() {
        let t = trusted(&[ip(1)]);
        let mut h = fwd("for=6.6.6.6");
        h.insert("x-forwarded-for", "   ".parse().unwrap()); // present but blank
        assert_eq!(resolve_source_ip(ip(1), &h, &t), ip(1));
    }

    /// The default (empty) allowlist trusts no proxy, so BOTH forwarded headers are
    /// ignored and the key is the peer — byte-identical to the pre-XFF behavior.
    #[test]
    fn resolve_empty_allowlist_ignores_forwarded_and_xff() {
        let t = TrustedProxies::default();
        let mut h = fwd("for=1.1.1.1");
        h.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        assert_eq!(resolve_source_ip(ip(9), &h, &t), ip(9));
    }

    /// A literal IPv4 helper distinct from the loopback `ip(n)` test sources.
    fn ip4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::from([a, b, c, d])
    }

    /// Empty-queue state with a small earner-registry cap, for the cap tests.
    async fn test_state_empty_with_earner_cap(max_earners: usize) -> Arc<AppState> {
        let state = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { max_earners, ..test_config() },
        )
        .unwrap();
        drain_seeded_jobs(&state).await;
        state
    }

    /// Empty-queue state with a small per-source registration allowance and a
    /// trusted-proxy allowlist, for the XFF-attribution rate-limit tests.
    async fn test_state_registrations_trusted(
        max_registrations: u32,
        trusted: TrustedProxies,
    ) -> Arc<AppState> {
        let state = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { max_registrations, trusted_proxies: trusted, ..test_config() },
        )
        .unwrap();
        drain_seeded_jobs(&state).await;
        state
    }

    /// Empty-queue state with a small per-source registration allowance, for the
    /// rate-limit tests.
    async fn test_state_empty_with_registrations(max_registrations: u32) -> Arc<AppState> {
        test_state_registrations_trusted(max_registrations, TrustedProxies::default()).await
    }

    /// Below the per-source allowance, registration is byte-identical to before — a
    /// configured but un-reached limit is invisible (all oneshot calls share the one
    /// loopback `test_peer`, so they draw on the same bucket).
    #[tokio::test]
    async fn register_below_rate_limit_is_unchanged() {
        let state = test_state_empty_with_registrations(5).await;
        for label in ["ra", "rb", "rc"] {
            assert_eq!(
                register_hello(&state, &hello(label, 24, vec![JobKind::Terrain])).await,
                StatusCode::OK
            );
        }
        let stats = body_json(get(state, "/stats").await).await;
        assert_eq!(stats["gpus_joined"], 3);
    }

    /// Over the per-source allowance, a registration from the same source is shed with
    /// `429 Too Many Requests` and admits nothing — the bucket is per source, so all
    /// loopback oneshots share it and the 3rd attempt at a cap of 2 is rejected.
    #[tokio::test]
    async fn register_over_rate_limit_returns_429() {
        let state = test_state_empty_with_registrations(2).await;
        assert_eq!(register_hello(&state, &hello("ra", 24, vec![JobKind::Terrain])).await, StatusCode::OK);
        assert_eq!(register_hello(&state, &hello("rb", 24, vec![JobKind::Terrain])).await, StatusCode::OK);
        assert_eq!(
            register_hello(&state, &hello("rc", 24, vec![JobKind::Terrain])).await,
            StatusCode::TOO_MANY_REQUESTS
        );
        // The shed registration admitted nothing — only the two under the limit count.
        let stats = body_json(get(state, "/stats").await).await;
        assert_eq!(stats["gpus_joined"], 2);
    }

    /// FM2 (cheap-reject-first): the rate check precedes `validate_hello`, so an
    /// over-limit attempt is `429` even when its Hello is malformed — it never reaches
    /// the structural/`signature` validation that would otherwise return `400`.
    #[tokio::test]
    async fn register_rate_limit_precedes_validation() {
        let state = test_state_empty_with_registrations(1).await;
        assert_eq!(register_hello(&state, &hello("solo", 24, vec![JobKind::Terrain])).await, StatusCode::OK);
        // Same source, over the limit, AND malformed (claims an address it can't sign
        // for). Rate-limited first → 429, not the 400 a reached validation would give.
        assert_eq!(
            register_hello(&state, &hello_claiming("0xnope", 24, vec![JobKind::Terrain])).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    /// Two distinct clients behind the SAME trusted proxy are attributed to SEPARATE
    /// buckets via X-Forwarded-For: one client exhausting its allowance does not
    /// throttle the other. Were the limiter still keyed on the proxy peer, the second
    /// client's first registration would be shed — this is the whole point of XFF
    /// attribution, exercised end-to-end through the HTTP `/register` handler.
    #[tokio::test]
    async fn register_trusted_proxy_separates_xff_clients() {
        let proxy = ip(1); // 10.0.0.1, the trusted reverse proxy
        let state = test_state_registrations_trusted(1, trusted(&[proxy])).await;
        let peer = SocketAddr::new(proxy, 4000);

        // Client X registers through the proxy — its own bucket, allowed.
        assert_eq!(
            register_from(&state, peer, Some("203.0.113.10"), &hello("cx", 24, vec![JobKind::Terrain])).await,
            StatusCode::OK
        );
        // Client X again, over its cap of 1 → shed.
        assert_eq!(
            register_from(&state, peer, Some("203.0.113.10"), &hello("cx2", 24, vec![JobKind::Terrain])).await,
            StatusCode::TOO_MANY_REQUESTS
        );
        // Client Y, same proxy peer but a different XFF client → its own bucket,
        // unaffected by X having exhausted theirs.
        assert_eq!(
            register_from(&state, peer, Some("203.0.113.20"), &hello("cy", 24, vec![JobKind::Terrain])).await,
            StatusCode::OK
        );
        // Exactly the two admitted clients joined; X's second attempt admitted nothing.
        let stats = body_json(get(state, "/stats").await).await;
        assert_eq!(stats["gpus_joined"], 2);
    }

    /// An UNTRUSTED peer's X-Forwarded-For is ignored for bucketing: two forged XFF
    /// clients from one direct peer collapse onto the peer's single bucket, so a
    /// direct attacker cannot spoof distinct XFF values to manufacture separate
    /// allowances and dodge the per-source limit.
    #[tokio::test]
    async fn register_untrusted_peer_ignores_xff_for_bucketing() {
        // Empty allowlist: no proxy trusted, so XFF is never honored.
        let state = test_state_empty_with_registrations(1).await;
        let peer = SocketAddr::new(ip(7), 5000); // 10.0.0.7, a direct (untrusted) peer

        assert_eq!(
            register_from(&state, peer, Some("203.0.113.10"), &hello("da", 24, vec![JobKind::Terrain])).await,
            StatusCode::OK
        );
        // Different forged XFF, same peer → same bucket → shed at cap 1.
        assert_eq!(
            register_from(&state, peer, Some("203.0.113.99"), &hello("db", 24, vec![JobKind::Terrain])).await,
            StatusCode::TOO_MANY_REQUESTS
        );
        let stats = body_json(get(state, "/stats").await).await;
        assert_eq!(stats["gpus_joined"], 1);
    }

    /// Register `msg` over HTTP `/register`, returning the status.
    async fn register_hello(state: &Arc<AppState>, msg: &EarnerMsg) -> StatusCode {
        post_json(state.clone(), "/register", &serde_json::to_value(msg).unwrap())
            .await
            .status()
    }

    /// Register `msg` over HTTP `/register` from a specific connection `peer`,
    /// optionally carrying an `X-Forwarded-For` header — the two inputs
    /// `resolve_source_ip` keys the per-source limiter on. Returns the status.
    async fn register_from(
        state: &Arc<AppState>,
        peer: SocketAddr,
        xff: Option<&str>,
        msg: &EarnerMsg,
    ) -> StatusCode {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/register")
            .header("content-type", "application/json");
        if let Some(v) = xff {
            builder = builder.header("x-forwarded-for", v);
        }
        router(state.clone())
            .layer(Extension(PeerAddr(peer)))
            .oneshot(builder.body(Body::from(serde_json::to_vec(msg).unwrap())).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// FM3: below the cap, registration behaves exactly as before — a configured but
    /// un-reached cap is invisible. One earner registers and surfaces in `/stats`.
    #[tokio::test]
    async fn register_below_cap_is_unchanged() {
        let state = test_state_empty_with_earner_cap(5).await;
        assert_eq!(
            register_hello(&state, &hello("below", 24, vec![JobKind::Terrain])).await,
            StatusCode::OK
        );
        let stats = body_json(get(state, "/stats").await).await;
        assert_eq!(stats["gpus_joined"], 1);
        assert_eq!(stats["total_vram_gb"], 24);
    }

    /// FM1/FM2 through the real HTTP handler: at the cap with all earners live, a new
    /// registration is shed with a retryable 503 and no live earner is displaced.
    #[tokio::test]
    async fn register_at_cap_all_live_returns_503() {
        let state = test_state_empty_with_earner_cap(2).await;
        assert_eq!(register_hello(&state, &hello("rega", 24, vec![JobKind::Terrain])).await, StatusCode::OK);
        assert_eq!(register_hello(&state, &hello("regb", 24, vec![JobKind::Terrain])).await, StatusCode::OK);
        assert_eq!(
            register_hello(&state, &hello("regc", 24, vec![JobKind::Terrain])).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(body_json(get(state.clone(), "/stats").await).await["gpus_joined"], 2);
        let earners = state.earners.lock().await;
        assert!(!earners.contains_key(&test_address("regc")), "the newcomer was not admitted");
    }

    /// FM1 through the real HTTP handler: at the cap, a registration evicts the
    /// stalest past-TTL earner to make room. "rega" is aged past its TTL, so the
    /// next registration reclaims its slot while the live "regb" is kept.
    #[tokio::test]
    async fn register_at_cap_evicts_stalest_past_ttl() {
        let state = test_state_empty_with_earner_cap(2).await;
        assert_eq!(register_hello(&state, &hello("rega", 24, vec![JobKind::Terrain])).await, StatusCode::OK);
        assert_eq!(register_hello(&state, &hello("regb", 24, vec![JobKind::Terrain])).await, StatusCode::OK);
        // Age "rega" past the 60s TTL so it becomes evictable.
        state.earners.lock().await.get_mut(&test_address("rega")).unwrap().last_seen = now_secs() - 10_000;
        assert_eq!(register_hello(&state, &hello("regc", 24, vec![JobKind::Terrain])).await, StatusCode::OK);
        let earners = state.earners.lock().await;
        assert_eq!(earners.len(), 2);
        assert!(!earners.contains_key(&test_address("rega")), "the stale earner was evicted");
        assert!(earners.contains_key(&test_address("regb")), "the live earner was kept");
        assert!(earners.contains_key(&test_address("regc")));
    }

    /// FM4 parity: the WS registration path enforces the SAME cap (both transports
    /// share `admit_earner`). With the cap filled by a live HTTP-registered earner, a
    /// ws Hello for a new distinct earner is rejected and the socket closed.
    #[tokio::test]
    async fn ws_registration_enforces_earner_cap() {
        let state = test_state_empty_with_earner_cap(1).await;
        assert_eq!(register_hello(&state, &hello("wsfull", 24, vec![JobKind::Terrain])).await, StatusCode::OK);
        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let nonce = recv_challenge(&mut ws).await;
        let sk = test_signing_key("wsnew");
        let hello_msg = signed_hello_with_nonce(
            &sk,
            &address_from_signing_key(&sk),
            "RTX 4090",
            24,
            vec![JobKind::Terrain],
            &nonce,
        );
        ws.send(WsMessage::text(serde_json::to_string(&hello_msg).unwrap()))
            .await
            .unwrap();
        expect_ws_closed(&mut ws).await;
        assert_eq!(
            body_json(get(state.clone(), "/stats").await).await["gpus_joined"],
            1,
            "the ws newcomer must not register past the cap",
        );
        assert!(!state.earners.lock().await.contains_key(&test_address("wsnew")));
    }

    /// A zero earner cap is rejected at construction (it would reject every
    /// registration the moment the map is non-empty), mirroring the other zero-knobs.
    #[test]
    fn with_store_rejects_zero_max_earners() {
        let r = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { max_earners: 0, ..test_config() },
        );
        assert!(r.is_err(), "zero max_earners must be rejected at construction");
    }

    /// A zero registration allowance is rejected at construction (it would shed every
    /// registration the moment a source's bucket is created), mirroring the other
    /// zero-knobs.
    #[test]
    fn with_store_rejects_zero_max_registrations() {
        let r = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { max_registrations: 0, ..test_config() },
        );
        assert!(r.is_err(), "zero max_registrations must be rejected at construction");
    }

    /// FM1: the queued-depth COUNT gating the cap must be served by
    /// `idx_jobs_status_created_at`, never a full scan of the jobs table (which
    /// carries unbounded terminal `completed`/`failed` history) — otherwise the very
    /// flood the cap guards would make each cap check O(table) and amplify the DoS.
    /// Since the cap holds, queued ≤ cap, so an index-served count visits ≤ cap
    /// entries — bounded.
    #[test]
    fn queue_cap_count_query_plan_uses_the_index() {
        let store = Store::open_in_memory().unwrap();
        store.enqueue(&seed_job()).unwrap();
        store.enqueue(&seed_job()).unwrap();
        // The plan of the REAL enqueue_within_cap statement (INSERT...SELECT...WHERE
        // (subquery COUNT) < cap), not a standalone proxy for the subquery.
        let plan = store.enqueue_within_cap_query_plan(10).unwrap();
        assert!(
            plan.contains("idx_jobs_status_created_at"),
            "the cap's queued COUNT must use the index, got plan: {plan}"
        );
        assert!(
            !plan.contains("SCAN jobs\n") && !plan.ends_with("SCAN jobs"),
            "the cap's queued COUNT must not full-scan jobs, got plan: {plan}"
        );
    }

    #[test]
    fn args_max_queued_jobs_default_and_override() {
        assert_eq!(
            Args::parse_from(["coordinator"]).max_queued_jobs,
            DEFAULT_MAX_QUEUED_JOBS,
            "unset default == the const"
        );
        assert_eq!(
            Args::parse_from(["coordinator", "--max-queued-jobs", "42"]).max_queued_jobs,
            42,
            "the flag is honored"
        );
    }

    #[test]
    fn args_relay_batch_size_default_and_override() {
        assert_eq!(
            Args::parse_from(["coordinator"]).relay_batch_size,
            DEFAULT_RELAY_BATCH_SIZE,
            "unset default == the const"
        );
        assert_eq!(
            Args::parse_from(["coordinator", "--relay-batch-size", "8"]).relay_batch_size,
            8,
            "the flag is honored"
        );
    }

    #[test]
    fn args_compute_rate_wei_default_and_override() {
        assert_eq!(
            Args::parse_from(["coordinator"]).compute_rate_wei,
            DEFAULT_COMPUTE_RATE_WEI,
            "unset default == the const (0 = metering disabled, opt-in)"
        );
        assert_eq!(DEFAULT_COMPUTE_RATE_WEI, 0, "the metering default must be disabled");
        // A 1e18-scale rate must parse as u128 (it overflows u64/i64) — proves the
        // knob can carry a realistic wei-per-render-second value.
        assert_eq!(
            Args::parse_from(["coordinator", "--compute-rate-wei", "1000000000000000000"]).compute_rate_wei,
            1_000_000_000_000_000_000,
            "the flag is honored at 1e18 scale"
        );
    }

    /// Graceful shutdown is preserved by the hand-rolled accept loop: firing the
    /// shutdown signal makes `serve()` stop accepting and return (drain), not hang
    /// — guards the FM that replacing `axum::serve`'s `with_graceful_shutdown`
    /// could leave the server unable to shut down.
    #[tokio::test]
    async fn serve_returns_on_shutdown_signal() {
        let state = test_state_empty().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = router(state);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            serve(listener, app, DEFAULT_HTTP_HEADER_TIMEOUT, DEFAULT_MAX_CONNECTIONS, async {
                let _ = rx.await;
            })
            .await
        });
        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("serve() returned promptly after the shutdown signal")
            .unwrap()
            .unwrap();
    }

    /// The other half of graceful shutdown (FM1): a connection that is LIVE in the
    /// drain set when shutdown fires is drained and closed, and `serve()` still
    /// returns. We first round-trip a request so the keep-alive connection is
    /// provably accepted into the `JoinSet` (not still in the listener backlog),
    /// then signal shutdown and assert both that `serve()` returns and that the
    /// drained connection is closed (EOF), not left dangling. Discriminating: a
    /// rewrite that drops live connections instead of `graceful_shutdown()`-ing
    /// them, or that hangs the drain on the idle keep-alive conn, fails here.
    #[tokio::test]
    async fn serve_drains_a_live_connection_on_shutdown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let state = test_state_empty().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let app = router(state);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            serve(listener, app, DEFAULT_HTTP_HEADER_TIMEOUT, DEFAULT_MAX_CONNECTIONS, async {
                let _ = rx.await;
            })
            .await
        });
        let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
        stream
            .write_all(b"GET /stats HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut head = [0u8; 12];
        stream.read_exact(&mut head).await.unwrap();
        assert!(
            head.starts_with(b"HTTP/1.1"),
            "the live keep-alive connection was served before shutdown"
        );
        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("serve() drained the live connection and returned")
            .unwrap()
            .unwrap();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut rest))
            .await
            .expect("the drained connection was closed (EOF), not left open")
            .ok();
    }

    /// A live ws session must not block graceful shutdown. axum drives the ws
    /// message loop on a detached task and the served connection future completes
    /// at the 101 upgrade handoff, so `serve()` returns promptly on shutdown even
    /// with a connected earner — it does NOT await the upgraded socket's close.
    /// A long handshake timeout keeps the session from self-terminating inside the
    /// window, so a regression where the drain awaited the ws to close would hang
    /// past the bound and fail here.
    #[tokio::test]
    async fn serve_returns_on_shutdown_with_live_ws_session() {
        let state = test_state_empty_handshake(Duration::from_secs(30)).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let app = router(state);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            serve(listener, app, DEFAULT_HTTP_HEADER_TIMEOUT, DEFAULT_MAX_CONNECTIONS, async {
                let _ = rx.await;
            })
            .await
        });
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        // Drain the challenge so the upgrade + detached ws_session are provably live.
        recv_challenge(&mut ws).await;
        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("serve() returned on shutdown despite a live ws session")
            .unwrap()
            .unwrap();
        drop(ws);
    }

    /// The concurrent-connection cap is enforced: with the cap at 1, a first
    /// keep-alive connection holds the only permit and a second connection accepted
    /// past the cap is dropped without serving (read returns EOF, no bytes), while
    /// the first is unaffected. Discriminating: without the cap the second
    /// connection is accepted and parks in the (generous-default) header-read
    /// phase, so its `read_to_end` hangs past the outer bound and fails the suite.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_caps_concurrent_connections() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let state = test_state_empty().await;
        let addr = serve_ephemeral_with_max_connections(state, 1).await;
        // Conn #1: complete a request and keep the socket open (idle keep-alive),
        // so its connection task — and the single permit — stay live for the test.
        let mut c1 = tokio::net::TcpStream::connect(&addr).await.unwrap();
        c1.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut head = [0u8; 12];
        c1.read_exact(&mut head).await.unwrap();
        assert!(head.starts_with(b"HTTP/1.1 200"), "conn #1 was served");
        // Two more connections past the cap of 1: each is dropped without serving
        // (read returns EOF, no bytes). The SECOND rejection also proves conn #1
        // still occupies its permit slot — were its permit wrongly released early,
        // the freed slot would let one of these be accepted and park in header-read,
        // hanging read_to_end past the outer bound.
        // Discriminating: without the cap these are accepted and park, not closed.
        for label in ["#2", "#3"] {
            let mut c = tokio::net::TcpStream::connect(&addr).await.unwrap();
            let mut buf = Vec::new();
            tokio::time::timeout(Duration::from_secs(3), c.read_to_end(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("conn {label} past the cap was closed promptly, not parked"))
                .ok();
            assert!(buf.is_empty(), "conn {label} was closed without serving, got {buf:?}");
        }
    }

    /// The cap admits EXACTLY N: with the cap at 2, two keep-alive connections are
    /// both served (holding both permits) and a third is dropped without serving.
    /// Pins against an off-by-one in the permit↔connection mapping (a cap of
    /// `max-1`, or two permits acquired per connection) that the cap-of-1 test
    /// cannot distinguish from correct behavior.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_cap_admits_exactly_n() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let state = test_state_empty().await;
        let addr = serve_ephemeral_with_max_connections(state, 2).await;
        // Two within-cap connections: each completes a request and stays open
        // (keep-alive), so both permits are held for the rest of the test.
        let mut held = Vec::new();
        for n in 1..=2 {
            let mut c = tokio::net::TcpStream::connect(&addr).await.unwrap();
            c.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
            let mut head = [0u8; 12];
            tokio::time::timeout(Duration::from_secs(3), c.read_exact(&mut head))
                .await
                .unwrap_or_else(|_| panic!("within-cap connection #{n} was served"))
                .unwrap();
            assert!(head.starts_with(b"HTTP/1.1 200"), "within-cap connection #{n} was served");
            held.push(c);
        }
        // The third, past the cap of 2, is dropped without serving.
        let mut c3 = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), c3.read_to_end(&mut buf))
            .await
            .expect("the over-cap connection was closed promptly, not parked")
            .ok();
        assert!(buf.is_empty(), "the over-cap connection was closed without serving, got {buf:?}");
        drop(held);
    }

    /// Shutdown returns even with the cap saturated (FM4): the cap is a
    /// `try_acquire` that never blocks the accept/shutdown select, and the permit is
    /// released when the drained task ends, so a full cap can't wedge shutdown. We
    /// saturate a cap of 1 with a live keep-alive connection, signal shutdown, and
    /// assert `serve()` returns and the held connection drains closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_returns_on_shutdown_when_cap_saturated() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let state = test_state_empty().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let app = router(state);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            serve(listener, app, DEFAULT_HTTP_HEADER_TIMEOUT, 1, async {
                let _ = rx.await;
            })
            .await
        });
        let mut c1 = tokio::net::TcpStream::connect(&addr).await.unwrap();
        c1.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut head = [0u8; 12];
        c1.read_exact(&mut head).await.unwrap();
        assert!(head.starts_with(b"HTTP/1.1 200"), "the cap-saturating connection was served");
        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("serve() returned on shutdown with the cap saturated")
            .unwrap()
            .unwrap();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), c1.read_to_end(&mut rest))
            .await
            .expect("the drained connection was closed")
            .ok();
    }

    /// A zero cap is rejected at construction: `Semaphore::new(0)` would reject
    /// every connection (a total outage), so `serve()` returns `Err` before binding
    /// the accept loop, mirroring the header-timeout zero guard.
    #[tokio::test]
    async fn serve_rejects_zero_max_connections() {
        let state = test_state_empty().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = router(state);
        let err = serve(listener, app, DEFAULT_HTTP_HEADER_TIMEOUT, 0, std::future::pending::<()>())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("max_connections must be > 0"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn ws_offer_accept_submit_flows_to_completed() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        // 1. Hello
        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();

        // 2. Expect a JobOffer for the seeded job.
        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);
        assert_eq!(offered.kind, JobKind::Terrain);

        // 3. Accept then 4. submit a validly-signed result.
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Accept { job_id }).unwrap(),
        ))
        .await
        .unwrap();

        let result = signed_result(job_id, "deadbeef");
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Submit(result)).unwrap(),
        ))
        .await
        .unwrap();

        // 5. Expect Accepted with the placeholder attestation uid.
        let verdict = next_coordinator_msg(&mut ws).await;
        match verdict {
            CoordinatorMsg::Accepted {
                job_id: jid,
                attestation_uid,
            } => {
                assert_eq!(jid, job_id);
                assert_eq!(attestation_uid, PLACEHOLDER_ATTESTATION_UID);
            }
            other => panic!("expected Accepted, got {other:?}"),
        }

        // /stats reflects the completed job.
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 1);
        assert_eq!(json["gpus_joined"], 1);
    }

    #[tokio::test]
    async fn ws_invalid_signature_yields_rejected() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();

        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);

        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Accept { job_id }).unwrap(),
        ))
        .await
        .unwrap();

        // Corrupt the signature's trailing byte.
        let mut bad = signed_result(job_id, "deadbeef");
        bad.signature_hex.pop();
        bad.signature_hex.push('f');
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Submit(bad)).unwrap(),
        ))
        .await
        .unwrap();

        let verdict = next_coordinator_msg(&mut ws).await;
        match verdict {
            CoordinatorMsg::Rejected { job_id: jid, .. } => assert_eq!(jid, job_id),
            other => panic!("expected Rejected, got {other:?}"),
        }

        // Nothing reached `completed`.
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 0);
    }

    /// FM1/FM3 (ws path): a WS earner that registers under a mixed-case address has
    /// its genuine quality fault attributed to the canonical lowercase identity — the
    /// session address `recv_hello` returns, which keys the registry AND the fault
    /// ledger. Without that normalization the fault would land under the case variant,
    /// splitting the count so the max-faults dead-letter budget is never reached.
    #[tokio::test]
    async fn ws_mixed_case_registration_faults_under_one_canonical_identity() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let sk = dev_signing_key();
        let canonical = address_from_signing_key(&sk);
        let upper = upper_case_variant(&canonical);
        assert_ne!(upper, canonical);

        let srv = serve_ephemeral(state.clone()).await;
        let (mut ws, _r) = tokio_tungstenite::connect_async(format!("ws://{srv}/ws"))
            .await
            .unwrap();
        let nonce = recv_challenge(&mut ws).await;
        // Register claiming the UPPER-case variant (signed over the nonce + that case).
        let hello = signed_hello_with_nonce(&sk, &upper, "RTX 4090", 24, vec![JobKind::Terrain], &nonce);
        ws.send(WsMessage::text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();

        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);

        // Accept, then submit a bad-signature result — a genuine earner fault.
        ws.send(WsMessage::text(serde_json::to_string(&EarnerMsg::Accept { job_id }).unwrap()))
            .await
            .unwrap();
        let mut bad = signed_result(job_id, "deadbeef");
        bad.signature_hex.pop();
        bad.signature_hex.push('f');
        ws.send(WsMessage::text(serde_json::to_string(&EarnerMsg::Submit(bad)).unwrap()))
            .await
            .unwrap();
        let verdict = next_coordinator_msg(&mut ws).await;
        assert!(matches!(verdict, CoordinatorMsg::Rejected { .. }), "the faulty submit is rejected");

        // The fault is attributed AFTER the Rejected reply is sent, so poll for it.
        let want = canonical.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state.store.lock().await.faults_by_earner().unwrap().get(&want).copied() == Some(1) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the fault is recorded under the canonical identity");

        // Registry and fault ledger agree on the ONE canonical identity — never the
        // case variant the client sent.
        let faults = state.store.lock().await.faults_by_earner().unwrap();
        assert!(!faults.contains_key(&upper), "the fault is not split onto the mixed-case variant");
        let earners = state.earners.lock().await;
        assert!(earners.contains_key(&canonical), "the registry keys on the canonical identity");
        assert!(!earners.contains_key(&upper), "not the mixed case");
    }

    /// A ws offer that goes stale before Submit — the job is dead-lettered out
    /// from under the session — is rejected, and the earner's late, validly
    /// signed result neither completes the job nor resurrects it from `failed`.
    /// Guards the double-credit / orphaned-settle failure mode on the ws path.
    #[tokio::test]
    async fn ws_submit_for_stale_offer_rejected_without_resurrecting() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        // Hello → JobOffer → Accept: the job is now in_flight under us.
        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();
        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Accept { job_id }).unwrap(),
        ))
        .await
        .unwrap();

        // Dead-letter the job out from under the session: at one attempt with
        // max_attempts=1, requeue moves it to the terminal `failed` state (what
        // the reaper would do once attempts are exhausted).
        {
            let store = state.store.lock().await;
            assert!(
                store.requeue(&offered, 1).unwrap(),
                "attempts>=max must dead-letter the job"
            );
            assert_eq!(
                store.job_status(&job_id).unwrap().as_deref(),
                Some("failed")
            );
        }

        // Submit a validly-signed result for the now-stale offer.
        let result = signed_result(job_id, "deadbeef");
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Submit(result)).unwrap(),
        ))
        .await
        .unwrap();

        // The verdict is Rejected, not Accepted.
        match next_coordinator_msg(&mut ws).await {
            CoordinatorMsg::Rejected { job_id: jid, .. } => assert_eq!(jid, job_id),
            other => panic!("expected Rejected for a stale offer, got {other:?}"),
        }

        // The job stays failed (not resurrected to done); nothing was credited.
        assert_eq!(
            state
                .store
                .lock()
                .await
                .job_status(&job_id)
                .unwrap()
                .as_deref(),
            Some("failed"),
            "stale submit must not resurrect a dead-lettered job"
        );
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["jobs_completed"], 0,
            "stale submit must not be credited"
        );
        assert_eq!(json["jobs_failed"], 1);
    }

    /// FM4 end-to-end: an earner that submits a faulty result (bad signature) is
    /// Rejected, and the coordinator does NOT re-offer it the same job — the
    /// per-session skip set breaks the reject/re-offer hot loop. The job returns
    /// to the queue, renderable for another earner, and an earner fault never
    /// dead-letters it (it stays `queued`, not `failed`).
    #[tokio::test]
    async fn ws_earner_fault_does_not_reoffer_same_job_nor_dead_letter() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();
        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Accept { job_id }).unwrap(),
        ))
        .await
        .unwrap();

        // Submit a corrupted signature → earner fault (flip the last nibble to a
        // guaranteed-different value so the corruption never no-ops).
        let mut bad = signed_result(job_id, "deadbeef");
        let last = bad.signature_hex.pop().unwrap();
        bad.signature_hex.push(if last == 'f' { '0' } else { 'f' });
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Submit(bad)).unwrap(),
        ))
        .await
        .unwrap();
        match next_coordinator_msg(&mut ws).await {
            CoordinatorMsg::Rejected { job_id: jid, .. } => assert_eq!(jid, job_id),
            other => panic!("expected Rejected for the faulty result, got {other:?}"),
        }

        // The same earner must NOT be re-offered the faulted job. A re-offer would
        // arrive within a poll tick (~100ms); we wait well past that and assert no
        // further message comes — if the skip set regressed, a JobOffer would land
        // here and the timeout would NOT fire.
        let reoffer =
            tokio::time::timeout(Duration::from_millis(500), next_coordinator_msg(&mut ws)).await;
        assert!(
            reoffer.is_err(),
            "faulted job re-offered to the same earner: {reoffer:?}"
        );

        // The job itself is back on the queue, renderable for another earner —
        // not dead-lettered by the fault. (Asserted directly on the job rather
        // than via /stats in-flight counts, which include test_state_empty's
        // drained seed jobs.)
        assert_eq!(
            state
                .store
                .lock()
                .await
                .job_status(&job_id)
                .unwrap()
                .as_deref(),
            Some("queued"),
            "an earner fault must not dead-letter a renderable job"
        );
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_failed"], 0, "nothing dead-lettered");
        assert_eq!(json["jobs_completed"], 0, "nothing credited");
    }

    /// A ws earner that DECLINES an offered job (its capability self-guard fired)
    /// is not re-offered the same job, and the job returns to the queue
    /// renderable for another earner — never dead-lettered by the decline.
    /// Mirrors the earner-fault disposition but driven by an `EarnerMsg::Decline`
    /// instead of a faulty Submit, and proves the coordinator acts on Decline (a
    /// dropped/undecoded decline would leave the job stuck `in_flight`).
    #[tokio::test]
    async fn ws_decline_requeues_job_and_does_not_reoffer_to_same_earner() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();
        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);

        // Decline instead of Accept — never rendered, never accepted.
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Decline {
                job_id,
                reason: "unsupported job kind: terrain".into(),
            })
            .unwrap(),
        ))
        .await
        .unwrap();

        // The same earner must NOT be re-offered the declined job (the skip set).
        // A re-offer would arrive within a poll tick (~100ms); wait well past it
        // and assert nothing comes — a regressed skip set would land a JobOffer.
        let reoffer =
            tokio::time::timeout(Duration::from_millis(500), next_coordinator_msg(&mut ws)).await;
        assert!(
            reoffer.is_err(),
            "declined job re-offered to the same earner: {reoffer:?}"
        );

        // The job is back on the queue, renderable for another earner — not left
        // in_flight (which a dropped decline would do) and not dead-lettered.
        assert_eq!(
            state
                .store
                .lock()
                .await
                .job_status(&job_id)
                .unwrap()
                .as_deref(),
            Some("queued"),
            "a decline must requeue the job, not strand it in_flight or dead-letter it"
        );
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_failed"], 0, "nothing dead-lettered");
        assert_eq!(json["jobs_completed"], 0, "nothing credited");
    }

    /// A Decline for a job id we are NOT currently offering is ignored (the
    /// `_ => warn` arm) and must not disturb the offer we DO hold. Pins the
    /// `job.id == job_id` guard: a regression that requeued the offered job on
    /// any decline would clear the offer, and the follow-up Accept+Submit (sent
    /// after the stale decline, so processed after it in FIFO order) would no
    /// longer settle — so receiving `Accepted` proves the stale decline was a
    /// no-op and the held offer survived intact.
    #[tokio::test]
    async fn ws_decline_for_unoffered_job_leaves_the_held_offer_settleable() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();
        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);

        // Decline a DIFFERENT (random) job id — not the one we hold.
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Decline {
                job_id: Uuid::new_v4(),
                reason: "stale".into(),
            })
            .unwrap(),
        ))
        .await
        .unwrap();

        // Then accept + submit the real offer. FIFO ordering means this is
        // processed after the stale decline; an Accepted verdict proves the
        // offer was untouched.
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Accept { job_id }).unwrap(),
        ))
        .await
        .unwrap();
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Submit(signed_result(job_id, "deadbeef"))).unwrap(),
        ))
        .await
        .unwrap();

        match next_coordinator_msg(&mut ws).await {
            CoordinatorMsg::Accepted { job_id: jid, .. } => assert_eq!(jid, job_id),
            other => panic!("a stale decline must leave the offer settleable, got {other:?}"),
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["jobs_completed"], 1,
            "the held offer must still settle"
        );
    }

    /// The `faults` count for `earner` in the current `/earners` snapshot, or
    /// `None` if the earner is absent (not live / never registered).
    async fn earner_faults_now(state: &Arc<AppState>, earner: &str) -> Option<u64> {
        let json = body_json(get(state.clone(), "/earners").await).await;
        json.as_array()?
            .iter()
            .find(|e| e["address"] == earner)
            .and_then(|e| e["faults"].as_u64())
    }

    /// Poll `/earners` until `earner` reports `expected` faults. The ws task
    /// records attribution AFTER sending its Rejected verdict, so the row can lag
    /// the verdict by a scheduling tick. Panics with the last snapshot on timeout.
    async fn await_earner_faults(state: &Arc<AppState>, earner: &str, expected: u64) {
        for _ in 0..40 {
            if earner_faults_now(state, earner).await == Some(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let json = body_json(get(state.clone(), "/earners").await).await;
        panic!("earner {earner} never reached {expected} faults on /earners: {json}");
    }

    /// End-to-end: a genuine quality fault (forged signature) over ws is attributed
    /// to the submitting earner's REGISTERED address and surfaces as `faults: 1` on
    /// its `/earners` row — the per-earner breakdown the gross `/stats total_faults`
    /// could not give. Attribution runs in the ws task after the Rejected verdict,
    /// so we poll the row until it reflects the fault.
    #[tokio::test]
    async fn ws_genuine_fault_is_attributed_on_earners() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();
        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Accept { job_id }).unwrap(),
        ))
        .await
        .unwrap();

        // Forge a result that CLAIMS a victim's address: the dev-key signature
        // won't recover to it → AddressMismatch fault. This also pins the keying —
        // attribution must charge THIS session (dev_address, the dispatch holder),
        // never the self-reported `result.earner_address`, so a faulting earner
        // can't smear a victim.
        let victim = test_address("victim");
        let mut bad = signed_result(job_id, "deadbeef");
        bad.earner_address = victim.clone();
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Submit(bad)).unwrap(),
        ))
        .await
        .unwrap();
        match next_coordinator_msg(&mut ws).await {
            CoordinatorMsg::Rejected { job_id: jid, .. } => assert_eq!(jid, job_id),
            other => panic!("expected Rejected for the faulty result, got {other:?}"),
        }

        // Attributed to the session (dev_address); the claimed victim gets nothing
        // (it never registered, so it is absent from the leaderboard entirely).
        await_earner_faults(&state, &dev_address(), 1).await;
        assert_eq!(
            earner_faults_now(&state, &victim).await,
            None,
            "a faulting earner must not smear the address it claims in the result"
        );
    }

    /// End-to-end counterpart: an honest `Decline` of an offered job is requeued
    /// like a fault (refund + return to queue) but is NOT a reputation fault — the
    /// declining earner's `/earners` row stays at `faults: 0`. Both share the
    /// EarnerFault requeue path; only the genuine fault attributes. Discriminating:
    /// the faults==0 assertion fires only AFTER the job is back on the queue (the
    /// decline's requeue ran), so a regression that attributed the decline would
    /// show faults: 1 here.
    #[tokio::test]
    async fn ws_honest_decline_is_not_attributed_on_earners() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();
        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);

        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Decline {
                job_id,
                reason: "unsupported job kind: terrain".into(),
            })
            .unwrap(),
        ))
        .await
        .unwrap();

        // Wait until the decline is processed: the job is back on the queue (its
        // EarnerFault requeue ran). Only then is faults==0 meaningful — it proves
        // the requeue happened yet recorded no attribution.
        let mut requeued = false;
        for _ in 0..40 {
            if state.store.lock().await.job_status(&job_id).unwrap().as_deref() == Some("queued") {
                requeued = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(requeued, "decline must requeue the job");

        assert_eq!(
            earner_faults_now(&state, &dev_address()).await,
            Some(0),
            "an honest decline must NOT count as a reputation fault"
        );
    }

    /// FM3: the per-earner `/earners` faults reconcile with the gross `/stats`
    /// total_faults — Σ(attributed faults) <= total_faults, which ALSO counts the
    /// non-attributable declines. One session genuinely faults one job (attributed)
    /// then declines another (not attributed): both bump the per-job fault budget,
    /// so `/stats total_faults` is 2, but only the genuine fault is attributed, so
    /// the earner's `/earners faults` is 1. Catches a regression that double-counted
    /// declines into the per-earner tally.
    #[tokio::test]
    async fn earner_faults_reconcile_with_stats_total_faults() {
        let state = test_state_empty().await;
        let a = seed_job();
        let b = seed_job();
        enqueue(&state, &a).await;
        enqueue(&state, &b).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();

        // First offer → accept → forged signature (genuine fault, attributed).
        let first = match next_coordinator_msg(&mut ws).await {
            CoordinatorMsg::JobOffer(j) => j.id,
            other => panic!("expected first JobOffer, got {other:?}"),
        };
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Accept { job_id: first }).unwrap(),
        ))
        .await
        .unwrap();
        let mut bad = signed_result(first, "deadbeef");
        let last = bad.signature_hex.pop().unwrap();
        bad.signature_hex.push(if last == 'f' { '0' } else { 'f' });
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Submit(bad)).unwrap(),
        ))
        .await
        .unwrap();
        match next_coordinator_msg(&mut ws).await {
            CoordinatorMsg::Rejected { job_id: jid, .. } => assert_eq!(jid, first),
            other => panic!("expected Rejected, got {other:?}"),
        }

        // Next offer is the OTHER job (first is skip-set'd) → decline (not attributed).
        let second = match next_coordinator_msg(&mut ws).await {
            CoordinatorMsg::JobOffer(j) => {
                assert_ne!(j.id, first, "first is skip-set'd, must offer the other job");
                j.id
            }
            other => panic!("expected second JobOffer, got {other:?}"),
        };
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Decline {
                job_id: second,
                reason: "unsupported".into(),
            })
            .unwrap(),
        ))
        .await
        .unwrap();

        // The genuine fault is attributed (poll until it lands); the decline is not.
        await_earner_faults(&state, &dev_address(), 1).await;

        // Both faults bumped the per-job budget → /stats total_faults == 2 (poll:
        // the decline's bump runs in the ws task after we send Decline).
        let mut total = 0;
        for _ in 0..40 {
            total = body_json(get(state.clone(), "/stats").await).await["total_faults"]
                .as_u64()
                .unwrap();
            if total == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(total, 2, "fault + decline both bump the gross per-job fault budget");
        assert_eq!(
            earner_faults_now(&state, &dev_address()).await,
            Some(1),
            "only the genuine fault is attributed per earner (Σ attributed < total)"
        );
    }

    /// A `JobResult` validly signed by an arbitrary key (distinct from the dev
    /// key `signed_result` uses), credited to that key's derived address — lets a
    /// test assert credit lands on a SPECIFIC earner, not just "someone".
    fn signed_result_by(key_hex: &str, job_id: Uuid, label: &str) -> JobResult {
        let sk = SigningKey::from_slice(&hex::decode(key_hex).unwrap()).unwrap();
        let point = sk.verifying_key().to_encoded_point(false);
        let address = format!(
            "0x{}",
            hex::encode(&Keccak256::digest(&point.as_bytes()[1..])[12..])
        );
        let output_hash = test_output_hash(label);
        JobResult {
            job_id,
            earner_address: address,
            signature_hex: verify::sign_for_test(&sk, &job_id, &output_hash),
            output_hash,
            output_url: "memory://x".into(),
            render_seconds: 1,
        }
    }

    /// The per-dispatch fence (FM1 + FM2). After a job is reaped and reassigned to
    /// a new earner B, the previous holder A — submitting a perfectly valid,
    /// signed result for the same job — can neither settle it (no misattributed
    /// credit, FM2) nor requeue it on disconnect (no preemption of B, FM1). B,
    /// holding the current dispatch, still settles and is credited. Drives
    /// `handle_submit`/`requeue` directly with each holder's remembered seq.
    #[tokio::test]
    async fn stale_holder_cannot_settle_or_preempt_reassigned_job() {
        // hardhat account #1 key — a valid secp256k1 scalar distinct from the dev key.
        const KEY_B: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        // A takes the job: dispatch seq 1, in_flight under A.
        let (job_a, seq_a) = state
            .store
            .lock()
            .await
            .take_next(|_| true)
            .unwrap()
            .unwrap();
        assert_eq!((job_a.id, seq_a), (job_id, 1));

        // The deadline lapses and A's job returns to the queue (whether via the
        // reaper or A's own disconnect — both are in_flight→queued, seq preserved).
        {
            let store = state.store.lock().await;
            assert!(
                !store.requeue(&job_a, state.max_attempts).unwrap(),
                "requeued, not dead-lettered"
            );
            assert_eq!(
                store.job_status(&job_id).unwrap().as_deref(),
                Some("queued")
            );
        }
        // B takes the now-queued job: seq 2.
        let (job_b, seq_b) = state
            .store
            .lock()
            .await
            .take_next(|_| true)
            .unwrap()
            .unwrap();
        assert_eq!(
            (job_b.id, seq_b),
            (job_id, 2),
            "B's dispatch carries a fresh, higher seq"
        );

        // FM2: A (seq 1) submits a valid result, but the lease is B's (seq 2).
        // Must Drop, settle nothing, credit no one.
        let result_a = signed_result(job_id, "aaaa"); // dev key == earner A
        match handle_submit(&state, &Some((job_a.clone(), seq_a)), true, result_a).await {
            SubmitOutcome::Drop(CoordinatorMsg::Rejected { job_id: jid, .. }) => {
                assert_eq!(jid, job_id)
            }
            other => panic!("stale holder's submit must Drop, got {other:?}"),
        }
        {
            let store = state.store.lock().await;
            assert_eq!(
                store.job_status(&job_id).unwrap().as_deref(),
                Some("in_flight")
            );
            assert_eq!(
                store.completed_count().unwrap(),
                0,
                "stale submit credited nothing"
            );
            assert_eq!(store.current_dispatch_seq(&job_id).unwrap(), Some(2));
        }

        // FM1: A's socket drops → requeue with A's stale seq. Must not preempt B.
        requeue(&state, job_a, seq_a, RequeueKind::Charge, None).await;
        assert_eq!(
            state
                .store
                .lock()
                .await
                .job_status(&job_id)
                .unwrap()
                .as_deref(),
            Some("in_flight"),
            "stale holder's disconnect must not requeue B's in-flight job"
        );

        // B (seq 2) holds the current dispatch: it settles and is credited.
        let result_b = signed_result_by(KEY_B, job_id, "bbbb");
        let b_addr = result_b.earner_address.clone();
        assert_ne!(b_addr, dev_address(), "B must be a different earner than A");
        match handle_submit(&state, &Some((job_b, seq_b)), true, result_b).await {
            SubmitOutcome::Accepted(CoordinatorMsg::Accepted { job_id: jid, .. }) => {
                assert_eq!(jid, job_id)
            }
            other => panic!("current holder's submit must be Accepted, got {other:?}"),
        }
        let store = state.store.lock().await;
        assert_eq!(store.job_status(&job_id).unwrap().as_deref(), Some("done"));
        let credited = store.completed_count_by_earner().unwrap();
        assert_eq!(
            credited.get(&b_addr),
            Some(&1),
            "credit lands on B, the current holder"
        );
        assert_eq!(
            credited.len(),
            1,
            "and on no one else (A was never credited)"
        );
    }

    /// ws content gate: a Submit whose result fails the content gate (here a
    /// malformed output_hash, validly signed so the signature gate passes) is
    /// Requeued — the job, which may be perfectly renderable, returns to the
    /// queue for another earner — and nothing is settled. The reject reason is the
    /// content gate's. `handle_submit` returns Requeue; the caller's seq-fenced
    /// `requeue` then puts the job back, both driven here.
    #[tokio::test]
    async fn ws_submit_failing_content_gate_requeues_without_settling() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let (offered, seq) = state
            .store
            .lock()
            .await
            .take_next(|_| true)
            .unwrap()
            .unwrap();
        assert_eq!((offered.id, seq), (job_id, 1));

        let bad = signed_result_raw_hash(job_id, "deadbeef"); // valid sig, malformed hash
        match handle_submit(&state, &Some((offered.clone(), seq)), true, bad).await {
            SubmitOutcome::Requeue(
                CoordinatorMsg::Rejected {
                    job_id: jid,
                    reason,
                },
                kind,
            ) => {
                assert_eq!(jid, job_id);
                assert_eq!(
                    reason,
                    validate::ValidationError::MalformedOutputHash.reason()
                );
                // A content fault is an earner fault: don't charge the attempt.
                assert_eq!(kind, RequeueKind::EarnerFault);
            }
            other => panic!("expected Requeue with the content-gate reason, got {other:?}"),
        }

        // Nothing settled: still in_flight under our (only) dispatch.
        assert_eq!(
            state
                .store
                .lock()
                .await
                .job_status(&job_id)
                .unwrap()
                .as_deref(),
            Some("in_flight")
        );

        // The caller's seq-fenced earner-fault requeue then returns it to queued.
        requeue(&state, offered, seq, RequeueKind::EarnerFault, None).await;
        assert_eq!(
            state
                .store
                .lock()
                .await
                .job_status(&job_id)
                .unwrap()
                .as_deref(),
            Some("queued")
        );
    }

    /// WS twin of the render-seconds bound: an implausible value (u32::MAX vs a
    /// 60s deadline) is a content fault → Requeue (the job may be renderable by
    /// an honest earner), nothing settled. The deadline comes from the offered
    /// job, so the bound runs pre-lock with no store read.
    #[tokio::test]
    async fn ws_submit_with_implausible_render_seconds_requeues_without_settling() {
        let state = test_state_empty().await;
        let job = seed_job(); // deadline_secs = 60 → bound 120
        let job_id = job.id;
        enqueue(&state, &job).await;

        let (offered, seq) = state
            .store
            .lock()
            .await
            .take_next(|_| true)
            .unwrap()
            .unwrap();
        assert_eq!((offered.id, seq), (job_id, 1));

        let mut bad = signed_result(job_id, "deadbeef"); // valid sig + hash
        bad.render_seconds = u32::MAX; // the only defect
        match handle_submit(&state, &Some((offered.clone(), seq)), true, bad).await {
            SubmitOutcome::Requeue(
                CoordinatorMsg::Rejected {
                    job_id: jid,
                    reason,
                },
                kind,
            ) => {
                assert_eq!(jid, job_id);
                assert_eq!(
                    reason,
                    validate::ValidationError::ImplausibleRenderSeconds.reason()
                );
                assert_eq!(kind, RequeueKind::EarnerFault);
            }
            other => panic!("expected Requeue with the content-gate reason, got {other:?}"),
        }

        // Nothing settled: still in_flight under our (only) dispatch.
        assert_eq!(
            state
                .store
                .lock()
                .await
                .job_status(&job_id)
                .unwrap()
                .as_deref(),
            Some("in_flight")
        );
    }

    /// Every earner-attributable reject on a renderable job is tagged
    /// `EarnerFault`, so the requeue refunds the dispatch attempt instead of
    /// charging the renderability budget: bad signature, submit-before-accept,
    /// and a submit whose job_id doesn't match the offer. (The content-gate and
    /// render-seconds faults assert `EarnerFault` in their own tests above.)
    #[tokio::test]
    async fn handle_submit_tags_earner_attributable_rejects_as_earner_fault() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;
        let (offered, seq) = state
            .store
            .lock()
            .await
            .take_next(|_| true)
            .unwrap()
            .unwrap();

        fn assert_earner_fault(outcome: SubmitOutcome, ctx: &str) {
            match outcome {
                SubmitOutcome::Requeue(_, kind) => {
                    assert_eq!(
                        kind,
                        RequeueKind::EarnerFault,
                        "{ctx} must be an earner fault"
                    )
                }
                other => panic!("{ctx}: expected Requeue(EarnerFault), got {other:?}"),
            }
        }

        // Bad signature (accepted, id matches): flip the last hex nibble to a
        // guaranteed-different value so the corruption never no-ops.
        let mut bad_sig = signed_result(job_id, "deadbeef");
        let last = bad_sig.signature_hex.pop().unwrap();
        bad_sig
            .signature_hex
            .push(if last == 'f' { '0' } else { 'f' });
        assert_earner_fault(
            handle_submit(&state, &Some((offered.clone(), seq)), true, bad_sig).await,
            "bad signature",
        );

        // Submit before Accept (validly signed, accepted = false).
        assert_earner_fault(
            handle_submit(
                &state,
                &Some((offered.clone(), seq)),
                false,
                signed_result(job_id, "deadbeef"),
            )
            .await,
            "submit before accept",
        );

        // Submit whose job_id is not the offered job (a result for another job).
        assert_earner_fault(
            handle_submit(
                &state,
                &Some((offered.clone(), seq)),
                true,
                signed_result(Uuid::new_v4(), "x"),
            )
            .await,
            "job_id mismatch",
        );

        // None of these touched the store: the job is still in_flight, uncredited.
        let store = state.store.lock().await;
        assert_eq!(
            store.job_status(&job_id).unwrap().as_deref(),
            Some("in_flight")
        );
        assert_eq!(store.completed_count().unwrap(), 0);
    }

    /// FM4 mechanism: a job in the per-session `skip` set is not handed back to
    /// the same earner, so a faulting earner can't be re-offered the job it just
    /// faulted on (no reject/re-offer hot loop). The job stays queued for others.
    #[tokio::test]
    async fn take_supported_job_skips_faulted_jobs() {
        let state = test_state_empty().await;
        let a = job_with_deadline(60); // both Terrain (seed_job base), distinct ids
        let b = job_with_deadline(60);
        enqueue(&state, &a).await;
        enqueue(&state, &b).await;
        let supported = vec![JobKind::Terrain];

        // Both queued jobs skipped → nothing offerable (the earner idles).
        let mut skip: HashSet<Uuid> = HashSet::new();
        skip.insert(a.id);
        skip.insert(b.id);
        assert!(
            take_supported_job(&state, "0xtestearner", &supported, &skip)
                .await
                .is_none(),
            "all supported jobs skipped → no offer (no hot loop)"
        );

        // Drop b from skip: a stays skipped (rejected by the accept closure), so b
        // is the only offerable job and is handed back regardless of dispatch order.
        skip.remove(&b.id);
        let (taken, _) = take_supported_job(&state, "0xtestearner", &supported, &skip)
            .await
            .unwrap();
        assert_eq!(taken.id, b.id, "only the non-skipped job is offerable");

        // a remains renderable but skipped for THIS earner → still not offered,
        // and still queued (available to a different earner).
        assert!(
            take_supported_job(&state, "0xtestearner", &supported, &skip)
                .await
                .is_none(),
            "a is renderable but skipped for this earner"
        );
        assert_eq!(
            state
                .store
                .lock()
                .await
                .job_status(&a.id)
                .unwrap()
                .as_deref(),
            Some("queued")
        );
    }

    /// The WS dispatch path records the offering earner as the job's holder
    /// (`dispatched_to`), so the liveness reaper can later reclaim it if that earner
    /// goes stale — the integration of `take_supported_job` → `take_next_for`.
    #[tokio::test]
    async fn take_supported_job_records_the_earner_as_holder() {
        let state = test_state_empty().await;
        let job = job_with_deadline(60);
        enqueue(&state, &job).await;

        let (taken, _) =
            take_supported_job(&state, "0xwsearner", &[JobKind::Terrain], &HashSet::new())
                .await
                .unwrap();
        assert_eq!(taken.id, job.id);
        assert_eq!(
            state
                .store
                .lock()
                .await
                .job_dispatched_to(&job.id)
                .unwrap()
                .as_deref(),
            Some("0xwsearner"),
            "the WS dispatcher stamps the earner as the holder"
        );
    }

    /// HTTP-poll fence (FM3): `/jobs/next` stamps `dispatch_seq` in a header, and
    /// `/jobs/{id}/submit` must echo the CURRENT seq. A submit with no header is
    /// refused (can't be tied to a dispatch); one echoing a seq from a since-
    /// reassigned dispatch is refused; the current seq settles. Closes the race
    /// on the stateless path so it isn't silently left open behind the ws fence.
    #[tokio::test]
    async fn http_submit_is_fenced_on_the_dispatch_seq_header() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        // /jobs/next dispatches the job (seq 1) and stamps it in the header.
        let resp = get(state.clone(), "/jobs/next").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("x-dispatch-seq")
                .and_then(|v| v.to_str().ok()),
            Some("1"),
            "next stamps the dispatch seq"
        );

        let good = signed_result(job_id, "deadbeef");
        let uri = format!("/jobs/{job_id}/submit");

        // No fence header → can't be tied to a dispatch → 400.
        let resp = post_json(state.clone(), &uri, &serde_json::to_value(&good).unwrap()).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "submit without a fence header is refused"
        );

        // Reassign: requeue and re-dispatch, so the current seq is now 2.
        {
            let store = state.store.lock().await;
            assert!(
                !store.requeue(&job, state.max_attempts).unwrap(),
                "requeued, not dead-lettered"
            );
        }
        state.store.lock().await.take_next(|_| true).unwrap(); // seq -> 2

        // Echoing the stale seq 1 → 409 (lease reassigned); nothing credited.
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "stale dispatch seq is refused"
        );
        assert_eq!(state.store.lock().await.completed_count().unwrap(), 0);

        // Echoing the current seq 2 → accepted + settled.
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            2,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "current dispatch seq settles"
        );
        assert_eq!(state.store.lock().await.completed_count().unwrap(), 1);
    }

    /// Simulated restart: enqueue a job and record a completed result against
    /// one file-backed state, drop it, then reopen the SAME DB file into a
    /// fresh `AppState` and assert the queued/completed counts survived.
    #[tokio::test]
    async fn queue_and_results_survive_restart() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();

        // --- first "process": fresh DB (auto-seeds one job), enqueue another,
        //     and record a completed result. ---
        let queued_before;
        let completed_before;
        {
            let state = AppState::with_store(Store::open(&db_path).unwrap(), test_config()).unwrap();

            // Enqueue a second job and submit a validly-signed result for it.
            let job = seed_job();
            let job_id = job.id;
            enqueue(&state, &job).await;
            // Move it in_flight first (submit gate requires it). Select this job by
            // id — FIFO would otherwise hand out the older auto-seeded job — leaving
            // the auto-seeded job queued.
            let taken = state
                .store
                .lock()
                .await
                .take_next(|j| j.id == job_id)
                .unwrap();
            assert_eq!(taken.unwrap().0.id, job_id);

            let result = signed_result(job_id, "deadbeef");
            let uri = format!("/jobs/{}/submit", job_id);
            let resp = post_submit(
                state.clone(),
                &uri,
                &serde_json::to_value(&result).unwrap(),
                1,
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);

            let json = body_json(get(state.clone(), "/stats").await).await;
            // The auto-seeded jobs (one per kind) stay queued; the extra job we
            // enqueued and took is now done.
            queued_before = json["jobs_queued"].as_u64().unwrap();
            completed_before = json["jobs_completed"].as_u64().unwrap();
            assert_eq!(queued_before, JobKind::ALL.len() as u64);
            assert_eq!(completed_before, 1);
        } // state (and its Store/Connection) dropped here → "process" ends.

        // --- second "process": reopen the same file. with_store must NOT
        //     re-seed (jobs already exist), and counts must match. ---
        let state = AppState::with_store(Store::open(&db_path).unwrap(), test_config()).unwrap();
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["jobs_queued"].as_u64().unwrap(),
            queued_before,
            "queued count must survive restart"
        );
        assert_eq!(
            json["jobs_completed"].as_u64().unwrap(),
            completed_before,
            "completed count must survive restart"
        );
    }

    // ---- crash recovery + deadline reaper ----

    /// A `seed_job` clone with a fresh id and the given deadline, for tests that
    /// need to control the reaper's deadline comparison.
    fn job_with_deadline(deadline_secs: u32) -> JobSpec {
        let mut job = seed_job();
        job.id = Uuid::new_v4();
        job.deadline_secs = deadline_secs;
        job
    }

    #[test]
    fn recover_in_flight_requeues_stale() {
        let store = Store::open_in_memory().unwrap();
        let a = job_with_deadline(60);
        let b = job_with_deadline(60);
        store.enqueue(&a).unwrap();
        store.enqueue(&b).unwrap();

        // Take both → both in_flight, queue empty.
        store.take_next(|_| true).unwrap();
        store.take_next(|_| true).unwrap();
        assert_eq!(store.queued_count().unwrap(), 0);

        // Simulated crash recovery reclaims both.
        assert_eq!(store.recover_in_flight().unwrap(), 2);
        assert_eq!(store.queued_count().unwrap(), 2);
        assert_eq!(store.in_flight_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn with_store_recovers_in_flight_on_restart() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();

        // First "process": take every auto-seeded job in_flight, then drop
        // state. We track one id across the restart.
        let taken_id;
        let seeded;
        {
            let state = AppState::with_store(Store::open(&db_path).unwrap(), test_config()).unwrap();
            let store = state.store.lock().await;
            let mut ids = Vec::new();
            while let Some((job, _)) = store.take_next(|_| true).unwrap() {
                ids.push(job.id);
            }
            seeded = ids.len();
            assert_eq!(
                seeded,
                JobKind::ALL.len(),
                "fresh DB seeds one job per kind"
            );
            taken_id = ids[0];
            assert_eq!(store.in_flight_count().unwrap(), seeded);
            assert_eq!(store.queued_count().unwrap(), 0);
        } // state dropped → "crash" with jobs stuck in_flight.

        // Second "process": reopen the SAME db. with_store must reclaim the
        // orphaned in_flight jobs on startup.
        let state = AppState::with_store(Store::open(&db_path).unwrap(), test_config()).unwrap();
        let store = state.store.lock().await;
        assert_eq!(
            store.job_status(&taken_id).unwrap().as_deref(),
            Some("queued"),
            "orphaned in_flight job must be queued again after restart"
        );
        assert_eq!(store.in_flight_count().unwrap(), 0);
        assert_eq!(store.queued_count().unwrap(), seeded);
    }

    /// Recovery reclaims ONLY in_flight jobs: queued/done/failed are left exactly
    /// as they were, so a restart can neither lose a finished job nor revive a
    /// terminal one. Locks the `WHERE status = in_flight` boundary.
    #[test]
    fn recover_in_flight_only_touches_in_flight() {
        let mut store = Store::open_in_memory().unwrap();

        // queued — never taken.
        let queued = job_with_deadline(60);
        store.enqueue(&queued).unwrap();
        // in_flight — taken, no result; the only job recovery should reclaim.
        let in_flight = job_with_deadline(60);
        store.enqueue(&in_flight).unwrap();
        store.take_next(|j| j.id == in_flight.id).unwrap();
        // done — taken + completed.
        let done = job_with_deadline(60);
        store.enqueue(&done).unwrap();
        store.take_next(|j| j.id == done.id).unwrap();
        store
            .record_completed(&signed_result(done.id, "d"))
            .unwrap();
        // failed — dead-lettered at max_attempts = 1.
        let failed = job_with_deadline(60);
        store.enqueue(&failed).unwrap();
        store.take_next(|j| j.id == failed.id).unwrap();
        assert!(store.requeue(&failed, 1).unwrap());

        // Exactly one in_flight job → exactly one reclaimed.
        assert_eq!(store.recover_in_flight().unwrap(), 1);
        assert_eq!(
            store.job_status(&in_flight.id).unwrap().as_deref(),
            Some("queued"),
            "the in_flight job must be reclaimed to queued"
        );
        assert_eq!(
            store.job_status(&queued.id).unwrap().as_deref(),
            Some("queued")
        );
        assert_eq!(
            store.job_status(&done.id).unwrap().as_deref(),
            Some("done"),
            "a completed job must NOT be reclaimed"
        );
        assert_eq!(
            store.job_status(&failed.id).unwrap().as_deref(),
            Some("failed"),
            "a dead-lettered job must NOT be revived"
        );
        assert_eq!(
            store.completed_count().unwrap(),
            1,
            "the recorded result is untouched"
        );

        // Idempotent: nothing is in_flight now, so a second recovery is a no-op.
        assert_eq!(store.recover_in_flight().unwrap(), 0);
    }

    /// A crash that left one job done (result committed) and one in_flight must,
    /// on restart, keep the done job credited and redispatch the in_flight job
    /// exactly once — the atomic boundary between "result recorded" and "still
    /// dispatched" survives a real on-disk reopen.
    #[tokio::test]
    async fn restart_redispatches_in_flight_once_preserving_done() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();

        let done_id;
        let in_flight_id;
        {
            // "Process 1": raw store (no auto-seed) with one done + one in_flight.
            let mut store = Store::open(&db_path).unwrap();
            let done = job_with_deadline(60);
            done_id = done.id;
            store.enqueue(&done).unwrap();
            store.take_next(|j| j.id == done.id).unwrap();
            store
                .record_completed(&signed_result(done.id, "d"))
                .unwrap();

            let in_flight = job_with_deadline(60);
            in_flight_id = in_flight.id;
            store.enqueue(&in_flight).unwrap();
            store.take_next(|j| j.id == in_flight.id).unwrap();
            assert_eq!(store.in_flight_count().unwrap(), 1);
        } // drop → "crash"

        // "Process 2": restart through with_store, which recovers on boot and must
        // NOT re-seed (jobs already exist).
        let state = AppState::with_store(Store::open(&db_path).unwrap(), test_config()).unwrap();
        let store = state.store.lock().await;

        // The done job survived and is still credited; it is not requeued.
        assert_eq!(store.job_status(&done_id).unwrap().as_deref(), Some("done"));
        assert_eq!(
            store.completed_count().unwrap(),
            1,
            "pre-crash result must survive"
        );

        // The in_flight job was reclaimed to queued and redispatches exactly once.
        assert_eq!(
            store.job_status(&in_flight_id).unwrap().as_deref(),
            Some("queued")
        );
        let taken = store.take_next(|_| true).unwrap();
        assert_eq!(
            taken.map(|(j, _)| j.id),
            Some(in_flight_id),
            "the reclaimed job redispatches"
        );
        assert!(
            store.take_next(|_| true).unwrap().is_none(),
            "and only once — nothing else is queued"
        );
    }

    /// A DB created before the `started_at`/`attempts` columns existed migrates
    /// cleanly on `Store::open` (idempotent `ALTER`s) and can still recover an
    /// in_flight job — exercises the on-disk migration path, not just the
    /// in-memory schema where every column is present from the start.
    #[test]
    fn open_migrates_pre_column_db_and_recovers() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();

        let job = job_with_deadline(60);
        let spec_json = serde_json::to_string(&job).unwrap();
        {
            // Hand-build the OLD schema: a jobs table without started_at/attempts,
            // holding a single in_flight row.
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE jobs (id TEXT PRIMARY KEY, spec_json TEXT NOT NULL, status TEXT NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO jobs (id, spec_json, status) VALUES (?1, ?2, 'in_flight')",
                (job.id.to_string(), &spec_json),
            )
            .unwrap();
        } // close the raw connection before reopening through Store

        // Store::open must run the ADD COLUMN migrations without erroring.
        let store = Store::open(&db_path).unwrap();

        // The migrated in_flight row is reclaimable and redispatchable.
        assert_eq!(
            store.recover_in_flight().unwrap(),
            1,
            "the pre-column in_flight job must be reclaimed after migration"
        );
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("queued")
        );
        // dispatch_seq was also absent in the old schema; the migration defaults
        // it to 0 (FM4), and the first dispatch bumps it to 1.
        assert_eq!(
            store.current_dispatch_seq(&job.id).unwrap(),
            Some(0),
            "migrated dispatch_seq defaults to 0"
        );
        // attempts defaulted to 0 on migration → first dispatch makes it 1.
        let (_, seq) = store.take_next(|_| true).unwrap().unwrap();
        assert_eq!(seq, 1, "first dispatch after migration stamps seq 1");
        assert_eq!(store.current_dispatch_seq(&job.id).unwrap(), Some(1));
        assert_eq!(
            store.redispatched_count().unwrap(),
            0,
            "migrated attempts default to 0"
        );
        // progress_pct was also absent in the old schema; the migration defaults it to
        // 0, so the redispatched in_flight job reads 0% until its first heartbeat lands.
        assert_eq!(
            store.in_flight_progress_pct_avg().unwrap(),
            Some(0),
            "migrated progress_pct defaults to 0"
        );
        assert!(store.touch(&job.id, seq, 1000, 35).unwrap());
        assert_eq!(store.in_flight_progress_pct_avg().unwrap(), Some(35));
    }

    /// A DB created before the `dispatched_to` column boots through its migration,
    /// the migrated row reads back a NULL holder (so the liveness reaper skips it —
    /// a legacy in_flight job falls back to the deadline reaper), and a subsequent
    /// WS dispatch stamps the holder.
    #[test]
    fn open_migrates_db_without_dispatched_to() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let job = job_with_deadline(60);
        let spec_json = serde_json::to_string(&job).unwrap();
        {
            // Old schema: a jobs table without the dispatched_to column, one queued row.
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE jobs (id TEXT PRIMARY KEY, spec_json TEXT NOT NULL, status TEXT NOT NULL);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO jobs (id, spec_json, status) VALUES (?1, ?2, 'queued')",
                (job.id.to_string(), &spec_json),
            )
            .unwrap();
        }
        // Boots through the dispatched_to ALTER without erroring.
        let store = Store::open(&db_path).unwrap();
        assert_eq!(
            store.job_dispatched_to(&job.id).unwrap(),
            None,
            "migrated row has no holder"
        );
        // A WS dispatch now stamps the holder on the migrated row.
        store.take_next_for("0xnew", |_| true).unwrap();
        assert_eq!(
            store.job_dispatched_to(&job.id).unwrap().as_deref(),
            Some("0xnew")
        );
    }

    /// `dispatch_seq` is monotonic per job: every `take_next` bumps it, and it
    /// survives a reap→requeue (the reaper does not reset it), so a reassigned
    /// job always carries a strictly higher seq than the previous dispatch. This
    /// is the invariant the fence relies on to tell "in_flight under me" from
    /// "in_flight under a later earner".
    #[test]
    fn dispatch_seq_increments_on_each_take() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(60);
        store.enqueue(&job).unwrap();
        assert_eq!(
            store.current_dispatch_seq(&job.id).unwrap(),
            Some(0),
            "queued job starts at 0"
        );

        let (_, seq1) = store.take_next(|_| true).unwrap().unwrap();
        assert_eq!(seq1, 1);

        // Reap past the deadline → back to queued; the seq is preserved, not reset.
        // take_next stamps started_at to the wall clock, so reap relative to it.
        let out = store.reap_expired(now_secs() + 10_000, 5).unwrap();
        assert_eq!(out.requeued, vec![job.id]);
        assert_eq!(
            store.current_dispatch_seq(&job.id).unwrap(),
            Some(1),
            "reap preserves the seq"
        );

        // Re-dispatch bumps strictly higher.
        let (_, seq2) = store.take_next(|_| true).unwrap().unwrap();
        assert_eq!(seq2, 2, "the reassigned dispatch carries a higher seq");
        assert_eq!(store.current_dispatch_seq(&job.id).unwrap(), Some(2));

        assert_eq!(
            store.current_dispatch_seq(&Uuid::new_v4()).unwrap(),
            None,
            "unknown job → None"
        );
    }

    #[test]
    fn reap_expired_requeues_only_past_deadline() {
        let store = Store::open_in_memory().unwrap();
        let a = job_with_deadline(0); // expires immediately
        let b = job_with_deadline(3600); // not for an hour
        let a_id = a.id;
        store.enqueue(&a).unwrap();
        store.enqueue(&b).unwrap();

        // Both in_flight (each take_next stamps started_at = now and bumps
        // attempts to 1).
        store.take_next(|_| true).unwrap();
        store.take_next(|_| true).unwrap();
        assert_eq!(store.in_flight_count().unwrap(), 2);

        // max_attempts = 5: after one attempt job `a` should be requeued, not
        // failed.
        let reaped = store.reap_expired(now_secs(), 5).unwrap();
        assert_eq!(
            reaped.requeued,
            vec![a_id],
            "only the past-deadline job is reaped"
        );
        assert!(
            reaped.failed.is_empty(),
            "no jobs should be dead-lettered yet"
        );
        assert_eq!(store.queued_count().unwrap(), 1);
        assert_eq!(store.in_flight_count().unwrap(), 1);
    }

    /// Lifecycle: a job with deadline_secs=0 and max_attempts=2 is taken,
    /// reaped (requeued, attempts=1), taken again, and then reaped again
    /// (dead-lettered, attempts=2). The job ends in `failed` status.
    #[test]
    fn reap_dead_letters_after_max_attempts() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(0); // expires the moment it is in_flight
        let id = job.id;
        store.enqueue(&job).unwrap();

        // Attempt 1: take → attempts becomes 1.
        store.take_next(|_| true).unwrap();
        assert_eq!(store.in_flight_count().unwrap(), 1);

        // Reap: attempts=1 < max_attempts=2 → requeued.
        let outcome = store.reap_expired(now_secs(), 2).unwrap();
        assert_eq!(outcome.requeued, vec![id]);
        assert!(outcome.failed.is_empty());
        assert_eq!(store.queued_count().unwrap(), 1);
        assert_eq!(store.failed_count().unwrap(), 0);

        // Attempt 2: take → attempts becomes 2.
        store.take_next(|_| true).unwrap();
        assert_eq!(store.in_flight_count().unwrap(), 1);

        // Reap: attempts=2 >= max_attempts=2 → dead-lettered.
        let outcome = store.reap_expired(now_secs(), 2).unwrap();
        assert!(outcome.requeued.is_empty());
        assert_eq!(outcome.failed, vec![id]);
        assert_eq!(store.queued_count().unwrap(), 0);
        assert_eq!(store.in_flight_count().unwrap(), 0);
        assert_eq!(store.failed_count().unwrap(), 1);
        assert_eq!(
            store.job_status(&id).unwrap().as_deref(),
            Some("failed"),
            "dead-lettered job must have status 'failed'"
        );
    }

    /// A single attempt is enough to dead-letter when max_attempts=1.
    #[test]
    fn reap_dead_letters_after_single_attempt() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(0);
        let id = job.id;
        store.enqueue(&job).unwrap();

        // Attempt 1: take → attempts becomes 1.
        store.take_next(|_| true).unwrap();

        // Reap: attempts=1 >= max_attempts=1 → dead-lettered immediately.
        let outcome = store.reap_expired(now_secs(), 1).unwrap();
        assert!(outcome.requeued.is_empty());
        assert_eq!(outcome.failed, vec![id]);
        assert_eq!(store.failed_count().unwrap(), 1);
        assert_eq!(store.job_status(&id).unwrap().as_deref(), Some("failed"));
    }

    // ---- liveness-based holder reaping (reap_stale_holders) ----

    /// `take_next_for` stamps the holder; the anonymous `take_next` leaves it NULL,
    /// so only WS dispatches are attributable to the liveness reaper.
    #[test]
    fn take_next_for_records_holder_and_take_next_leaves_null() {
        let store = Store::open_in_memory().unwrap();
        let ws = job_with_deadline(3600);
        let http = job_with_deadline(3600);
        store.enqueue(&ws).unwrap();
        store.enqueue(&http).unwrap();

        let (taken_ws, _) = store
            .take_next_for("0xho1der", |j| j.id == ws.id)
            .unwrap()
            .unwrap();
        assert_eq!(taken_ws.id, ws.id);
        let (taken_http, _) = store.take_next(|j| j.id == http.id).unwrap().unwrap();
        assert_eq!(taken_http.id, http.id);

        assert_eq!(
            store.job_dispatched_to(&ws.id).unwrap().as_deref(),
            Some("0xho1der")
        );
        assert_eq!(
            store.job_dispatched_to(&http.id).unwrap(),
            None,
            "HTTP dispatch records no holder"
        );
    }

    /// A re-dispatch overwrites a stale `dispatched_to`: a WS job requeued and then
    /// re-taken by HTTP reads back NULL (every in_flight transition sets the holder),
    /// so the reaper never acts on a stale holder of a job it didn't currently hold.
    #[test]
    fn redispatch_overwrites_dispatched_to() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(0); // expires immediately so reap_expired requeues it
        store.enqueue(&job).unwrap();

        store.take_next_for("0xws", |_| true).unwrap();
        assert_eq!(
            store.job_dispatched_to(&job.id).unwrap().as_deref(),
            Some("0xws")
        );
        store.reap_expired(now_secs(), 5).unwrap(); // → queued
                                                    // The stale holder is RETAINED while queued (the requeue paths don't clear it);
                                                    // this is safe precisely because reap_stale_holders only scans in_flight rows.
        assert_eq!(
            store.job_dispatched_to(&job.id).unwrap().as_deref(),
            Some("0xws"),
            "a requeued (queued) row keeps its stale holder until re-dispatch"
        );
        store.take_next(|_| true).unwrap(); // HTTP re-dispatch
        assert_eq!(
            store.job_dispatched_to(&job.id).unwrap(),
            None,
            "HTTP re-dispatch clears the holder"
        );
    }

    /// An in_flight job whose holder is not in the live set, past the grace, is
    /// requeued (holder had attempts < max). Discriminating: an unfiltered scan that
    /// ignored liveness would also reap a live-holder job; here only the dead one goes.
    #[test]
    fn reap_stale_holders_requeues_dead_holder_only() {
        let store = Store::open_in_memory().unwrap();
        let dead = job_with_deadline(3600); // long deadline: the deadline reaper would NOT catch this
        let live = job_with_deadline(3600);
        store.enqueue(&dead).unwrap();
        store.enqueue(&live).unwrap();
        store.take_next_for("0xdead", |j| j.id == dead.id).unwrap();
        store.take_next_for("0xlive", |j| j.id == live.id).unwrap();

        let live_set = HashSet::from(["0xlive".to_string()]);
        // grace 0, well after dispatch → the dead holder's job is reclaimed.
        let outcome = store
            .reap_stale_holders(&live_set, now_secs() + 10_000, 0, 5)
            .unwrap();
        assert_eq!(
            outcome.requeued,
            vec![dead.id],
            "only the dead-holder job is reclaimed"
        );
        assert!(outcome.failed.is_empty());
        assert_eq!(
            store.job_status(&dead.id).unwrap().as_deref(),
            Some("queued")
        );
        assert_eq!(
            store.job_status(&live.id).unwrap().as_deref(),
            Some("in_flight"),
            "live holder untouched"
        );
    }

    /// FM2: a NULL holder (anonymous HTTP dispatch) is never reaped on liveness,
    /// even when no earner is live — it stays on the deadline reaper. (A NULL
    /// mishandled as "no live holder" would requeue every HTTP job every tick.)
    #[test]
    fn reap_stale_holders_skips_null_holder() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(3600);
        store.enqueue(&job).unwrap();
        store.take_next(|_| true).unwrap(); // HTTP → dispatched_to NULL

        let empty = HashSet::new();
        let outcome = store
            .reap_stale_holders(&empty, now_secs() + 10_000, 0, 5)
            .unwrap();
        assert!(
            outcome.requeued.is_empty() && outcome.failed.is_empty(),
            "NULL holder never liveness-reaped"
        );
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("in_flight")
        );
    }

    /// FM1: within the grace, a not-live holder's job is left alone — a transient
    /// registry gap (a missed heartbeat window) must not trigger a spurious requeue.
    #[test]
    fn reap_stale_holders_respects_grace() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(3600);
        store.enqueue(&job).unwrap();
        store.take_next_for("0xdead", |_| true).unwrap(); // started_at = now

        let empty = HashSet::new();
        // now == dispatch time, grace 60 → within grace, not reaped despite a dead holder.
        let outcome = store.reap_stale_holders(&empty, now_secs(), 60, 5).unwrap();
        assert!(outcome.requeued.is_empty(), "within grace: no requeue");
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("in_flight")
        );
    }

    /// A dead holder's job at/over max_attempts is dead-lettered, mirroring the
    /// deadline reaper (the attempt was charged at dispatch).
    #[test]
    fn reap_stale_holders_dead_letters_at_max_attempts() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(3600);
        store.enqueue(&job).unwrap();
        store.take_next_for("0xdead", |_| true).unwrap(); // attempts → 1

        let empty = HashSet::new();
        let outcome = store
            .reap_stale_holders(&empty, now_secs() + 10_000, 0, 1)
            .unwrap();
        assert!(outcome.requeued.is_empty());
        assert_eq!(
            outcome.failed,
            vec![job.id],
            "attempts(1) >= max(1) → dead-lettered"
        );
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("failed")
        );
    }

    /// FM4: after the liveness reap requeues a job, a late `requeue` from the
    /// original holder's disconnect path is a no-op (the in_flight guard), so the
    /// attempts counter is not corrupted by a double-requeue. The dispatch_seq is
    /// preserved across the reap, and a re-dispatch bumps it strictly higher — the
    /// basis on which the fence rejects the reaped holder's late settle.
    #[test]
    fn reap_stale_holders_then_late_requeue_is_noop() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(3600);
        store.enqueue(&job).unwrap();
        let (_, seq1) = store.take_next_for("0xdead", |_| true).unwrap().unwrap();

        let empty = HashSet::new();
        assert_eq!(
            store
                .reap_stale_holders(&empty, now_secs() + 10_000, 0, 5)
                .unwrap()
                .requeued,
            vec![job.id]
        );
        assert_eq!(
            store.current_dispatch_seq(&job.id).unwrap(),
            Some(seq1),
            "reap preserves the seq"
        );

        // The original holder's late disconnect tries to requeue the now-queued job:
        // the in_flight guard makes it a no-op (no double-requeue, attempts intact).
        assert!(
            !store.requeue(&job, 5).unwrap(),
            "late requeue of a non-in_flight job is a no-op"
        );
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("queued")
        );

        // A reassigning dispatch bumps the seq strictly higher than the reaped one.
        let (_, seq2) = store.take_next_for("0xnew", |_| true).unwrap().unwrap();
        assert!(
            seq2 > seq1,
            "the reassigned dispatch carries a higher seq (fence basis)"
        );
    }

    // ---- heartbeat / liveness-aware reaping ----

    /// `touch` slides `started_at` forward and the reaper honours the new value.
    ///
    /// Procedure: enqueue a job with deadline=100; take it (`in_flight`); bump
    /// `started_at` to t=5000 via `touch`; at t=5099 the job is not yet due
    /// (99 < 100) — reaper returns empty; at t=5100 (100 >= 100) it fires.
    #[test]
    fn touch_bumps_started_at_and_reaper_respects_it() {
        const BIG_MAX: u32 = 999;
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(100);
        let id = job.id;
        store.enqueue(&job).unwrap();
        store.take_next(|_| true).unwrap(); // → in_flight

        assert!(
            store.touch(&id, 1, 5000, 0).unwrap(),
            "touch must return true for an in_flight job"
        );

        // 99 seconds after t=5000 → not yet expired.
        let outcome = store.reap_expired(5099, BIG_MAX).unwrap();
        assert!(outcome.requeued.is_empty(), "must not reap before deadline");
        assert!(
            outcome.failed.is_empty(),
            "must not dead-letter before deadline"
        );

        // Exactly at deadline → reaped.
        let outcome = store.reap_expired(5100, BIG_MAX).unwrap();
        assert_eq!(outcome.requeued, vec![id], "must reap at deadline");
        assert!(outcome.failed.is_empty());
    }

    #[test]
    fn oldest_in_flight_started_at_is_min_over_in_flight_jobs() {
        const BIG_MAX: u32 = 999;
        let store = Store::open_in_memory().unwrap();
        // Nothing in flight yet → None.
        assert_eq!(store.oldest_in_flight_started_at().unwrap(), None);

        // Two jobs in flight, touched to known (distinct) started_at timestamps.
        let a = job_with_deadline(100);
        let b = job_with_deadline(100);
        let (id_a, id_b) = (a.id, b.id);
        store.enqueue(&a).unwrap();
        store.enqueue(&b).unwrap();
        store.take_next(|_| true).unwrap();
        store.take_next(|_| true).unwrap();
        store.touch(&id_a, 1, 5000, 0).unwrap();
        store.touch(&id_b, 1, 4000, 0).unwrap();

        // The oldest is the MINIMUM started_at across the in-flight set.
        assert_eq!(store.oldest_in_flight_started_at().unwrap(), Some(4000));

        // Reaping past b's deadline requeues it (started_at cleared); only a is
        // left in flight, so the oldest advances to a's timestamp.
        let outcome = store.reap_expired(4100, BIG_MAX).unwrap();
        assert_eq!(outcome.requeued, vec![id_b]);
        assert_eq!(store.oldest_in_flight_started_at().unwrap(), Some(5000));
    }

    #[tokio::test]
    async fn stats_reports_oldest_in_flight_secs() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        // No in-flight jobs → null (stable key, absent value).
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert!(
            json["oldest_in_flight_secs"].is_null(),
            "no in-flight jobs → null"
        );

        // Put one job in flight; the field becomes the age of its dispatch. Just
        // taken, so it is ~0 and certainly a small NUMBER (proving None → Some).
        let job = job_with_deadline(60);
        {
            let store = state.store.lock().await;
            store.enqueue(&job).unwrap();
            store.take_next(|_| true).unwrap();
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        let age = json["oldest_in_flight_secs"]
            .as_u64()
            .expect("in-flight → numeric age");
        assert!(
            age < 5,
            "freshly dispatched job age should be ~0, got {age}"
        );
    }

    /// FM1: the dead-letter age anchors on the OLDEST `dead_lettered_at` (`MIN`), not the
    /// newest. Two quarantined rows stamped with distinct times must yield the older stamp
    /// for both the attestation and debit backlogs (a store-level test with explicit
    /// stamps, since real dead-letters in a test all land within the same second).
    #[tokio::test]
    async fn oldest_dead_lettered_age_is_min_not_newest() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;
        let older = seed_job();
        let newer = seed_job();
        settle_one_metered(&state, &older).await; // pending attestation + pending debit
        settle_one_metered(&state, &newer).await;

        let store = state.store.lock().await;
        // Quarantine each with an explicit, distinct stamp (older = 1000, newer = 2000).
        assert!(store.mark_attestation_dead_lettered(&older.id, 1000).unwrap());
        assert!(store.mark_attestation_dead_lettered(&newer.id, 2000).unwrap());
        assert!(store.mark_debit_dead_lettered(&older.id, 1000).unwrap());
        assert!(store.mark_debit_dead_lettered(&newer.id, 2000).unwrap());

        assert_eq!(
            store.oldest_dead_lettered_attestation_at().unwrap(),
            Some(1000),
            "MIN stamp, not the newer 2000"
        );
        assert_eq!(
            store.oldest_dead_lettered_debit_at().unwrap(),
            Some(1000),
            "MIN stamp, not the newer 2000"
        );
    }

    /// FM2: `/stats` reports `null` for both dead-letter ages on a clean mesh (no row
    /// quarantined), and a small positive numeric age once a row is dead-lettered — proving
    /// None → Some and that the field is the AGE, not the count. FM4: the new fields are
    /// additive — the existing `oldest_in_flight_secs` and the dead-letter DEPTH counts are
    /// unchanged in the same response.
    #[tokio::test]
    async fn stats_reports_oldest_dead_lettered_age() {
        let state = test_state_empty_with_compute_rate(DRAIN_RATE).await;

        // Clean mesh → both ages null (present-but-null, the additive shape).
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert!(json["oldest_dead_lettered_attestation_secs"].is_null(), "clean → null");
        assert!(json["oldest_dead_lettered_debit_secs"].is_null(), "clean → null");
        // Additive shape: the pre-existing key is still serialized (present, value
        // irrelevant — seeded jobs left in_flight by the drain make it a number here).
        assert!(json.get("oldest_in_flight_secs").is_some(), "existing field still present");
        assert_eq!(json["dead_lettered_attestations"], 0);
        assert_eq!(json["dead_lettered_debits"], 0);

        // Quarantine one metered job's receipt AND its debit.
        let job = seed_job();
        settle_one_metered(&state, &job).await;
        drain_debits(&state, &MockSpender::permanent()).await; // dead-letters the debit
        let poison = MockRelay::succeeding()
            .with_batch_reverts()
            .with_permanent_job(eas::job_id_hex(&job.id));
        drain_attestations(&state, &poison, TEST_BATCH).await; // dead-letters the receipt

        let json = body_json(get(state.clone(), "/stats").await).await;
        let att_age = json["oldest_dead_lettered_attestation_secs"]
            .as_u64()
            .expect("dead-lettered → numeric age");
        let debit_age = json["oldest_dead_lettered_debit_secs"]
            .as_u64()
            .expect("dead-lettered → numeric age");
        assert!(att_age < 5, "freshly quarantined receipt age ~0, got {att_age}");
        assert!(debit_age < 5, "freshly quarantined debit age ~0, got {debit_age}");
        // Depth and age agree that exactly one of each is stuck.
        assert_eq!(json["dead_lettered_attestations"], 1);
        assert_eq!(json["dead_lettered_debits"], 1);
    }

    /// FM3: a `dead_lettered_at` ahead of `now` (clock skew) must floor the age at 0, never
    /// a negative value wrapped into a huge u64.
    #[tokio::test]
    async fn stats_dead_lettered_age_floors_at_zero_on_future_stamp() {
        let state = test_state_empty().await;
        let job = seed_job();
        settle_one(&state, &job).await; // pending attestation
        {
            let store = state.store.lock().await;
            // A stamp far in the future (skew); the age must clamp to 0, not wrap.
            assert!(store
                .mark_attestation_dead_lettered(&job.id, now_secs() + 100_000)
                .unwrap());
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["oldest_dead_lettered_attestation_secs"].as_u64().unwrap(),
            0,
            "a future stamp floors to 0, never a wrapped u64"
        );
    }

    #[tokio::test]
    async fn stats_reports_in_flight_progress_pct_avg() {
        // Build state with a truly-empty store (test_state_empty drains seeds via
        // take_next, which leaves them in_flight — non-empty for this aggregate).
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        // No in-flight jobs → null (stable key, absent value).
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert!(
            json["in_flight_progress_pct_avg"].is_null(),
            "no in-flight jobs → null"
        );

        // One job in flight, heartbeated to 70% → the field becomes that mean.
        let job = job_with_deadline(60);
        let id = job.id;
        {
            let store = state.store.lock().await;
            store.enqueue(&job).unwrap();
            let (_, seq) = store.take_next(|_| true).unwrap().unwrap();
            assert!(store.touch(&id, seq, 1000, 70).unwrap());
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["in_flight_progress_pct_avg"], 70,
            "in-flight progress mean surfaces at /stats"
        );
    }

    #[test]
    fn redispatched_count_counts_jobs_dispatched_more_than_once() {
        const BIG_MAX: u32 = 999;
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.redispatched_count().unwrap(), 0);

        let job = job_with_deadline(100);
        let id = job.id;
        store.enqueue(&job).unwrap();

        // First dispatch (attempts → 1): not yet redispatched.
        store.take_next(|_| true).unwrap();
        store.touch(&id, 1, 1000, 0).unwrap();
        assert_eq!(store.redispatched_count().unwrap(), 0);

        // Reaper requeues it past the deadline (attempts unchanged), then it is
        // dispatched a second time (attempts → 2): now counted as redispatched.
        let outcome = store.reap_expired(1100, BIG_MAX).unwrap();
        assert_eq!(outcome.requeued, vec![id]);
        store.take_next(|_| true).unwrap();
        assert_eq!(store.redispatched_count().unwrap(), 1);
    }

    /// `attempt_fault_totals` sums the gross `attempts`/`faults` columns across
    /// all jobs: dispatches accumulate attempts, a reaper requeue leaves them
    /// untouched, and an earner fault refunds the attempt while charging a fault.
    /// Pins FM3 — gross `total_attempts` (2) outpaces the count of distinct
    /// redispatched jobs (`redispatched_count` == 1) — and the cross-row SUM.
    #[test]
    fn attempt_fault_totals_sums_gross_attempts_and_faults() {
        const BIG: u32 = 999;
        let store = Store::open_in_memory().unwrap();
        // Fresh store: nothing dispatched, COALESCE turns the NULL SUM into 0/0.
        assert_eq!(store.attempt_fault_totals().unwrap(), (0, 0));

        let job = job_with_deadline(100);
        let id = job.id;
        store.enqueue(&job).unwrap();

        // Dispatch #1 (attempts → 1).
        store.take_next(|_| true).unwrap();
        assert_eq!(store.attempt_fault_totals().unwrap(), (1, 0));

        // Reaper requeues past the deadline (attempts unchanged), then a second
        // dispatch (attempts → 2). Gross attempts (2) now exceeds the number of
        // distinct redispatched jobs (1): the contradiction FM3 warns about.
        store.touch(&id, 1, 1000, 0).unwrap();
        store.reap_expired(1100, BIG).unwrap();
        store.take_next(|_| true).unwrap();
        assert_eq!(store.attempt_fault_totals().unwrap(), (2, 0));
        assert_eq!(store.redispatched_count().unwrap(), 1);

        // An earner fault refunds the dispatch attempt (2 → 1) and charges a fault.
        assert!(!store.requeue_earner_fault(&job, BIG, None).unwrap());
        assert_eq!(store.attempt_fault_totals().unwrap(), (1, 1));

        // Redispatch then a second distinct fault: attempts back to 1, faults → 2.
        store.take_next(|_| true).unwrap();
        assert!(!store.requeue_earner_fault(&job, BIG, None).unwrap());
        assert_eq!(store.attempt_fault_totals().unwrap(), (1, 2));

        // A second job dispatched once proves the SUM spans rows, not just the
        // first job. Select job2 by id (FIFO would otherwise redispatch the older
        // `job` and leave job2 at attempts 0, making the cross-row sum vacuous).
        let job2 = job_with_deadline(100);
        store.enqueue(&job2).unwrap();
        store.take_next(|j| j.id == job2.id).unwrap();
        assert_eq!(store.attempt_fault_totals().unwrap(), (2, 2));
    }

    /// Repeated heartbeats keep extending the liveness window.
    ///
    /// Sequence:
    ///   take_next → in_flight;
    ///   touch(5000) → reaper at 5090 sees only 90s elapsed → no reap;
    ///   touch(5090) → reaper at 5180 sees only 90s elapsed → still no reap;
    ///   reaper at 5190 sees 100s of silence since t=5090 → fires.
    #[test]
    fn heartbeats_keep_a_live_job_unreaped() {
        const BIG: u32 = 999;
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(100);
        let id = job.id;
        store.enqueue(&job).unwrap();
        store.take_next(|_| true).unwrap();

        store.touch(&id, 1, 5000, 0).unwrap();
        let outcome = store.reap_expired(5090, BIG).unwrap();
        assert!(
            outcome.requeued.is_empty(),
            "90s after first beat: should not reap"
        );
        assert!(outcome.failed.is_empty());

        // Second heartbeat at t=5090.
        store.touch(&id, 1, 5090, 0).unwrap();
        let outcome = store.reap_expired(5180, BIG).unwrap();
        assert!(
            outcome.requeued.is_empty(),
            "90s after second beat (180s total): should still not reap"
        );
        assert!(outcome.failed.is_empty());

        // 100s of silence since the last beat (t=5090+100=5190) → reap.
        let outcome = store.reap_expired(5190, BIG).unwrap();
        assert_eq!(
            outcome.requeued,
            vec![id],
            "100s after last beat: must reap"
        );
        assert!(outcome.failed.is_empty());
    }

    /// `touch` is a no-op for a job that is not `in_flight`.
    ///
    /// A queued job returns `false`; so does a job whose id the store has never
    /// seen (SQLite UPDATE affects 0 rows → `updated == 0`).
    #[test]
    fn touch_is_noop_for_non_in_flight() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(60);
        let id = job.id;
        // Enqueue but do NOT take → status stays `queued`.
        store.enqueue(&job).unwrap();

        assert!(
            !store.touch(&id, 0, 5000, 0).unwrap(),
            "touch must return false for a queued (not in_flight) job"
        );

        // A completed job is likewise not in_flight → touch is a no-op. Uses a
        // fresh store because `record_completed` needs `&mut Store`.
        let mut store = Store::open_in_memory().unwrap();
        store.enqueue(&job).unwrap();
        store.take_next(|_| true).unwrap();
        store
            .record_completed(&signed_result(id, "cafebabe"))
            .unwrap();
        assert!(
            !store.touch(&id, 1, 5000, 0).unwrap(),
            "touch must return false for a done job"
        );
    }

    /// A heartbeat persists its `progress_pct`, and an out-of-range value (a `u8`
    /// reaches 255) is clamped to 100 before it lands — FM2: a faulty/adversarial
    /// earner can't poison the `/stats` figure. With exactly one job in flight, the
    /// mean aggregate reads back that job's stored progress.
    #[test]
    fn touch_records_and_clamps_progress() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(100);
        let id = job.id;
        store.enqueue(&job).unwrap();
        store.take_next(|_| true).unwrap(); // seq 1, in_flight, progress reset to 0
        assert_eq!(
            store.in_flight_progress_pct_avg().unwrap(),
            Some(0),
            "a fresh dispatch reads 0%"
        );
        assert!(store.touch(&id, 1, 1000, 40).unwrap());
        assert_eq!(
            store.in_flight_progress_pct_avg().unwrap(),
            Some(40),
            "the heartbeat's progress persisted"
        );
        // 200 > 100 (and a u8 can reach 255) → clamped to 100, not stored raw.
        assert!(store.touch(&id, 1, 1000, 200).unwrap());
        assert_eq!(
            store.in_flight_progress_pct_avg().unwrap(),
            Some(100),
            "out-of-range progress clamps to 100"
        );
    }

    /// FM1: a heartbeat from a PREVIOUS holder (job reaped + reassigned, so its
    /// `dispatch_seq` advanced) must not overwrite the NEW holder's progress. The
    /// stale-seq write is fenced out atomically by `touch`'s `dispatch_seq = ?` clause.
    /// Also pins FM4: `take_next` resets progress to 0 on the redispatch.
    #[test]
    fn stale_dispatch_heartbeat_does_not_overwrite_progress() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(100);
        let id = job.id;
        store.enqueue(&job).unwrap();

        // A takes the job (seq 1) and reports 40%.
        let (_, seq_a) = store.take_next(|_| true).unwrap().unwrap();
        assert_eq!(seq_a, 1);
        assert!(store.touch(&id, seq_a, 1000, 40).unwrap());
        assert_eq!(store.in_flight_progress_pct_avg().unwrap(), Some(40));

        // Deadline lapses → requeue → B takes it (seq 2). The redispatch resets
        // progress, so the new lease starts at 0 rather than inheriting A's 40 (FM4).
        assert!(!store.requeue(&job, 999).unwrap(), "requeued, not dead-lettered");
        let (_, seq_b) = store.take_next(|_| true).unwrap().unwrap();
        assert_eq!(seq_b, 2);
        assert_eq!(
            store.in_flight_progress_pct_avg().unwrap(),
            Some(0),
            "redispatch reset progress to 0"
        );

        // A's STALE heartbeat (seq 1) for the now-B lease is a no-op — it can neither
        // slide the deadline nor write progress over B's lease.
        assert!(
            !store.touch(&id, seq_a, 2000, 90).unwrap(),
            "a stale-dispatch_seq heartbeat is fenced out"
        );
        assert_eq!(
            store.in_flight_progress_pct_avg().unwrap(),
            Some(0),
            "B's progress is untouched by A's stale beat"
        );

        // B's current heartbeat (seq 2) lands normally.
        assert!(store.touch(&id, seq_b, 2000, 60).unwrap());
        assert_eq!(store.in_flight_progress_pct_avg().unwrap(), Some(60));
    }

    /// The `/stats` aggregate is the MEAN over `in_flight` jobs and reports `None`
    /// when none are running. A terminal (or queued) job's last-known progress is
    /// excluded — FM4: `/stats` never reports progress for a non-`in_flight` job.
    #[test]
    fn in_flight_progress_pct_avg_means_only_in_flight() {
        let mut store = Store::open_in_memory().unwrap();
        assert_eq!(
            store.in_flight_progress_pct_avg().unwrap(),
            None,
            "nothing in flight → None"
        );

        let a = job_with_deadline(100);
        let b = job_with_deadline(100);
        let (id_a, id_b) = (a.id, b.id);
        store.enqueue(&a).unwrap();
        store.enqueue(&b).unwrap();
        store.take_next(|j| j.id == id_a).unwrap(); // seq 1
        store.take_next(|j| j.id == id_b).unwrap(); // seq 1
        store.touch(&id_a, 1, 1000, 40).unwrap();
        store.touch(&id_b, 1, 1000, 60).unwrap();
        assert_eq!(
            store.in_flight_progress_pct_avg().unwrap(),
            Some(50),
            "mean of 40 and 60 across the in-flight set"
        );

        // Complete a: it leaves in_flight, so its progress drops out of the mean and
        // only b's 60 remains — a terminal job is never aggregated.
        store
            .record_completed(&signed_result(id_a, "cafebabe"))
            .unwrap();
        assert_eq!(
            store.in_flight_progress_pct_avg().unwrap(),
            Some(60),
            "a completed job's progress is excluded from the mean"
        );
    }

    /// An `EarnerMsg::Heartbeat` mid-session must not disturb the normal
    /// offer → accept → submit → Accepted flow.
    ///
    /// Mirrors `ws_offer_accept_submit_flows_to_completed` but inserts a
    /// heartbeat frame between `Accept` and `Submit`, then asserts the flow
    /// still completes successfully.
    #[tokio::test]
    async fn ws_heartbeat_during_session_then_submit_completes() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        let addr = serve_ephemeral(state.clone()).await;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();

        // 1. Hello
        let nonce = recv_challenge(&mut ws).await;
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello(&nonce)).unwrap()))
            .await
            .unwrap();

        // 2. Expect a JobOffer for the seeded job.
        let offer = next_coordinator_msg(&mut ws).await;
        let CoordinatorMsg::JobOffer(offered) = offer else {
            panic!("expected JobOffer, got {offer:?}");
        };
        assert_eq!(offered.id, job_id);

        // 3. Accept.
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Accept { job_id }).unwrap(),
        ))
        .await
        .unwrap();

        // 4. Send a heartbeat (progress=50) between Accept and Submit; the
        //    coordinator must handle it gracefully and keep the session alive.
        let beat = EarnerMsg::Heartbeat {
            job_id: Some(job_id),
            progress_pct: 50,
        };
        ws.send(WsMessage::text(serde_json::to_string(&beat).unwrap()))
            .await
            .unwrap();

        // 5. Submit a validly-signed result.
        let result = signed_result(job_id, "deadbeef");
        ws.send(WsMessage::text(
            serde_json::to_string(&EarnerMsg::Submit(result)).unwrap(),
        ))
        .await
        .unwrap();

        // 6. Expect Accepted — heartbeat must not have broken the session.
        let verdict = next_coordinator_msg(&mut ws).await;
        match verdict {
            CoordinatorMsg::Accepted {
                job_id: jid,
                attestation_uid,
            } => {
                assert_eq!(jid, job_id);
                assert_eq!(attestation_uid, PLACEHOLDER_ATTESTATION_UID);
            }
            other => panic!("expected Accepted, got {other:?}"),
        }

        // /stats reflects the completed job.
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["jobs_completed"], 1,
            "completed count must be 1 after ws heartbeat test"
        );
    }

    // ---- idempotent + gated submit ----

    #[tokio::test]
    async fn submit_rejected_for_unknown_job() {
        let state = test_state_empty().await;
        // Validly-signed result for a job the store has never seen.
        let job_id = Uuid::new_v4();
        let good = signed_result(job_id, "deadbeef");
        let uri = format!("/jobs/{}/submit", job_id);
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn submit_rejected_when_not_in_flight() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await; // left queued — never polled

        let good = signed_result(job_id, "deadbeef");
        let uri = format!("/jobs/{}/submit", job_id);
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 0);
    }

    #[tokio::test]
    async fn double_submit_does_not_double_count() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        // Poll → in_flight.
        let resp = get(state.clone(), "/jobs/next").await;
        assert_eq!(resp.status(), StatusCode::OK);

        let good = signed_result(job_id, "deadbeef");
        let uri = format!("/jobs/{}/submit", job_id);

        // First submit: accepted (in_flight → done).
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Second submit: job is now done, so the gate rejects with CONFLICT and
        // nothing is double-counted.
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 1);
    }

    #[test]
    fn record_completed_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(60);
        let job_id = job.id;
        store.enqueue(&job).unwrap();
        store.take_next(|_| true).unwrap();

        let result = signed_result(job_id, "deadbeef");
        // Recording twice must not double-count the result.
        store.record_completed(&result).unwrap();
        store.record_completed(&result).unwrap();
        assert_eq!(store.completed_count().unwrap(), 1);
    }

    /// `record_completed` settles ONLY an in_flight job. A result for an unknown,
    /// queued, already-done, or dead-lettered job is refused (`false`) and leaves
    /// the job untouched — the data-layer backstop against double-credit and the
    /// "orphaned settle" of a stale/replayed submit.
    #[test]
    fn record_completed_refuses_non_in_flight() {
        let mut store = Store::open_in_memory().unwrap();

        // Unknown job (never enqueued) → refused, nothing recorded.
        let ghost = job_with_deadline(60);
        assert!(
            !store
                .record_completed(&signed_result(ghost.id, "x"))
                .unwrap(),
            "a result for an unknown job must be refused"
        );
        assert_eq!(store.completed_count().unwrap(), 0);

        // Queued (enqueued, never taken) → refused; stays queued.
        let queued = job_with_deadline(60);
        store.enqueue(&queued).unwrap();
        assert!(
            !store
                .record_completed(&signed_result(queued.id, "x"))
                .unwrap(),
            "a result for a queued job must be refused"
        );
        assert_eq!(
            store.job_status(&queued.id).unwrap().as_deref(),
            Some("queued")
        );
        assert_eq!(store.completed_count().unwrap(), 0);

        // In_flight → settles once; a replayed result is then refused and neither
        // double-counts nor changes the now-`done` job.
        let live = job_with_deadline(60);
        store.enqueue(&live).unwrap();
        store.take_next(|j| j.id == live.id).unwrap();
        assert!(
            store
                .record_completed(&signed_result(live.id, "x"))
                .unwrap(),
            "first settle of an in_flight job succeeds"
        );
        assert_eq!(store.job_status(&live.id).unwrap().as_deref(), Some("done"));
        assert!(
            !store
                .record_completed(&signed_result(live.id, "x"))
                .unwrap(),
            "a replayed result for an already-done job must be refused"
        );
        assert_eq!(store.completed_count().unwrap(), 1);

        // Dead-lettered (failed) → refused and must NOT be resurrected to done.
        let failed = job_with_deadline(60);
        store.enqueue(&failed).unwrap();
        store.take_next(|j| j.id == failed.id).unwrap();
        assert!(
            store.requeue(&failed, 1).unwrap(),
            "attempts>=max dead-letters"
        );
        assert_eq!(
            store.job_status(&failed.id).unwrap().as_deref(),
            Some("failed")
        );
        assert!(
            !store
                .record_completed(&signed_result(failed.id, "x"))
                .unwrap(),
            "a result for a dead-lettered job must be refused (no orphaned settle)"
        );
        assert_eq!(
            store.job_status(&failed.id).unwrap().as_deref(),
            Some("failed"),
            "dead-lettered job must stay failed, not resurrect to done"
        );
    }

    /// `requeue` (an earner's reject/disconnect) acts ONLY on an in_flight job.
    /// A job that has been reaped/reassigned/settled out from under the earner is
    /// left untouched, so a late reject can't clobber a reassigned job back to
    /// queued or resurrect a terminal one.
    #[test]
    fn requeue_is_noop_for_non_in_flight() {
        let store = Store::open_in_memory().unwrap();

        // Unknown job → no-op; nothing created.
        let ghost = job_with_deadline(60);
        assert!(!store.requeue(&ghost, 5).unwrap());
        assert!(store.job_status(&ghost.id).unwrap().is_none());

        // Queued (not in_flight) → no-op; stays queued.
        let queued = job_with_deadline(60);
        store.enqueue(&queued).unwrap();
        assert!(!store.requeue(&queued, 5).unwrap());
        assert_eq!(
            store.job_status(&queued.id).unwrap().as_deref(),
            Some("queued")
        );

        // Done → no-op; must NOT be resurrected to queued.
        let mut store = Store::open_in_memory().unwrap();
        let done = job_with_deadline(60);
        store.enqueue(&done).unwrap();
        store.take_next(|j| j.id == done.id).unwrap();
        store
            .record_completed(&signed_result(done.id, "x"))
            .unwrap();
        assert!(!store.requeue(&done, 5).unwrap());
        assert_eq!(
            store.job_status(&done.id).unwrap().as_deref(),
            Some("done"),
            "a stale requeue must not resurrect a completed job"
        );

        // In_flight → acts: attempts(1) < max(5) → back to queued.
        let live = job_with_deadline(60);
        store.enqueue(&live).unwrap();
        store.take_next(|j| j.id == live.id).unwrap();
        assert!(
            !store.requeue(&live, 5).unwrap(),
            "requeued (not dead-lettered) → false"
        );
        assert_eq!(
            store.job_status(&live.id).unwrap().as_deref(),
            Some("queued")
        );
    }

    /// The core of the earner-fault fix (FM1/FM3): an earner-fault requeue refunds
    /// the dispatch attempt `take_next` charged, so a faulty earner can't burn a
    /// job's renderability budget. The job survives more earner faults than
    /// `max_attempts`, then still dead-letters after exactly `max_attempts` genuine
    /// charge requeues — proving the faults never touched the attempt budget. If
    /// the refund were missing, the accumulated attempts would dead-letter the job
    /// on the very first charge requeue and the "1st charge → queued" step below
    /// would fail.
    #[test]
    fn requeue_earner_fault_refunds_attempt_so_faults_dont_burn_attempt_budget() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(60);
        store.enqueue(&job).unwrap();

        // Five earner faults (max_faults high so none dead-letters), each preceded
        // by a redispatch. With max_attempts=2 the OLD attempt-charging behavior
        // would have dead-lettered after the 2nd.
        for i in 1..=5 {
            let (taken, _) = store.take_next(|j| j.id == job.id).unwrap().unwrap();
            assert!(
                !store.requeue_earner_fault(&taken, 100, None).unwrap(),
                "earner fault {i} must requeue, never dead-letter (faults < max_faults)"
            );
            assert_eq!(
                store.job_status(&job.id).unwrap().as_deref(),
                Some("queued")
            );
        }

        // The renderability budget is untouched: it still takes exactly
        // max_attempts=2 genuine charge requeues to dead-letter, despite 5 faults.
        let (taken, _) = store.take_next(|j| j.id == job.id).unwrap().unwrap();
        assert!(
            !store.requeue(&taken, 2).unwrap(),
            "1st charge: attempts 1 < 2 → queued"
        );
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("queued")
        );
        let (taken, _) = store.take_next(|j| j.id == job.id).unwrap().unwrap();
        assert!(
            store.requeue(&taken, 2).unwrap(),
            "2nd charge: attempts 2 >= 2 → dead-letter"
        );
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("failed")
        );
    }

    /// FM1: a poison job (a spec no connected earner can satisfy → every earner
    /// faults) must still terminate. Earner faults accumulate on the separate
    /// `faults` budget and dead-letter at exactly `max_faults`, independent of the
    /// (here generous) attempt budget — so the no-charge path can't loop forever.
    #[test]
    fn requeue_earner_fault_dead_letters_poison_job_at_max_faults() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(60);
        store.enqueue(&job).unwrap();

        for fault in 1..=3 {
            let (taken, _) = store.take_next(|j| j.id == job.id).unwrap().unwrap();
            let dead = store.requeue_earner_fault(&taken, 3, None).unwrap();
            if fault < 3 {
                assert!(!dead, "fault {fault} < max_faults 3 → requeued");
                assert_eq!(
                    store.job_status(&job.id).unwrap().as_deref(),
                    Some("queued")
                );
            } else {
                assert!(dead, "fault 3 == max_faults → dead-lettered");
                assert_eq!(
                    store.job_status(&job.id).unwrap().as_deref(),
                    Some("failed")
                );
            }
        }
    }

    /// Like `requeue`, the earner-fault requeue acts ONLY on an in_flight job: an
    /// unknown, queued, or settled job is a no-op, so a late fault reject for a
    /// reaped/reassigned/settled job can't clobber it or charge a phantom fault.
    #[test]
    fn requeue_earner_fault_is_noop_for_non_in_flight() {
        let mut store = Store::open_in_memory().unwrap();

        // Unknown → no-op; nothing created.
        let ghost = job_with_deadline(60);
        assert!(!store.requeue_earner_fault(&ghost, 3, None).unwrap());
        assert!(store.job_status(&ghost.id).unwrap().is_none());

        // Queued (not in_flight) → no-op; stays queued.
        let queued = job_with_deadline(60);
        store.enqueue(&queued).unwrap();
        assert!(!store.requeue_earner_fault(&queued, 3, None).unwrap());
        assert_eq!(
            store.job_status(&queued.id).unwrap().as_deref(),
            Some("queued")
        );

        // Done → no-op; must NOT be resurrected to queued.
        let done = job_with_deadline(60);
        store.enqueue(&done).unwrap();
        store.take_next(|j| j.id == done.id).unwrap();
        store
            .record_completed(&signed_result(done.id, "x"))
            .unwrap();
        assert!(!store.requeue_earner_fault(&done, 3, None).unwrap());
        assert_eq!(store.job_status(&done.id).unwrap().as_deref(), Some("done"));
    }

    // ---- per-earner fault attribution (earner_faults table) ----

    /// `record_earner_fault` tallies each genuine quality fault to the faulting
    /// earner, and `faults_by_earner` reports the per-earner count. NO dedup: an
    /// earner faulting three times counts three — each is a distinct bad submission.
    #[test]
    fn faults_by_earner_groups_genuine_faults_per_earner() {
        let store = Store::open_in_memory().unwrap();
        let a = test_address("earner-a");
        let b = test_address("earner-b");

        // A faults three times (no dedup) → 3; B faults once → 1.
        store.record_earner_fault(&a).unwrap();
        store.record_earner_fault(&a).unwrap();
        store.record_earner_fault(&a).unwrap();
        store.record_earner_fault(&b).unwrap();

        let by_earner = store.faults_by_earner().unwrap();
        assert_eq!(by_earner.get(&a).copied(), Some(3), "A: 3 faults");
        assert_eq!(by_earner.get(&b).copied(), Some(1), "B: 1 fault");
        assert_eq!(by_earner.len(), 2, "only earners with faults appear");
    }

    /// A fresh mesh has attributed no faults: `faults_by_earner` is empty, so the
    /// `/earners` handler defaults every earner's `faults` to 0.
    #[test]
    fn faults_by_earner_is_empty_on_a_fresh_store() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.faults_by_earner().unwrap().is_empty());
    }

    /// Reaper-race: a job a reaper parked back to `queued` (status no longer
    /// in_flight, but `dispatch_seq` UNCHANGED — reapers don't bump it) must NOT be
    /// attributed a fault. The coordinator's seq-fence one layer up still passes for
    /// it (same seq), so attribution is co-gated with the per-job bump INSIDE
    /// requeue_earner_fault: a `Some(earner)` call on a reaped job records nothing,
    /// keeping `Σ(attributed) <= total_faults`. The in_flight contrast at the end
    /// proves the gate isn't vacuously always-empty.
    #[test]
    fn requeue_earner_fault_does_not_attribute_a_reaper_parked_job() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(60);
        store.enqueue(&job).unwrap();
        let (taken, _) = store.take_next(|j| j.id == job.id).unwrap().unwrap();

        // Simulate a deadline reap: back to `queued`, dispatch_seq intact.
        store.reap_expired(now_secs() + 10_000, 5).unwrap();
        assert_eq!(store.job_status(&job.id).unwrap().as_deref(), Some("queued"));

        // A late forged-fault for that stale dispatch: attribute_to = Some, but the
        // job is no longer in_flight → no budget bump AND no attribution.
        let earner = test_address("flapper");
        assert!(
            !store
                .requeue_earner_fault(&taken, 100, Some(&earner))
                .unwrap(),
            "reaper-parked job is a no-op (not in_flight)"
        );
        assert!(
            store.faults_by_earner().unwrap().is_empty(),
            "a no-op fault must not attribute — keeps Σ(attributed) <= total_faults"
        );

        // Contrast: a genuinely in_flight fault with the same Some(earner) DOES
        // attribute exactly once.
        let (taken2, _) = store.take_next(|j| j.id == job.id).unwrap().unwrap();
        store
            .requeue_earner_fault(&taken2, 100, Some(&earner))
            .unwrap();
        assert_eq!(
            store.faults_by_earner().unwrap().get(&earner).copied(),
            Some(1),
            "an in_flight fault attributes exactly once"
        );
    }

    // ---- created_at: the immutable wall-clock-TTL anchor ----

    /// `enqueue` stamps a creation time, and it is the anchor the TTL measures
    /// against — so it must NOT move when the job is dispatched, reaped, faulted,
    /// or re-enqueued. If any of those slid it, a job that keeps churning would
    /// reset its own clock and never hit the TTL (the task's FM1).
    #[test]
    fn created_at_is_stamped_once_and_never_slides() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(60);
        store.enqueue(&job).unwrap();

        let anchor = store
            .job_created_at(&job.id)
            .unwrap()
            .expect("created_at set at enqueue");

        // Dispatch (started_at moves, created_at must not).
        store.take_next(|j| j.id == job.id).unwrap().unwrap();
        assert_eq!(
            store.job_created_at(&job.id).unwrap(),
            Some(anchor),
            "dispatch must not slide created_at"
        );

        // Deadline reap → requeue.
        store.reap_expired(now_secs() + 10_000, 5).unwrap();
        assert_eq!(
            store.job_created_at(&job.id).unwrap(),
            Some(anchor),
            "reap/requeue must not slide created_at"
        );

        // Earner fault → requeue.
        let (taken, _) = store.take_next(|j| j.id == job.id).unwrap().unwrap();
        store.requeue_earner_fault(&taken, 100, None).unwrap();
        assert_eq!(
            store.job_created_at(&job.id).unwrap(),
            Some(anchor),
            "fault/requeue must not slide created_at"
        );

        // Re-enqueue the SAME id (operator re-submit) must preserve the anchor,
        // not reset it — otherwise a stuck job could be kept alive by re-submits.
        store.enqueue(&job).unwrap();
        assert_eq!(
            store.job_created_at(&job.id).unwrap(),
            Some(anchor),
            "re-enqueue must preserve created_at"
        );
    }

    /// A DB written before the `created_at` column existed must, after the
    /// migration, carry a non-NULL backfilled creation time on every row — so an
    /// already-queued job gets a finite TTL from the upgrade boot forward rather
    /// than a NULL the reaper skips forever (FM4).
    #[test]
    fn created_at_backfills_legacy_rows_on_migration() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let job_id = Uuid::new_v4();

        // Simulate a legacy DB: a jobs table WITHOUT created_at, one queued row.
        {
            let raw = rusqlite::Connection::open(&db_path).unwrap();
            raw.execute_batch(
                "CREATE TABLE jobs (
                     id TEXT PRIMARY KEY, spec_json TEXT NOT NULL, status TEXT NOT NULL,
                     started_at INTEGER, attempts INTEGER NOT NULL DEFAULT 0,
                     faults INTEGER NOT NULL DEFAULT 0, dispatch_seq INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .unwrap();
            raw.execute(
                "INSERT INTO jobs (id, spec_json, status) VALUES (?1, ?2, 'queued')",
                (job_id.to_string(), "{}"),
            )
            .unwrap();
        }

        // Opening through Store runs the idempotent migration + backfill.
        let store = Store::open(&db_path).unwrap();
        let backfilled = store.job_created_at(&job_id).unwrap();
        assert!(
            backfilled.is_some(),
            "legacy row must be backfilled to a non-NULL created_at"
        );
        assert!(
            backfilled.unwrap() > 0,
            "backfilled created_at must be a real timestamp"
        );
    }

    // ---- absolute wall-clock TTL (reap_ttl_expired) ----

    // A small multiple for clear test arithmetic; the live reaper uses the larger
    // JOB_TTL_DEADLINE_MULTIPLE. reap_ttl_expired takes the multiple as a param so
    // these stay independent of the production constant's exact value.
    const TEST_TTL_MULTIPLE: u32 = 10;

    /// The headline case the fault budget cannot terminate (mesh-attempt-budget-
    /// earner-fault's FM1): a poison job a single connected earner keeps faulting
    /// on parks in `queued` with one fault — never re-offered to that earner, so
    /// never re-dispatched, so the fault counter never advances and `max_faults`
    /// (which needs DISTINCT earners) is never reached. The deadline reaper only
    /// scans in_flight jobs, so it can't touch a queued job either. Only the
    /// absolute TTL dead-letters it.
    #[test]
    fn ttl_dead_letters_a_queued_poison_job_a_single_earner_cannot() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(60);
        store.enqueue(&job).unwrap();
        let anchor = store.job_created_at(&job.id).unwrap().unwrap();
        let ttl = 60 * TEST_TTL_MULTIPLE as i64;

        // One earner faults: back to queued, one fault, NOT dead-lettered.
        let (taken, _) = store.take_next(|j| j.id == job.id).unwrap().unwrap();
        assert!(!store.requeue_earner_fault(&taken, 10, None).unwrap());
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("queued")
        );

        // The deadline reaper, even far past the TTL, leaves a *queued* job alone:
        // it only reaps in_flight dispatches. This is the gap the TTL closes.
        assert!(store
            .reap_expired(anchor + ttl + 1, 5)
            .unwrap()
            .failed
            .is_empty());
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("queued")
        );

        // Just inside the TTL: still spared.
        assert!(store
            .reap_ttl_expired(anchor + ttl - 1, TEST_TTL_MULTIPLE)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("queued")
        );

        // At the TTL boundary (>=): dead-lettered to the terminal failed state.
        let expired = store
            .reap_ttl_expired(anchor + ttl, TEST_TTL_MULTIPLE)
            .unwrap();
        assert_eq!(
            expired,
            vec![job.id],
            "the poison job is dead-lettered exactly at the TTL"
        );
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("failed")
        );
        assert_eq!(store.failed_count().unwrap(), 1);
    }

    /// An in-flight job past the TTL is also caught (the TTL is created_at-anchored,
    /// so it bounds total wall-clock regardless of dispatch/heartbeat state), while
    /// a job still within its window — queued or in-flight — is left untouched.
    #[test]
    fn ttl_spares_jobs_within_window_and_reaps_in_flight_past_it() {
        let store = Store::open_in_memory().unwrap();
        let queued = job_with_deadline(60);
        let inflight = job_with_deadline(60);
        store.enqueue(&queued).unwrap();
        store.enqueue(&inflight).unwrap();
        store.take_next(|j| j.id == inflight.id).unwrap().unwrap();
        let anchor = store.job_created_at(&queued.id).unwrap().unwrap();
        let ttl = 60 * TEST_TTL_MULTIPLE as i64;

        // Both within window: nothing reaped, statuses intact.
        assert!(store
            .reap_ttl_expired(anchor + ttl - 1, TEST_TTL_MULTIPLE)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.job_status(&queued.id).unwrap().as_deref(),
            Some("queued")
        );
        assert_eq!(
            store.job_status(&inflight.id).unwrap().as_deref(),
            Some("in_flight")
        );

        // Past window: both dead-lettered (queued and in_flight alike).
        let mut expired = store
            .reap_ttl_expired(anchor + ttl, TEST_TTL_MULTIPLE)
            .unwrap();
        expired.sort();
        let mut want = vec![queued.id, inflight.id];
        want.sort();
        assert_eq!(expired, want);
        assert_eq!(
            store.job_status(&queued.id).unwrap().as_deref(),
            Some("failed")
        );
        assert_eq!(
            store.job_status(&inflight.id).unwrap().as_deref(),
            Some("failed")
        );
        assert_eq!(
            store.in_flight_count().unwrap(),
            0,
            "the reaped in_flight job left in_flight"
        );
    }

    /// `deadline_secs == 0` is the operator's "unbounded" signal: such a job has no
    /// per-dispatch deadline and must have no wall-clock TTL either, however old.
    #[test]
    fn ttl_exempts_unbounded_deadline_zero_jobs() {
        let store = Store::open_in_memory().unwrap();
        let queued = job_with_deadline(0);
        let inflight = job_with_deadline(0);
        store.enqueue(&queued).unwrap();
        store.enqueue(&inflight).unwrap();
        store.take_next(|j| j.id == inflight.id).unwrap().unwrap();
        let anchor = store.job_created_at(&queued.id).unwrap().unwrap();

        // A decade past creation: still untouched, because deadline_secs == 0.
        let far_future = anchor + 10 * 365 * 24 * 3600;
        assert!(store
            .reap_ttl_expired(far_future, TEST_TTL_MULTIPLE)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.job_status(&queued.id).unwrap().as_deref(),
            Some("queued")
        );
        assert_eq!(
            store.job_status(&inflight.id).unwrap().as_deref(),
            Some("in_flight")
        );
    }

    /// FM3: a job that settled to `done` (or was already `failed`) since the scan
    /// must not be re-transitioned. A completed job past the TTL is a no-op and is
    /// never reported as expired — the status-guarded UPDATE protects the settle.
    #[test]
    fn ttl_is_a_noop_for_a_settled_job() {
        let mut store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(60);
        store.enqueue(&job).unwrap();
        let anchor = store.job_created_at(&job.id).unwrap().unwrap();
        let ttl = 60 * TEST_TTL_MULTIPLE as i64;

        // Dispatch and settle it before the TTL sweep runs.
        store.take_next(|j| j.id == job.id).unwrap().unwrap();
        assert!(store
            .record_completed(&signed_result(job.id, "ok"))
            .unwrap());
        assert_eq!(store.job_status(&job.id).unwrap().as_deref(), Some("done"));

        // Sweep far past the TTL: the done job is neither reaped nor reported.
        let expired = store
            .reap_ttl_expired(anchor + ttl + 1_000, TEST_TTL_MULTIPLE)
            .unwrap();
        assert!(expired.is_empty(), "a settled job must not be TTL-expired");
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("done"),
            "settle is preserved"
        );
        assert_eq!(store.failed_count().unwrap(), 0);
    }

    /// An extreme deadline_secs × multiple must saturate, not panic: the sweep
    /// runs in the background reaper, where an arithmetic overflow would unwind
    /// and kill the loop. `u32::MAX * u32::MAX ≈ 1.8e19` overflows `i64::MAX`, so
    /// without the saturating multiply this panics in debug. Saturated to
    /// i64::MAX the TTL effectively never expires, and the call returns cleanly.
    #[test]
    fn ttl_saturates_on_extreme_inputs_instead_of_panicking() {
        let store = Store::open_in_memory().unwrap();
        let job = job_with_deadline(u32::MAX);
        store.enqueue(&job).unwrap();

        let expired = store.reap_ttl_expired(i64::MAX, u32::MAX).unwrap();
        assert!(
            expired.is_empty(),
            "a saturated TTL effectively never expires"
        );
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("queued")
        );
    }

    /// The `ttl_deadline_multiple` knob is honored end-to-end: a small configured
    /// multiple collapses the TTL to `deadline * multiple`, and the reaper (reading
    /// `state.ttl_deadline_multiple`) dead-letters at exactly that boundary — NOT
    /// the 1440 const. With the old hard-coded const a deadline-10 job would live
    /// to anchor+14400, so reaping at anchor+20 here discriminates the knob (FM2).
    #[tokio::test]
    async fn ttl_deadline_multiple_knob_is_honored() {
        let state =
            AppState::with_store(Store::open_in_memory().unwrap(), StoreConfig { ttl_deadline_multiple: 2, ..test_config() })
                .unwrap();
        assert_eq!(state.ttl_deadline_multiple, 2, "the knob is carried into state");

        let job = job_with_deadline(10);
        let store = state.store.lock().await;
        store.enqueue(&job).unwrap();
        let anchor = store.job_created_at(&job.id).unwrap().unwrap();
        // deadline 10 × multiple 2 = 20.
        store
            .reap_ttl_expired(anchor + 20 - 1, state.ttl_deadline_multiple)
            .unwrap();
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("queued"),
            "one second before the configured TTL: still alive"
        );
        store
            .reap_ttl_expired(anchor + 20, state.ttl_deadline_multiple)
            .unwrap();
        assert_eq!(
            store.job_status(&job.id).unwrap().as_deref(),
            Some("failed"),
            "at the configured TTL (deadline×2): dead-lettered"
        );
    }

    /// A zero multiple is rejected at construction (FM1): TTL = `deadline * 0 == 0`
    /// would dead-letter every non-terminal job on the first reap tick, turning the
    /// safety backstop into a guillotine. `with_store` returns Err; a positive
    /// multiple constructs fine.
    #[test]
    fn with_store_rejects_zero_ttl_deadline_multiple() {
        assert!(
            AppState::with_store(Store::open_in_memory().unwrap(), StoreConfig { ttl_deadline_multiple: 0, ..test_config() })
                .is_err(),
            "multiple=0 must be rejected"
        );
        assert!(
            AppState::with_store(Store::open_in_memory().unwrap(), StoreConfig { ttl_deadline_multiple: 1, ..test_config() })
                .is_ok(),
            "the smallest valid multiple (1) constructs"
        );
    }

    /// A zero handshake timeout is rejected at construction: `timeout(ZERO, …)`
    /// fires immediately, so every connection would close before sending its
    /// Hello — the slowloris bound becomes a total registration outage. Any
    /// positive bound constructs fine.
    #[test]
    fn with_store_rejects_zero_handshake_timeout() {
        assert!(
            AppState::with_store(Store::open_in_memory().unwrap(), StoreConfig { handshake_timeout: Duration::ZERO, ..test_config() })
                .is_err(),
            "a zero handshake timeout must be rejected"
        );
        assert!(
            AppState::with_store(Store::open_in_memory().unwrap(), StoreConfig { handshake_timeout: Duration::from_millis(1), ..test_config() })
                .is_ok(),
            "the smallest positive handshake timeout constructs"
        );
    }

    /// A zero session-idle timeout is rejected at construction: the first idle check
    /// would see `last_inbound.elapsed() >= 0` and close every established session
    /// immediately — the read-idle bound becomes a total session outage. Any positive
    /// bound constructs fine.
    #[test]
    fn with_store_rejects_zero_session_idle_timeout() {
        assert!(
            AppState::with_store(Store::open_in_memory().unwrap(), StoreConfig { session_idle_timeout: Duration::ZERO, ..test_config() })
                .is_err(),
            "a zero session-idle timeout must be rejected"
        );
        assert!(
            AppState::with_store(Store::open_in_memory().unwrap(), StoreConfig { session_idle_timeout: Duration::from_millis(1), ..test_config() })
                .is_ok(),
            "the smallest positive session-idle timeout constructs"
        );
    }

    /// The CLI/env default reproduces the prior hard-coded behavior, and the flag
    /// overrides it — so an unset deployment is unchanged and a dev/operator can
    /// retune without a rebuild.
    #[test]
    fn args_ttl_deadline_multiple_default_and_override() {
        assert_eq!(
            Args::parse_from(["coordinator"]).ttl_deadline_multiple,
            JOB_TTL_DEADLINE_MULTIPLE,
            "unset default == the prior const (no behavior change)"
        );
        assert_eq!(
            Args::parse_from(["coordinator", "--ttl-deadline-multiple", "7"]).ttl_deadline_multiple,
            7,
            "the flag is honored"
        );
    }

    /// The handshake-timeout knob defaults to the const (an unset deployment gets
    /// the generous 10s headroom) and honors the flag — an operator can tighten or
    /// loosen the slowloris bound without a rebuild.
    #[test]
    fn args_handshake_timeout_default_and_override() {
        assert_eq!(
            Args::parse_from(["coordinator"]).handshake_timeout_secs,
            DEFAULT_HANDSHAKE_TIMEOUT.as_secs(),
            "unset default == the const"
        );
        assert_eq!(
            Args::parse_from(["coordinator", "--handshake-timeout-secs", "3"]).handshake_timeout_secs,
            3,
            "the flag is honored"
        );
    }

    /// The session-idle-timeout knob defaults to the const (an unset deployment gets
    /// the generous 90s read-idle bound) and honors the flag — an operator can tune
    /// the post-Hello slowloris bound against the real fleet without a rebuild.
    #[test]
    fn args_session_idle_timeout_default_and_override() {
        assert_eq!(
            Args::parse_from(["coordinator"]).session_idle_timeout_secs,
            DEFAULT_SESSION_IDLE_TIMEOUT.as_secs(),
            "unset default == the const"
        );
        assert_eq!(
            Args::parse_from(["coordinator", "--session-idle-timeout-secs", "45"])
                .session_idle_timeout_secs,
            45,
            "the flag is honored"
        );
    }

    #[test]
    fn args_http_body_timeout_default_and_override() {
        assert_eq!(
            Args::parse_from(["coordinator"]).http_body_timeout_secs,
            DEFAULT_HTTP_BODY_TIMEOUT.as_secs(),
            "unset default == the const"
        );
        assert_eq!(
            Args::parse_from(["coordinator", "--http-body-timeout-secs", "7"]).http_body_timeout_secs,
            7,
            "the flag is honored"
        );
    }

    #[test]
    fn args_max_connections_default_and_override() {
        assert_eq!(
            Args::parse_from(["coordinator"]).max_connections,
            DEFAULT_MAX_CONNECTIONS,
            "unset default == the const"
        );
        assert_eq!(
            Args::parse_from(["coordinator", "--max-connections", "9"]).max_connections,
            9,
            "the flag is honored"
        );
    }

    #[tokio::test]
    async fn stats_reports_in_flight() {
        // Build state on a non-seeded in-memory store so the queue truly starts
        // empty (test_state_empty parks the auto-seeded job in_flight, which
        // would skew the in_flight count this test asserts on).
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        // Poll → job in_flight.
        let resp = get(state.clone(), "/jobs/next").await;
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_in_flight"], 1);
        assert_eq!(json["jobs_queued"], 0);

        // Submit → job done.
        let good = signed_result(job_id, "deadbeef");
        let uri = format!("/jobs/{}/submit", job_id);
        let resp = post_submit(
            state.clone(),
            &uri,
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_in_flight"], 0);
        assert_eq!(json["jobs_completed"], 1);
    }

    /// `/stats` always exposes a `jobs_failed` field, even when it is 0.
    #[tokio::test]
    async fn stats_reports_jobs_failed_field() {
        let state = test_state_empty().await;
        let json = body_json(get(state.clone(), "/stats").await).await;
        // Field must be present and equal 0 when no jobs have been dead-lettered.
        assert_eq!(
            json["jobs_failed"], 0,
            "jobs_failed must be present and zero when no jobs are dead-lettered"
        );
    }

    #[tokio::test]
    async fn stats_reports_attempt_and_fault_totals() {
        // Seeded jobs are all queued (attempts == 0), so a fresh mesh reports 0/0
        // even with a non-empty backlog.
        let state = test_state();
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["total_attempts"], 0, "fresh mesh reports 0 attempts");
        assert_eq!(json["total_faults"], 0, "fresh mesh reports 0 faults");

        // Drive ONE job (selected by id so the queued seeds stay untouched at
        // attempts 0 regardless of FIFO order) through a redispatch and an earner
        // fault, directly via the store the handler reads.
        let job = job_with_deadline(100);
        let id = job.id;
        {
            let store = state.store.lock().await;
            store.enqueue(&job).unwrap();
            store.take_next(|j| j.id == id).unwrap(); // attempts → 1
            store.touch(&id, 1, 1000, 0).unwrap();
            store.reap_expired(1100, 999).unwrap(); // requeue past deadline (attempts unchanged)
            store.take_next(|j| j.id == id).unwrap(); // attempts → 2 (redispatched)
            store.requeue_earner_fault(&job, 999, None).unwrap(); // attempts → 1, faults → 1
            store.take_next(|j| j.id == id).unwrap(); // attempts → 2
        }

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["total_attempts"], 2, "two net dispatches of one job");
        assert_eq!(json["total_faults"], 1, "one earner fault charged");
        // Additive + FM3: the gross attempt total (2) coexists on the same fixture
        // with the pre-existing distinct-redispatched-jobs count (1) — they are
        // different numbers by design, not a contradiction. Existing fields stay.
        assert_eq!(json["jobs_redispatched"], 1, "one distinct job redispatched");
        assert_eq!(json["jobs_failed"], 0, "nothing dead-lettered");
    }

    #[test]
    fn earner_is_live_at_boundary_then_stale() {
        let info = EarnerInfo {
            gpu_model: "RTX 4090".into(),
            vram_gb: 24,
            supported: vec![JobKind::Terrain],
            last_seen: 1000,
        };
        assert!(info.is_live(1000, 60), "0s elapsed is live");
        assert!(info.is_live(1060, 60), "exactly ttl elapsed is still live");
        assert!(!info.is_live(1061, 60), "ttl+1 elapsed is stale");
    }

    #[test]
    fn prune_removes_only_stale_earners() {
        let mut map: HashMap<String, EarnerInfo> = HashMap::new();
        map.insert(
            "fresh".into(),
            EarnerInfo {
                gpu_model: "a".into(),
                vram_gb: 24,
                supported: vec![JobKind::Terrain],
                last_seen: 1000,
            },
        );
        map.insert(
            "stale".into(),
            EarnerInfo {
                gpu_model: "b".into(),
                vram_gb: 16,
                supported: vec![JobKind::NpcTick],
                last_seen: 900,
            },
        );
        // now=1000, ttl=60: fresh elapsed 0 (live); stale elapsed 100 (>60, dead).
        let removed = prune_stale_earners(&mut map, 1000, 60);
        assert_eq!(removed, 1);
        assert!(map.contains_key("fresh"));
        assert!(!map.contains_key("stale"));
    }

    #[tokio::test]
    async fn stats_excludes_stale_earners() {
        let state = test_state_empty().await;
        // Register via HTTP — freshly seen, so it counts.
        let addr = test_address("abc");
        let msg = hello("abc", 24, vec![JobKind::Terrain]);
        let resp = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(&msg).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 1);
        assert_eq!(json["total_vram_gb"], 24);

        // Force last_seen far into the past → stale (default ttl in test_state is 60).
        {
            let mut earners = state.earners.lock().await;
            earners.get_mut(&addr).unwrap().last_seen = 0;
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["gpus_joined"], 0,
            "stale earner must drop out of gpus_joined"
        );
        assert_eq!(
            json["total_vram_gb"], 0,
            "stale earner's vram must not be counted"
        );
        let terrain = &json["supported_breakdown"]["terrain"];
        assert!(
            terrain.is_null() || terrain == &serde_json::json!(0),
            "stale earner's supported kind must not be counted, got {terrain:?}"
        );
    }

    #[tokio::test]
    async fn job_status_endpoint_reports_lifecycle() {
        let state = test_state_empty().await;
        let job = seed_job();
        let job_id = job.id;
        enqueue(&state, &job).await;

        // queued
        let resp = get(state.clone(), &format!("/jobs/{job_id}/status")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["status"], "queued");
        assert_eq!(json["id"], job_id.to_string());

        // in_flight after a poll
        state.store.lock().await.take_next(|_| true).unwrap();
        let json = body_json(get(state.clone(), &format!("/jobs/{job_id}/status")).await).await;
        assert_eq!(json["status"], "in_flight");

        // done after a valid submit
        let good = signed_result(job_id, "deadbeef");
        let resp = post_submit(
            state.clone(),
            &format!("/jobs/{job_id}/submit"),
            &serde_json::to_value(&good).unwrap(),
            1,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(get(state.clone(), &format!("/jobs/{job_id}/status")).await).await;
        assert_eq!(json["status"], "done");
    }

    #[tokio::test]
    async fn job_status_endpoint_404_for_unknown_job() {
        let state = test_state_empty().await;
        let unknown = Uuid::new_v4();
        let resp = get(state.clone(), &format!("/jobs/{unknown}/status")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ---- reaper index (idx_jobs_status_created_at) ----

    /// Schema init creates the reaper's covering index, so the per-tick scan seeks
    /// instead of full-scanning the never-archived jobs table. Idempotent: a second
    /// init over the same DB (a restart) must not error.
    #[test]
    fn reaper_status_created_at_index_exists_after_init() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.has_index("idx_jobs_status_created_at").unwrap());
    }

    /// FM2: creation alone doesn't imply the planner uses it. The TTL reaper's exact
    /// predicate (`status IN (queued, in_flight) AND created_at IS NOT NULL`) must
    /// EXPLAIN to an index SEARCH on idx_jobs_status_created_at, not a full SCAN.
    #[test]
    fn reap_ttl_query_plan_uses_the_index() {
        let store = Store::open_in_memory().unwrap();
        // A couple of rows so the plan reflects a populated table (the planner is
        // schema-driven without ANALYZE, but this keeps the test realistic).
        store.enqueue(&job_with_deadline(60)).unwrap();
        store.enqueue(&job_with_deadline(60)).unwrap();

        let plan = store.reap_ttl_query_plan().unwrap();
        assert!(
            plan.contains("idx_jobs_status_created_at"),
            "TTL reap query must use the index, got plan: {plan}"
        );
        assert!(
            !plan.contains("SCAN jobs"),
            "TTL reap query must not full-scan jobs, got plan: {plan}"
        );
    }

    // ---- retention sweep (prune_terminal_jobs) ----

    /// A terminal job older than the horizon is pruned; one inside the window is
    /// retained — retention is a sliding window, not a wipe (FM4).
    #[test]
    fn retention_prunes_aged_terminal_jobs_and_keeps_recent_ones() {
        let mut store = Store::open_in_memory().unwrap();
        let horizon = 1000;
        let now = now_secs();

        let old = job_with_deadline(60);
        store.enqueue(&old).unwrap();
        store.take_next(|_| true).unwrap();
        assert!(store.requeue(&old, 1).unwrap(), "dead-lettered to failed");
        store.set_created_at(&old.id, now - horizon - 1).unwrap();

        let fresh = job_with_deadline(60);
        store.enqueue(&fresh).unwrap();
        store.take_next(|_| true).unwrap();
        assert!(store.requeue(&fresh, 1).unwrap());
        store.set_created_at(&fresh.id, now).unwrap();

        assert_eq!(store.prune_terminal_jobs(now, horizon, 256).unwrap(), 1);
        assert_eq!(store.job_status(&old.id).unwrap(), None, "aged job deleted");
        assert_eq!(
            store.job_status(&fresh.id).unwrap().as_deref(),
            Some("failed"),
            "recent terminal job retained"
        );
    }

    /// The horizon boundary is inclusive (`created_at <= now - horizon`): a job
    /// exactly at the cutoff prunes, one a second newer is retained.
    #[test]
    fn retention_horizon_boundary_is_inclusive() {
        let mut store = Store::open_in_memory().unwrap();
        let horizon = 1000;
        let now = now_secs();

        let at = job_with_deadline(60);
        store.enqueue(&at).unwrap();
        store.take_next(|_| true).unwrap();
        assert!(store.requeue(&at, 1).unwrap());
        store.set_created_at(&at.id, now - horizon).unwrap(); // exactly at cutoff

        let inside = job_with_deadline(60);
        store.enqueue(&inside).unwrap();
        store.take_next(|_| true).unwrap();
        assert!(store.requeue(&inside, 1).unwrap());
        store.set_created_at(&inside.id, now - horizon + 1).unwrap(); // one second newer

        assert_eq!(store.prune_terminal_jobs(now, horizon, 256).unwrap(), 1);
        assert_eq!(store.job_status(&at.id).unwrap(), None, "at-cutoff pruned");
        assert_eq!(
            store.job_status(&inside.id).unwrap().as_deref(),
            Some("failed"),
            "one second inside the window retained"
        );
    }

    /// FM1: an aged DONE job is kept while its EAS receipt is still pending
    /// (`uid IS NULL`); once relayed it prunes, taking its result + attestation rows
    /// with it (no orphan).
    #[test]
    fn retention_keeps_a_done_job_until_its_attestation_is_relayed() {
        let mut store = Store::open_in_memory().unwrap();
        let horizon = 1000;
        let now = now_secs();

        let job = seed_job();
        store.enqueue(&job).unwrap();
        store.take_next(|_| true).unwrap();
        assert!(store.record_completed(&signed_result(job.id, "r")).unwrap());
        store.set_created_at(&job.id, now - horizon - 1).unwrap();

        assert_eq!(
            store.prune_terminal_jobs(now, horizon, 256).unwrap(),
            0,
            "pending attestation retains the aged done job"
        );
        assert_eq!(store.job_status(&job.id).unwrap().as_deref(), Some("done"));

        assert!(store.mark_submitted(&job.id, "uid-1", now).unwrap());
        assert_eq!(store.prune_terminal_jobs(now, horizon, 256).unwrap(), 1);
        assert_eq!(store.job_status(&job.id).unwrap(), None);
        assert!(
            store.get_result(&job.id).unwrap().is_none(),
            "result row deleted with the job"
        );
        assert!(
            store.pending_attestation(&job.id).unwrap().is_none(),
            "attestation row deleted with the job"
        );
    }

    /// FM1: a metered DONE job is kept while its ComputeMeter debit is still pending
    /// (`tx_hash IS NULL`) even after its attestation has been relayed; only once
    /// BOTH on-chain obligations are discharged does it prune.
    #[test]
    fn retention_keeps_a_done_job_until_its_debit_is_relayed() {
        let rate = 1_000_000_000_000u128;
        let mut store = Store::open_in_memory().unwrap().with_compute_rate_wei(rate);
        let horizon = 1000;
        let now = now_secs();

        let job = seed_job();
        assert!(store.enqueue_within_cap(&job, 100, Some(TEST_BUYER)).unwrap());
        store.take_next(|_| true).unwrap();
        assert!(store.record_completed(&signed_result(job.id, "r")).unwrap());
        store.set_created_at(&job.id, now - horizon - 1).unwrap();

        // Relay the attestation but leave the debit pending — the unspent charge
        // still keeps the record.
        assert!(store.mark_submitted(&job.id, "uid-1", now).unwrap());
        assert_eq!(
            store.prune_terminal_jobs(now, horizon, 256).unwrap(),
            0,
            "pending debit retains the job"
        );
        assert_eq!(store.job_status(&job.id).unwrap().as_deref(), Some("done"));

        assert!(store.mark_debit_submitted(&job.id, "0xtx", now).unwrap());
        assert_eq!(store.prune_terminal_jobs(now, horizon, 256).unwrap(), 1);
        assert!(
            store.pending_debit(&job.id).unwrap().is_none(),
            "debit row deleted with the job"
        );
    }

    /// Retention NEVER deletes live work: an aged `queued` or `in_flight` job is
    /// the reapers' domain, not the retention sweep's.
    #[test]
    fn retention_only_deletes_terminal_jobs() {
        let mut store = Store::open_in_memory().unwrap();
        let horizon = 1000;
        let now = now_secs();

        let in_flight = job_with_deadline(60);
        store.enqueue(&in_flight).unwrap();
        store.take_next(|_| true).unwrap();
        store.set_created_at(&in_flight.id, now - horizon - 1).unwrap();

        let queued = job_with_deadline(60);
        store.enqueue(&queued).unwrap();
        store.set_created_at(&queued.id, now - horizon - 1).unwrap();

        assert_eq!(
            store.prune_terminal_jobs(now, horizon, 256).unwrap(),
            0,
            "live work is never pruned by retention"
        );
        assert_eq!(
            store.job_status(&queued.id).unwrap().as_deref(),
            Some("queued")
        );
        assert_eq!(
            store.job_status(&in_flight.id).unwrap().as_deref(),
            Some("in_flight")
        );
    }

    /// FM2: each call prunes at most `batch` jobs, so the reaper holds the store
    /// lock for a bounded delete; the rest drains on the next call.
    #[test]
    fn retention_batch_bounds_deletions_per_call() {
        let mut store = Store::open_in_memory().unwrap();
        let horizon = 1000;
        let now = now_secs();

        let mut ids = Vec::new();
        for _ in 0..5 {
            let j = job_with_deadline(60);
            store.enqueue(&j).unwrap();
            store.take_next(|_| true).unwrap();
            assert!(store.requeue(&j, 1).unwrap());
            store.set_created_at(&j.id, now - horizon - 1).unwrap();
            ids.push(j.id);
        }

        assert_eq!(store.prune_terminal_jobs(now, horizon, 2).unwrap(), 2);
        assert_eq!(store.prune_terminal_jobs(now, horizon, 2).unwrap(), 2);
        assert_eq!(store.prune_terminal_jobs(now, horizon, 2).unwrap(), 1);
        assert_eq!(
            store.prune_terminal_jobs(now, horizon, 2).unwrap(),
            0,
            "backlog drained"
        );
        for id in ids {
            assert_eq!(store.job_status(&id).unwrap(), None);
        }
    }

    /// The nextAction's "/stats totals still correct": after a prune the lifetime
    /// counts drop CONSISTENTLY — the results row goes with its done job, so
    /// `completed_count` (results) never outlives the job it counted.
    #[test]
    fn retention_prune_keeps_stats_counts_consistent() {
        let mut store = Store::open_in_memory().unwrap();
        let horizon = 1000;
        let now = now_secs();

        let done = seed_job();
        store.enqueue(&done).unwrap();
        store.take_next(|_| true).unwrap();
        assert!(store.record_completed(&signed_result(done.id, "r")).unwrap());
        assert!(store.mark_submitted(&done.id, "uid", now).unwrap());
        store.set_created_at(&done.id, now - horizon - 1).unwrap();

        let failed = job_with_deadline(60);
        store.enqueue(&failed).unwrap();
        store.take_next(|_| true).unwrap();
        assert!(store.requeue(&failed, 1).unwrap());
        store.set_created_at(&failed.id, now - horizon - 1).unwrap();

        assert_eq!(store.completed_count().unwrap(), 1);
        assert_eq!(store.failed_count().unwrap(), 1);
        assert_eq!(store.total_render_seconds().unwrap(), 1);

        assert_eq!(store.prune_terminal_jobs(now, horizon, 256).unwrap(), 2);

        assert_eq!(
            store.completed_count().unwrap(),
            0,
            "results row pruned with the done job — no orphan inflating the count"
        );
        assert_eq!(store.failed_count().unwrap(), 0);
        assert_eq!(
            store.total_render_seconds().unwrap(),
            0,
            "render-seconds drop with the pruned results"
        );
    }

    /// FM1-class: the prune candidate scan is served by the (status, created_at)
    /// index over only the aged terminal rows — never a full `SCAN jobs` over the
    /// history it exists to bound, and with no temp-b-tree sort.
    #[test]
    fn retention_query_plan_uses_the_index() {
        let store = Store::open_in_memory().unwrap();
        store.enqueue(&job_with_deadline(60)).unwrap();
        store.enqueue(&job_with_deadline(60)).unwrap();
        let plan = store.prune_terminal_query_plan().unwrap();
        assert!(
            plan.contains("idx_jobs_status_created_at"),
            "prune candidate scan must use the index, got plan: {plan}"
        );
        assert!(
            !plan.contains("SCAN jobs"),
            "prune candidate scan must not full-scan jobs, got plan: {plan}"
        );
        assert!(
            !plan.contains("USE TEMP B-TREE"),
            "prune candidate scan needs no global sort (LIMIT batch), got plan: {plan}"
        );
    }

    /// FM3: a zero retention horizon is rejected at construction — it would set the
    /// cutoff to `now` and delete every terminal record on the first sweep.
    #[test]
    fn store_config_rejects_zero_retention_secs() {
        let r = AppState::with_store(
            Store::open_in_memory().unwrap(),
            StoreConfig { retention_secs: 0, ..test_config() },
        );
        assert!(r.is_err(), "zero retention_secs must be rejected at construction");
    }

    /// The reaper's per-tick helper reads `state.retention_secs`, prunes aged
    /// terminal jobs under the store lock (released between batches), and returns
    /// the count — the wiring seam between the reaper and `prune_terminal_jobs`.
    #[tokio::test]
    async fn reaper_retention_sweep_prunes_through_appstate() {
        let state = test_state_empty().await; // DEFAULT_RETENTION_SECS horizon
        let now = now_secs();
        let job = job_with_deadline(60);
        {
            let store = state.store.lock().await;
            store.enqueue(&job).unwrap();
            store.take_next(|_| true).unwrap();
            assert!(store.requeue(&job, 1).unwrap()); // → failed
            store
                .set_created_at(&job.id, now - state.retention_secs - 1)
                .unwrap(); // aged past the default horizon
        }
        assert_eq!(prune_terminal_history(&state).await, 1);
        assert_eq!(
            state.store.lock().await.job_status(&job.id).unwrap(),
            None,
            "the reaper helper deleted the aged terminal job"
        );
    }

    // ---- FIFO dispatch fairness (oldest-queued-first) ----

    /// Dispatch is oldest-first: two jobs enqueued in sequence are handed out in
    /// enqueue order (A then B), the opposite of the LIFO `rowid DESC` the
    /// coordinator used before. Within one wall-clock second the `rowid ASC`
    /// tiebreaker keeps the order deterministic.
    #[tokio::test]
    async fn dispatch_is_oldest_first() {
        let state = test_state_empty().await;
        let a = job_with_deadline(60);
        let b = job_with_deadline(60);
        enqueue(&state, &a).await;
        enqueue(&state, &b).await;

        let store = state.store.lock().await;
        let first = store.take_next(|_| true).unwrap().unwrap().0;
        let second = store.take_next(|_| true).unwrap().unwrap().0;
        assert_eq!(first.id, a.id, "oldest (first-enqueued) job dispatched first");
        assert_eq!(second.id, b.id, "newer job dispatched second");
    }

    /// `created_at` is the PRIMARY sort key, `rowid` only the tiebreaker: with three
    /// jobs whose age order (C, A, B) disagrees with both `rowid ASC` (A, B, C) and
    /// `rowid DESC` (C, B, A), dispatch follows age. Discriminates the FIFO ordering
    /// from any rowid-only ordering (the old `rowid DESC` and a naive `rowid ASC`).
    #[tokio::test]
    async fn dispatch_orders_by_created_at_over_rowid() {
        let state = test_state_empty().await;
        let a = job_with_deadline(60); // rowid 1
        let b = job_with_deadline(60); // rowid 2
        let c = job_with_deadline(60); // rowid 3
        enqueue(&state, &a).await;
        enqueue(&state, &b).await;
        enqueue(&state, &c).await;

        let store = state.store.lock().await;
        // Ages disagree with rowid: C oldest, then A, then B newest.
        assert!(store.set_created_at(&c.id, 1000).unwrap());
        assert!(store.set_created_at(&a.id, 2000).unwrap());
        assert!(store.set_created_at(&b.id, 3000).unwrap());

        let order: Vec<_> = (0..3)
            .map(|_| store.take_next(|_| true).unwrap().unwrap().0.id)
            .collect();
        assert_eq!(
            order,
            vec![c.id, a.id, b.id],
            "dispatch follows created_at age, not rowid"
        );
    }

    /// A requeue must not slide the immutable `created_at` anchor, so a requeued
    /// (older) job stays ahead of a job enqueued later. If requeue reset the anchor
    /// to "now", the older job would fall behind the newer one and be dispatched
    /// second — this pins that the requeued job returns to the front of the queue.
    #[tokio::test]
    async fn requeued_job_returns_to_front() {
        let state = test_state_empty().await;
        let old = job_with_deadline(60);
        let fresh = job_with_deadline(60);
        enqueue(&state, &old).await;
        enqueue(&state, &fresh).await;

        let store = state.store.lock().await;
        assert!(store.set_created_at(&old.id, 1000).unwrap());
        assert!(store.set_created_at(&fresh.id, 2000).unwrap());

        // Dispatch the older job, then requeue it on a deadline-miss (attempt-
        // charging) path; it is renderable so it returns to the queue.
        let taken = store.take_next(|j| j.id == old.id).unwrap().unwrap().0;
        assert_eq!(taken.id, old.id);
        // requeue() returns true only on dead-letter; a renderable requeue back to
        // queued returns false (attempts 1 < max 5).
        assert!(
            !store.requeue(&old, 5).unwrap(),
            "renderable job is requeued, not dead-lettered"
        );

        // It is still the oldest by created_at, so it is dispatched again before the
        // job enqueued after it — the requeue did not slide its anchor to the back.
        let next = store.take_next(|_| true).unwrap().unwrap().0;
        assert_eq!(
            next.id, old.id,
            "requeued job keeps its age and returns to the front"
        );
    }

    /// FM: the new oldest-first ordering must stay served by
    /// `idx_jobs_status_created_at` — `status = queued ORDER BY created_at ASC,
    /// rowid ASC` EXPLAINs to an index SEARCH with no full SCAN and no temp-b-tree
    /// sort, so the per-dispatch cost doesn't regress on a large terminal-history
    /// table.
    #[test]
    fn dispatch_query_plan_uses_the_index() {
        let store = Store::open_in_memory().unwrap();
        store.enqueue(&job_with_deadline(60)).unwrap();
        store.enqueue(&job_with_deadline(60)).unwrap();

        let plan = store.dispatch_query_plan().unwrap();
        assert!(
            plan.contains("idx_jobs_status_created_at"),
            "dispatch query must use the index, got plan: {plan}"
        );
        assert!(
            !plan.contains("SCAN jobs"),
            "dispatch query must not full-scan jobs, got plan: {plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "dispatch ordering must be index-served, not a temp-b-tree sort, got plan: {plan}"
        );
    }

    // ---- /stats + /earners aggregation indexes ----

    /// Schema init creates the covering index for the `/stats` lifetime
    /// SUM(attempts), SUM(faults) over the never-archived jobs table. Idempotent
    /// across restarts (CREATE INDEX IF NOT EXISTS).
    #[test]
    fn attempt_fault_totals_index_exists_after_init() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.has_index("idx_jobs_attempts_faults").unwrap());
    }

    /// FM1: creation alone doesn't imply use. The unfiltered SUM has no predicate to
    /// SEARCH on, so it SCANs — but it must scan the SKINNY `idx_jobs_attempts_faults`
    /// COVERING index (the marker), not the fat jobs table whose every row carries an
    /// inline multi-KB `spec_json`. A bare `SCAN jobs` with no `USING COVERING INDEX`
    /// is the regression this guards against.
    #[test]
    fn attempt_fault_totals_query_plan_uses_the_covering_index() {
        let store = Store::open_in_memory().unwrap();
        store.enqueue(&job_with_deadline(60)).unwrap();
        store.enqueue(&job_with_deadline(60)).unwrap();

        let plan = store.attempt_fault_totals_query_plan().unwrap();
        // The helper newline-terminates every detail row, so matching the name with
        // a trailing `\n` discriminates against a rename superstring
        // (`idx_jobs_attempts_faults_x`) too — the plan test stands on its own, not
        // only paired with the index-exists test.
        assert!(
            plan.contains("USING COVERING INDEX idx_jobs_attempts_faults\n"),
            "attempt/fault SUM must scan the skinny covering index by exact name, \
             not the fat jobs table, got plan: {plan}"
        );
    }

    /// The counter stays bounded under sustained faults: 50 faults from one earner
    /// collapse to a SINGLE row whose `fault_count` is the total — the table grows by
    /// distinct faulting earner, never by total faults recorded. This is the
    /// unbounded-growth residual the per-earner counter closes (vs the old one-row-
    /// per-fault table). Mutation check: the old INSERT-per-fault would make this 50.
    #[test]
    fn earner_faults_is_a_bounded_per_earner_counter() {
        let store = Store::open_in_memory().unwrap();
        let a = test_address("earner-a");
        for _ in 0..50 {
            store.record_earner_fault(&a).unwrap();
        }
        assert_eq!(
            store.earner_faults_row_count().unwrap(),
            1,
            "one row per earner, not one per fault"
        );
        assert_eq!(store.faults_by_earner().unwrap().get(&a).copied(), Some(50));
    }

    /// The per-earner counter read is a plain scan of `earner_faults` with NO
    /// `USE TEMP B-TREE FOR GROUP BY` — the counter (one row per earner) collapses
    /// the old GROUP BY entirely, so the structural sort the prior covering index
    /// removed can no longer arise at all.
    #[test]
    fn faults_by_earner_query_plan_has_no_temp_btree() {
        let store = Store::open_in_memory().unwrap();
        store.record_earner_fault(&test_address("earner-a")).unwrap();
        store.record_earner_fault(&test_address("earner-b")).unwrap();

        let plan = store.faults_by_earner_query_plan().unwrap();
        assert!(
            plan.contains("earner_faults"),
            "the read must scan earner_faults, got plan: {plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "the counter read has no GROUP BY, so no temp-b-tree sort, got plan: {plan}"
        );
    }

    /// FM3 (plan half): the `/stats` in-flight progress mean is a hot poll query, so
    /// its `status =` filter must be SEARCHed via `idx_jobs_status_created_at`'s
    /// leading column over the bounded live set — never a full `SCAN jobs` that drags
    /// every terminal row's inline `spec_json` through the unbounded history on every
    /// poll. Guards the `in_flight_progress_pct_avg` doc-comment claim against an index
    /// drop or a query change that silently regresses it to a table scan.
    #[test]
    fn in_flight_progress_pct_avg_query_plan_uses_the_index() {
        let store = Store::open_in_memory().unwrap();
        store.enqueue(&job_with_deadline(60)).unwrap();
        store.enqueue(&job_with_deadline(60)).unwrap();

        let plan = store.in_flight_progress_pct_avg_query_plan().unwrap();
        assert!(
            plan.contains("idx_jobs_status_created_at"),
            "the in-flight progress mean must use the index, got plan: {plan}"
        );
        assert!(
            !plan.contains("SCAN jobs"),
            "the in-flight progress mean must not full-scan jobs, got plan: {plan}"
        );
    }

    /// A legacy ROW-shaped earner_faults (one row per fault, pre-counter) migrates
    /// to the counter on open: each earner's row COUNT rolls into `fault_count`,
    /// preserving every lifetime total EXACTLY, and the table collapses to one row
    /// per earner. The rebuild is idempotent — reopening does NOT re-run it or
    /// double-count (it is guarded on the now-absent `job_id` column).
    #[test]
    fn legacy_earner_faults_rows_migrate_to_the_counter_idempotently() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();

        // Seed the pre-counter table shape directly: 3 rows for aaa, 1 for bbb.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE earner_faults (
                     earner     TEXT NOT NULL,
                     job_id     TEXT NOT NULL,
                     created_at INTEGER NOT NULL
                 );
                 INSERT INTO earner_faults (earner, job_id, created_at) VALUES
                     ('0xaaa', 'job-1', 100),
                     ('0xaaa', 'job-1', 101),
                     ('0xaaa', 'job-2', 102),
                     ('0xbbb', 'job-3', 103);",
            )
            .unwrap();
        }

        // Open with the current code → init() runs the rebuild migration.
        let store = Store::open(&db_path).unwrap();
        let by = store.faults_by_earner().unwrap();
        assert_eq!(by.get("0xaaa").copied(), Some(3), "aaa: 3 legacy rows -> fault_count 3");
        assert_eq!(by.get("0xbbb").copied(), Some(1), "bbb: 1 legacy row -> fault_count 1");
        assert_eq!(
            store.earner_faults_row_count().unwrap(),
            2,
            "the table collapses to one counter row per earner"
        );
        drop(store);

        // Reopen: the guard sees no job_id column, so the migration does not re-run
        // and the totals are unchanged (not doubled).
        let reopened = Store::open(&db_path).unwrap();
        let by2 = reopened.faults_by_earner().unwrap();
        assert_eq!(by2.get("0xaaa").copied(), Some(3), "reopen does not re-run the migration");
        assert_eq!(by2.get("0xbbb").copied(), Some(1));
        assert_eq!(reopened.earner_faults_row_count().unwrap(), 2);
    }

    // ---- GET /jobs/{id} full detail ----

    /// `GET /jobs/{id}` returns the full spec; `result` is null while the job is
    /// queued and carries the recorded `JobResult` once it is done; an unknown
    /// id is a 404.
    #[tokio::test]
    async fn job_detail_returns_spec_then_spec_and_result() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });

        // A queued job: detail returns its spec, no result yet.
        let queued = job_with_deadline(60);
        enqueue(&state, &queued).await;
        let resp = get(state.clone(), &format!("/jobs/{}", queued.id)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["spec"]["id"], queued.id.to_string());
        assert_eq!(json["spec"]["kind"], "terrain");
        assert!(
            json["result"].is_null(),
            "a queued job has no recorded result"
        );

        // A completed job: detail returns the spec plus the recorded result.
        let done = job_with_deadline(60);
        let done_id = done.id;
        enqueue(&state, &done).await;
        {
            let mut store = state.store.lock().await;
            store.take_next(|j| j.id == done_id).unwrap();
            store
                .record_completed(&signed_result(done_id, "deadbeef"))
                .unwrap();
        }
        let json = body_json(get(state.clone(), &format!("/jobs/{done_id}")).await).await;
        assert_eq!(json["spec"]["id"], done_id.to_string());
        assert_eq!(json["result"]["job_id"], done_id.to_string());
        assert_eq!(json["result"]["output_hash"], test_output_hash("deadbeef"));
        assert_eq!(json["result"]["render_seconds"], 1);

        // Unknown id → 404.
        let resp = get(state.clone(), &format!("/jobs/{}", Uuid::new_v4())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ---- GET /jobs listing ----

    #[tokio::test]
    async fn list_jobs_returns_enqueued_shape() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });

        let terrain_job = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Terrain,
            region: RegionCoord {
                x: 1,
                y: 2,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({"heightfield_seed": 1u64}),
        };
        let foliage_job = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Foliage,
            region: RegionCoord {
                x: 3,
                y: 4,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({"density": 0.5}),
        };

        enqueue(&state, &terrain_job).await;
        enqueue(&state, &foliage_job).await;

        let resp = get(state.clone(), "/jobs").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // newest first: foliage was enqueued last
        assert_eq!(arr[0]["kind"], "foliage");
        assert_eq!(arr[1]["kind"], "terrain");
        assert!(!arr[0]["id"].is_null());
        assert!(!arr[1]["id"].is_null());
        assert_eq!(arr[0]["status"], "queued");
        assert_eq!(arr[1]["status"], "queued");
    }

    #[tokio::test]
    async fn list_jobs_empty_is_empty_array() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        let resp = get(state.clone(), "/jobs").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert!(json.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_jobs_caps_at_limit() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });

        let mut ids = Vec::new();
        for i in 0..5u64 {
            let job = JobSpec {
                id: Uuid::new_v4(),
                kind: JobKind::Terrain,
                region: RegionCoord {
                    x: i as i32,
                    y: 0,
                    layer: 0,
                },
                deadline_secs: 60,
                max_payout_wei: "1000000000000000000".into(),
                inputs: serde_json::json!({"heightfield_seed": i}),
            };
            ids.push(job.id);
            enqueue(&state, &job).await;
        }

        let rows = state.store.lock().await.list_jobs(3, None).unwrap();
        assert_eq!(rows.len(), 3);
        // newest first: last 3 ids enqueued are ids[4], ids[3], ids[2]
        assert_eq!(rows[0].0, ids[4]);
        assert_eq!(rows[1].0, ids[3]);
        assert_eq!(rows[2].0, ids[2]);
    }

    /// `list_jobs(.., Some(status))` returns exactly the jobs in that status,
    /// across all four lifecycle states; `None` returns every job.
    #[test]
    fn list_jobs_status_filter_covers_all_statuses() {
        let mut store = Store::open_in_memory().unwrap();

        // queued: enqueued, never taken.
        let queued = job_with_deadline(60);
        store.enqueue(&queued).unwrap();

        // in_flight: enqueued then taken (by id, so we pick exactly this one).
        let in_flight = job_with_deadline(60);
        store.enqueue(&in_flight).unwrap();
        store.take_next(|j| j.id == in_flight.id).unwrap();

        // done: enqueued, taken, completed.
        let done = job_with_deadline(60);
        store.enqueue(&done).unwrap();
        store.take_next(|j| j.id == done.id).unwrap();
        store
            .record_completed(&signed_result(done.id, "deadbeef"))
            .unwrap();

        // failed: enqueued, taken (attempts→1), requeued at max_attempts=1 →
        // dead-lettered to `failed`.
        let failed = job_with_deadline(60);
        store.enqueue(&failed).unwrap();
        store.take_next(|j| j.id == failed.id).unwrap();
        assert!(
            store.requeue(&failed, 1).unwrap(),
            "attempts>=max must dead-letter"
        );

        // Each filter returns exactly its one job.
        for (status, expected) in [
            ("queued", queued.id),
            ("in_flight", in_flight.id),
            ("done", done.id),
            ("failed", failed.id),
        ] {
            let rows = store.list_jobs(100, Some(status)).unwrap();
            assert_eq!(rows.len(), 1, "exactly one job in status {status}");
            assert_eq!(rows[0].0, expected, "wrong job for status {status}");
            assert_eq!(rows[0].2, status, "status column must match the filter");
        }

        // No filter → all four.
        assert_eq!(store.list_jobs(100, None).unwrap().len(), 4);
    }

    /// `GET /jobs?status=` filters by status, returns all jobs when absent, and
    /// 400s on an unrecognized status value.
    #[tokio::test]
    async fn list_jobs_status_query_filters_and_400s() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });

        let queued = job_with_deadline(60);
        enqueue(&state, &queued).await;
        let in_flight = job_with_deadline(60);
        enqueue(&state, &in_flight).await;
        state
            .store
            .lock()
            .await
            .take_next(|j| j.id == in_flight.id)
            .unwrap();

        // ?status=queued → only the queued job.
        let json = body_json(get(state.clone(), "/jobs?status=queued").await).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], queued.id.to_string());
        assert_eq!(arr[0]["status"], "queued");

        // ?status=in_flight → only the in_flight job.
        let json = body_json(get(state.clone(), "/jobs?status=in_flight").await).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], in_flight.id.to_string());

        // ?status=done → none match yet → empty array, still 200.
        let resp = get(state.clone(), "/jobs?status=done").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_json(resp).await.as_array().unwrap().is_empty());

        // No filter → both jobs.
        let json = body_json(get(state.clone(), "/jobs").await).await;
        assert_eq!(json.as_array().unwrap().len(), 2);

        // Unrecognized status → 400.
        let resp = get(state.clone(), "/jobs?status=bogus").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn stats_reports_queued_by_kind() {
        let state = test_state_empty().await;
        // enqueue 2 Terrain + 1 Foliage, all queued
        let terrain1 = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Terrain,
            region: RegionCoord {
                x: 1,
                y: 2,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({"heightfield_seed": 1u64}),
        };
        let terrain2 = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Terrain,
            region: RegionCoord {
                x: 3,
                y: 4,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({"heightfield_seed": 2u64}),
        };
        let foliage1 = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Foliage,
            region: RegionCoord {
                x: 5,
                y: 6,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({"density": 0.5}),
        };
        enqueue(&state, &terrain1).await;
        enqueue(&state, &terrain2).await;
        enqueue(&state, &foliage1).await;
        let resp = get(state.clone(), "/stats").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["queued_by_kind"]["terrain"], 2);
        assert_eq!(json["queued_by_kind"]["foliage"], 1);
        // a kind with no queued jobs is absent from the map (serde_json: missing key -> null)
        assert!(json["queued_by_kind"]["optimization"].is_null());
    }

    #[tokio::test]
    async fn stats_reports_in_flight_by_kind() {
        // Raw non-seeded store so only the jobs we take are in_flight
        // (test_state_empty parks the auto-seeded jobs in_flight, which would
        // skew the per-kind in_flight counts — same reason stats_reports_in_flight
        // builds state directly).
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        let mk = |kind: JobKind, seed: u64| JobSpec {
            id: Uuid::new_v4(),
            kind,
            region: RegionCoord {
                x: seed as i32,
                y: 0,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({ "seed": seed }),
        };
        // 2 Terrain + 1 Foliage, then move all three to in_flight.
        enqueue(&state, &mk(JobKind::Terrain, 1)).await;
        enqueue(&state, &mk(JobKind::Terrain, 2)).await;
        enqueue(&state, &mk(JobKind::Foliage, 3)).await;
        {
            let store = state.store.lock().await;
            while store.take_next(|_| true).unwrap().is_some() {}
        }

        let resp = get(state.clone(), "/stats").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["in_flight_by_kind"]["terrain"], 2);
        assert_eq!(json["in_flight_by_kind"]["foliage"], 1);
        // a kind with nothing in flight is absent (serde_json: missing key -> null)
        assert!(json["in_flight_by_kind"]["optimization"].is_null());
        // sanity: the three jobs are in_flight and none remain queued
        assert_eq!(json["jobs_in_flight"], 3);
        assert_eq!(json["jobs_queued"], 0);
    }

    #[tokio::test]
    async fn stats_reports_completed_by_kind() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        let mk = |kind: JobKind, seed: u64| JobSpec {
            id: Uuid::new_v4(),
            kind,
            region: RegionCoord {
                x: seed as i32,
                y: 0,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({ "seed": seed }),
        };
        let terrain1 = mk(JobKind::Terrain, 1);
        let terrain2 = mk(JobKind::Terrain, 2);
        let foliage1 = mk(JobKind::Foliage, 3);
        enqueue(&state, &terrain1).await;
        enqueue(&state, &terrain2).await;
        enqueue(&state, &foliage1).await;
        {
            // Take all three in_flight, then complete terrain1 + foliage1,
            // leaving terrain2 in_flight.
            let mut store = state.store.lock().await;
            store.take_next(|j| j.id == terrain1.id).unwrap();
            store.take_next(|j| j.id == terrain2.id).unwrap();
            store.take_next(|j| j.id == foliage1.id).unwrap();
            store
                .record_completed(&signed_result(terrain1.id, "a"))
                .unwrap();
            store
                .record_completed(&signed_result(foliage1.id, "b"))
                .unwrap();
        }

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["completed_by_kind"]["terrain"], 1);
        assert_eq!(json["completed_by_kind"]["foliage"], 1);
        // a kind with nothing completed is absent (serde_json: missing key -> null)
        assert!(json["completed_by_kind"]["optimization"].is_null());
        // cross-checks: 2 done total; terrain2 is still in_flight, not completed
        assert_eq!(json["jobs_completed"], 2);
        assert_eq!(json["in_flight_by_kind"]["terrain"], 1);
    }

    #[tokio::test]
    async fn stats_reports_failed_by_kind() {
        // Raw non-seeded store so only the jobs we dead-letter are `failed`
        // (test_state_empty parks the auto-seeded jobs in_flight, never failed,
        // but a raw store keeps the failed composition unambiguous).
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        let mk = |kind: JobKind, seed: u64| JobSpec {
            id: Uuid::new_v4(),
            kind,
            region: RegionCoord {
                x: seed as i32,
                y: 0,
                layer: 0,
            },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({ "seed": seed }),
        };
        // 2 Terrain + 1 Foliage: take each once (attempts→1) then requeue at
        // max_attempts=1 → dead-lettered to `failed`.
        let jobs = [
            mk(JobKind::Terrain, 1),
            mk(JobKind::Terrain, 2),
            mk(JobKind::Foliage, 3),
        ];
        {
            let store = state.store.lock().await;
            for job in &jobs {
                store.enqueue(job).unwrap();
                store.take_next(|j| j.id == job.id).unwrap();
                assert!(
                    store.requeue(job, 1).unwrap(),
                    "attempts>=max must dead-letter"
                );
            }
        }

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["failed_by_kind"]["terrain"], 2);
        assert_eq!(json["failed_by_kind"]["foliage"], 1);
        // a kind with nothing failed is absent (serde_json: missing key -> null)
        assert!(json["failed_by_kind"]["optimization"].is_null());
        // cross-check the scalar total
        assert_eq!(json["jobs_failed"], 3);
    }

    #[tokio::test]
    async fn stats_reports_total_render_seconds() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        // No completed jobs yet → 0.
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["total_render_seconds"], 0);

        // Complete two jobs carrying distinct render_seconds (5 + 7); the field
        // is not part of the signed digest, so setting it post-sign is valid and
        // record_completed persists it verbatim.
        let a = job_with_deadline(60);
        let b = job_with_deadline(60);
        {
            let mut store = state.store.lock().await;
            for job in [&a, &b] {
                store.enqueue(job).unwrap();
                store.take_next(|j| j.id == job.id).unwrap();
            }
            let mut ra = signed_result(a.id, "aa");
            ra.render_seconds = 5;
            let mut rb = signed_result(b.id, "bb");
            rb.render_seconds = 7;
            store.record_completed(&ra).unwrap();
            store.record_completed(&rb).unwrap();
        }

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(
            json["total_render_seconds"], 12,
            "sum of 5 + 7 render-seconds"
        );
        assert_eq!(json["jobs_completed"], 2);
    }

    #[tokio::test]
    async fn stats_reports_total_payout_wei() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        // No completed jobs yet → "0" (serialized as a string).
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["total_payout_wei"], "0");

        // Complete two jobs with known max_payout_wei; the stat sums them as a
        // decimal wei string (1e18-scale, beyond JSON's safe-integer range).
        let mut a = job_with_deadline(60);
        a.max_payout_wei = "1500000000000000000".into(); // 1.5e18
        let mut b = job_with_deadline(60);
        b.max_payout_wei = "2500000000000000000".into(); // 2.5e18
        {
            let mut store = state.store.lock().await;
            for job in [&a, &b] {
                store.enqueue(job).unwrap();
                store.take_next(|j| j.id == job.id).unwrap();
                store.record_completed(&signed_result(job.id, "x")).unwrap();
            }
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        // 1.5e18 + 2.5e18 = 4.0e18 wei, serialized as a decimal string.
        assert_eq!(json["total_payout_wei"], "4000000000000000000");
        assert_eq!(json["jobs_completed"], 2);
    }

    // ---- GET /earners live leaderboard ----

    /// A `JobResult` attributed to `earner` with the given `render_seconds`.
    /// `record_completed` stores `earner_address` verbatim and does not verify
    /// the signature, so results can be credited to any address for the
    /// leaderboard aggregates.
    fn result_for(job_id: Uuid, earner: &str, render_seconds: u32) -> JobResult {
        JobResult {
            job_id,
            earner_address: earner.into(),
            output_hash: "h".into(),
            output_url: "memory://x".into(),
            render_seconds,
            signature_hex: "00".into(),
        }
    }

    /// `GET /earners` lists only live earners (stale ones excluded, like
    /// `/stats`), each carrying its advertised capabilities plus its lifetime
    /// completed-job and render-second totals from the `results` table.
    #[tokio::test]
    async fn earners_endpoint_lists_only_live_with_aggregates() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });

        // Register two earners; then force one far into the past → stale (ttl=60).
        let live = test_address("live");
        let stale = test_address("stale");
        for m in [
            &hello("live", 24, vec![JobKind::Terrain, JobKind::Foliage]),
            &hello("stale", 16, vec![JobKind::NpcTick]),
        ] {
            let resp = post_json(
                state.clone(),
                "/register",
                &serde_json::to_value(m).unwrap(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        {
            let mut earners = state.earners.lock().await;
            earners.get_mut(&stale).unwrap().last_seen = 0;
        }

        // Complete two jobs credited to the live earner (render_seconds 5 + 7).
        let a = job_with_deadline(60);
        let b = job_with_deadline(60);
        {
            let mut store = state.store.lock().await;
            store.enqueue(&a).unwrap();
            store.enqueue(&b).unwrap();
            store.take_next(|j| j.id == a.id).unwrap();
            store.take_next(|j| j.id == b.id).unwrap();
            store.record_completed(&result_for(a.id, &live, 5)).unwrap();
            store.record_completed(&result_for(b.id, &live, 7)).unwrap();
        }

        let resp = get(state.clone(), "/earners").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1, "only the live earner appears");
        assert_eq!(arr[0]["address"], live);
        assert_eq!(arr[0]["gpu_model"], "RTX 4090");
        assert_eq!(arr[0]["vram_gb"], 24);
        assert_eq!(arr[0]["completed"], 2);
        assert_eq!(arr[0]["render_seconds"], 12, "sum of 5 + 7");
        let supported = arr[0]["supported"].as_array().unwrap();
        assert_eq!(supported.len(), 2, "advertised kinds preserved");
    }

    /// `GET /earners` orders the leaderboard by completed desc (then
    /// render_seconds, then address), so the busiest live earner leads.
    #[tokio::test]
    async fn earners_endpoint_orders_by_completed_desc() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        let busy = test_address("busy");
        let idle = test_address("idle");
        for m in [
            &hello("busy", 24, vec![JobKind::Terrain]),
            &hello("idle", 24, vec![JobKind::Terrain]),
        ] {
            let resp = post_json(
                state.clone(),
                "/register",
                &serde_json::to_value(m).unwrap(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        // busy completes 2 jobs, idle completes 1.
        let jobs = [
            job_with_deadline(60),
            job_with_deadline(60),
            job_with_deadline(60),
        ];
        {
            let mut store = state.store.lock().await;
            for job in &jobs {
                store.enqueue(job).unwrap();
                store.take_next(|j| j.id == job.id).unwrap();
            }
            store
                .record_completed(&result_for(jobs[0].id, &busy, 1))
                .unwrap();
            store
                .record_completed(&result_for(jobs[1].id, &busy, 1))
                .unwrap();
            store
                .record_completed(&result_for(jobs[2].id, &idle, 1))
                .unwrap();
        }

        let json = body_json(get(state.clone(), "/earners").await).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["address"], busy, "busiest earner leads");
        assert_eq!(arr[0]["completed"], 2);
        assert_eq!(arr[1]["address"], idle);
        assert_eq!(arr[1]["completed"], 1);
    }

    /// `GET /earners` carries each earner's `payout_wei`: the sum of
    /// `max_payout_wei` across that earner's DONE jobs, as a decimal wei string
    /// (the per-earner counterpart to `/stats` `total_payout_wei`).
    #[tokio::test]
    async fn earners_endpoint_reports_per_earner_payout_wei() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            max_faults: 10,
            earner_ttl_secs: 60,
            ttl_deadline_multiple: JOB_TTL_DEADLINE_MULTIPLE,
            retention_secs: DEFAULT_RETENTION_SECS as i64,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            ingest_token: None,
            max_queued_jobs: DEFAULT_MAX_QUEUED_JOBS,
            max_earners: DEFAULT_MAX_EARNERS,
            registration_buckets: Mutex::new(HashMap::new()),
            max_registrations: DEFAULT_MAX_REGISTRATIONS,
            trusted_proxies: TrustedProxies::default(),
        });
        let pay = test_address("pay");
        let resp = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello("pay", 24, vec![JobKind::Terrain])).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Two DONE jobs credited to pay with known payouts (1.5e18 + 2.5e18).
        let mut a = job_with_deadline(60);
        a.max_payout_wei = "1500000000000000000".into();
        let mut b = job_with_deadline(60);
        b.max_payout_wei = "2500000000000000000".into();
        {
            let mut store = state.store.lock().await;
            for job in [&a, &b] {
                store.enqueue(job).unwrap();
                store.take_next(|j| j.id == job.id).unwrap();
            }
            store.record_completed(&result_for(a.id, &pay, 1)).unwrap();
            store.record_completed(&result_for(b.id, &pay, 1)).unwrap();
        }

        let json = body_json(get(state.clone(), "/earners").await).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["address"], pay);
        // 1.5e18 + 2.5e18 = 4.0e18 wei, serialized as a decimal string.
        assert_eq!(arr[0]["payout_wei"], "4000000000000000000");
        assert_eq!(arr[0]["completed"], 2);
    }

    /// Read text frames until one decodes to a `CoordinatorMsg` (skipping
    /// ping/pong control frames the server may interleave).
    async fn next_coordinator_msg(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> CoordinatorMsg {
        loop {
            let frame = ws.next().await.expect("ws closed").expect("ws error");
            match frame {
                WsMessage::Text(t) => {
                    return serde_json::from_str(&t).expect("decode CoordinatorMsg")
                }
                WsMessage::Close(_) => panic!("server closed before sending a message"),
                _ => continue,
            }
        }
    }
}
