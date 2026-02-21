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
        Path, State,
    },
    http::StatusCode,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use proto::{CoordinatorMsg, EarnerMsg, JobKind, JobResult, JobSpec, RegionCoord};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Placeholder EAS attestation UID returned on acceptance until the real
/// RenderReceipts.sol relay lands (later task). 32 zero bytes, 0x-prefixed.
const PLACEHOLDER_ATTESTATION_UID: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000000";

mod store;
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
}

/// A registered earner's capabilities, recorded on `EarnerMsg::Hello`.
#[derive(Debug, Clone)]
struct EarnerInfo {
    /// Recorded for operator visibility / future scheduling; not yet surfaced.
    #[allow(dead_code)]
    gpu_model: String,
    vram_gb: u32,
    supported: Vec<JobKind>,
}

struct AppState {
    /// Job queue + completed results, persisted to SQLite so they survive a
    /// restart. Earner registrations stay in-memory by design.
    store: Mutex<Store>,
    earners: Mutex<HashMap<String, EarnerInfo>>,
}

impl AppState {
    /// Build state backed by `store`. Seeds one job only when the DB has no
    /// jobs yet, so a fresh DB gives earners something to do while a restart
    /// with existing jobs does NOT double-seed.
    fn with_store(store: Store) -> Result<Arc<Self>> {
        if store.jobs_empty()? {
            store.enqueue(&seed_job())?;
        }
        Ok(Arc::new(AppState {
            store: Mutex::new(store),
            earners: Mutex::new(HashMap::new()),
        }))
    }
}

/// Aggregate mesh stats. Backs the wedge requirement
/// "Mesh GPUs joined count exposed at /stats".
#[derive(Debug, Serialize)]
struct Stats {
    gpus_joined: usize,
    total_vram_gb: u64,
    jobs_queued: usize,
    jobs_completed: usize,
    /// How many registered earners advertise support for each job kind.
    supported_breakdown: HashMap<JobKind, usize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("coordinator=info,tower_http=info")
        .init();
    let args = Args::parse();
    let store = Store::open(&args.db)?;
    // Seeds a fresh DB only; a restart reloads existing jobs from the file.
    let state = AppState::with_store(store)?;
    tracing::info!(db = %args.db, "store ready");

    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    tracing::info!(bind = %args.bind, "coordinator up");
    axum::serve(listener, app).await?;
    Ok(())
}

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
        .route("/jobs/next", get(next_job))
        .route("/jobs/{id}/submit", post(submit))
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
        EarnerInfo { gpu_model, vram_gb, supported },
    );
    Ok("registered")
}

async fn stats(State(state): State<Arc<AppState>>) -> Result<Json<Stats>, StatusCode> {
    let earners = state.earners.lock().await;
    let mut total_vram_gb: u64 = 0;
    let mut supported_breakdown: HashMap<JobKind, usize> = HashMap::new();
    for info in earners.values() {
        total_vram_gb += info.vram_gb as u64;
        for kind in &info.supported {
            *supported_breakdown.entry(*kind).or_insert(0) += 1;
        }
    }
    let store = state.store.lock().await;
    let (jobs_queued, jobs_completed) = match (store.queued_count(), store.completed_count()) {
        (Ok(q), Ok(c)) => (q, c),
        (q, c) => {
            tracing::error!(?q, ?c, "stats: store count failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    Ok(Json(Stats {
        gpus_joined: earners.len(),
        total_vram_gb,
        jobs_queued,
        jobs_completed,
        supported_breakdown,
    }))
}

async fn next_job(State(state): State<Arc<AppState>>) -> Result<Json<Option<JobSpec>>, StatusCode> {
    let store = state.store.lock().await;
    match store.take_next(|_| true) {
        Ok(job) => Ok(Json(job)),
        Err(e) => {
            tracing::error!(?e, "next_job: take_next failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn submit(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(result): Json<JobResult>,
) -> Result<&'static str, StatusCode> {
    if result.job_id != id {
        return Err(StatusCode::BAD_REQUEST);
    }
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
    tracing::info!(?id, earner = %result.earner_address, "result received");
    // TODO validator gate, EAS attestation relay
    let mut store = state.store.lock().await;
    if let Err(e) = store.record_completed(&result) {
        tracing::error!(?id, ?e, "submit: failed to persist result");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok("accepted")
}

/// Websocket job dispatch (the v1 upgrade). Protocol, all JSON text frames:
///
///   1. earner → `EarnerMsg::Hello` (required first message; registers like
///      `/register`). Any other first message closes the socket.
///   2. coordinator polls the queue; when a job whose `kind` the earner
///      advertised in `supported` is available, it pops it and sends
///      `CoordinatorMsg::JobOffer(job)`. Only one job is offered at a time.
///   3. earner → `EarnerMsg::Accept { job_id }` marks the offer in-flight; an
///      `Accept` for a different/unknown job is ignored.
///   4. earner → `EarnerMsg::Submit(result)`: the signature + job_id are
///      verified. Valid → push to `completed` and reply
///      `CoordinatorMsg::Accepted { job_id, attestation_uid }`. Invalid →
///      `CoordinatorMsg::Rejected { job_id, reason }` and the job is requeued.
///   5. earner → `EarnerMsg::Heartbeat { .. }` is logged and ignored.
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

    // The job we've currently offered (and whether the earner accepted it).
    let mut offered: Option<JobSpec> = None;
    let mut accepted = false;
    // Poll the queue on this cadence when we have nothing offered.
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        // If we have no outstanding offer, try to grab a supported job.
        if offered.is_none() {
            if let Some(job) = take_supported_job(&state, &supported).await {
                if !send_msg(&mut socket, &CoordinatorMsg::JobOffer(job.clone())).await {
                    requeue(&state, job).await;
                    return;
                }
                tracing::info!(earner = %earner_address, job_id = %job.id, "job offered");
                offered = Some(job);
                accepted = false;
            }
        }

        tokio::select! {
            // Poll for new jobs while idle.
            _ = tick.tick(), if offered.is_none() => { continue; }
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
                match msg {
                    EarnerMsg::Hello { .. } => {
                        // Re-hello mid-session is unexpected; log and ignore.
                        tracing::warn!(earner = %earner_address, "unexpected Hello mid-session");
                    }
                    EarnerMsg::Accept { job_id } => {
                        match &offered {
                            Some(job) if job.id == job_id => {
                                accepted = true;
                                tracing::info!(earner = %earner_address, %job_id, "offer accepted");
                            }
                            _ => tracing::warn!(earner = %earner_address, %job_id, "accept for unknown/stale job"),
                        }
                    }
                    EarnerMsg::Submit(result) => {
                        let reply = handle_submit(&state, &offered, accepted, result).await;
                        let ok = matches!(reply, CoordinatorMsg::Accepted { .. });
                        if !send_msg(&mut socket, &reply).await {
                            // Couldn't deliver the verdict; requeue any offer.
                            if let (false, Some(job)) = (ok, offered.take()) {
                                requeue(&state, job).await;
                            }
                            return;
                        }
                        if ok {
                            offered = None;
                            accepted = false;
                        } else if let Some(job) = offered.take() {
                            // Rejected → requeue so another earner can try.
                            requeue(&state, job).await;
                            accepted = false;
                        }
                    }
                    EarnerMsg::Heartbeat { job_id, progress_pct } => {
                        tracing::debug!(earner = %earner_address, ?job_id, progress_pct, "heartbeat");
                    }
                }
            }
        }
    }

    // Socket ended with an un-submitted offer in flight → requeue it.
    if let Some(job) = offered.take() {
        requeue(&state, job).await;
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
            EarnerInfo { gpu_model, vram_gb, supported },
        );
        return Some(earner_address);
    }
}

/// Take the most recent queued job whose kind the earner supports, marking it
/// in-flight. Leaves unsupported jobs queued for other earners.
async fn take_supported_job(state: &Arc<AppState>, supported: &[JobKind]) -> Option<JobSpec> {
    let store = state.store.lock().await;
    match store.take_next(|job| supported.contains(&job.kind)) {
        Ok(job) => job,
        Err(e) => {
            tracing::error!(?e, "take_supported_job: take_next failed");
            None
        }
    }
}

/// Put a job back on the queue (rejected submission or dropped connection).
async fn requeue(state: &Arc<AppState>, job: JobSpec) {
    let store = state.store.lock().await;
    if let Err(e) = store.requeue(&job) {
        tracing::error!(job_id = %job.id, ?e, "requeue failed");
    }
}

/// Verify a ws `Submit` against the outstanding offer and produce the verdict.
/// On `Accepted`, the result is pushed into `completed`.
async fn handle_submit(
    state: &Arc<AppState>,
    offered: &Option<JobSpec>,
    accepted: bool,
    result: JobResult,
) -> CoordinatorMsg {
    let job_id = result.job_id;
    let reject = |reason: &str| CoordinatorMsg::Rejected {
        job_id,
        reason: reason.to_string(),
    };

    match offered {
        Some(job) if job.id == job_id => {}
        Some(_) => return reject("submit job_id does not match the offered job"),
        None => return reject("no job was offered on this connection"),
    }
    if !accepted {
        return reject("submit before accept");
    }

    if let Err(e) = verify::verify_signature(
        &result.job_id,
        &result.output_hash,
        &result.earner_address,
        &result.signature_hex,
    ) {
        tracing::warn!(%job_id, earner = %result.earner_address, ?e, "ws rejected: bad attestation");
        return reject("attestation signature verification failed");
    }

    tracing::info!(%job_id, earner = %result.earner_address, "ws result accepted");
    // TODO validator gate, real EAS attestation relay (placeholder uid for now).
    {
        let mut store = state.store.lock().await;
        if let Err(e) = store.record_completed(&result) {
            tracing::error!(%job_id, ?e, "ws: failed to persist result");
            return reject("failed to persist result");
        }
    }
    CoordinatorMsg::Accepted {
        job_id,
        attestation_uid: PLACEHOLDER_ATTESTATION_UID.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    /// In-memory store-backed state (no disk). `with_store` seeds one job
    /// because the in-memory DB starts empty; tests that need an empty queue
    /// drain it first via `/jobs/next` or use `test_state_empty`.
    fn test_state() -> Arc<AppState> {
        AppState::with_store(Store::open_in_memory().unwrap()).unwrap()
    }

    /// In-memory state with the auto-seeded job removed, so the queue starts
    /// empty (matches the old `AppState::default()` behavior used by tests
    /// that assert `jobs_queued == 0`).
    async fn test_state_empty() -> Arc<AppState> {
        let state = test_state();
        // Drain the seeded job so the queue is empty.
        let store = state.store.lock().await;
        let _ = store.take_next(|_| true).unwrap();
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
    async fn next_job_returns_seed_then_none() {
        // `test_state` auto-seeds exactly one terrain job.
        let state = test_state();

        let resp = get(state.clone(), "/jobs/next").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert!(!json.is_null(), "first poll should return the seeded job");
        assert_eq!(json["kind"], "terrain");

        let resp = get(state.clone(), "/jobs/next").await;
        let json = body_json(resp).await;
        assert!(json.is_null(), "second poll should return null");
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

    /// A `JobResult` validly signed by the dev key for the given job/hash.
    fn signed_result(job_id: Uuid, output_hash: &str) -> JobResult {
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

        let good = signed_result(job_id, "deadbeef");
        let uri = format!("/jobs/{}/submit", job_id);
        let resp = post_json(state.clone(), &uri, &serde_json::to_value(&good).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // mismatched path id vs body job_id → rejected
        let other = Uuid::new_v4();
        let uri = format!("/jobs/{}/submit", other);
        let resp = post_json(state.clone(), &uri, &serde_json::to_value(&good).unwrap()).await;
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
        let resp = post_json(state.clone(), &uri, &serde_json::to_value(&bad).unwrap()).await;
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
        let resp = post_json(state.clone(), &uri, &serde_json::to_value(&bad).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let json = body_json(get(state.clone(), "/stats").await).await;
        assert_eq!(json["jobs_completed"], 0);
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
            let state = AppState::with_store(Store::open(&db_path).unwrap()).unwrap();

            // Enqueue a second job and submit a validly-signed result for it.
            let job = seed_job();
            let job_id = job.id;
            enqueue(&state, &job).await;

            let result = signed_result(job_id, "deadbeef");
            let uri = format!("/jobs/{}/submit", job_id);
            let resp =
                post_json(state.clone(), &uri, &serde_json::to_value(&result).unwrap()).await;
            assert_eq!(resp.status(), StatusCode::OK);

            let json = body_json(get(state.clone(), "/stats").await).await;
            // One auto-seeded job still queued; the enqueued one is now done.
            queued_before = json["jobs_queued"].as_u64().unwrap();
            completed_before = json["jobs_completed"].as_u64().unwrap();
            assert_eq!(queued_before, 1);
            assert_eq!(completed_before, 1);
        } // state (and its Store/Connection) dropped here → "process" ends.

        // --- second "process": reopen the same file. with_store must NOT
        //     re-seed (jobs already exist), and counts must match. ---
        let state = AppState::with_store(Store::open(&db_path).unwrap()).unwrap();
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
