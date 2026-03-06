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

    // -----------------------------------------------------------------------
    // Wire-shape contract tests — lock the JSON serialization of every public
    // type so an accidental rename or tag change fails CI immediately.
    // -----------------------------------------------------------------------

    const FIXED_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn jobkind_tags_are_stable() {
        // Each variant must round-trip to exactly the documented snake_case tag.
        let cases = [
            (JobKind::Terrain, "terrain"),
            (JobKind::Foliage, "foliage"),
            (JobKind::NpcTick, "npc_tick"),
            (JobKind::DiffusionTile, "diffusion_tile"),
            (JobKind::Optimization, "optimization"),
        ];
        for (variant, tag) in cases {
            let serialized = serde_json::to_value(variant).unwrap();
            assert_eq!(
                serialized,
                serde_json::json!(tag),
                "JobKind::{:?} tag drifted",
                variant
            );
            let deserialized: JobKind = serde_json::from_value(serialized).unwrap();
            assert_eq!(
                deserialized, variant,
                "JobKind::{:?} did not round-trip",
                variant
            );
        }

        // as_u16() is part of the on-chain versioning contract — pin it.
        assert_eq!(JobKind::Terrain.as_u16(), 0);
        assert_eq!(JobKind::Foliage.as_u16(), 1);
        assert_eq!(JobKind::NpcTick.as_u16(), 2);
        assert_eq!(JobKind::DiffusionTile.as_u16(), 3);
        assert_eq!(JobKind::Optimization.as_u16(), 4);
    }

    #[test]
    fn jobspec_wire_shape_is_stable() {
        let canonical = serde_json::json!({
            "id": FIXED_UUID,
            "kind": "diffusion_tile",
            "region": { "x": 10, "y": -5, "layer": 1 },
            "deadline_secs": 120,
            "max_payout_wei": "1000000000000000000",
            "inputs": { "asset_url": "https://cdn.example.com/tile.usd", "lod": 2 }
        });

        let parsed: JobSpec = serde_json::from_value(canonical.clone()).unwrap();
        let reserialized = serde_json::to_value(&parsed).unwrap();
        assert_eq!(reserialized, canonical, "JobSpec wire shape drifted");
    }

    #[test]
    fn jobresult_wire_shape_is_stable() {
        let canonical = serde_json::json!({
            "job_id": FIXED_UUID,
            "earner_address": "0xabcdef1234567890abcdef1234567890abcdef12",
            "output_hash": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "output_url": "https://cdn.example.com/output/tile.usd",
            "render_seconds": 47,
            "signature_hex": "0xcafebabe"
        });

        let parsed: JobResult = serde_json::from_value(canonical.clone()).unwrap();
        let reserialized = serde_json::to_value(&parsed).unwrap();
        assert_eq!(reserialized, canonical, "JobResult wire shape drifted");
    }

    #[test]
    fn coordinator_msg_wire_shapes_are_stable() {
        // JobOffer — newtype variant: inner JobSpec fields are flattened next to "type".
        let canonical_offer = serde_json::json!({
            "type": "job_offer",
            "id": FIXED_UUID,
            "kind": "diffusion_tile",
            "region": { "x": 10, "y": -5, "layer": 1 },
            "deadline_secs": 120,
            "max_payout_wei": "1000000000000000000",
            "inputs": { "asset_url": "https://cdn.example.com/tile.usd", "lod": 2 }
        });
        let parsed_offer: CoordinatorMsg =
            serde_json::from_value(canonical_offer.clone()).unwrap();
        let reserialized_offer = serde_json::to_value(&parsed_offer).unwrap();
        assert_eq!(
            reserialized_offer, canonical_offer,
            "CoordinatorMsg::JobOffer wire shape drifted"
        );

        // Accepted — struct variant.
        let canonical_accepted = serde_json::json!({
            "type": "accepted",
            "job_id": FIXED_UUID,
            "attestation_uid": "0xaabbccddeeff"
        });
        let parsed_accepted: CoordinatorMsg =
            serde_json::from_value(canonical_accepted.clone()).unwrap();
        let reserialized_accepted = serde_json::to_value(&parsed_accepted).unwrap();
        assert_eq!(
            reserialized_accepted, canonical_accepted,
            "CoordinatorMsg::Accepted wire shape drifted"
        );

        // Rejected — struct variant.
        let canonical_rejected = serde_json::json!({
            "type": "rejected",
            "job_id": FIXED_UUID,
            "reason": "output hash mismatch"
        });
        let parsed_rejected: CoordinatorMsg =
            serde_json::from_value(canonical_rejected.clone()).unwrap();
        let reserialized_rejected = serde_json::to_value(&parsed_rejected).unwrap();
        assert_eq!(
            reserialized_rejected, canonical_rejected,
            "CoordinatorMsg::Rejected wire shape drifted"
        );
    }

    #[test]
    fn earner_msg_wire_shapes_are_stable() {
        // Hello — struct variant.
        let canonical_hello = serde_json::json!({
            "type": "hello",
            "earner_address": "0xabcdef1234567890abcdef1234567890abcdef12",
            "gpu_model": "RTX 4090",
            "vram_gb": 24,
            "supported": ["terrain", "foliage"]
        });
        let parsed_hello: EarnerMsg =
            serde_json::from_value(canonical_hello.clone()).unwrap();
        let reserialized_hello = serde_json::to_value(&parsed_hello).unwrap();
        assert_eq!(
            reserialized_hello, canonical_hello,
            "EarnerMsg::Hello wire shape drifted"
        );

        // Accept — struct variant.
        let canonical_accept = serde_json::json!({
            "type": "accept",
            "job_id": FIXED_UUID
        });
        let parsed_accept: EarnerMsg =
            serde_json::from_value(canonical_accept.clone()).unwrap();
        let reserialized_accept = serde_json::to_value(&parsed_accept).unwrap();
        assert_eq!(
            reserialized_accept, canonical_accept,
            "EarnerMsg::Accept wire shape drifted"
        );

        // Submit — newtype variant: inner JobResult fields are flattened next to "type".
        let canonical_submit = serde_json::json!({
            "type": "submit",
            "job_id": FIXED_UUID,
            "earner_address": "0xabcdef1234567890abcdef1234567890abcdef12",
            "output_hash": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "output_url": "https://cdn.example.com/output/tile.usd",
            "render_seconds": 47,
            "signature_hex": "0xcafebabe"
        });
        let parsed_submit: EarnerMsg =
            serde_json::from_value(canonical_submit.clone()).unwrap();
        let reserialized_submit = serde_json::to_value(&parsed_submit).unwrap();
        assert_eq!(
            reserialized_submit, canonical_submit,
            "EarnerMsg::Submit wire shape drifted"
        );

        // Heartbeat with Some(uuid) — job_id is present as a string.
        let canonical_hb_some = serde_json::json!({
            "type": "heartbeat",
            "job_id": FIXED_UUID,
            "progress_pct": 55
        });
        let parsed_hb_some: EarnerMsg =
            serde_json::from_value(canonical_hb_some.clone()).unwrap();
        let reserialized_hb_some = serde_json::to_value(&parsed_hb_some).unwrap();
        assert_eq!(
            reserialized_hb_some, canonical_hb_some,
            "EarnerMsg::Heartbeat(Some) wire shape drifted"
        );

        // Heartbeat with None — job_id field must be present and set to null.
        let canonical_hb_none = serde_json::json!({
            "type": "heartbeat",
            "job_id": null,
            "progress_pct": 0
        });
        let parsed_hb_none: EarnerMsg =
            serde_json::from_value(canonical_hb_none.clone()).unwrap();
        let reserialized_hb_none = serde_json::to_value(&parsed_hb_none).unwrap();
        assert_eq!(
            reserialized_hb_none, canonical_hb_none,
            "EarnerMsg::Heartbeat(None) wire shape drifted — job_id must serialize as null, not be omitted"
        );
    }

    #[test]
    fn region_id_format_is_stable() {
        // Pin the {:+05} sign+zero-pad format used by region_id().
        assert_eq!(
            RegionCoord { x: 42, y: -17, layer: 0 }.region_id(),
            "r+0042_-0017_l0"
        );
        // Negative x, positive y, non-zero layer.
        assert_eq!(
            RegionCoord { x: -3, y: 100, layer: 2 }.region_id(),
            "r-0003_+0100_l2"
        );
    }
}
