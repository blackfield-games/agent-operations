//! Wire types shared by coordinator + earner.
//!
//! Job dispatch is websocket-first per research-earner-client.md. This file
//! holds the JSON shapes; transport lives in coordinator/earner crates.

use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use uuid::Uuid;

/// Canonical message digest signed by the earner's session key and verified by
/// the coordinator. Both sides MUST agree byte-for-byte, so the construction is
/// fixed here and shared.
///
/// Message bytes = `job_id` (16 raw bytes, UUID big-endian) || `output_hash`
/// (the ASCII bytes of the lowercase hex string, e.g. the 64 chars of a sha256
/// digest). The digest is `keccak256(message)`.
///
/// Note we hash the hex *string* bytes of `output_hash`, not the decoded
/// digest, because `output_hash` is carried on the wire as a hex string and we
/// want the signature to commit to exactly what is transmitted.
pub fn signing_digest(job_id: &Uuid, output_hash: &str) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(job_id.as_bytes());
    hasher.update(output_hash.as_bytes());
    hasher.finalize().into()
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};

    /// Derive an Ethereum-style address (0x-prefixed, lowercase) from a
    /// verifying key: keccak256(uncompressed_pubkey[1..])[12..].
    fn address_from_verifying_key(vk: &VerifyingKey) -> String {
        let point = vk.to_encoded_point(false);
        let bytes = point.as_bytes(); // 65 bytes: 0x04 || X || Y
        let hash = Keccak256::digest(&bytes[1..]);
        format!("0x{}", hex::encode(&hash[12..]))
    }

    #[test]
    fn sign_then_recover_yields_derived_address() {
        // Known dev key (same default the earner ships).
        let key_bytes =
            hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318")
                .unwrap();
        let sk = SigningKey::from_slice(&key_bytes).unwrap();
        let expected = address_from_verifying_key(sk.verifying_key());

        let job_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let output_hash = "deadbeef".repeat(8); // 64-char hex string
        let digest = signing_digest(&job_id, &output_hash);

        let (sig, recid): (Signature, RecoveryId) =
            sk.sign_prehash_recoverable(&digest).unwrap();

        // Recover the verifying key and re-derive the address.
        let recovered_vk =
            VerifyingKey::recover_from_prehash(&digest, &sig, recid).unwrap();
        let recovered = address_from_verifying_key(&recovered_vk);

        assert_eq!(recovered, expected);
    }
}
