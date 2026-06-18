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

/// Canonical digest the earner signs at registration to prove possession of the
/// key behind `earner_address` — the registration analogue of [`signing_digest`].
/// The coordinator recovers the signer from this digest plus the `Hello`'s
/// `signature_hex` and rejects the registration unless the recovered address
/// matches the claimed `earner_address`.
///
/// Bytes = `keccak256( DOMAIN || len(addr) || addr || len(gpu) || gpu ||
/// vram_gb_be || len(supported) || Σ kind_as_u16_be )`, every `len` a big-endian
/// `u32`. The `DOMAIN` tag separates it from [`signing_digest`], so a result
/// attestation can never double as a registration signature; the length prefixes
/// make the field boundaries unambiguous, so no two distinct Hellos collide on a
/// digest (e.g. `addr="0xab",gpu="cd"` ≠ `addr="0xabcd",gpu=""`). The signature
/// therefore binds the *whole* advertised capability set, not just the address.
/// Both sides MUST build it identically, so the construction is fixed here.
pub fn hello_digest(
    earner_address: &str,
    gpu_model: &str,
    vram_gb: u32,
    supported: &[JobKind],
) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(b"blackfield/hello/v1");
    hasher.update((earner_address.len() as u32).to_be_bytes());
    hasher.update(earner_address.as_bytes());
    hasher.update((gpu_model.len() as u32).to_be_bytes());
    hasher.update(gpu_model.as_bytes());
    hasher.update(vram_gb.to_be_bytes());
    hasher.update((supported.len() as u32).to_be_bytes());
    for kind in supported {
        hasher.update(kind.as_u16().to_be_bytes());
    }
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

    /// Inverse of [`JobKind::as_u16`]: map an on-chain numeric tag back to a
    /// variant, or `None` if out of range. Keep in lockstep with `as_u16`.
    pub fn from_u16(v: u16) -> Option<JobKind> {
        match v {
            0 => Some(JobKind::Terrain),
            1 => Some(JobKind::Foliage),
            2 => Some(JobKind::NpcTick),
            3 => Some(JobKind::DiffusionTile),
            4 => Some(JobKind::Optimization),
            _ => None,
        }
    }

    /// All variants in numeric (`as_u16`) order — the single source of truth for
    /// the full set (earner capability lists, stats breakdowns, …).
    pub const ALL: [JobKind; 5] = [
        JobKind::Terrain,
        JobKind::Foliage,
        JobKind::NpcTick,
        JobKind::DiffusionTile,
        JobKind::Optimization,
    ];

    /// The canonical snake_case tag — identical to the serde representation.
    /// Inverse of the `FromStr` impl below.
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Terrain => "terrain",
            JobKind::Foliage => "foliage",
            JobKind::NpcTick => "npc_tick",
            JobKind::DiffusionTile => "diffusion_tile",
            JobKind::Optimization => "optimization",
        }
    }
}

/// Error returned by `<JobKind as std::str::FromStr>::from_str` for an unknown tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseJobKindError;

impl std::fmt::Display for ParseJobKindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unknown JobKind tag")
    }
}

impl std::error::Error for ParseJobKindError {}

impl std::str::FromStr for JobKind {
    type Err = ParseJobKindError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "terrain" => Ok(JobKind::Terrain),
            "foliage" => Ok(JobKind::Foliage),
            "npc_tick" => Ok(JobKind::NpcTick),
            "diffusion_tile" => Ok(JobKind::DiffusionTile),
            "optimization" => Ok(JobKind::Optimization),
            _ => Err(ParseJobKindError),
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
    ///
    /// `signature_hex` is a recoverable secp256k1 `[r||s||v]` over
    /// [`hello_digest`] proving the earner controls the key behind
    /// `earner_address`; the coordinator recovers the signer and rejects the
    /// registration on mismatch. This authenticates the self-reported identity
    /// the capability filter, fault attribution, and `/stats` totals key on.
    Hello {
        earner_address: String,
        gpu_model: String,
        vram_gb: u32,
        supported: Vec<JobKind>,
        signature_hex: String,
    },
    /// Earner → coordinator: I accept this job offer.
    Accept { job_id: Uuid },
    /// Earner → coordinator: I decline this job offer without rendering it —
    /// typically because its `kind` is not in the `supported` set I advertised
    /// in `Hello` (a capability self-guard against a coordinator that offered a
    /// kind I can't render). Unlike a dropped result this is not a rendering
    /// attempt: the coordinator requeues the job for a capable earner rather
    /// than charging its dispatch budget.
    Decline { job_id: Uuid, reason: String },
    /// Earner → coordinator: here's the result.
    Submit(JobResult),
    /// Earner → coordinator: heartbeat / progress.
    Heartbeat { job_id: Option<Uuid>, progress_pct: u8 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
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

    #[test]
    fn hello_sign_then_recover_yields_claimed_address() {
        // The registration analogue of the result-attestation recovery above:
        // the earner signs `hello_digest` with its session key and the
        // coordinator must recover exactly the claimed address.
        let key_bytes =
            hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318")
                .unwrap();
        let sk = SigningKey::from_slice(&key_bytes).unwrap();
        let expected = address_from_verifying_key(sk.verifying_key());

        let supported = [JobKind::Terrain, JobKind::DiffusionTile];
        let digest = hello_digest(&expected, "RTX 4090", 24, &supported);

        let (sig, recid): (Signature, RecoveryId) =
            sk.sign_prehash_recoverable(&digest).unwrap();
        let recovered_vk =
            VerifyingKey::recover_from_prehash(&digest, &sig, recid).unwrap();

        assert_eq!(address_from_verifying_key(&recovered_vk), expected);
    }

    #[test]
    fn hello_digest_binds_every_field() {
        // The signature commits to the whole advertised Hello, so changing any
        // field changes the digest — a captured signature can't be reattached to
        // an inflated vram / different capability set (full-Hello replay is the
        // separate residual deferred to the nonce slice).
        let base = hello_digest("0xabc", "gpu", 24, &[JobKind::Terrain]);
        assert_ne!(base, hello_digest("0xabd", "gpu", 24, &[JobKind::Terrain]), "address");
        assert_ne!(base, hello_digest("0xabc", "gpx", 24, &[JobKind::Terrain]), "gpu_model");
        assert_ne!(base, hello_digest("0xabc", "gpu", 25, &[JobKind::Terrain]), "vram_gb");
        assert_ne!(base, hello_digest("0xabc", "gpu", 24, &[JobKind::Foliage]), "kind");
        assert_ne!(
            base,
            hello_digest("0xabc", "gpu", 24, &[JobKind::Terrain, JobKind::Foliage]),
            "supported length"
        );
        // Length-delimited: shifting a byte across the address/gpu boundary must
        // not alias to the same digest.
        assert_ne!(
            hello_digest("0xab", "cgpu", 24, &[JobKind::Terrain]),
            hello_digest("0xabc", "gpu", 24, &[JobKind::Terrain]),
            "field boundaries must be unambiguous"
        );
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
    fn jobkind_from_u16_is_inverse_of_as_u16() {
        let all = [
            JobKind::Terrain,
            JobKind::Foliage,
            JobKind::NpcTick,
            JobKind::DiffusionTile,
            JobKind::Optimization,
        ];
        for k in all {
            assert_eq!(JobKind::from_u16(k.as_u16()), Some(k), "from_u16 must invert as_u16 for {k:?}");
        }
        // Out-of-range numeric tags map to no variant.
        assert_eq!(JobKind::from_u16(5), None);
        assert_eq!(JobKind::from_u16(u16::MAX), None);
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
            "supported": ["terrain", "foliage"],
            "signature_hex": "0xcafebabe"
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

        // Decline — struct variant carrying the job id and a human-readable reason.
        let canonical_decline = serde_json::json!({
            "type": "decline",
            "job_id": FIXED_UUID,
            "reason": "unsupported job kind: diffusion_tile"
        });
        let parsed_decline: EarnerMsg =
            serde_json::from_value(canonical_decline.clone()).unwrap();
        let reserialized_decline = serde_json::to_value(&parsed_decline).unwrap();
        assert_eq!(
            reserialized_decline, canonical_decline,
            "EarnerMsg::Decline wire shape drifted"
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

    #[test]
    fn jobkind_as_str_matches_serde_tag() {
        for k in JobKind::ALL {
            assert_eq!(serde_json::to_value(k).unwrap(), serde_json::json!(k.as_str()),
                "as_str disagrees with serde tag for {k:?}");
        }
    }

    #[test]
    fn jobkind_from_str_inverts_as_str() {
        for k in JobKind::ALL {
            assert_eq!(JobKind::from_str(k.as_str()), Ok(k), "from_str must invert as_str for {k:?}");
        }
        assert!(JobKind::from_str("nope").is_err());
        assert!(JobKind::from_str("").is_err());
    }

    #[test]
    fn jobkind_all_is_in_numeric_order() {
        assert_eq!(
            JobKind::ALL,
            [JobKind::Terrain, JobKind::Foliage, JobKind::NpcTick, JobKind::DiffusionTile, JobKind::Optimization]
        );
        for (i, k) in JobKind::ALL.iter().enumerate() {
            assert_eq!(k.as_u16() as usize, i, "ALL not in as_u16 order at {i}");
        }
        assert_eq!(JobKind::ALL.len(), 5);
    }
}
