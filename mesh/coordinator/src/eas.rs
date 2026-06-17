//! RenderReceipts EAS attestation mapping.
//!
//! When a result is validated and settled, the coordinator records a *pending
//! attestation* — the off-chain twin of an EAS render receipt — so a crash
//! between the settle and the on-chain `RenderReceipts.issueReceipt` cannot lose
//! it. This module is the single place that maps the mesh's proto types onto the
//! contract's registered schema fields:
//!
//! ```text
//! address earner, bytes32 jobId, uint64 renderSeconds,
//! uint16 jobKind, bytes32 outputHash, bytes32 regionId
//! ```
//!
//! The relayer (operator-gated, not built here) reads a pending row and calls
//! `issueReceipt(earner, jobId, renderSeconds, jobKind, outputHash, regionId)`,
//! so every value here must match the contract's expectations exactly — see the
//! per-field docs for the wire mapping. `jobKind` reuses `proto::JobKind::as_u16`
//! so the mesh and the contract share one source of truth for the kind codes.

use proto::{JobResult, JobSpec, RegionCoord};
use uuid::Uuid;

/// Embed a 128-bit job UUID in the contract's `bytes32 jobId`, as 64 lowercase
/// hex chars (no `0x`). The UUID's 16 big-endian bytes occupy the LOW 16 bytes
/// (right-aligned); the high 16 bytes are zero — the natural numeric widening of
/// a u128 to a bytes32. Deterministic and injective, so it preserves the
/// contract's per-job dedup (`receiptIssued[jobId]`).
pub fn job_id_hex(id: &Uuid) -> String {
    let mut out = [0u8; 32];
    out[16..].copy_from_slice(id.as_bytes());
    hex::encode(out)
}

/// Pack a `RegionCoord` into the contract's `bytes32 regionId`, as 64 lowercase
/// hex chars (no `0x`). Layout (left-aligned, big-endian):
/// `bytes[0..4] = x (i32 BE) · bytes[4..8] = y (i32 BE) · bytes[8] = layer ·
/// bytes[9..32] = 0`. Deterministic and injective over `(x, y, layer)`.
pub fn region_id_hex(region: &RegionCoord) -> String {
    let mut out = [0u8; 32];
    out[0..4].copy_from_slice(&region.x.to_be_bytes());
    out[4..8].copy_from_slice(&region.y.to_be_bytes());
    out[8] = region.layer;
    hex::encode(out)
}

/// A settled render's pending EAS attestation: exactly the RenderReceipts schema
/// fields, in the contract's argument types. `job_id`, `output_hash`, and
/// `region_id` are 64-char lowercase-hex `bytes32` (no `0x`); `earner` is the
/// 0x-prefixed address verbatim from the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttestation {
    pub earner: String,
    pub job_id: String,
    pub render_seconds: u64,
    pub job_kind: u16,
    pub output_hash: String,
    pub region_id: String,
}

impl PendingAttestation {
    /// Build from the job's spec (kind + region) and the earner's result. The
    /// result is expected to have already cleared the content gate, so
    /// `output_hash` is a 64-char lowercase-hex digest; this re-checks that shape
    /// and returns `None` otherwise (defensive — unreachable on the gated path,
    /// but `record_completed` is also reachable directly in tests).
    pub fn build(spec: &JobSpec, result: &JobResult) -> Option<Self> {
        if !is_bytes32_hex(&result.output_hash) {
            return None;
        }
        Some(Self {
            earner: result.earner_address.clone(),
            job_id: job_id_hex(&result.job_id),
            render_seconds: u64::from(result.render_seconds),
            job_kind: spec.kind.as_u16(),
            output_hash: result.output_hash.clone(),
            region_id: region_id_hex(&spec.region),
        })
    }
}

/// A 256-bit digest as 64 lowercase-hex chars — the `bytes32` text form.
fn is_bytes32_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::{JobKind, JobResult, JobSpec, RegionCoord};

    fn spec(kind: JobKind, x: i32, y: i32, layer: u8) -> JobSpec {
        JobSpec {
            id: Uuid::new_v4(),
            kind,
            region: RegionCoord { x, y, layer },
            deadline_secs: 60,
            max_payout_wei: "0".into(),
            inputs: serde_json::Value::Null,
        }
    }

    fn result(job_id: Uuid, output_hash: &str) -> JobResult {
        JobResult {
            job_id,
            earner_address: "0x00000000000000000000000000000000000000a1".into(),
            output_hash: output_hash.into(),
            output_url: "memory://x".into(),
            render_seconds: 7,
            signature_hex: String::new(),
        }
    }

    #[test]
    fn job_kind_codes_match_the_contract_set() {
        // 0=terrain, 1=foliage, 2=npc_tick, 3=diffusion_tile, 4=optimization —
        // the order RenderReceipts.sol documents. Reuses proto's as_u16.
        assert_eq!(JobKind::Terrain.as_u16(), 0);
        assert_eq!(JobKind::Foliage.as_u16(), 1);
        assert_eq!(JobKind::NpcTick.as_u16(), 2);
        assert_eq!(JobKind::DiffusionTile.as_u16(), 3);
        assert_eq!(JobKind::Optimization.as_u16(), 4);
    }

    #[test]
    fn job_id_hex_right_aligns_the_uuid_in_bytes32() {
        let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let h = job_id_hex(&id);
        assert_eq!(h.len(), 64);
        // High 16 bytes (32 hex chars) zero, low 16 bytes the UUID.
        assert_eq!(&h[..32], "00000000000000000000000000000000");
        assert_eq!(&h[32..], "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn job_id_hex_is_injective() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_ne!(a, b);
        assert_ne!(job_id_hex(&a), job_id_hex(&b));
    }

    #[test]
    fn region_id_hex_packs_xy_layer_big_endian() {
        // x = 1, y = -1, layer = 2.
        let h = region_id_hex(&RegionCoord { x: 1, y: -1, layer: 2 });
        assert_eq!(h.len(), 64);
        assert_eq!(&h[0..8], "00000001"); // x = 1
        assert_eq!(&h[8..16], "ffffffff"); // y = -1 (two's complement BE)
        assert_eq!(&h[16..18], "02"); // layer = 2
        assert_eq!(&h[18..], &"0".repeat(46)); // remainder zero
    }

    #[test]
    fn region_id_hex_distinguishes_layers() {
        let l0 = region_id_hex(&RegionCoord { x: 5, y: 9, layer: 0 });
        let l1 = region_id_hex(&RegionCoord { x: 5, y: 9, layer: 1 });
        assert_ne!(l0, l1);
    }

    #[test]
    fn build_maps_every_field() {
        let s = spec(JobKind::DiffusionTile, 3, 4, 1);
        let oh = "a".repeat(64);
        let r = result(s.id, &oh);
        let att = PendingAttestation::build(&s, &r).expect("valid hash builds");
        assert_eq!(att.earner, r.earner_address);
        assert_eq!(att.job_id, job_id_hex(&s.id));
        assert_eq!(att.render_seconds, 7);
        assert_eq!(att.job_kind, 3);
        assert_eq!(att.output_hash, oh);
        assert_eq!(att.region_id, region_id_hex(&s.region));
    }

    #[test]
    fn build_rejects_a_malformed_output_hash() {
        let s = spec(JobKind::Terrain, 0, 0, 0);
        assert!(PendingAttestation::build(&s, &result(s.id, "deadbeef")).is_none());
        assert!(PendingAttestation::build(&s, &result(s.id, "")).is_none());
        assert!(PendingAttestation::build(&s, &result(s.id, &"A".repeat(64))).is_none());
    }
}
