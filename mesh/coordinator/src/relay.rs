//! On-chain relay of pending EAS render receipts.
//!
//! `record_completed` durably records a pending attestation per validated settle
//! (see `eas.rs` + `store.rs`). The relayer drains those rows and submits each as
//! a `RenderReceipts.issueReceipt(earner, jobId, renderSeconds, jobKind,
//! outputHash, regionId)` call on Base, returning the EAS attestation UID. The
//! drain loop itself lives in `main.rs` (`drain_attestations`); this module is
//! the transport boundary: the [`Relay`] trait plus a [`MockRelay`] for tests and
//! local dev.
//!
//! The live Base implementation (an RPC provider + an authorized coordinator
//! signer with gas) is operator-gated and not built here — it is the single
//! remaining piece behind real credentials. Everything up to this trait is built
//! and tested against the mock.

use crate::eas::PendingAttestation;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Why a submission failed — the drain loop reacts differently to each.
#[derive(Debug)]
pub enum RelayError {
    /// The receipt for this `jobId` is already on-chain (the contract reverted
    /// `DuplicateReceipt`). This is an *idempotent success*: a crash between a
    /// prior `issueReceipt` and its local mark left the row pending, and the
    /// re-submit hit the contract's per-job fence. The drain marks it submitted
    /// and moves on rather than retrying forever.
    AlreadyIssued,
    /// The coordinator signer is not in `RenderReceipts.authorizedCoordinators`
    /// (the contract reverts `NotAuthorized`). Every `issueReceipt` reverts until
    /// the owner calls `setAuthorizedCoordinator`, so this is a loud, distinct
    /// operator-config error — NOT a per-row fault to quarantine one receipt over.
    /// The drain stops; the backlog (visible at `/stats pending_attestations`)
    /// stays put until authorization lands, then resumes on the next tick.
    NotAuthorized,
    /// A transient fault (RPC timeout, nonce contention, a reorg). The receipt is
    /// not on-chain; the row stays pending and is retried on the next drain tick.
    Transient(String),
    /// A non-retryable fault that is NOT a global config problem — e.g. the
    /// contract reverts on a receipt naming a region whose render fee the
    /// coordinator can't cover. Retrying in a hot loop or dropping the receipt are
    /// both wrong, so the drain DEAD-LETTERS this one row (quarantined + retained,
    /// surfaced at `/stats dead_lettered_attestations`) and CONTINUES, so one poison
    /// receipt never blocks the rest of the backlog. Unlike `NotAuthorized`, the
    /// fault is per-row, not global.
    Permanent(String),
}

/// Marker UID recorded when the contract reports a receipt is already on-chain
/// (`AlreadyIssued`): the attestation landed but the relay didn't capture its real
/// UID (a crash recovered between a prior `issueReceipt` and its local mark). The
/// row is settled — it just carries this sentinel instead of an EAS UID.
pub const ALREADY_ISSUED_UID: &str = "already-issued";

/// Why a *batch* submission (`issueReceipts`) failed — the drain reacts to each
/// differently, mirroring the single-submit reactions but at batch granularity.
#[derive(Debug)]
pub enum BatchRelayError {
    /// The batch reverted atomically on-chain — `issueReceipts` is all-or-nothing,
    /// so ONE element's `DuplicateReceipt` fence or one bad arg rolls the whole
    /// batch back. The revert names no single offender, so the drain falls back to
    /// per-receipt [`Relay::submit`] to isolate it: the already-issued/bad element
    /// self-identifies while the rest still drain.
    Reverted(String),
    /// A transient transport fault for the whole batch (RPC timeout, nonce
    /// contention, a reorg). Nothing landed; the drain backs off to the next tick
    /// with every row still pending — single submits would hit the same fault, so
    /// the fallback is skipped.
    Transient(String),
    /// A non-retryable fault for the whole batch (the signer is not an authorized
    /// coordinator). The drain stops loudly; every row stays pending.
    Permanent(String),
}

/// Submits validated render receipts as `RenderReceipts` calls, returning the EAS
/// attestation UID(s) on success. [`submit`](Relay::submit) is one
/// `issueReceipt`; [`submit_batch`](Relay::submit_batch) is one
/// `issueReceipts(ReceiptRequest[])` (one `EAS.multiAttest`). The futures are
/// `Send` so the drain loop can run on the multi-threaded runtime; implementors
/// must not block.
pub trait Relay: Send + Sync {
    fn submit(
        &self,
        att: &PendingAttestation,
    ) -> impl Future<Output = Result<String, RelayError>> + Send;

    /// Submit a whole chunk as ONE `issueReceipts` call, returning the UIDs in
    /// submission order — `uids[i]` is `atts[i]`'s, so the caller maps each job to
    /// its own UID by a positional zip. Atomic on-chain: any element reverting
    /// rolls the whole batch back ([`BatchRelayError::Reverted`]).
    ///
    /// The default submits each element sequentially via [`submit`](Relay::submit)
    /// — correct, just un-amortized — so a transport that can't batch works
    /// unchanged; the live Base impl overrides it with a real `multiAttest`. An
    /// already-issued element resolves to the [`ALREADY_ISSUED_UID`] sentinel (an
    /// idempotent success), and the first `Transient`/`Permanent` short-circuits to
    /// the matching batch error.
    fn submit_batch(
        &self,
        atts: &[PendingAttestation],
    ) -> impl Future<Output = Result<Vec<String>, BatchRelayError>> + Send {
        async move {
            let mut uids = Vec::with_capacity(atts.len());
            for att in atts {
                match self.submit(att).await {
                    Ok(uid) => uids.push(uid),
                    Err(RelayError::AlreadyIssued) => uids.push(ALREADY_ISSUED_UID.to_string()),
                    Err(RelayError::NotAuthorized) => {
                        // A global config fault (unauthorized signer) — halt the
                        // whole batch loudly, same as a native `multiAttest` revert
                        // for authorization would; never a per-element dead-letter.
                        return Err(BatchRelayError::Permanent(
                            "coordinator signer not authorized on RenderReceipts".into(),
                        ));
                    }
                    Err(RelayError::Transient(m)) => return Err(BatchRelayError::Transient(m)),
                    Err(RelayError::Permanent(m)) => return Err(BatchRelayError::Permanent(m)),
                }
            }
            Ok(uids)
        }
    }
}

/// In-process [`Relay`] for tests and the `--relay-dev-mock` local path. Never
/// touches a chain: it replays a scripted outcome and records every call so the
/// drain loop can be exercised deterministically — success, transient-then-ok,
/// permanent, or already-issued — without RPC or funds.
pub struct MockRelay {
    inner: Mutex<MockInner>,
    /// Test-only gate: when set, `submit` signals `started` and then awaits
    /// `release` before resolving, so a test can hold the submit in-flight and
    /// assert the drain loop keeps the store lock free during it. `None` (always)
    /// on the production dev path.
    started: Option<Arc<Notify>>,
    release: Option<Arc<Notify>>,
}

struct MockInner {
    /// Fail this many single `submit` calls with `Transient` before succeeding.
    transient_remaining: usize,
    /// Always fail single `submit` with `Permanent`.
    permanent: bool,
    /// Always fail single `submit` with `NotAuthorized` (the coordinator signer is
    /// not authorized on RenderReceipts — a global fault the drain halts on, never
    /// a per-row dead-letter).
    not_authorized: bool,
    /// Always fail single `submit` with `AlreadyIssued` (models a receipt already
    /// on-chain).
    already_issued: bool,
    /// Specific `job_id`s whose single `submit` returns `AlreadyIssued` (the rest
    /// succeed) — models ONE already-on-chain receipt inside a reverted batch, so
    /// the fallback isolates it. Empty on the production dev path.
    already_issued_jobs: Vec<String>,
    /// Specific `job_id`s whose single `submit` returns `Permanent` (the rest
    /// succeed) — models ONE poison receipt (e.g. an unpayable region fee) inside a
    /// reverted batch, so the fallback dead-letters it while the rest drain. Empty
    /// on the production dev path.
    permanent_jobs: Vec<String>,
    /// Fail `submit_batch` with `Reverted` (models the contract's atomic batch
    /// revert — one bad/duplicate element rolls the whole call back).
    batch_reverts: bool,
    /// Fail `submit_batch` with `Transient` (a whole-batch transport fault).
    batch_transient: bool,
    /// Fail `submit_batch` with `Permanent` (a whole-batch non-retryable fault).
    batch_permanent: bool,
    /// Total single `submit` calls (successful or not).
    calls: usize,
    /// Total `submit_batch` calls — distinct from `calls`, so a test proves the
    /// happy path does ONE `multiAttest`, not N single submits.
    batch_calls: usize,
    /// `job_id` (bytes32 hex) of every attestation that returned a fresh single UID.
    submitted: Vec<String>,
    /// `job_id`s included in a successful batch, in submission order.
    batch_submitted: Vec<String>,
}

impl MockRelay {
    /// Always succeeds, returning a deterministic mock UID. Used by `main`'s
    /// `--relay-dev-mock` path and by the happy-path tests.
    pub fn succeeding() -> Self {
        Self {
            inner: Mutex::new(MockInner {
                transient_remaining: 0,
                permanent: false,
                not_authorized: false,
                already_issued: false,
                already_issued_jobs: Vec::new(),
                permanent_jobs: Vec::new(),
                batch_reverts: false,
                batch_transient: false,
                batch_permanent: false,
                calls: 0,
                batch_calls: 0,
                submitted: Vec::new(),
                batch_submitted: Vec::new(),
            }),
            started: None,
            release: None,
        }
    }

    /// Fail the first `n` submits with `Transient`, then succeed.
    #[cfg(test)]
    pub fn transient_then_ok(n: usize) -> Self {
        let mut m = Self::succeeding();
        m.inner.get_mut().unwrap().transient_remaining = n;
        m
    }

    /// Always fail with `Permanent` (a per-row non-retryable fault).
    #[cfg(test)]
    pub fn permanent() -> Self {
        let mut m = Self::succeeding();
        m.inner.get_mut().unwrap().permanent = true;
        m
    }

    /// Always fail with `AlreadyIssued` (the receipt is already on-chain).
    #[cfg(test)]
    pub fn already_issued() -> Self {
        let mut m = Self::succeeding();
        m.inner.get_mut().unwrap().already_issued = true;
        m
    }

    /// A succeeding relay that signals `started` and then awaits `release` inside
    /// `submit`, so a test can observe the in-flight submit window and assert the
    /// store lock is free during it.
    #[cfg(test)]
    pub fn gated(started: Arc<Notify>, release: Arc<Notify>) -> Self {
        let mut m = Self::succeeding();
        m.started = Some(started);
        m.release = Some(release);
        m
    }

    /// Make `submit_batch` revert atomically (the contract's all-or-nothing batch
    /// fence). Composed with a single-submit outcome, it drives the drain's
    /// per-receipt fallback path.
    #[cfg(test)]
    pub fn with_batch_reverts(mut self) -> Self {
        self.inner.get_mut().unwrap().batch_reverts = true;
        self
    }

    /// Make `submit_batch` fail `Transient` (a whole-batch transport fault).
    #[cfg(test)]
    pub fn with_batch_transient(mut self) -> Self {
        self.inner.get_mut().unwrap().batch_transient = true;
        self
    }

    /// Make `submit_batch` fail `Permanent` (a whole-batch non-retryable fault).
    #[cfg(test)]
    pub fn with_batch_permanent(mut self) -> Self {
        self.inner.get_mut().unwrap().batch_permanent = true;
        self
    }

    /// Make this specific `job_id`'s single `submit` return `AlreadyIssued` (the
    /// rest succeed) — one already-on-chain receipt for the fallback to isolate.
    #[cfg(test)]
    pub fn with_already_issued_job(mut self, job_id: String) -> Self {
        self.inner.get_mut().unwrap().already_issued_jobs.push(job_id);
        self
    }

    /// Make this specific `job_id`'s single `submit` return `Permanent` (the rest
    /// succeed) — one poison receipt for the fallback to dead-letter + isolate.
    #[cfg(test)]
    pub fn with_permanent_job(mut self, job_id: String) -> Self {
        self.inner.get_mut().unwrap().permanent_jobs.push(job_id);
        self
    }

    /// Total `submit` calls so far.
    #[cfg(test)]
    pub fn calls(&self) -> usize {
        self.inner.lock().unwrap().calls
    }

    /// `job_id`s that returned a fresh UID (excludes `AlreadyIssued`, which
    /// submits nothing new).
    #[cfg(test)]
    pub fn submitted(&self) -> Vec<String> {
        self.inner.lock().unwrap().submitted.clone()
    }

    /// Total `submit_batch` calls so far (the multiAttest count).
    #[cfg(test)]
    pub fn batch_calls(&self) -> usize {
        self.inner.lock().unwrap().batch_calls
    }

    /// `job_id`s included in a successful batch, in submission order.
    #[cfg(test)]
    pub fn batch_submitted(&self) -> Vec<String> {
        self.inner.lock().unwrap().batch_submitted.clone()
    }
}

/// Deterministic mock UID for a single `submit`, so a test can assert the drain
/// stored exactly what the relay returned.
fn mock_uid(att: &PendingAttestation) -> String {
    format!("0xmock-{}", att.job_id)
}

/// Deterministic mock UID for a batched element — a distinct prefix from
/// [`mock_uid`] so a test can tell a batch UID from a single-submit one and prove
/// the happy path went through `multiAttest`.
fn batch_uid(att: &PendingAttestation) -> String {
    format!("0xmockbatch-{}", att.job_id)
}

impl Relay for MockRelay {
    async fn submit(&self, att: &PendingAttestation) -> Result<String, RelayError> {
        if let Some(s) = &self.started {
            s.notify_one();
        }
        if let Some(r) = &self.release {
            r.notified().await;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.calls += 1;
        if inner.not_authorized {
            return Err(RelayError::NotAuthorized);
        }
        if inner.permanent || inner.permanent_jobs.contains(&att.job_id) {
            return Err(RelayError::Permanent("mock permanent".into()));
        }
        if inner.already_issued || inner.already_issued_jobs.contains(&att.job_id) {
            return Err(RelayError::AlreadyIssued);
        }
        if inner.transient_remaining > 0 {
            inner.transient_remaining -= 1;
            return Err(RelayError::Transient("mock transient".into()));
        }
        inner.submitted.push(att.job_id.clone());
        Ok(mock_uid(att))
    }

    async fn submit_batch(
        &self,
        atts: &[PendingAttestation],
    ) -> Result<Vec<String>, BatchRelayError> {
        if let Some(s) = &self.started {
            s.notify_one();
        }
        if let Some(r) = &self.release {
            r.notified().await;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.batch_calls += 1;
        if inner.batch_permanent {
            return Err(BatchRelayError::Permanent("mock batch permanent".into()));
        }
        if inner.batch_reverts {
            return Err(BatchRelayError::Reverted("mock batch revert".into()));
        }
        if inner.batch_transient {
            return Err(BatchRelayError::Transient("mock batch transient".into()));
        }
        let uids: Vec<String> = atts.iter().map(batch_uid).collect();
        for a in atts {
            inner.batch_submitted.push(a.job_id.clone());
        }
        Ok(uids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att() -> PendingAttestation {
        PendingAttestation {
            earner: "0x00000000000000000000000000000000000000a1".into(),
            job_id: "a".repeat(64),
            render_seconds: 3,
            job_kind: 0,
            output_hash: "b".repeat(64),
            region_id: "c".repeat(64),
        }
    }

    #[tokio::test]
    async fn succeeding_returns_uid_and_records_the_call() {
        let r = MockRelay::succeeding();
        let a = att();
        let uid = r.submit(&a).await.expect("succeeds");
        assert_eq!(uid, mock_uid(&a));
        assert_eq!(r.calls(), 1);
        assert_eq!(r.submitted(), vec![a.job_id]);
    }

    #[tokio::test]
    async fn transient_then_ok_fails_then_succeeds() {
        let r = MockRelay::transient_then_ok(2);
        let a = att();
        assert!(matches!(r.submit(&a).await, Err(RelayError::Transient(_))));
        assert!(matches!(r.submit(&a).await, Err(RelayError::Transient(_))));
        assert!(r.submit(&a).await.is_ok());
        assert_eq!(r.calls(), 3);
        // Only the successful call recorded a submission.
        assert_eq!(r.submitted(), vec![a.job_id]);
    }

    #[tokio::test]
    async fn permanent_never_submits() {
        let r = MockRelay::permanent();
        assert!(matches!(r.submit(&att()).await, Err(RelayError::Permanent(_))));
        assert!(r.submitted().is_empty());
    }

    #[tokio::test]
    async fn already_issued_never_submits() {
        let r = MockRelay::already_issued();
        assert!(matches!(r.submit(&att()).await, Err(RelayError::AlreadyIssued)));
        assert_eq!(r.calls(), 1);
        assert!(r.submitted().is_empty());
    }

    fn atts(tags: &[&str]) -> Vec<PendingAttestation> {
        tags.iter()
            .map(|t| PendingAttestation {
                job_id: t.to_string(),
                ..att()
            })
            .collect()
    }

    #[tokio::test]
    async fn submit_batch_returns_uids_in_submission_order_via_one_call() {
        let r = MockRelay::succeeding();
        let batch = atts(&["a", "b", "c"]);
        let uids = r.submit_batch(&batch).await.expect("batch succeeds");
        assert_eq!(uids, batch.iter().map(batch_uid).collect::<Vec<_>>());
        assert_eq!(r.batch_calls(), 1, "one multiAttest");
        assert_eq!(r.calls(), 0, "no single submits on the batch path");
        assert_eq!(r.batch_submitted(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn submit_batch_reverts_when_scripted() {
        let r = MockRelay::succeeding().with_batch_reverts();
        assert!(matches!(
            r.submit_batch(&atts(&["a", "b"])).await,
            Err(BatchRelayError::Reverted(_))
        ));
        assert_eq!(r.batch_calls(), 1);
        assert!(r.batch_submitted().is_empty(), "a revert records nothing");
    }

    #[tokio::test]
    async fn submit_batch_transient_and_permanent_when_scripted() {
        let t = MockRelay::succeeding().with_batch_transient();
        assert!(matches!(
            t.submit_batch(&atts(&["a"])).await,
            Err(BatchRelayError::Transient(_))
        ));
        let p = MockRelay::succeeding().with_batch_permanent();
        assert!(matches!(
            p.submit_batch(&atts(&["a"])).await,
            Err(BatchRelayError::Permanent(_))
        ));
    }

    #[tokio::test]
    async fn submit_isolates_an_already_issued_job() {
        let r = MockRelay::succeeding().with_already_issued_job("a".into());
        let batch = atts(&["a", "b"]);
        assert!(matches!(
            r.submit(&batch[0]).await,
            Err(RelayError::AlreadyIssued)
        ));
        assert!(r.submit(&batch[1]).await.is_ok(), "the rest still submit");
        assert_eq!(r.submitted(), vec!["b"]);
    }

    #[tokio::test]
    async fn submit_isolates_a_permanent_job() {
        let r = MockRelay::succeeding().with_permanent_job("a".into());
        let batch = atts(&["a", "b"]);
        assert!(matches!(
            r.submit(&batch[0]).await,
            Err(RelayError::Permanent(_))
        ));
        assert!(r.submit(&batch[1]).await.is_ok(), "the rest still submit");
        assert_eq!(r.submitted(), vec!["b"]);
    }

    /// A `Relay` that implements only `submit` gets `submit_batch` from the trait
    /// default — sequential single submits, no amortization.
    enum SeqMode {
        Ok,
        AlreadyIssued,
        Transient,
    }
    struct SeqRelay {
        mode: SeqMode,
        calls: Mutex<usize>,
    }
    impl Relay for SeqRelay {
        async fn submit(&self, att: &PendingAttestation) -> Result<String, RelayError> {
            *self.calls.lock().unwrap() += 1;
            match self.mode {
                SeqMode::Ok => Ok(format!("0xseq-{}", att.job_id)),
                SeqMode::AlreadyIssued => Err(RelayError::AlreadyIssued),
                SeqMode::Transient => Err(RelayError::Transient("seq transient".into())),
            }
        }
    }

    #[tokio::test]
    async fn default_submit_batch_does_sequential_single_submits() {
        let r = SeqRelay {
            mode: SeqMode::Ok,
            calls: Mutex::new(0),
        };
        let batch = atts(&["a", "b", "c"]);
        let uids = r.submit_batch(&batch).await.expect("succeeds");
        assert_eq!(uids, vec!["0xseq-a", "0xseq-b", "0xseq-c"]);
        assert_eq!(*r.calls.lock().unwrap(), 3, "one submit per element");
    }

    #[tokio::test]
    async fn default_submit_batch_maps_already_issued_to_the_sentinel() {
        let r = SeqRelay {
            mode: SeqMode::AlreadyIssued,
            calls: Mutex::new(0),
        };
        let uids = r.submit_batch(&atts(&["a"])).await.expect("idempotent");
        assert_eq!(uids, vec![ALREADY_ISSUED_UID]);
    }

    #[tokio::test]
    async fn default_submit_batch_short_circuits_on_transient() {
        let r = SeqRelay {
            mode: SeqMode::Transient,
            calls: Mutex::new(0),
        };
        assert!(matches!(
            r.submit_batch(&atts(&["a", "b"])).await,
            Err(BatchRelayError::Transient(_))
        ));
        assert_eq!(*r.calls.lock().unwrap(), 1, "stops at the first failure");
    }
}
