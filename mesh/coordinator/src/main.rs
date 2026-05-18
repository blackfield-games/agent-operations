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
use proto::{JobKind, JobResult, JobSpec, RegionCoord};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "COORDINATOR_BIND", default_value = "127.0.0.1:8787")]
    bind: String,
}

#[derive(Default)]
struct AppState {
    queue: Mutex<Vec<JobSpec>>,
    completed: Mutex<Vec<JobResult>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("coordinator=info,tower_http=info")
        .init();
    let args = Args::parse();
    let state = Arc::new(AppState::default());

    // seed one job so the earner has something to take
    state.queue.lock().await.push(JobSpec {
        id: Uuid::new_v4(),
        kind: JobKind::Terrain,
        region: RegionCoord { x: 42, y: -17, layer: 0 },
        deadline_secs: 60,
        max_payout_wei: "1000000000000000000".into(),
        inputs: serde_json::json!({"heightfield_seed": 0xb1acf1e1du64}),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/jobs/next", get(next_job))
        .route("/jobs/{id}/submit", post(submit))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    tracing::info!(bind = %args.bind, "coordinator up");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
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
