//! ComputeMeter debit mapping — the metering twin of [`crate::eas`].
//!
//! When a result is validated and settled, the coordinator records a *pending
//! debit* — the off-chain twin of a `ComputeMeter.spend` — so a crash between
//! the settle and the on-chain debit cannot lose a buyer's charge. This module
//! maps the mesh's proto types onto the contract's `spend` arguments:
//!
//! ```text
//! address buyer, uint256 amount, bytes32 jobId
//! ```
//!
//! The relayer (operator-gated, not built here) reads a pending row and calls
//! `spend(buyer, amount, jobId)`. `amount` is `rate * renderSeconds` in wei,
//! computed with saturating arithmetic so an extreme rate can never wrap or
//! panic the settle path. `jobId` reuses [`crate::eas::job_id_hex`] so the debit
//! and the attestation address the same job under one `bytes32` layout.

use crate::eas::job_id_hex;
use proto::JobResult;

/// A settled render's pending ComputeMeter debit: the `spend` arguments in the
/// contract's types. `buyer` is the 0x-prefixed EVM address charged (validated
/// at ingestion); `amount_wei` is a decimal wei string (like `max_payout_wei`,
/// so a 1e18-scale value never overflows JSON's safe-integer range); `job_id` is
/// the 64-char lowercase-hex `bytes32` (no `0x`), shared with the attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDebit {
    pub buyer: String,
    pub amount_wei: String,
    pub job_id: String,
}

impl PendingDebit {
    /// Build the pending debit for a settled job, or `None` to SKIP it (never an
    /// error, never a panic — a skipped debit must never block the settle):
    ///
    /// * `buyer` absent → the job was ingested unattributed, so there is nobody
    ///   to charge (mirrors the unknown-region render-fee skip).
    /// * `amount == 0` → either metering is disabled (`rate == 0`, the opt-in
    ///   default) or the job recorded zero render-seconds; a zero `spend` is a
    ///   no-op debit, so skip it rather than write a meaningless row.
    ///
    /// `amount = rate * render_seconds` is computed with `saturating_mul`, so an
    /// extreme rate or render-seconds saturates at `u128::MAX` rather than
    /// wrapping or panicking; it is then stored as a decimal string. The `buyer`
    /// is taken as-is — the ingestion gate already proved it a well-formed EVM
    /// address (unlike `output_hash` in [`crate::eas`], which is earner-supplied
    /// and re-checked there), so re-validating here would be redundant.
    pub fn build(
        buyer: Option<&str>,
        result: &JobResult,
        rate_wei_per_render_second: u128,
    ) -> Option<Self> {
        let buyer = buyer?;
        let amount = rate_wei_per_render_second.saturating_mul(u128::from(result.render_seconds));
        if amount == 0 {
            return None;
        }
        Some(Self {
            buyer: buyer.to_string(),
            amount_wei: amount.to_string(),
            job_id: job_id_hex(&result.job_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn result(render_seconds: u32) -> JobResult {
        JobResult {
            job_id: Uuid::new_v4(),
            earner_address: "0x00000000000000000000000000000000000000a1".into(),
            output_hash: "a".repeat(64),
            output_url: "memory://x".into(),
            render_seconds,
            signature_hex: String::new(),
        }
    }

    const BUYER: &str = "0x00000000000000000000000000000000000000b1";

    #[test]
    fn build_derives_amount_from_rate_times_render_seconds() {
        let r = result(7);
        let d = PendingDebit::build(Some(BUYER), &r, 1_000).expect("buyer + nonzero amount builds");
        assert_eq!(d.buyer, BUYER);
        assert_eq!(d.amount_wei, "7000"); // 1_000 * 7
        assert_eq!(d.job_id, job_id_hex(&r.job_id));
    }

    #[test]
    fn build_saturates_instead_of_overflowing() {
        // u128::MAX * 2 would wrap (or panic in debug); saturating_mul pins it at
        // u128::MAX so an extreme rate can never crash the settle path.
        let d = PendingDebit::build(Some(BUYER), &result(2), u128::MAX).expect("saturated builds");
        assert_eq!(d.amount_wei, u128::MAX.to_string());
    }

    #[test]
    fn build_skips_when_buyer_absent() {
        // No buyer → nobody to charge → skip (not an error).
        assert!(PendingDebit::build(None, &result(7), 1_000).is_none());
    }

    #[test]
    fn build_skips_when_rate_is_zero() {
        // rate 0 = metering disabled (the opt-in default): no debit, even with a buyer.
        assert!(PendingDebit::build(Some(BUYER), &result(7), 0).is_none());
    }

    #[test]
    fn build_skips_when_render_seconds_is_zero() {
        // A zero-second job owes nothing — a zero spend is a no-op, so skip the row.
        assert!(PendingDebit::build(Some(BUYER), &result(0), 1_000).is_none());
    }
}
