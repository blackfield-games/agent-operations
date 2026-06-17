//! Result-content validation gate.
//!
//! Runs after signature verification, before a result is settled: the signature
//! proves *who* produced a result, this proves the result is *well-formed*
//! enough to meter and attest. A render that signs an empty or malformed output
//! hash, an unfetchable artifact URL, or zero render-seconds is refused before
//! it can be credited or relayed to `RenderReceipts` (whose `bytes32 outputHash`
//! field is exactly this `output_hash`).
//!
//! This is a policy gate, not a re-render: it cannot prove the bytes at
//! `output_url` actually hash to `output_hash` — that needs the artifact and a
//! GPU. It rejects the results that are self-evidently unmeterable, closing the
//! gap where any valid signature over garbage settled as completed work.

use proto::JobResult;

/// Why a submitted result failed the content gate. Distinct variants so the
/// reject reason is legible in logs and the ws `Rejected` frame.
#[derive(Debug, PartialEq, Eq)]
pub enum ValidationError {
    /// `output_hash` is not a 256-bit digest in lowercase hex (64 hex chars).
    /// EAS reads it as `bytes32 outputHash`; a malformed value attests garbage.
    MalformedOutputHash,
    /// `output_url` is not `scheme://rest` with a non-empty scheme and
    /// remainder, so the rendered artifact can never be fetched to validate or
    /// pin.
    MalformedOutputUrl,
    /// `render_seconds` is zero — a job that consumed no compute did no work and
    /// must not be metered or paid.
    ZeroRenderSeconds,
}

impl ValidationError {
    /// Stable, low-cardinality reason string for the ws `Rejected` frame.
    pub fn reason(&self) -> &'static str {
        match self {
            ValidationError::MalformedOutputHash => "output_hash is not a 256-bit lowercase-hex digest",
            ValidationError::MalformedOutputUrl => "output_url is not a fetchable scheme://… url",
            ValidationError::ZeroRenderSeconds => "render_seconds is zero",
        }
    }
}

/// 32-byte digest rendered as lowercase hex.
const OUTPUT_HASH_LEN: usize = 64;

/// Validate the *content* of an authenticated result. Pure function of the
/// result — no store, no network — so it is cheap to run before taking any lock.
pub fn validate_result(result: &JobResult) -> Result<(), ValidationError> {
    if !is_digest_hex(&result.output_hash) {
        return Err(ValidationError::MalformedOutputHash);
    }
    if !is_fetchable_url(&result.output_url) {
        return Err(ValidationError::MalformedOutputUrl);
    }
    if result.render_seconds == 0 {
        return Err(ValidationError::ZeroRenderSeconds);
    }
    Ok(())
}

/// Exactly 64 lowercase-hex chars (a 256-bit digest). Rejects empty, wrong
/// length, uppercase, `0x`-prefixed, and any non-hex byte. Algorithm-agnostic:
/// sha256 and keccak256 both render to this shape, and the coordinator cannot
/// know which the earner used — only that it is a well-formed 256-bit digest.
fn is_digest_hex(s: &str) -> bool {
    s.len() == OUTPUT_HASH_LEN && s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

/// A `scheme://rest` url with a non-empty scheme (alphanumeric plus `+-.`, per
/// RFC 3986) and a non-empty remainder. Accepts the earner's `memory://<id>`
/// plus `https`/`ipfs`/`ar`; rejects empty, scheme-only (`https://`), and
/// scheme-less strings. Deliberately permissive on the host — reachability is a
/// fetch-time concern the relay handles, not something the gate can prove.
fn is_fetchable_url(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    !scheme.is_empty()
        && !rest.is_empty()
        && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::JobResult;
    use uuid::Uuid;

    /// A result whose content fields are all valid; mutate one per test.
    fn valid() -> JobResult {
        JobResult {
            job_id: Uuid::new_v4(),
            earner_address: "0x000000000000000000000000000000000000dead".into(),
            output_hash: "a".repeat(64),
            output_url: "memory://job".into(),
            render_seconds: 1,
            signature_hex: String::new(),
        }
    }

    #[test]
    fn accepts_a_wellformed_result() {
        assert_eq!(validate_result(&valid()), Ok(()));
    }

    #[test]
    fn accepts_real_earner_shaped_fields() {
        // What the earner actually emits: lowercase sha256 hex + memory:// url.
        let mut r = valid();
        r.output_hash =
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into();
        r.output_url = format!("memory://{}", r.job_id);
        assert_eq!(validate_result(&r), Ok(()));
    }

    #[test]
    fn rejects_empty_hash() {
        let mut r = valid();
        r.output_hash = String::new();
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputHash));
    }

    #[test]
    fn rejects_short_hash() {
        // A common stub value ("deadbeef") is only 8 chars — not a 256-bit digest.
        let mut r = valid();
        r.output_hash = "deadbeef".into();
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputHash));
    }

    #[test]
    fn rejects_overlong_hash() {
        let mut r = valid();
        r.output_hash = "a".repeat(65);
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputHash));
    }

    #[test]
    fn rejects_uppercase_hash() {
        // The earner emits lowercase hex; uppercase would change the attested
        // byte string's textual form and is refused for one canonical encoding.
        let mut r = valid();
        r.output_hash = "A".repeat(64);
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputHash));
    }

    #[test]
    fn rejects_0x_prefixed_hash() {
        // 0x + 62 hex is still 64 chars but the 'x' is non-hex — rejected.
        let mut r = valid();
        r.output_hash = format!("0x{}", "a".repeat(62));
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputHash));
    }

    #[test]
    fn rejects_nonhex_hash_of_right_length() {
        let mut r = valid();
        r.output_hash = "g".repeat(64);
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputHash));
    }

    #[test]
    fn rejects_empty_url() {
        let mut r = valid();
        r.output_url = String::new();
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputUrl));
    }

    #[test]
    fn rejects_schemeless_url() {
        let mut r = valid();
        r.output_url = "not-a-url".into();
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputUrl));
    }

    #[test]
    fn rejects_scheme_only_url() {
        let mut r = valid();
        r.output_url = "https://".into();
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputUrl));
    }

    #[test]
    fn rejects_empty_scheme_url() {
        let mut r = valid();
        r.output_url = "://host".into();
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputUrl));
    }

    #[test]
    fn accepts_https_and_ipfs_urls() {
        for url in ["https://cdn.blackfield.games/x.usda", "ipfs://bafy123", "ar://tx"] {
            let mut r = valid();
            r.output_url = url.into();
            assert_eq!(validate_result(&r), Ok(()), "{url} should be fetchable");
        }
    }

    #[test]
    fn rejects_zero_render_seconds() {
        let mut r = valid();
        r.render_seconds = 0;
        assert_eq!(validate_result(&r), Err(ValidationError::ZeroRenderSeconds));
    }

    #[test]
    fn hash_is_checked_before_render_seconds() {
        // A result bad on multiple axes surfaces the hash error first, so the log
        // reason is deterministic regardless of how many fields are malformed.
        let mut r = valid();
        r.output_hash = "bad".into();
        r.render_seconds = 0;
        assert_eq!(validate_result(&r), Err(ValidationError::MalformedOutputHash));
    }
}
