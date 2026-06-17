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
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use proto::{CoordinatorMsg, EarnerMsg, JobKind, JobResult, JobSpec, RegionCoord};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
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
mod store;
mod validate;
mod verify;

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
    /// An earner is counted in `/stats` and kept in the in-memory registry only
    /// while it has been seen within this many seconds. Refreshed on Hello, on
    /// any websocket frame, on a periodic liveness tick while a ws earner is
    /// idle, and on an authenticated HTTP submit. Past it, the reaper prunes it.
    #[arg(long, env = "COORDINATOR_EARNER_TTL_SECS", default_value = "60")]
    earner_ttl_secs: u64,
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
    /// How long (seconds) an earner stays counted in `/stats` after its last
    /// sign of life. Mirrors `--earner-ttl-secs` / `COORDINATOR_EARNER_TTL_SECS`.
    earner_ttl_secs: i64,
}

impl AppState {
    /// Build state backed by `store`. Seeds one job only when the DB has no
    /// jobs yet, so a fresh DB gives earners something to do while a restart
    /// with existing jobs does NOT double-seed.
    fn with_store(store: Store, max_attempts: u32, earner_ttl_secs: i64) -> Result<Arc<Self>> {
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
            max_attempts,
            earner_ttl_secs,
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
    /// Count of jobs dispatched more than once (`attempts > 1`): the reaper
    /// requeued them on a missed deadline and they were handed out again. A
    /// cumulative reaper-churn signal for the HUD; 0 on a healthy mesh.
    jobs_redispatched: usize,
    /// Settled jobs whose EAS render receipt has not yet been relayed on-chain —
    /// the attestation backlog depth. Each validated settle durably enqueues a
    /// pending receipt; this drains once the (operator-gated) on-chain relayer
    /// submits them. Until then it tracks `jobs_completed`.
    pending_attestations: usize,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("coordinator=info,tower_http=info")
        .init();
    let args = Args::parse();
    let store = Store::open(&args.db)?;
    // Seeds a fresh DB only; a restart reloads existing jobs from the file.
    // `with_store` also reclaims jobs left in_flight by a previous crash.
    let state = AppState::with_store(store, args.max_attempts, args.earner_ttl_secs as i64)?;
    tracing::info!(db = %args.db, "store ready");

    // Background reaper: periodically requeue in-flight jobs past their
    // deadline so a stalled/vanished earner doesn't strand a job forever. The
    // router-based tests don't spawn this; it only runs in the real binary.
    spawn_reaper(state.clone(), args.reap_interval_secs);

    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    tracing::info!(bind = %args.bind, "coordinator up");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve when the process receives a shutdown signal — `SIGINT` (Ctrl-C)
/// on any platform, or `SIGTERM` on unix (Render sends SIGTERM on stop).
/// Awaited by axum's graceful shutdown so in-flight requests and ws sessions
/// drain before exit.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("install Ctrl-C handler");
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

/// Spawn the deadline reaper: every `interval_secs`, requeue any in-flight job
/// whose deadline has elapsed (or dead-letter it when it has exhausted all
/// attempts). Logs requeued and dead-lettered counts separately; store errors
/// are logged and the loop continues.
fn spawn_reaper(state: Arc<AppState>, interval_secs: u64) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            let max_attempts = state.max_attempts;
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
            } // store lock dropped here, before we touch earners

            let removed = {
                let mut earners = state.earners.lock().await;
                prune_stale_earners(&mut earners, now_secs(), state.earner_ttl_secs)
            };
            if removed > 0 {
                tracing::info!(removed, "pruned stale earners");
            }
        }
    });
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
            region: RegionCoord { x: 42 + i as i32, y: -17, layer: 0 },
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
        region: RegionCoord { x: 42, y: -17, layer: 0 },
        deadline_secs: 60,
        max_payout_wei: "1000000000000000000".into(),
        inputs: serde_json::json!({"heightfield_seed": 0xb1acf1e1du64}),
    }
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/register", post(register))
        .route("/stats", get(stats))
        .route("/earners", get(earners))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(job_detail))
        .route("/jobs/next", get(next_job))
        .route("/jobs/{id}/submit", post(submit))
        .route("/jobs/{id}/status", get(job_status))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// Earner → coordinator registration. Accepts an `EarnerMsg::Hello` and
/// upserts the earner keyed by address. Other `EarnerMsg` variants are
/// rejected here (job dispatch lives on its own routes for now).
async fn register(
    State(state): State<Arc<AppState>>,
    Json(msg): Json<EarnerMsg>,
) -> Result<&'static str, StatusCode> {
    let EarnerMsg::Hello {
        earner_address,
        gpu_model,
        vram_gb,
        supported,
    } = msg
    else {
        return Err(StatusCode::BAD_REQUEST);
    };

    tracing::info!(address = %earner_address, gpu = %gpu_model, vram_gb, "earner registered");
    state.earners.lock().await.insert(
        earner_address,
        EarnerInfo { gpu_model, vram_gb, supported, last_seen: now_secs() },
    );
    Ok("registered")
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
    let (jobs_queued, jobs_in_flight, jobs_completed, jobs_failed) =
        match (
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
    let jobs_redispatched = match store.redispatched_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, "stats: redispatched_count failed");
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
        jobs_redispatched,
        pending_attestations,
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

async fn next_job(State(state): State<Arc<AppState>>) -> Response {
    let store = state.store.lock().await;
    match store.take_next(|_| true) {
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
    Json(result): Json<JobResult>,
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
            tracing::warn!(?id, "rejected: missing/invalid {DISPATCH_SEQ_HEADER} header");
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
    tracing::info!(?id, earner = %result.earner_address, "result received");
    // Content gate already cleared above; TODO EAS attestation relay on settle.
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
///   1. earner → `EarnerMsg::Hello` (required first message; registers like
///      `/register`). Any other first message closes the socket.
///   2. coordinator polls the queue; when a job whose `kind` the earner
///      advertised in `supported` is available, it pops it (stamping a fresh
///      `dispatch_seq`, which the session remembers) and sends
///      `CoordinatorMsg::JobOffer(job)`. Only one job is offered at a time.
///   3. earner → `EarnerMsg::Accept { job_id }` marks the offer in-flight; an
///      `Accept` for a different/unknown job is ignored.
///   4. earner → `EarnerMsg::Submit(result)`: the signature + job_id are
///      verified and the job is settled only while it still holds the
///      `dispatch_seq` we remembered (the fence). Valid + current dispatch →
///      push to `completed` and reply `CoordinatorMsg::Accepted { job_id,
///      attestation_uid }`. Bad signature / wrong job → `Rejected` and the job
///      is requeued for another earner. Stale offer (the job was reaped and
///      reassigned out from under us, so its seq has advanced) → `Rejected`, and
///      the job is left untouched (not requeued, not settled). See
///      [`SubmitOutcome`].
///   5. earner → `EarnerMsg::Heartbeat { job_id, progress_pct }`: when
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
) -> Response {
    ws.on_upgrade(move |socket| ws_session(socket, state))
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

async fn ws_session(mut socket: WebSocket, state: Arc<AppState>) {
    // 1. First message MUST be a Hello.
    let earner_address = match recv_hello(&mut socket, &state).await {
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
    // Poll the queue on this cadence when we have nothing offered.
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut last_liveness_bump = now_secs();

    loop {
        // If we have no outstanding offer, try to grab a supported job.
        if offered.is_none() {
            if let Some((job, seq)) = take_supported_job(&state, &supported).await {
                if !send_msg(&mut socket, &CoordinatorMsg::JobOffer(job.clone())).await {
                    requeue(&state, job, seq).await;
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
            incoming = socket.recv() => {
                let Some(frame) = incoming else { break }; // socket closed
                let frame = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(earner = %earner_address, ?e, "ws recv error");
                        break;
                    }
                };
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
                            SubmitOutcome::Requeue(_) => {
                                if let Some((job, seq)) = offered.take() {
                                    requeue(&state, job, seq).await;
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
                        // currently offered job, bump `started_at` so the
                        // reaper's deadline window resets from "last sign of
                        // life" rather than from dispatch. A job that keeps
                        // heartbeating is making progress and won't be reaped;
                        // a silent earner still hits the original window.
                        match (job_id, &offered) {
                            (Some(jid), Some((job, seq))) if jid == job.id => {
                                let store = state.store.lock().await;
                                // Fence: only the holder of the current dispatch
                                // may slide the deadline. A stale heartbeat for a
                                // job since reaped+reassigned (newer seq) must not
                                // keep the new holder's lease alive.
                                let holds_current = matches!(
                                    store.current_dispatch_seq(&jid),
                                    Ok(Some(cur)) if cur == *seq
                                );
                                if !holds_current {
                                    tracing::debug!(
                                        earner = %earner_address,
                                        %jid,
                                        progress_pct,
                                        "heartbeat for a reassigned/stale dispatch ignored",
                                    );
                                    continue;
                                }
                                match store.touch(&jid, now_secs()) {
                                    Ok(true) => tracing::debug!(
                                        earner = %earner_address,
                                        %jid,
                                        progress_pct,
                                        "heartbeat: liveness bumped",
                                    ),
                                    Ok(false) => tracing::debug!(
                                        earner = %earner_address,
                                        %jid,
                                        progress_pct,
                                        "heartbeat for non-in-flight job ignored",
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
    // our dispatch seq, so a job already reaped+reassigned isn't yanked back).
    if let Some((job, seq)) = offered.take() {
        requeue(&state, job, seq).await;
    }
    tracing::info!(earner = %earner_address, "ws session ended");
}

/// Block on the first frame, requiring an `EarnerMsg::Hello`. Registers the
/// earner (shared with `/register`) and returns its address, or `None` if the
/// socket closed / sent something other than a Hello.
async fn recv_hello(socket: &mut WebSocket, state: &Arc<AppState>) -> Option<String> {
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
        } = msg
        else {
            tracing::warn!("ws: first message was not Hello; closing");
            return None;
        };
        tracing::info!(address = %earner_address, gpu = %gpu_model, vram_gb, "earner registered (ws)");
        state.earners.lock().await.insert(
            earner_address.clone(),
            EarnerInfo { gpu_model, vram_gb, supported, last_seen: now_secs() },
        );
        return Some(earner_address);
    }
}

/// Take the most recent queued job whose kind the earner supports, marking it
/// in-flight. Returns the job with the `dispatch_seq` stamped on this hand-out,
/// which the session remembers to fence later settle/requeue/heartbeat actions.
/// Leaves unsupported jobs queued for other earners.
async fn take_supported_job(state: &Arc<AppState>, supported: &[JobKind]) -> Option<(JobSpec, i64)> {
    let store = state.store.lock().await;
    match store.take_next(|job| supported.contains(&job.kind)) {
        Ok(job) => job,
        Err(e) => {
            tracing::error!(?e, "take_supported_job: take_next failed");
            None
        }
    }
}

/// Put a job back on the queue (rejected submission or dropped connection), or
/// dead-letter it if it has exhausted `state.max_attempts` dispatches — but only
/// while we still hold the dispatch identified by `seq`.
///
/// The fence read and the requeue run under the same store lock, so they are
/// atomic against any other dispatch. If the job was reaped and reassigned to a
/// new earner since we took it, its `dispatch_seq` has advanced past `seq` and we
/// skip: requeueing would preempt the new holder mid-render. `store.requeue` then
/// applies its own `in_flight` guard (a reaper-parked `queued` job is a no-op).
async fn requeue(state: &Arc<AppState>, job: JobSpec, seq: i64) {
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
    match store.requeue(&job, state.max_attempts) {
        Ok(true) => {
            tracing::warn!(job_id = %job.id, "job dead-lettered (max attempts exhausted)");
        }
        Ok(false) => {}
        Err(e) => tracing::error!(job_id = %job.id, ?e, "requeue failed"),
    }
}

/// Disposition of a ws `Submit`: the verdict to send the earner, plus what
/// `ws_session` should do with the outstanding offer.
#[derive(Debug)]
enum SubmitOutcome {
    /// Verified + settled. Clear the offer; do not requeue.
    Accepted(CoordinatorMsg),
    /// Rejected, and the job is still this earner's in-flight work — put it back
    /// on the queue so another earner can try.
    Requeue(CoordinatorMsg),
    /// Rejected, and the offer is stale: the job is no longer in_flight under us
    /// (reaped, reassigned, or already terminal). Drop our in-memory offer but
    /// leave the job's store state untouched — requeueing would clobber a job
    /// another earner may now hold or resurrect a dead-lettered one.
    Drop(CoordinatorMsg),
}

impl SubmitOutcome {
    fn reply(&self) -> &CoordinatorMsg {
        match self {
            SubmitOutcome::Accepted(m) | SubmitOutcome::Requeue(m) | SubmitOutcome::Drop(m) => m,
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
    result: JobResult,
) -> SubmitOutcome {
    let job_id = result.job_id;
    let rejected = |reason: &str| CoordinatorMsg::Rejected {
        job_id,
        reason: reason.to_string(),
    };

    let expected_seq = match offered {
        Some((job, seq)) if job.id == job_id => *seq,
        // The offered job is still validly ours; don't settle it on a mismatched
        // submit, but free it for another earner rather than stranding it.
        Some(_) => {
            return SubmitOutcome::Requeue(rejected("submit job_id does not match the offered job"))
        }
        None => return SubmitOutcome::Drop(rejected("no job was offered on this connection")),
    };
    if !accepted {
        return SubmitOutcome::Requeue(rejected("submit before accept"));
    }

    if let Err(e) = verify::verify_signature(
        &result.job_id,
        &result.output_hash,
        &result.earner_address,
        &result.signature_hex,
    ) {
        tracing::warn!(%job_id, earner = %result.earner_address, ?e, "ws rejected: bad attestation");
        return SubmitOutcome::Requeue(rejected("attestation signature verification failed"));
    }

    // Content gate: reject a result that is not well-formed enough to meter or
    // attest before touching the store. Requeue (not Drop) so the job — which may
    // be perfectly renderable — gets another earner; the requeue helper is itself
    // dispatch_seq-fenced, so a content-reject for a since-reassigned lease can't
    // clobber the new holder. Mirrors the bad-attestation Requeue above.
    if let Err(e) = validate::validate_result(&result) {
        tracing::warn!(%job_id, earner = %result.earner_address, reason = e.reason(), "ws rejected: result failed content gate");
        return SubmitOutcome::Requeue(rejected(e.reason()));
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
                tracing::error!(%job_id, ?e, "ws: dispatch_seq read failed");
                return SubmitOutcome::Requeue(rejected("failed to read dispatch state"));
            }
        }
        match store.record_completed(&result) {
            Ok(settled) => settled,
            Err(e) => {
                tracing::error!(%job_id, ?e, "ws: failed to persist result");
                return SubmitOutcome::Requeue(rejected("failed to persist result"));
            }
        }
    };
    if !settled {
        tracing::warn!(%job_id, earner = %result.earner_address, "ws rejected: job no longer in_flight");
        return SubmitOutcome::Drop(rejected("job is no longer in flight"));
    }

    tracing::info!(%job_id, earner = %result.earner_address, "ws result accepted");
    // Content gate already cleared above; TODO real EAS attestation relay on
    // settle (placeholder uid until mesh-eas-attestation-relay lands).
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

    /// In-memory store-backed state (no disk). `with_store` seeds one job per
    /// `JobKind` because the in-memory DB starts empty; tests that need an empty
    /// queue drain it first via `/jobs/next` or use `test_state_empty`.
    fn test_state() -> Arc<AppState> {
        AppState::with_store(Store::open_in_memory().unwrap(), 5, 60).unwrap()
    }

    /// In-memory state with every auto-seeded job removed, so the queue starts
    /// empty (matches the old `AppState::default()` behavior used by tests
    /// that assert `jobs_queued == 0`).
    async fn test_state_empty() -> Arc<AppState> {
        let state = test_state();
        // Drain every seeded job so the queue is empty regardless of how many
        // kinds `seed_jobs` enqueues.
        let store = state.store.lock().await;
        while store.take_next(|_| true).unwrap().is_some() {}
        drop(store);
        state
    }

    /// Enqueue a job directly via the store (test helper replacing the old
    /// `state.queue.lock().await.push(job)`).
    async fn enqueue(state: &Arc<AppState>, job: &JobSpec) {
        state.store.lock().await.enqueue(job).unwrap();
    }

    fn hello(address: &str, vram_gb: u32, supported: Vec<JobKind>) -> EarnerMsg {
        EarnerMsg::Hello {
            earner_address: address.into(),
            gpu_model: "RTX 4090".into(),
            vram_gb,
            supported,
        }
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_json(state: Arc<AppState>, uri: &str, value: &serde_json::Value) -> axum::response::Response {
        router(state)
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
        let msg = hello("0xabc", 24, vec![JobKind::Terrain, JobKind::Foliage]);
        let resp = post_json(state.clone(), "/register", &serde_json::to_value(&msg).unwrap()).await;
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
        // same address twice → upsert (count stays 1, vram updated)
        let m1 = hello("0xabc", 24, vec![JobKind::Terrain]);
        let m2 = hello("0xabc", 48, vec![JobKind::Terrain, JobKind::DiffusionTile]);
        let m3 = hello("0xdef", 16, vec![JobKind::NpcTick]);
        for m in [&m1, &m2, &m3] {
            let resp = post_json(state.clone(), "/register", &serde_json::to_value(m).unwrap()).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 2); // 0xabc upserted, 0xdef new
        assert_eq!(json["total_vram_gb"], 48 + 16);
        assert_eq!(json["supported_breakdown"]["terrain"], 1);
        assert_eq!(json["supported_breakdown"]["diffusion_tile"], 1);
        assert_eq!(json["supported_breakdown"]["npc_tick"], 1);
    }

    #[tokio::test]
    async fn register_rejects_non_hello() {
        let state = test_state();
        let msg = EarnerMsg::Accept { job_id: Uuid::new_v4() };
        let resp = post_json(state.clone(), "/register", &serde_json::to_value(&msg).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
        let bytes =
            hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318")
                .unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    fn dev_address() -> String {
        let sk = dev_signing_key();
        let vk = sk.verifying_key();
        let point = vk.to_encoded_point(false);
        let hash = Keccak256::digest(&point.as_bytes()[1..]);
        format!("0x{}", hex::encode(&hash[12..]))
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
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&good).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // mismatched path id vs body job_id → rejected
        let other = Uuid::new_v4();
        let uri = format!("/jobs/{}/submit", other);
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&good).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // completed count reflects the one accepted submit
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 1);
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

        let stored = store.pending_attestation(&job_id).unwrap().expect("pending row exists");
        // Round-trips through the canonical builder...
        assert_eq!(stored, eas::PendingAttestation::build(&job, &result).unwrap());
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
        assert!(!store.record_completed(&result).unwrap(), "second settle refused (done)");
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
        assert!(store.record_completed(&signed_result_raw_hash(job_id, "deadbeef")).unwrap());
        assert_eq!(store.pending_attestation_count().unwrap(), 0);
    }

    /// `/stats` exposes the attestation backlog: with no on-chain relayer yet,
    /// every settled job stays pending, so it tracks `jobs_completed`.
    #[tokio::test]
    async fn stats_reports_pending_attestation_backlog() {
        let state = test_state_empty().await;
        let jobs = [seed_job(), seed_job()];
        for job in &jobs {
            enqueue(&state, job).await;
            state.store.lock().await.take_next(|j| j.id == job.id).unwrap();
            state.store.lock().await.record_completed(&signed_result(job.id, "r")).unwrap();
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 2);
        assert_eq!(json["pending_attestations"], 2);
    }

    // ---- websocket dispatch integration tests ----

    use futures_util::{SinkExt, StreamExt};
    use proto::CoordinatorMsg;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// Bind the router on an ephemeral port and serve it on a spawned task.
    /// Returns the bound `host:port` so tests can build ws/http URLs.
    async fn serve_ephemeral(state: Arc<AppState>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr.to_string()
    }

    fn ws_hello() -> EarnerMsg {
        EarnerMsg::Hello {
            earner_address: dev_address(),
            gpu_model: "RTX 4090".into(),
            vram_gb: 24,
            supported: vec![
                JobKind::Terrain,
                JobKind::Foliage,
                JobKind::NpcTick,
                JobKind::DiffusionTile,
                JobKind::Optimization,
            ],
        }
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
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello()).unwrap()))
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
            CoordinatorMsg::Accepted { job_id: jid, attestation_uid } => {
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

        ws.send(WsMessage::text(serde_json::to_string(&ws_hello()).unwrap()))
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
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello()).unwrap()))
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
            assert_eq!(store.job_status(&job_id).unwrap().as_deref(), Some("failed"));
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
            state.store.lock().await.job_status(&job_id).unwrap().as_deref(),
            Some("failed"),
            "stale submit must not resurrect a dead-lettered job"
        );
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 0, "stale submit must not be credited");
        assert_eq!(json["jobs_failed"], 1);
    }

    /// A `JobResult` validly signed by an arbitrary key (distinct from the dev
    /// key `signed_result` uses), credited to that key's derived address — lets a
    /// test assert credit lands on a SPECIFIC earner, not just "someone".
    fn signed_result_by(key_hex: &str, job_id: Uuid, label: &str) -> JobResult {
        let sk = SigningKey::from_slice(&hex::decode(key_hex).unwrap()).unwrap();
        let point = sk.verifying_key().to_encoded_point(false);
        let address = format!("0x{}", hex::encode(&Keccak256::digest(&point.as_bytes()[1..])[12..]));
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
        let (job_a, seq_a) = state.store.lock().await.take_next(|_| true).unwrap().unwrap();
        assert_eq!((job_a.id, seq_a), (job_id, 1));

        // The deadline lapses and A's job returns to the queue (whether via the
        // reaper or A's own disconnect — both are in_flight→queued, seq preserved).
        {
            let store = state.store.lock().await;
            assert!(!store.requeue(&job_a, state.max_attempts).unwrap(), "requeued, not dead-lettered");
            assert_eq!(store.job_status(&job_id).unwrap().as_deref(), Some("queued"));
        }
        // B takes the now-queued job: seq 2.
        let (job_b, seq_b) = state.store.lock().await.take_next(|_| true).unwrap().unwrap();
        assert_eq!((job_b.id, seq_b), (job_id, 2), "B's dispatch carries a fresh, higher seq");

        // FM2: A (seq 1) submits a valid result, but the lease is B's (seq 2).
        // Must Drop, settle nothing, credit no one.
        let result_a = signed_result(job_id, "aaaa"); // dev key == earner A
        match handle_submit(&state, &Some((job_a.clone(), seq_a)), true, result_a).await {
            SubmitOutcome::Drop(CoordinatorMsg::Rejected { job_id: jid, .. }) => assert_eq!(jid, job_id),
            other => panic!("stale holder's submit must Drop, got {other:?}"),
        }
        {
            let store = state.store.lock().await;
            assert_eq!(store.job_status(&job_id).unwrap().as_deref(), Some("in_flight"));
            assert_eq!(store.completed_count().unwrap(), 0, "stale submit credited nothing");
            assert_eq!(store.current_dispatch_seq(&job_id).unwrap(), Some(2));
        }

        // FM1: A's socket drops → requeue with A's stale seq. Must not preempt B.
        requeue(&state, job_a, seq_a).await;
        assert_eq!(
            state.store.lock().await.job_status(&job_id).unwrap().as_deref(),
            Some("in_flight"),
            "stale holder's disconnect must not requeue B's in-flight job"
        );

        // B (seq 2) holds the current dispatch: it settles and is credited.
        let result_b = signed_result_by(KEY_B, job_id, "bbbb");
        let b_addr = result_b.earner_address.clone();
        assert_ne!(b_addr, dev_address(), "B must be a different earner than A");
        match handle_submit(&state, &Some((job_b, seq_b)), true, result_b).await {
            SubmitOutcome::Accepted(CoordinatorMsg::Accepted { job_id: jid, .. }) => assert_eq!(jid, job_id),
            other => panic!("current holder's submit must be Accepted, got {other:?}"),
        }
        let store = state.store.lock().await;
        assert_eq!(store.job_status(&job_id).unwrap().as_deref(), Some("done"));
        let credited = store.completed_count_by_earner().unwrap();
        assert_eq!(credited.get(&b_addr), Some(&1), "credit lands on B, the current holder");
        assert_eq!(credited.len(), 1, "and on no one else (A was never credited)");
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

        let (offered, seq) = state.store.lock().await.take_next(|_| true).unwrap().unwrap();
        assert_eq!((offered.id, seq), (job_id, 1));

        let bad = signed_result_raw_hash(job_id, "deadbeef"); // valid sig, malformed hash
        match handle_submit(&state, &Some((offered.clone(), seq)), true, bad).await {
            SubmitOutcome::Requeue(CoordinatorMsg::Rejected { job_id: jid, reason }) => {
                assert_eq!(jid, job_id);
                assert_eq!(reason, validate::ValidationError::MalformedOutputHash.reason());
            }
            other => panic!("expected Requeue with the content-gate reason, got {other:?}"),
        }

        // Nothing settled: still in_flight under our (only) dispatch.
        assert_eq!(
            state.store.lock().await.job_status(&job_id).unwrap().as_deref(),
            Some("in_flight")
        );

        // The caller's seq-fenced requeue then returns the job to the queue.
        requeue(&state, offered, seq).await;
        assert_eq!(state.store.lock().await.job_status(&job_id).unwrap().as_deref(), Some("queued"));
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
            resp.headers().get("x-dispatch-seq").and_then(|v| v.to_str().ok()),
            Some("1"),
            "next stamps the dispatch seq"
        );

        let good = signed_result(job_id, "deadbeef");
        let uri = format!("/jobs/{job_id}/submit");

        // No fence header → can't be tied to a dispatch → 400.
        let resp = post_json(state.clone(), &uri, &serde_json::to_value(&good).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "submit without a fence header is refused");

        // Reassign: requeue and re-dispatch, so the current seq is now 2.
        {
            let store = state.store.lock().await;
            assert!(!store.requeue(&job, state.max_attempts).unwrap(), "requeued, not dead-lettered");
        }
        state.store.lock().await.take_next(|_| true).unwrap(); // seq -> 2

        // Echoing the stale seq 1 → 409 (lease reassigned); nothing credited.
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&good).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "stale dispatch seq is refused");
        assert_eq!(state.store.lock().await.completed_count().unwrap(), 0);

        // Echoing the current seq 2 → accepted + settled.
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&good).unwrap(), 2).await;
        assert_eq!(resp.status(), StatusCode::OK, "current dispatch seq settles");
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
            let state = AppState::with_store(Store::open(&db_path).unwrap(), 5, 60).unwrap();

            // Enqueue a second job and submit a validly-signed result for it.
            let job = seed_job();
            let job_id = job.id;
            enqueue(&state, &job).await;
            // Move it in_flight first (submit gate requires it). take_next pops
            // the most-recently inserted queued job — the one we just enqueued —
            // leaving the auto-seeded job queued.
            let taken = state.store.lock().await.take_next(|_| true).unwrap();
            assert_eq!(taken.unwrap().0.id, job_id);

            let result = signed_result(job_id, "deadbeef");
            let uri = format!("/jobs/{}/submit", job_id);
            let resp =
                post_submit(state.clone(), &uri, &serde_json::to_value(&result).unwrap(), 1).await;
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
        let state = AppState::with_store(Store::open(&db_path).unwrap(), 5, 60).unwrap();
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
            let state = AppState::with_store(Store::open(&db_path).unwrap(), 5, 60).unwrap();
            let store = state.store.lock().await;
            let mut ids = Vec::new();
            while let Some((job, _)) = store.take_next(|_| true).unwrap() {
                ids.push(job.id);
            }
            seeded = ids.len();
            assert_eq!(seeded, JobKind::ALL.len(), "fresh DB seeds one job per kind");
            taken_id = ids[0];
            assert_eq!(store.in_flight_count().unwrap(), seeded);
            assert_eq!(store.queued_count().unwrap(), 0);
        } // state dropped → "crash" with jobs stuck in_flight.

        // Second "process": reopen the SAME db. with_store must reclaim the
        // orphaned in_flight jobs on startup.
        let state = AppState::with_store(Store::open(&db_path).unwrap(), 5, 60).unwrap();
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
        store.record_completed(&signed_result(done.id, "d")).unwrap();
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
        assert_eq!(store.job_status(&queued.id).unwrap().as_deref(), Some("queued"));
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
        assert_eq!(store.completed_count().unwrap(), 1, "the recorded result is untouched");

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
            store.record_completed(&signed_result(done.id, "d")).unwrap();

            let in_flight = job_with_deadline(60);
            in_flight_id = in_flight.id;
            store.enqueue(&in_flight).unwrap();
            store.take_next(|j| j.id == in_flight.id).unwrap();
            assert_eq!(store.in_flight_count().unwrap(), 1);
        } // drop → "crash"

        // "Process 2": restart through with_store, which recovers on boot and must
        // NOT re-seed (jobs already exist).
        let state = AppState::with_store(Store::open(&db_path).unwrap(), 5, 60).unwrap();
        let store = state.store.lock().await;

        // The done job survived and is still credited; it is not requeued.
        assert_eq!(store.job_status(&done_id).unwrap().as_deref(), Some("done"));
        assert_eq!(store.completed_count().unwrap(), 1, "pre-crash result must survive");

        // The in_flight job was reclaimed to queued and redispatches exactly once.
        assert_eq!(store.job_status(&in_flight_id).unwrap().as_deref(), Some("queued"));
        let taken = store.take_next(|_| true).unwrap();
        assert_eq!(taken.map(|(j, _)| j.id), Some(in_flight_id), "the reclaimed job redispatches");
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
        assert_eq!(store.job_status(&job.id).unwrap().as_deref(), Some("queued"));
        // dispatch_seq was also absent in the old schema; the migration defaults
        // it to 0 (FM4), and the first dispatch bumps it to 1.
        assert_eq!(store.current_dispatch_seq(&job.id).unwrap(), Some(0), "migrated dispatch_seq defaults to 0");
        // attempts defaulted to 0 on migration → first dispatch makes it 1.
        let (_, seq) = store.take_next(|_| true).unwrap().unwrap();
        assert_eq!(seq, 1, "first dispatch after migration stamps seq 1");
        assert_eq!(store.current_dispatch_seq(&job.id).unwrap(), Some(1));
        assert_eq!(store.redispatched_count().unwrap(), 0, "migrated attempts default to 0");
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
        assert_eq!(store.current_dispatch_seq(&job.id).unwrap(), Some(0), "queued job starts at 0");

        let (_, seq1) = store.take_next(|_| true).unwrap().unwrap();
        assert_eq!(seq1, 1);

        // Reap past the deadline → back to queued; the seq is preserved, not reset.
        // take_next stamps started_at to the wall clock, so reap relative to it.
        let out = store.reap_expired(now_secs() + 10_000, 5).unwrap();
        assert_eq!(out.requeued, vec![job.id]);
        assert_eq!(store.current_dispatch_seq(&job.id).unwrap(), Some(1), "reap preserves the seq");

        // Re-dispatch bumps strictly higher.
        let (_, seq2) = store.take_next(|_| true).unwrap().unwrap();
        assert_eq!(seq2, 2, "the reassigned dispatch carries a higher seq");
        assert_eq!(store.current_dispatch_seq(&job.id).unwrap(), Some(2));

        assert_eq!(store.current_dispatch_seq(&Uuid::new_v4()).unwrap(), None, "unknown job → None");
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
        assert_eq!(reaped.requeued, vec![a_id], "only the past-deadline job is reaped");
        assert!(reaped.failed.is_empty(), "no jobs should be dead-lettered yet");
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
        assert_eq!(
            store.job_status(&id).unwrap().as_deref(),
            Some("failed")
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

        assert!(store.touch(&id, 5000).unwrap(), "touch must return true for an in_flight job");

        // 99 seconds after t=5000 → not yet expired.
        let outcome = store.reap_expired(5099, BIG_MAX).unwrap();
        assert!(outcome.requeued.is_empty(), "must not reap before deadline");
        assert!(outcome.failed.is_empty(), "must not dead-letter before deadline");

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
        store.touch(&id_a, 5000).unwrap();
        store.touch(&id_b, 4000).unwrap();

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
            earner_ttl_secs: 60,
        });
        // No in-flight jobs → null (stable key, absent value).
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert!(json["oldest_in_flight_secs"].is_null(), "no in-flight jobs → null");

        // Put one job in flight; the field becomes the age of its dispatch. Just
        // taken, so it is ~0 and certainly a small NUMBER (proving None → Some).
        let job = job_with_deadline(60);
        {
            let store = state.store.lock().await;
            store.enqueue(&job).unwrap();
            store.take_next(|_| true).unwrap();
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        let age = json["oldest_in_flight_secs"].as_u64().expect("in-flight → numeric age");
        assert!(age < 5, "freshly dispatched job age should be ~0, got {age}");
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
        store.touch(&id, 1000).unwrap();
        assert_eq!(store.redispatched_count().unwrap(), 0);

        // Reaper requeues it past the deadline (attempts unchanged), then it is
        // dispatched a second time (attempts → 2): now counted as redispatched.
        let outcome = store.reap_expired(1100, BIG_MAX).unwrap();
        assert_eq!(outcome.requeued, vec![id]);
        store.take_next(|_| true).unwrap();
        assert_eq!(store.redispatched_count().unwrap(), 1);
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

        store.touch(&id, 5000).unwrap();
        let outcome = store.reap_expired(5090, BIG).unwrap();
        assert!(outcome.requeued.is_empty(), "90s after first beat: should not reap");
        assert!(outcome.failed.is_empty());

        // Second heartbeat at t=5090.
        store.touch(&id, 5090).unwrap();
        let outcome = store.reap_expired(5180, BIG).unwrap();
        assert!(
            outcome.requeued.is_empty(),
            "90s after second beat (180s total): should still not reap"
        );
        assert!(outcome.failed.is_empty());

        // 100s of silence since the last beat (t=5090+100=5190) → reap.
        let outcome = store.reap_expired(5190, BIG).unwrap();
        assert_eq!(outcome.requeued, vec![id], "100s after last beat: must reap");
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
            !store.touch(&id, 5000).unwrap(),
            "touch must return false for a queued (not in_flight) job"
        );

        // A completed job is likewise not in_flight → touch is a no-op. Uses a
        // fresh store because `record_completed` needs `&mut Store`.
        let mut store = Store::open_in_memory().unwrap();
        store.enqueue(&job).unwrap();
        store.take_next(|_| true).unwrap();
        store.record_completed(&signed_result(id, "cafebabe")).unwrap();
        assert!(
            !store.touch(&id, 5000).unwrap(),
            "touch must return false for a done job"
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
        ws.send(WsMessage::text(serde_json::to_string(&ws_hello()).unwrap()))
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
        let beat = EarnerMsg::Heartbeat { job_id: Some(job_id), progress_pct: 50 };
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
            CoordinatorMsg::Accepted { job_id: jid, attestation_uid } => {
                assert_eq!(jid, job_id);
                assert_eq!(attestation_uid, PLACEHOLDER_ATTESTATION_UID);
            }
            other => panic!("expected Accepted, got {other:?}"),
        }

        // /stats reflects the completed job.
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 1, "completed count must be 1 after ws heartbeat test");
    }

    // ---- idempotent + gated submit ----

    #[tokio::test]
    async fn submit_rejected_for_unknown_job() {
        let state = test_state_empty().await;
        // Validly-signed result for a job the store has never seen.
        let job_id = Uuid::new_v4();
        let good = signed_result(job_id, "deadbeef");
        let uri = format!("/jobs/{}/submit", job_id);
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&good).unwrap(), 1).await;
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
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&good).unwrap(), 1).await;
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
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&good).unwrap(), 1).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Second submit: job is now done, so the gate rejects with CONFLICT and
        // nothing is double-counted.
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&good).unwrap(), 1).await;
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
            !store.record_completed(&signed_result(ghost.id, "x")).unwrap(),
            "a result for an unknown job must be refused"
        );
        assert_eq!(store.completed_count().unwrap(), 0);

        // Queued (enqueued, never taken) → refused; stays queued.
        let queued = job_with_deadline(60);
        store.enqueue(&queued).unwrap();
        assert!(
            !store.record_completed(&signed_result(queued.id, "x")).unwrap(),
            "a result for a queued job must be refused"
        );
        assert_eq!(store.job_status(&queued.id).unwrap().as_deref(), Some("queued"));
        assert_eq!(store.completed_count().unwrap(), 0);

        // In_flight → settles once; a replayed result is then refused and neither
        // double-counts nor changes the now-`done` job.
        let live = job_with_deadline(60);
        store.enqueue(&live).unwrap();
        store.take_next(|j| j.id == live.id).unwrap();
        assert!(
            store.record_completed(&signed_result(live.id, "x")).unwrap(),
            "first settle of an in_flight job succeeds"
        );
        assert_eq!(store.job_status(&live.id).unwrap().as_deref(), Some("done"));
        assert!(
            !store.record_completed(&signed_result(live.id, "x")).unwrap(),
            "a replayed result for an already-done job must be refused"
        );
        assert_eq!(store.completed_count().unwrap(), 1);

        // Dead-lettered (failed) → refused and must NOT be resurrected to done.
        let failed = job_with_deadline(60);
        store.enqueue(&failed).unwrap();
        store.take_next(|j| j.id == failed.id).unwrap();
        assert!(store.requeue(&failed, 1).unwrap(), "attempts>=max dead-letters");
        assert_eq!(store.job_status(&failed.id).unwrap().as_deref(), Some("failed"));
        assert!(
            !store.record_completed(&signed_result(failed.id, "x")).unwrap(),
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
        assert_eq!(store.job_status(&queued.id).unwrap().as_deref(), Some("queued"));

        // Done → no-op; must NOT be resurrected to queued.
        let mut store = Store::open_in_memory().unwrap();
        let done = job_with_deadline(60);
        store.enqueue(&done).unwrap();
        store.take_next(|j| j.id == done.id).unwrap();
        store.record_completed(&signed_result(done.id, "x")).unwrap();
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
        assert!(!store.requeue(&live, 5).unwrap(), "requeued (not dead-lettered) → false");
        assert_eq!(store.job_status(&live.id).unwrap().as_deref(), Some("queued"));
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
            earner_ttl_secs: 60,
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
        let resp = post_submit(state.clone(), &uri, &serde_json::to_value(&good).unwrap(), 1).await;
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
        map.insert("fresh".into(), EarnerInfo {
            gpu_model: "a".into(), vram_gb: 24, supported: vec![JobKind::Terrain], last_seen: 1000,
        });
        map.insert("stale".into(), EarnerInfo {
            gpu_model: "b".into(), vram_gb: 16, supported: vec![JobKind::NpcTick], last_seen: 900,
        });
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
        let msg = hello("0xabc", 24, vec![JobKind::Terrain]);
        let resp = post_json(state.clone(), "/register", &serde_json::to_value(&msg).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 1);
        assert_eq!(json["total_vram_gb"], 24);

        // Force last_seen far into the past → stale (default ttl in test_state is 60).
        {
            let mut earners = state.earners.lock().await;
            earners.get_mut("0xabc").unwrap().last_seen = 0;
        }
        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["gpus_joined"], 0, "stale earner must drop out of gpus_joined");
        assert_eq!(json["total_vram_gb"], 0, "stale earner's vram must not be counted");
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
        let resp = post_submit(state.clone(), &format!("/jobs/{job_id}/submit"),
            &serde_json::to_value(&good).unwrap(), 1).await;
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
            earner_ttl_secs: 60,
        });

        // A queued job: detail returns its spec, no result yet.
        let queued = job_with_deadline(60);
        enqueue(&state, &queued).await;
        let resp = get(state.clone(), &format!("/jobs/{}", queued.id)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["spec"]["id"], queued.id.to_string());
        assert_eq!(json["spec"]["kind"], "terrain");
        assert!(json["result"].is_null(), "a queued job has no recorded result");

        // A completed job: detail returns the spec plus the recorded result.
        let done = job_with_deadline(60);
        let done_id = done.id;
        enqueue(&state, &done).await;
        {
            let mut store = state.store.lock().await;
            store.take_next(|j| j.id == done_id).unwrap();
            store.record_completed(&signed_result(done_id, "deadbeef")).unwrap();
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
            earner_ttl_secs: 60,
        });

        let terrain_job = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Terrain,
            region: RegionCoord { x: 1, y: 2, layer: 0 },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({"heightfield_seed": 1u64}),
        };
        let foliage_job = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Foliage,
            region: RegionCoord { x: 3, y: 4, layer: 0 },
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
            earner_ttl_secs: 60,
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
            earner_ttl_secs: 60,
        });

        let mut ids = Vec::new();
        for i in 0..5u64 {
            let job = JobSpec {
                id: Uuid::new_v4(),
                kind: JobKind::Terrain,
                region: RegionCoord { x: i as i32, y: 0, layer: 0 },
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
        store.record_completed(&signed_result(done.id, "deadbeef")).unwrap();

        // failed: enqueued, taken (attempts→1), requeued at max_attempts=1 →
        // dead-lettered to `failed`.
        let failed = job_with_deadline(60);
        store.enqueue(&failed).unwrap();
        store.take_next(|j| j.id == failed.id).unwrap();
        assert!(store.requeue(&failed, 1).unwrap(), "attempts>=max must dead-letter");

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
            earner_ttl_secs: 60,
        });

        let queued = job_with_deadline(60);
        enqueue(&state, &queued).await;
        let in_flight = job_with_deadline(60);
        enqueue(&state, &in_flight).await;
        state.store.lock().await.take_next(|j| j.id == in_flight.id).unwrap();

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
            region: RegionCoord { x: 1, y: 2, layer: 0 },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({"heightfield_seed": 1u64}),
        };
        let terrain2 = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Terrain,
            region: RegionCoord { x: 3, y: 4, layer: 0 },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({"heightfield_seed": 2u64}),
        };
        let foliage1 = JobSpec {
            id: Uuid::new_v4(),
            kind: JobKind::Foliage,
            region: RegionCoord { x: 5, y: 6, layer: 0 },
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
            earner_ttl_secs: 60,
        });
        let mk = |kind: JobKind, seed: u64| JobSpec {
            id: Uuid::new_v4(),
            kind,
            region: RegionCoord { x: seed as i32, y: 0, layer: 0 },
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
            earner_ttl_secs: 60,
        });
        let mk = |kind: JobKind, seed: u64| JobSpec {
            id: Uuid::new_v4(),
            kind,
            region: RegionCoord { x: seed as i32, y: 0, layer: 0 },
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
            store.record_completed(&signed_result(terrain1.id, "a")).unwrap();
            store.record_completed(&signed_result(foliage1.id, "b")).unwrap();
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
            earner_ttl_secs: 60,
        });
        let mk = |kind: JobKind, seed: u64| JobSpec {
            id: Uuid::new_v4(),
            kind,
            region: RegionCoord { x: seed as i32, y: 0, layer: 0 },
            deadline_secs: 60,
            max_payout_wei: "1000000000000000000".into(),
            inputs: serde_json::json!({ "seed": seed }),
        };
        // 2 Terrain + 1 Foliage: take each once (attempts→1) then requeue at
        // max_attempts=1 → dead-lettered to `failed`.
        let jobs = [mk(JobKind::Terrain, 1), mk(JobKind::Terrain, 2), mk(JobKind::Foliage, 3)];
        {
            let store = state.store.lock().await;
            for job in &jobs {
                store.enqueue(job).unwrap();
                store.take_next(|j| j.id == job.id).unwrap();
                assert!(store.requeue(job, 1).unwrap(), "attempts>=max must dead-letter");
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
            earner_ttl_secs: 60,
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
        assert_eq!(json["total_render_seconds"], 12, "sum of 5 + 7 render-seconds");
        assert_eq!(json["jobs_completed"], 2);
    }

    #[tokio::test]
    async fn stats_reports_total_payout_wei() {
        let state = Arc::new(AppState {
            store: Mutex::new(Store::open_in_memory().unwrap()),
            earners: Mutex::new(HashMap::new()),
            max_attempts: 5,
            earner_ttl_secs: 60,
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
            earner_ttl_secs: 60,
        });

        // Register two earners; then force one far into the past → stale (ttl=60).
        for m in [
            &hello("0xlive", 24, vec![JobKind::Terrain, JobKind::Foliage]),
            &hello("0xstale", 16, vec![JobKind::NpcTick]),
        ] {
            let resp = post_json(state.clone(), "/register", &serde_json::to_value(m).unwrap()).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        {
            let mut earners = state.earners.lock().await;
            earners.get_mut("0xstale").unwrap().last_seen = 0;
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
            store.record_completed(&result_for(a.id, "0xlive", 5)).unwrap();
            store.record_completed(&result_for(b.id, "0xlive", 7)).unwrap();
        }

        let resp = get(state.clone(), "/earners").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1, "only the live earner appears");
        assert_eq!(arr[0]["address"], "0xlive");
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
            earner_ttl_secs: 60,
        });
        for m in [
            &hello("0xbusy", 24, vec![JobKind::Terrain]),
            &hello("0xidle", 24, vec![JobKind::Terrain]),
        ] {
            let resp = post_json(state.clone(), "/register", &serde_json::to_value(m).unwrap()).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        // 0xbusy completes 2 jobs, 0xidle completes 1.
        let jobs = [job_with_deadline(60), job_with_deadline(60), job_with_deadline(60)];
        {
            let mut store = state.store.lock().await;
            for job in &jobs {
                store.enqueue(job).unwrap();
                store.take_next(|j| j.id == job.id).unwrap();
            }
            store.record_completed(&result_for(jobs[0].id, "0xbusy", 1)).unwrap();
            store.record_completed(&result_for(jobs[1].id, "0xbusy", 1)).unwrap();
            store.record_completed(&result_for(jobs[2].id, "0xidle", 1)).unwrap();
        }

        let json = body_json(get(state.clone(), "/earners").await).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["address"], "0xbusy", "busiest earner leads");
        assert_eq!(arr[0]["completed"], 2);
        assert_eq!(arr[1]["address"], "0xidle");
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
            earner_ttl_secs: 60,
        });
        let resp = post_json(
            state.clone(),
            "/register",
            &serde_json::to_value(hello("0xpay", 24, vec![JobKind::Terrain])).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Two DONE jobs credited to 0xpay with known payouts (1.5e18 + 2.5e18).
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
            store.record_completed(&result_for(a.id, "0xpay", 1)).unwrap();
            store.record_completed(&result_for(b.id, "0xpay", 1)).unwrap();
        }

        let json = body_json(get(state.clone(), "/earners").await).await;
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["address"], "0xpay");
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
