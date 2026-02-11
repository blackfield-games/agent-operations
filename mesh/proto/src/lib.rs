//! Wire types shared by coordinator + earner.
//!
//! Job dispatch is websocket-first per research-earner-client.md. This file
//! holds the JSON shapes; transport lives in coordinator/earner crates.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Terrain,
    Foliage,
    NpcTick,
    DiffusionTile,
    Optimization,
}

impl JobKind {
    pub fn as_u16(self) -> u16 {
        match self {
            JobKind::Terrain => 0,
            JobKind::Foliage => 1,
            JobKind::NpcTick => 2,
            JobKind::DiffusionTile => 3,
            JobKind::Optimization => 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionCoord {
    pub x: i32,
    pub y: i32,
    pub layer: u8,
}

impl RegionCoord {
    pub fn region_id(&self) -> String {
        format!("r{:+05}_{:+05}_l{}", self.x, self.y, self.layer)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: Uuid,
    pub kind: JobKind,
    pub region: RegionCoord,
    pub deadline_secs: u32,
    /// Max $BLCKFLD payable on acceptance (in 1e18 wei).
    pub max_payout_wei: String,
    /// Inputs needed to run the job (CDN URLs, asset hashes).
    pub inputs: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub job_id: Uuid,
    pub earner_address: String,
    /// sha256 of the produced USD layer file.
    pub output_hash: String,
    /// CDN-pinned URL where the coordinator can fetch the output.
    pub output_url: String,
    pub render_seconds: u32,
    /// Signed attestation envelope — earner's session key signs `(job_id, output_hash)`.
    /// Coordinator validates + relays to RenderReceipts.sol on Base.
    pub signature_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordinatorMsg {
    /// Coordinator → earner: a job is available.
    JobOffer(JobSpec),
    /// Coordinator → earner: result accepted, here's the EAS attestation UID.
    Accepted { job_id: Uuid, attestation_uid: String },
    /// Coordinator → earner: result rejected (validator failed).
    Rejected { job_id: Uuid, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EarnerMsg {
    /// Earner → coordinator: I'm online, here are my capabilities.
    Hello {
        earner_address: String,
        gpu_model: String,
        vram_gb: u32,
        supported: Vec<JobKind>,
    },
    /// Earner → coordinator: I accept this job offer.
    Accept { job_id: Uuid },
    /// Earner → coordinator: here's the result.
    Submit(JobResult),
    /// Earner → coordinator: heartbeat / progress.
    Heartbeat { job_id: Option<Uuid>, progress_pct: u8 },
}
