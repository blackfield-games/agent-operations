//! BLACKFIELD coordinator — accepts earner registrations, dispatches jobs,
//! collects results, relays validated receipts to RenderReceipts.sol on Base.
//!
//! v0: HTTP-only stub. v1: replace `/jobs/next` poll with a websocket fanout.
//! v2: federated coordinators (research-earner-client.md flagged libp2p
//!     gossipsub as experimental; revisit when sub-200ms dispatch matters).

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use proto::{EarnerMsg, JobKind, JobResult, JobSpec, RegionCoord};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "COORDINATOR_BIND", default_value = "127.0.0.1:8787")]
    bind: String,
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

#[derive(Default)]
struct AppState {
    queue: Mutex<Vec<JobSpec>>,
    completed: Mutex<Vec<JobResult>>,
    earners: Mutex<HashMap<String, EarnerInfo>>,
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
    let state = Arc::new(AppState::default());

    // seed one job so the earner has something to take
    state.queue.lock().await.push(seed_job());

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

async fn stats(State(state): State<Arc<AppState>>) -> Json<Stats> {
    let earners = state.earners.lock().await;
    let mut total_vram_gb: u64 = 0;
    let mut supported_breakdown: HashMap<JobKind, usize> = HashMap::new();
    for info in earners.values() {
        total_vram_gb += info.vram_gb as u64;
        for kind in &info.supported {
            *supported_breakdown.entry(*kind).or_insert(0) += 1;
        }
    }
    Json(Stats {
        gpus_joined: earners.len(),
        total_vram_gb,
        jobs_queued: state.queue.lock().await.len(),
        jobs_completed: state.completed.lock().await.len(),
        supported_breakdown,
    })
}

async fn next_job(State(state): State<Arc<AppState>>) -> Result<Json<Option<JobSpec>>, StatusCode> {
    let mut q = state.queue.lock().await;
    Ok(Json(q.pop()))
}

async fn submit(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(result): Json<JobResult>,
) -> Result<&'static str, StatusCode> {
    if result.job_id != id {
        return Err(StatusCode::BAD_REQUEST);
    }
    tracing::info!(?id, earner = %result.earner_address, "result received");
    // TODO validator gate, EAS attestation relay
    state.completed.lock().await.push(result);
    Ok("accepted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState::default())
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
        let state = test_state();
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
        let state = test_state();
        state.queue.lock().await.push(seed_job());

        let resp = get(state.clone(), "/jobs/next").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert!(!json.is_null(), "first poll should return the seeded job");
        assert_eq!(json["kind"], "terrain");

        let resp = get(state.clone(), "/jobs/next").await;
        let json = body_json(resp).await;
        assert!(json.is_null(), "second poll should return null");
    }

    #[tokio::test]
    async fn submit_matching_id_accepted_mismatch_rejected() {
        let state = test_state();
        let job = seed_job();
        let job_id = job.id;
        state.queue.lock().await.push(job);

        let good = JobResult {
            job_id,
            earner_address: "0xabc".into(),
            output_hash: "deadbeef".into(),
            output_url: "memory://x".into(),
            render_seconds: 1,
            signature_hex: "00".repeat(65),
        };
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
}
