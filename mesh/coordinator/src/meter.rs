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
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

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

// ---- on-chain spend transport (the metering twin of `crate::relay`) ----
//
// The drain loop (`drain_debits` in `main.rs`) reads pending debit rows and
// submits each as a `ComputeMeter.spend(buyer, amount, jobId)` call on Base,
// returning the spend tx hash. This is the transport boundary: the [`Spender`]
// trait plus a [`MockSpender`] for tests and the `--spender-dev-mock` local path.
// Kept in `meter.rs` (rather than a separate module like `relay.rs`) so the small
// metering concern stays in one file. The live Base impl (an RPC provider + an
// authorized spender key with gas) is operator-gated and not built here.
//
// IDEMPOTENCY — verified against `contracts/src/ComputeMeter.sol`: unlike
// `RenderReceipts.issueReceipt` (which has a per-job `DuplicateReceipt` fence),
// `spend` is NOT idempotent — it debits `credit[buyer]` on every call and only
// emits `Spent(jobId)`; it tracks no spent jobIds. So a crash between an on-chain
// `spend` and the local `mark_debit_submitted` would re-claim the row and
// DOUBLE-DEBIT the buyer on restart. [`SpendError::AlreadySpent`] is the seam for
// an on-chain jobId fence (refiled `contracts-computemeter-spend-idempotency`):
// the live transport is safe to wire ONLY once that fence exists, so its go-live
// is gated on BOTH credentials AND the fence (see BLOCKERS). `MockSpender` models
// the post-fence contract.

/// Why an on-chain `spend` failed — the drain loop reacts differently to each.
#[derive(Debug)]
pub enum SpendError {
    /// This `jobId` is already debited on-chain — an *idempotent success*: a crash
    /// between a prior `spend` and its local mark left the row pending, and the
    /// re-submit hit the contract's (refiled) per-job fence. The drain marks the
    /// row and moves on rather than double-debiting. REQUIRES the on-chain jobId
    /// fence to be real (see the module note above).
    AlreadySpent,
    /// The spender key is not in `ComputeMeter.authorizedSpenders` (the contract
    /// reverts `NotAuthorized`). Every spend reverts until the owner calls
    /// `setSpender`, so this is a loud, distinct operator-config error — NOT a
    /// transient retry that masquerades as progress. The drain stops; the backlog
    /// (visible at `/stats pending_debits`) stays put until authorization lands.
    NotAuthorized,
    /// A transient fault (RPC timeout, nonce contention, a reorg). The debit is not
    /// on-chain; the row stays pending and is retried on the next drain tick.
    Transient(String),
    /// A non-retryable fault that is NOT a global config problem — e.g. the
    /// contract reverts `InsufficientCredit` for an underfunded buyer. Retrying in
    /// a hot loop or dropping the debit are both wrong, so the drain stops and
    /// surfaces it; the row stays pending.
    Permanent(String),
}

/// Submits a pending debit as a `ComputeMeter.spend(buyer, amount, jobId)` call,
/// returning the spend tx hash on success. The future is `Send` so the drain loop
/// can run on the multi-threaded runtime; implementors must not block.
pub trait Spender: Send + Sync {
    fn spend(
        &self,
        debit: &PendingDebit,
    ) -> impl Future<Output = Result<String, SpendError>> + Send;
}

/// In-process [`Spender`] for tests and the `--spender-dev-mock` local path. Never
/// touches a chain: it replays a scripted outcome and records every call so the
/// drain loop can be exercised deterministically — success, transient-then-ok,
/// permanent, not-authorized, or already-spent — without RPC or funds.
pub struct MockSpender {
    inner: Mutex<MockSpenderInner>,
    /// Test-only gate: when set, `spend` signals `started` then awaits `release`
    /// before resolving, so a test can hold a spend in-flight and assert the drain
    /// keeps the store lock free during it. `None` (always) on the dev path.
    started: Option<Arc<Notify>>,
    release: Option<Arc<Notify>>,
}

struct MockSpenderInner {
    /// Fail this many calls with `Transient` before succeeding.
    transient_remaining: usize,
    /// Always fail with `Permanent`.
    permanent: bool,
    /// Always fail with `NotAuthorized` (models an unauthorized spender key).
    not_authorized: bool,
    /// Always fail with `AlreadySpent` (models a debit already on-chain).
    already_spent: bool,
    /// Total `spend` calls (successful or not).
    calls: usize,
    /// Every debit freshly spent (excludes `AlreadySpent`), in submit order — the
    /// full row so a test can assert the amount/buyer submitted are exactly what
    /// was persisted at settle.
    spent: Vec<PendingDebit>,
}

impl MockSpender {
    /// Always succeeds, returning a deterministic mock tx hash. Used by `main`'s
    /// `--spender-dev-mock` path and by the happy-path tests.
    pub fn succeeding() -> Self {
        Self {
            inner: Mutex::new(MockSpenderInner {
                transient_remaining: 0,
                permanent: false,
                not_authorized: false,
                already_spent: false,
                calls: 0,
                spent: Vec::new(),
            }),
            started: None,
            release: None,
        }
    }

    /// Fail the first `n` spends with `Transient`, then succeed.
    #[cfg(test)]
    pub fn transient_then_ok(n: usize) -> Self {
        let mut m = Self::succeeding();
        m.inner.get_mut().unwrap().transient_remaining = n;
        m
    }

    /// Always fail with `Permanent` (e.g. an underfunded buyer's `InsufficientCredit`).
    #[cfg(test)]
    pub fn permanent() -> Self {
        let mut m = Self::succeeding();
        m.inner.get_mut().unwrap().permanent = true;
        m
    }

    /// Always fail with `NotAuthorized` (the spender key isn't authorized on-chain).
    #[cfg(test)]
    pub fn not_authorized() -> Self {
        let mut m = Self::succeeding();
        m.inner.get_mut().unwrap().not_authorized = true;
        m
    }

    /// Always fail with `AlreadySpent` (the debit is already on-chain).
    #[cfg(test)]
    pub fn already_spent() -> Self {
        let mut m = Self::succeeding();
        m.inner.get_mut().unwrap().already_spent = true;
        m
    }

    /// A succeeding spender that signals `started` then awaits `release` inside
    /// `spend`, so a test can observe the in-flight window and assert the store
    /// lock is free during it.
    #[cfg(test)]
    pub fn gated(started: Arc<Notify>, release: Arc<Notify>) -> Self {
        let mut m = Self::succeeding();
        m.started = Some(started);
        m.release = Some(release);
        m
    }

    /// Total `spend` calls so far.
    #[cfg(test)]
    pub fn calls(&self) -> usize {
        self.inner.lock().unwrap().calls
    }

    /// `job_id`s freshly spent (excludes `AlreadySpent`, which spends nothing new).
    #[cfg(test)]
    pub fn spent(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .spent
            .iter()
            .map(|d| d.job_id.clone())
            .collect()
    }

    /// The full debits freshly spent — lets a test assert the amount + buyer
    /// submitted are EXACTLY what was persisted at settle (never re-derived).
    #[cfg(test)]
    pub fn spent_debits(&self) -> Vec<PendingDebit> {
        self.inner.lock().unwrap().spent.clone()
    }
}

/// Deterministic mock tx hash for a given debit, so a test can assert the drain
/// stored exactly what the spender returned.
fn mock_tx_hash(debit: &PendingDebit) -> String {
    format!("0xspend-{}", debit.job_id)
}

impl Spender for MockSpender {
    async fn spend(&self, debit: &PendingDebit) -> Result<String, SpendError> {
        if let Some(s) = &self.started {
            s.notify_one();
        }
        if let Some(r) = &self.release {
            r.notified().await;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.calls += 1;
        if inner.permanent {
            return Err(SpendError::Permanent("mock permanent".into()));
        }
        if inner.not_authorized {
            return Err(SpendError::NotAuthorized);
        }
        if inner.already_spent {
            return Err(SpendError::AlreadySpent);
        }
        if inner.transient_remaining > 0 {
            inner.transient_remaining -= 1;
            return Err(SpendError::Transient("mock transient".into()));
        }
        inner.spent.push(debit.clone());
        Ok(mock_tx_hash(debit))
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

    fn debit() -> PendingDebit {
        PendingDebit::build(Some(BUYER), &result(3), 1_000).expect("buyer + nonzero amount")
    }

    #[tokio::test]
    async fn spend_succeeding_returns_tx_hash_and_records_the_call() {
        let s = MockSpender::succeeding();
        let d = debit();
        let tx = s.spend(&d).await.expect("succeeds");
        assert_eq!(tx, mock_tx_hash(&d));
        assert_eq!(s.calls(), 1);
        assert_eq!(s.spent(), vec![d.job_id]);
    }

    #[tokio::test]
    async fn spend_transient_then_ok_fails_then_succeeds() {
        let s = MockSpender::transient_then_ok(2);
        let d = debit();
        assert!(matches!(s.spend(&d).await, Err(SpendError::Transient(_))));
        assert!(matches!(s.spend(&d).await, Err(SpendError::Transient(_))));
        assert!(s.spend(&d).await.is_ok());
        assert_eq!(s.calls(), 3);
        // Only the successful call recorded a spend.
        assert_eq!(s.spent(), vec![d.job_id]);
    }

    #[tokio::test]
    async fn spend_permanent_never_spends() {
        let s = MockSpender::permanent();
        assert!(matches!(s.spend(&debit()).await, Err(SpendError::Permanent(_))));
        assert!(s.spent().is_empty());
    }

    #[tokio::test]
    async fn spend_not_authorized_never_spends() {
        let s = MockSpender::not_authorized();
        assert!(matches!(
            s.spend(&debit()).await,
            Err(SpendError::NotAuthorized)
        ));
        assert_eq!(s.calls(), 1);
        assert!(s.spent().is_empty());
    }

    #[tokio::test]
    async fn spend_already_spent_never_spends() {
        let s = MockSpender::already_spent();
        assert!(matches!(s.spend(&debit()).await, Err(SpendError::AlreadySpent)));
        assert_eq!(s.calls(), 1);
        assert!(s.spent().is_empty());
    }
}
