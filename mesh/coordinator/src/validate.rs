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
    /// `render_seconds` claims more compute than the job's deadline plausibly
    /// allowed. An unbounded value (e.g. `u32::MAX`) would poison
    /// `/stats total_render_seconds` and over-credit any render-second-weighted
    /// payout, so a result claiming far more time than the job was budgeted is
    /// refused. See [`validate_render_seconds`].
    ImplausibleRenderSeconds,
}

impl ValidationError {
    /// Stable, low-cardinality reason string for the ws `Rejected` frame.
    pub fn reason(&self) -> &'static str {
        match self {
            ValidationError::MalformedOutputHash => "output_hash is not a 256-bit lowercase-hex digest",
            ValidationError::MalformedOutputUrl => "output_url is not a fetchable scheme://… url",
            ValidationError::ZeroRenderSeconds => "render_seconds is zero",
            ValidationError::ImplausibleRenderSeconds => {
                "render_seconds exceeds the job's deadline budget"
            }
        }
    }
}

/// Why a `POST /jobs` creation request failed the ingestion gate. The job-result
/// content gate ([`ValidationError`]) guards what an *earner* submits; this
/// guards what a *job creator* submits, before the spec is ever enqueued.
#[derive(Debug, PartialEq, Eq)]
pub enum JobSpecError {
    /// `deadline_secs` is zero. A zero deadline is the operator-only unbounded
    /// mode — the wall-clock TTL reaper exempts it (never reaped) and
    /// [`validate_render_seconds`] skips the plausibility bound for it — so an
    /// untrusted creator must not be able to mint one.
    ZeroDeadline,
    /// `deadline_secs` exceeds [`MAX_DEADLINE_SECS`]. Render jobs are seconds to
    /// minutes; an absurd deadline is rejected as hygiene.
    DeadlineTooLong,
    /// `max_payout_wei` does not parse as a `u128` (empty, non-numeric, negative,
    /// or above `u128::MAX`). The store sums it as `u128` for `/stats`, so an
    /// unparseable value enqueued here is a deferred 500 the moment the job
    /// completes.
    MalformedPayout,
    /// `max_payout_wei` parses but exceeds [`MAX_PAYOUT_WEI`]. Kept well under
    /// `u128::MAX` so the `/stats total_payout_wei` sum cannot overflow.
    PayoutTooLarge,
    /// The serialized `inputs` JSON exceeds [`MAX_INPUTS_BYTES`] — a blob that
    /// would bloat the SQLite row and amplify request memory.
    InputsTooLarge,
}

impl JobSpecError {
    /// Stable, low-cardinality reason string for the reject log.
    pub fn reason(&self) -> &'static str {
        match self {
            JobSpecError::ZeroDeadline => "deadline_secs is zero (the operator-only unbounded mode)",
            JobSpecError::DeadlineTooLong => "deadline_secs exceeds the maximum",
            JobSpecError::MalformedPayout => "max_payout_wei is not a non-negative integer (u128 wei)",
            JobSpecError::PayoutTooLarge => "max_payout_wei exceeds the maximum",
            JobSpecError::InputsTooLarge => "inputs exceeds the maximum serialized size",
        }
    }
}

/// Upper bound on a job's `deadline_secs` accepted at ingestion. 24h is generous
/// for any single render job (terrain/diffusion-tile/optimization run in seconds
/// to minutes), so an honest job always passes; `0` is rejected separately as
/// the operator-only unbounded escape hatch.
pub const MAX_DEADLINE_SECS: u32 = 86_400;

/// Upper bound on `max_payout_wei` (1e30 wei = 1e12 $BLCKFLD). Astronomically
/// above any plausible single-job payout, yet far below `u128::MAX` (~3.4e38), so
/// even an unbounded backlog of max-payout jobs cannot overflow the `u128`
/// `/stats total_payout_wei` sum.
pub const MAX_PAYOUT_WEI: u128 = 10u128.pow(30);

/// Upper bound on the serialized `inputs` JSON (16 KiB). Inputs are asset URLs +
/// hashes (small); the cap stops a multi-megabyte blob from bloating the stored
/// SQLite row. This bounds the *persisted* size; the transient request buffer is
/// bounded before parsing by the coordinator's pre-parse body-size cap
/// (`MAX_REQUEST_BODY_BYTES`).
pub const MAX_INPUTS_BYTES: usize = 16 * 1024;

/// Validate a job-creation request before it is enqueued. Pure function of the
/// caller-supplied fields — no store, no network — so it runs before any lock.
/// `kind` and `region` are type-checked by the JSON deserializer, so only the
/// free-form scalars need gating here. The job `id` is deliberately NOT a
/// parameter: the coordinator mints it, so a caller can never collide with (and
/// overwrite, via `enqueue`'s `ON CONFLICT(id) DO UPDATE`) an existing job.
pub fn validate_job_spec(
    deadline_secs: u32,
    max_payout_wei: &str,
    inputs: &serde_json::Value,
) -> Result<(), JobSpecError> {
    if deadline_secs == 0 {
        return Err(JobSpecError::ZeroDeadline);
    }
    if deadline_secs > MAX_DEADLINE_SECS {
        return Err(JobSpecError::DeadlineTooLong);
    }
    // Must parse as the same u128 the store sums for `/stats`; this rejects
    // empty, non-numeric, signed, and > u128::MAX before the value can be stored.
    let wei: u128 = max_payout_wei
        .parse()
        .map_err(|_| JobSpecError::MalformedPayout)?;
    if wei > MAX_PAYOUT_WEI {
        return Err(JobSpecError::PayoutTooLarge);
    }
    // A `Value` always serializes; treat the unreachable error as oversized
    // rather than unwrap so a pathological input fails closed.
    let inputs_len = serde_json::to_vec(inputs).map(|v| v.len()).unwrap_or(usize::MAX);
    if inputs_len > MAX_INPUTS_BYTES {
        return Err(JobSpecError::InputsTooLarge);
    }
    Ok(())
}

/// 32-byte digest rendered as lowercase hex.
const OUTPUT_HASH_LEN: usize = 64;

/// Slack multiplier on `deadline_secs` for the render-seconds plausibility
/// bound. A result may claim up to this many times the job's own deadline of
/// compute before it is refused. Deliberately generous: the bound exists to
/// reject self-evidently fabricated values (e.g. `u32::MAX`), not to enforce a
/// tight SLA, so an honest earner that finished slightly past the soft deadline
/// (before the reaper reclaimed the lease) still settles.
const RENDER_SECONDS_DEADLINE_SLACK: u64 = 2;

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

/// Bound `render_seconds` against the job's own `deadline_secs` — the lease
/// window the reaper enforces (`reap_expired`). A result claiming more than
/// `deadline_secs * RENDER_SECONDS_DEADLINE_SLACK` of compute is implausible: the
/// job would have been reclaimed long before the earner spent that much time, so
/// the value is fabricated and would poison metering and payout.
///
/// Needs the job's deadline, so — unlike [`validate_result`] — it runs where the
/// `JobSpec` is in hand: the ws path passes the offered job's deadline, the http
/// path looks it up under the store lock. `deadline_secs == 0` carries no compute
/// budget to bound against, so the upper bound is skipped (the zero-render-seconds
/// check in [`validate_result`] still applies); job creation is expected to set a
/// positive deadline, and the reaper treats a zero deadline as already expired.
pub fn validate_render_seconds(
    render_seconds: u32,
    deadline_secs: u32,
) -> Result<(), ValidationError> {
    if deadline_secs == 0 {
        return Ok(());
    }
    let max = (deadline_secs as u64).saturating_mul(RENDER_SECONDS_DEADLINE_SLACK);
    if render_seconds as u64 > max {
        return Err(ValidationError::ImplausibleRenderSeconds);
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

    #[test]
    fn render_seconds_at_the_deadline_is_plausible() {
        // The common honest case: a render that used about its whole budget.
        assert_eq!(validate_render_seconds(60, 60), Ok(()));
    }

    #[test]
    fn render_seconds_under_the_deadline_is_plausible() {
        assert_eq!(validate_render_seconds(5, 60), Ok(()));
    }

    #[test]
    fn render_seconds_at_the_slack_bound_is_plausible() {
        // deadline * SLACK (2) is the inclusive ceiling — an earner that finished
        // past the soft deadline but within the slack still settles.
        assert_eq!(validate_render_seconds(120, 60), Ok(()));
    }

    #[test]
    fn render_seconds_just_over_the_slack_bound_is_rejected() {
        // One second past deadline * SLACK is implausible.
        assert_eq!(
            validate_render_seconds(121, 60),
            Err(ValidationError::ImplausibleRenderSeconds)
        );
    }

    #[test]
    fn render_seconds_u32_max_is_rejected() {
        // The attack the bound exists for: a fabricated value that would poison
        // /stats and over-credit a render-second-weighted payout.
        assert_eq!(
            validate_render_seconds(u32::MAX, 60),
            Err(ValidationError::ImplausibleRenderSeconds)
        );
    }

    #[test]
    fn zero_deadline_skips_the_upper_bound() {
        // No compute budget to bound against → the upper bound is skipped; even a
        // huge value passes here (the zero-render-seconds check lives in
        // validate_result, and the reaper reclaims a zero-deadline job at once).
        assert_eq!(validate_render_seconds(u32::MAX, 0), Ok(()));
    }

    #[test]
    fn large_deadline_does_not_overflow_the_bound() {
        // deadline * SLACK is computed in u64, so a large deadline can't wrap and
        // spuriously reject a plausible render.
        assert_eq!(validate_render_seconds(u32::MAX, u32::MAX), Ok(()));
    }

    // -- job-creation ingestion gate (validate_job_spec) -------------------

    fn ok_inputs() -> serde_json::Value {
        serde_json::json!({ "asset_url": "https://cdn.example.com/tile.usd", "lod": 2 })
    }

    #[test]
    fn accepts_a_wellformed_job_spec() {
        assert_eq!(
            validate_job_spec(60, "1000000000000000000", &ok_inputs()),
            Ok(())
        );
    }

    #[test]
    fn rejects_zero_deadline() {
        // The operator-only unbounded escape hatch must never come from a caller.
        assert_eq!(
            validate_job_spec(0, "1000000000000000000", &ok_inputs()),
            Err(JobSpecError::ZeroDeadline)
        );
    }

    #[test]
    fn accepts_deadline_at_the_max() {
        assert_eq!(
            validate_job_spec(MAX_DEADLINE_SECS, "0", &ok_inputs()),
            Ok(())
        );
    }

    #[test]
    fn rejects_deadline_over_the_max() {
        assert_eq!(
            validate_job_spec(MAX_DEADLINE_SECS + 1, "0", &ok_inputs()),
            Err(JobSpecError::DeadlineTooLong)
        );
    }

    #[test]
    fn rejects_empty_payout() {
        assert_eq!(
            validate_job_spec(60, "", &ok_inputs()),
            Err(JobSpecError::MalformedPayout)
        );
    }

    #[test]
    fn rejects_nonnumeric_payout() {
        assert_eq!(
            validate_job_spec(60, "1e18", &ok_inputs()),
            Err(JobSpecError::MalformedPayout)
        );
    }

    #[test]
    fn rejects_negative_payout() {
        assert_eq!(
            validate_job_spec(60, "-1", &ok_inputs()),
            Err(JobSpecError::MalformedPayout)
        );
    }

    #[test]
    fn rejects_payout_above_u128_max() {
        // u128::MAX is ~3.4e38; one more digit does not parse as u128 — the exact
        // value the store's total_payout_wei sum would choke on if it were stored.
        let over_u128 = "1000000000000000000000000000000000000000"; // 1e39
        assert_eq!(
            validate_job_spec(60, over_u128, &ok_inputs()),
            Err(JobSpecError::MalformedPayout)
        );
    }

    #[test]
    fn rejects_payout_over_the_policy_cap() {
        // Parses as u128 but exceeds MAX_PAYOUT_WEI — rejected before it can be
        // summed, so the /stats u128 total can never approach overflow.
        let over_cap = (MAX_PAYOUT_WEI + 1).to_string();
        assert_eq!(
            validate_job_spec(60, &over_cap, &ok_inputs()),
            Err(JobSpecError::PayoutTooLarge)
        );
    }

    #[test]
    fn accepts_payout_at_the_policy_cap() {
        let at_cap = MAX_PAYOUT_WEI.to_string();
        assert_eq!(validate_job_spec(60, &at_cap, &ok_inputs()), Ok(()));
    }

    #[test]
    fn rejects_oversized_inputs() {
        // A blob past the cap is refused so it can't bloat the SQLite row.
        let big = serde_json::json!({ "blob": "x".repeat(MAX_INPUTS_BYTES) });
        assert_eq!(
            validate_job_spec(60, "0", &big),
            Err(JobSpecError::InputsTooLarge)
        );
    }

    #[test]
    fn accepts_inputs_at_the_cap() {
        // Serialized length exactly at the cap is allowed (inclusive bound).
        // {"blob":"x..x"} has 11 bytes of framing around the padded string.
        let pad = MAX_INPUTS_BYTES - 11;
        let at_cap = serde_json::json!({ "blob": "x".repeat(pad) });
        assert_eq!(serde_json::to_vec(&at_cap).unwrap().len(), MAX_INPUTS_BYTES);
        assert_eq!(validate_job_spec(60, "0", &at_cap), Ok(()));
    }
}
