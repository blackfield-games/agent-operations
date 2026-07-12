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
// IDEMPOTENCY — `ComputeMeter.spend` is NOT idempotent (it debits `credit[buyer]`
// on every call, tracking no spent jobIds — ArtifactTemplate relies on this for its
// per-mint fee), so the live transport MUST target `ComputeMeter.spendOnce`: the
// idempotent entry point (built in `contracts-computemeter-spend-idempotency`) that
// fences each `jobId` and reverts `AlreadySpent` on a crash-retried debit. The live
// `Spender` impl maps that revert to [`SpendError::AlreadySpent`], so a crash
// between an on-chain `spendOnce` and the local `mark_debit_submitted` is recovered
// (the re-submit reverts, the row is marked) instead of double-debiting. With the
// fence built, the live transport's go-live is now gated only on credentials (a
// Base RPC + an authorized spender key; see BLOCKERS). `MockSpender` models
// `spendOnce`.

/// Why an on-chain `spend` failed — the drain loop reacts differently to each.
#[derive(Debug)]
pub enum SpendError {
    /// This `jobId` is already debited on-chain — an *idempotent success*: a crash
    /// between a prior `spendOnce` and its local mark left the row pending, and the
    /// re-submit hit `ComputeMeter.spendOnce`'s per-job fence. The drain marks the
    /// row and moves on rather than double-debiting (see the module note above).
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
    /// a hot loop or dropping the debit are both wrong, so the drain DEAD-LETTERS
    /// this one row (quarantines it — retained + auditable, surfaced at
    /// `/stats dead_lettered_debits`) and CONTINUES, so one poison debit never blocks
    /// the rest of the backlog. Unlike `NotAuthorized`, the fault is per-row, not global.
    Permanent(String),
}

/// Marker `tx_hash` recorded when the contract reports a debit is already spent
/// (`ComputeMeter.spendOnce`'s per-`jobId` fence): the debit landed on-chain but the
/// relay didn't capture its real tx hash (a crash recovered between a prior
/// `spendOnce` and its local mark). The row is settled — it just carries this
/// sentinel instead of a spend tx. The metering twin of
/// [`crate::relay::ALREADY_ISSUED_UID`].
pub const ALREADY_SPENT_TX: &str = "already-spent";

/// Why a *batch* debit (`ComputeMeter.spendOnceBatch`) failed — the drain reacts to
/// each differently, mirroring the single-spend reactions but at batch granularity.
/// The metering twin of [`crate::relay::BatchRelayError`].
#[derive(Debug)]
pub enum BatchSpendError {
    /// The batch reverted atomically on-chain — `spendOnceBatch` is all-or-nothing,
    /// so ONE element's `AlreadySpent` fence or an underfunded buyer's
    /// `InsufficientCredit` rolls the whole batch back, debiting nothing. The revert
    /// names no single offender, so the drain falls back to per-debit
    /// [`Spender::spend`] to isolate it: the already-spent/underfunded element
    /// self-identifies (marked or dead-lettered) while the rest still drain.
    Reverted(String),
    /// A transient transport fault for the whole batch (RPC timeout, nonce
    /// contention, a reorg). Nothing landed; the drain backs off to the next tick
    /// with every row still pending — single spends would hit the same fault, so the
    /// fallback is skipped.
    Transient(String),
    /// A non-retryable fault for the whole batch (e.g. the spender key is not in
    /// `ComputeMeter.authorizedSpenders`, so `spendOnceBatch` reverts `NotAuthorized`
    /// before any element). The drain stops loudly; every row stays pending until the
    /// owner calls `setSpender`. The single-spend twin of this global halt is
    /// [`SpendError::NotAuthorized`]; a per-row single-spend fault is
    /// [`SpendError::Permanent`] (dead-letter).
    Permanent(String),
}

/// Submits a pending debit as a `ComputeMeter.spend(buyer, amount, jobId)` call,
/// returning the spend tx hash on success. [`spend`](Spender::spend) is one
/// `spendOnce`; [`spend_batch`](Spender::spend_batch) is one
/// `spendOnceBatch(DebitRequest[])`. The futures are `Send` so the drain loop can
/// run on the multi-threaded runtime; implementors must not block.
pub trait Spender: Send + Sync {
    fn spend(
        &self,
        debit: &PendingDebit,
    ) -> impl Future<Output = Result<String, SpendError>> + Send;

    /// Submit a whole chunk as ONE `spendOnceBatch` call, returning one settle tx
    /// hash PER element in submission order — `hashes[i]` is `debits[i]`'s, so the
    /// caller marks each row by a positional zip. Atomic on-chain: any element
    /// reverting rolls the whole batch back ([`BatchSpendError::Reverted`]).
    ///
    /// Unlike the attestation twin's `multiAttest` (N distinct UIDs), a native
    /// `spendOnceBatch` is ONE transaction — so a live impl returns that single tx
    /// hash repeated for every element (each debit settled in that one tx). The
    /// default here submits each element sequentially via [`spend`](Spender::spend) —
    /// correct, just un-amortized (N separate txs, so N distinct hashes) — so a
    /// transport that can't batch works unchanged; the live Base impl overrides it
    /// with a real `spendOnceBatch`. An already-spent element resolves to the
    /// [`ALREADY_SPENT_TX`] sentinel (an idempotent success), and the first
    /// `Transient`/`Permanent` short-circuits to the matching batch error.
    fn spend_batch(
        &self,
        debits: &[PendingDebit],
    ) -> impl Future<Output = Result<Vec<String>, BatchSpendError>> + Send {
        async move {
            let mut hashes = Vec::with_capacity(debits.len());
            for debit in debits {
                match self.spend(debit).await {
                    Ok(tx) => hashes.push(tx),
                    Err(SpendError::AlreadySpent) => hashes.push(ALREADY_SPENT_TX.to_string()),
                    Err(SpendError::NotAuthorized) => {
                        // A global config fault (unauthorized spender key) — halt the
                        // whole batch loudly, same as a native `spendOnceBatch`'s
                        // auth-before-any-element revert; never a per-element
                        // dead-letter.
                        return Err(BatchSpendError::Permanent(
                            "spender key not authorized on ComputeMeter".into(),
                        ));
                    }
                    Err(SpendError::Transient(m)) => return Err(BatchSpendError::Transient(m)),
                    Err(SpendError::Permanent(m)) => return Err(BatchSpendError::Permanent(m)),
                }
            }
            Ok(hashes)
        }
    }
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
    /// Fail with `Permanent` ONLY for this specific `job_id` (others succeed) —
    /// models one poison debit among many, so a test can pin that the drain
    /// dead-letters it and still drains the rest.
    permanent_for: Option<String>,
    /// Always fail with `NotAuthorized` (models an unauthorized spender key).
    not_authorized: bool,
    /// Always fail with `AlreadySpent` (models a debit already on-chain).
    already_spent: bool,
    /// Fail `spend_batch` with `Reverted` regardless of the element flags (models the
    /// contract's atomic batch revert — one bad/duplicate element rolls the whole
    /// call back).
    batch_reverts: bool,
    /// Fail `spend_batch` with `Transient` (a whole-batch transport fault).
    batch_transient: bool,
    /// Fail `spend_batch` with `Permanent` (a whole-batch non-retryable fault).
    batch_permanent: bool,
    /// Total `spend` calls (successful or not).
    calls: usize,
    /// Total `spend_batch` calls — distinct from `calls`, so a test proves the happy
    /// path does ONE `spendOnceBatch`, not N single spends.
    batch_calls: usize,
    /// Every debit freshly spent (excludes `AlreadySpent`), in submit order — the
    /// full row so a test can assert the amount/buyer submitted are exactly what
    /// was persisted at settle.
    spent: Vec<PendingDebit>,
    /// Every debit freshly spent through a SUCCESSFUL `spend_batch`, in submission
    /// order — the batch twin of `spent`, so a test proves the happy path settled
    /// through one `spendOnceBatch` and asserts the exact rows submitted.
    batch_spent: Vec<PendingDebit>,
}

impl MockSpender {
    /// Always succeeds, returning a deterministic mock tx hash. Used by `main`'s
    /// `--spender-dev-mock` path and by the happy-path tests.
    pub fn succeeding() -> Self {
        Self {
            inner: Mutex::new(MockSpenderInner {
                transient_remaining: 0,
                permanent: false,
                permanent_for: None,
                not_authorized: false,
                already_spent: false,
                batch_reverts: false,
                batch_transient: false,
                batch_permanent: false,
                calls: 0,
                batch_calls: 0,
                spent: Vec::new(),
                batch_spent: Vec::new(),
            }),
            started: None,
            release: None,
        }
    }

    /// Fail with `Permanent` only for `job_id` (every other debit succeeds) — a
    /// single poison row, so a test can drain a backlog where one debit dead-letters
    /// and the rest still settle in the same pass.
    #[cfg(test)]
    pub fn permanent_for(job_id: impl Into<String>) -> Self {
        let mut m = Self::succeeding();
        m.inner.get_mut().unwrap().permanent_for = Some(job_id.into());
        m
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

    /// Make `spend_batch` revert atomically (the contract's all-or-nothing batch
    /// fence) regardless of the element flags — the explicit twin of the batch
    /// revert `permanent()`/`already_spent()` derive, for a test that pins the
    /// per-debit fallback path directly.
    #[cfg(test)]
    pub fn with_batch_reverts(mut self) -> Self {
        self.inner.get_mut().unwrap().batch_reverts = true;
        self
    }

    /// Make `spend_batch` fail `Transient` (a whole-batch transport fault).
    #[cfg(test)]
    pub fn with_batch_transient(mut self) -> Self {
        self.inner.get_mut().unwrap().batch_transient = true;
        self
    }

    /// Make `spend_batch` fail `Permanent` (a whole-batch non-retryable fault).
    #[cfg(test)]
    pub fn with_batch_permanent(mut self) -> Self {
        self.inner.get_mut().unwrap().batch_permanent = true;
        self
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

    /// Total `spend_batch` calls so far (the `spendOnceBatch` count).
    #[cfg(test)]
    pub fn batch_calls(&self) -> usize {
        self.inner.lock().unwrap().batch_calls
    }

    /// `job_id`s settled through a successful `spend_batch`, in submission order.
    #[cfg(test)]
    pub fn batch_spent(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .batch_spent
            .iter()
            .map(|d| d.job_id.clone())
            .collect()
    }

    /// The full debits settled through a successful `spend_batch` — lets a test assert
    /// the amount + buyer batched are EXACTLY what was persisted at settle (never
    /// re-derived).
    #[cfg(test)]
    pub fn batch_spent_debits(&self) -> Vec<PendingDebit> {
        self.inner.lock().unwrap().batch_spent.clone()
    }
}

/// Deterministic mock tx hash for a given debit, so a test can assert the drain
/// stored exactly what the spender returned.
fn mock_tx_hash(debit: &PendingDebit) -> String {
    format!("0xspend-{}", debit.job_id)
}

/// Deterministic mock tx hash for a batched element — a distinct prefix from
/// [`mock_tx_hash`] so a test can tell a batch settle from a single spend and prove
/// the happy path went through one `spendOnceBatch`.
fn batch_tx_hash(debit: &PendingDebit) -> String {
    format!("0xspendbatch-{}", debit.job_id)
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
        if inner.permanent_for.as_deref() == Some(debit.job_id.as_str()) {
            return Err(SpendError::Permanent("mock permanent (poison row)".into()));
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

    async fn spend_batch(
        &self,
        debits: &[PendingDebit],
    ) -> Result<Vec<String>, BatchSpendError> {
        if let Some(s) = &self.started {
            s.notify_one();
        }
        if let Some(r) = &self.release {
            r.notified().await;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.batch_calls += 1;
        // Explicit whole-batch faults first (for a test that pins a batch outcome
        // independent of the element flags).
        if inner.batch_permanent {
            return Err(BatchSpendError::Permanent("mock batch permanent".into()));
        }
        if inner.batch_transient {
            return Err(BatchSpendError::Transient("mock batch transient".into()));
        }
        if inner.batch_reverts {
            return Err(BatchSpendError::Reverted("mock batch revert".into()));
        }
        // Otherwise DERIVE the batch outcome from the element flags, on-chain honest:
        // `spendOnceBatch` checks authorization before any element (so an
        // unauthorized key is a whole-batch global halt = Permanent), and is
        // all-or-nothing (so a batch CONTAINING an underfunded/duplicate/already-spent
        // element reverts atomically = Reverted → the drain's per-debit fallback
        // isolates it). This lets a `permanent()`/`already_spent()`/`not_authorized()`
        // spender drive the batch drain exactly as it drove the single drain, so the
        // dead-letter/idempotency scenarios read identically.
        if inner.not_authorized {
            return Err(BatchSpendError::Permanent(
                "spender key not authorized on ComputeMeter".into(),
            ));
        }
        let has_poison = inner.permanent
            || inner.already_spent
            || debits
                .iter()
                .any(|d| inner.permanent_for.as_deref() == Some(d.job_id.as_str()));
        if has_poison {
            return Err(BatchSpendError::Reverted("mock batch element reverted".into()));
        }
        if inner.transient_remaining > 0 {
            inner.transient_remaining -= 1;
            return Err(BatchSpendError::Transient("mock batch transient".into()));
        }
        let hashes: Vec<String> = debits.iter().map(batch_tx_hash).collect();
        for d in debits {
            inner.batch_spent.push(d.clone());
        }
        Ok(hashes)
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

    fn debits(tags: &[&str]) -> Vec<PendingDebit> {
        tags.iter()
            .map(|t| PendingDebit {
                buyer: BUYER.into(),
                amount_wei: "1000".into(),
                job_id: (*t).into(),
            })
            .collect()
    }

    #[tokio::test]
    async fn spend_batch_settles_every_element_via_one_call() {
        let s = MockSpender::succeeding();
        let batch = debits(&["a", "b", "c"]);
        let hashes = s.spend_batch(&batch).await.expect("batch succeeds");
        assert_eq!(hashes, batch.iter().map(batch_tx_hash).collect::<Vec<_>>());
        assert_eq!(s.batch_calls(), 1, "one spendOnceBatch");
        assert_eq!(s.calls(), 0, "no single spends on the batch path");
        assert_eq!(s.batch_spent(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn spend_batch_reverts_when_scripted() {
        let s = MockSpender::succeeding().with_batch_reverts();
        assert!(matches!(
            s.spend_batch(&debits(&["a", "b"])).await,
            Err(BatchSpendError::Reverted(_))
        ));
        assert_eq!(s.batch_calls(), 1);
        assert!(s.batch_spent().is_empty(), "a revert records nothing");
    }

    #[tokio::test]
    async fn spend_batch_transient_and_permanent_when_scripted() {
        let t = MockSpender::succeeding().with_batch_transient();
        assert!(matches!(
            t.spend_batch(&debits(&["a"])).await,
            Err(BatchSpendError::Transient(_))
        ));
        let p = MockSpender::succeeding().with_batch_permanent();
        assert!(matches!(
            p.spend_batch(&debits(&["a"])).await,
            Err(BatchSpendError::Permanent(_))
        ));
    }

    /// A batch that CONTAINS a permanently-failing (e.g. underfunded) element reverts
    /// atomically — `spendOnceBatch` is all-or-nothing — so a `permanent()` spender
    /// drives the batch to `Reverted` (the drain then isolates the offender per-row).
    #[tokio::test]
    async fn spend_batch_reverts_when_an_element_is_permanent() {
        let s = MockSpender::permanent();
        assert!(matches!(
            s.spend_batch(&debits(&["a", "b"])).await,
            Err(BatchSpendError::Reverted(_))
        ));
        assert!(s.batch_spent().is_empty());
    }

    /// An unauthorized spender reverts the batch at the auth check BEFORE any element,
    /// so it is a whole-batch global halt (`Permanent`), never a per-element revert to
    /// dead-letter one row over.
    #[tokio::test]
    async fn spend_batch_halts_permanent_when_not_authorized() {
        let s = MockSpender::not_authorized();
        assert!(matches!(
            s.spend_batch(&debits(&["a", "b"])).await,
            Err(BatchSpendError::Permanent(_))
        ));
    }

    /// An already-spent element trips `spendOnceBatch`'s per-`jobId` fence and reverts
    /// the whole batch, so the drain's fallback re-submits singly and marks the
    /// idempotent success.
    #[tokio::test]
    async fn spend_batch_reverts_when_an_element_is_already_spent() {
        let s = MockSpender::already_spent();
        assert!(matches!(
            s.spend_batch(&debits(&["a"])).await,
            Err(BatchSpendError::Reverted(_))
        ));
    }

    /// A `Spender` that implements only `spend` gets `spend_batch` from the trait
    /// default — sequential single spends, no amortization (N distinct tx hashes).
    enum SeqMode {
        Ok,
        AlreadySpent,
        Transient,
        NotAuthorized,
    }
    struct SeqSpender {
        mode: SeqMode,
        calls: Mutex<usize>,
    }
    impl Spender for SeqSpender {
        async fn spend(&self, debit: &PendingDebit) -> Result<String, SpendError> {
            *self.calls.lock().unwrap() += 1;
            match self.mode {
                SeqMode::Ok => Ok(format!("0xseq-{}", debit.job_id)),
                SeqMode::AlreadySpent => Err(SpendError::AlreadySpent),
                SeqMode::Transient => Err(SpendError::Transient("seq transient".into())),
                SeqMode::NotAuthorized => Err(SpendError::NotAuthorized),
            }
        }
    }

    #[tokio::test]
    async fn default_spend_batch_does_sequential_single_spends() {
        let s = SeqSpender {
            mode: SeqMode::Ok,
            calls: Mutex::new(0),
        };
        let batch = debits(&["a", "b", "c"]);
        let hashes = s.spend_batch(&batch).await.expect("succeeds");
        assert_eq!(hashes, vec!["0xseq-a", "0xseq-b", "0xseq-c"]);
        assert_eq!(*s.calls.lock().unwrap(), 3, "one spend per element");
    }

    #[tokio::test]
    async fn default_spend_batch_maps_already_spent_to_the_sentinel() {
        let s = SeqSpender {
            mode: SeqMode::AlreadySpent,
            calls: Mutex::new(0),
        };
        let hashes = s.spend_batch(&debits(&["a"])).await.expect("idempotent");
        assert_eq!(hashes, vec![ALREADY_SPENT_TX]);
    }

    #[tokio::test]
    async fn default_spend_batch_short_circuits_on_transient() {
        let s = SeqSpender {
            mode: SeqMode::Transient,
            calls: Mutex::new(0),
        };
        assert!(matches!(
            s.spend_batch(&debits(&["a", "b"])).await,
            Err(BatchSpendError::Transient(_))
        ));
        assert_eq!(*s.calls.lock().unwrap(), 1, "stops at the first failure");
    }

    /// A spend-only transport that hits an unauthorized key surfaces it as a
    /// whole-batch `Permanent` (a global config fault the drain halts on) — never a
    /// per-element outcome that could be dead-lettered — and short-circuits.
    #[tokio::test]
    async fn default_spend_batch_maps_not_authorized_to_permanent() {
        let s = SeqSpender {
            mode: SeqMode::NotAuthorized,
            calls: Mutex::new(0),
        };
        assert!(matches!(
            s.spend_batch(&debits(&["a", "b"])).await,
            Err(BatchSpendError::Permanent(_))
        ));
        assert_eq!(*s.calls.lock().unwrap(), 1, "stops at the first failure");
    }
}
