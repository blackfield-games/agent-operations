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
use std::sync::Mutex;

/// Why a submission failed — the drain loop reacts differently to each.
#[derive(Debug)]
pub enum RelayError {
    /// The receipt for this `jobId` is already on-chain (the contract reverted
    /// `DuplicateReceipt`). This is an *idempotent success*: a crash between a
    /// prior `issueReceipt` and its local mark left the row pending, and the
    /// re-submit hit the contract's per-job fence. The drain marks it submitted
    /// and moves on rather than retrying forever.
    AlreadyIssued,
    /// A transient fault (RPC timeout, nonce contention, a reorg). The receipt is
    /// not on-chain; the row stays pending and is retried on the next drain tick.
    Transient(String),
    /// A non-retryable fault (the signer is not an authorized coordinator, an
    /// argument the contract rejects). Retrying in a hot loop or dropping the
    /// receipt are both wrong, so the drain stops and surfaces it loudly for the
    /// operator; the row stays pending.
    Permanent(String),
}

/// Submits a pending attestation as a `RenderReceipts.issueReceipt` call,
/// returning the EAS attestation UID on success. The future is `Send` so the
/// drain loop can run on the multi-threaded runtime; implementors must not block.
pub trait Relay: Send + Sync {
    fn submit(
        &self,
        att: &PendingAttestation,
    ) -> impl Future<Output = Result<String, RelayError>> + Send;
}

/// In-process [`Relay`] for tests and the `--relay-dev-mock` local path. Never
/// touches a chain: it replays a scripted outcome and records every call so the
/// drain loop can be exercised deterministically — success, transient-then-ok,
/// permanent, or already-issued — without RPC or funds.
pub struct MockRelay {
    inner: Mutex<MockInner>,
}

struct MockInner {
    /// Fail this many calls with `Transient` before succeeding.
    transient_remaining: usize,
    /// Always fail with `Permanent`.
    permanent: bool,
    /// Always fail with `AlreadyIssued` (models a receipt already on-chain).
    already_issued: bool,
    /// Total `submit` calls (successful or not).
    calls: usize,
    /// `job_id` (bytes32 hex) of every attestation that returned a fresh UID.
    submitted: Vec<String>,
}

impl MockRelay {
    /// Always succeeds, returning a deterministic mock UID. Used by `main`'s
    /// `--relay-dev-mock` path and by the happy-path tests.
    pub fn succeeding() -> Self {
        Self {
            inner: Mutex::new(MockInner {
                transient_remaining: 0,
                permanent: false,
                already_issued: false,
                calls: 0,
                submitted: Vec::new(),
            }),
        }
    }

    /// Fail the first `n` submits with `Transient`, then succeed.
    #[cfg(test)]
    pub fn transient_then_ok(n: usize) -> Self {
        let mut m = Self::succeeding();
        m.inner.get_mut().unwrap().transient_remaining = n;
        m
    }

    /// Always fail with `Permanent` (e.g. an unauthorized signer).
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
}

/// Deterministic mock UID for a given attestation, so a test can assert the
/// drain stored exactly what the relay returned.
fn mock_uid(att: &PendingAttestation) -> String {
    format!("0xmock-{}", att.job_id)
}

impl Relay for MockRelay {
    fn submit(
        &self,
        att: &PendingAttestation,
    ) -> impl Future<Output = Result<String, RelayError>> + Send {
        let mut inner = self.inner.lock().unwrap();
        inner.calls += 1;
        let outcome = if inner.permanent {
            Err(RelayError::Permanent("mock permanent".into()))
        } else if inner.already_issued {
            Err(RelayError::AlreadyIssued)
        } else if inner.transient_remaining > 0 {
            inner.transient_remaining -= 1;
            Err(RelayError::Transient("mock transient".into()))
        } else {
            inner.submitted.push(att.job_id.clone());
            Ok(mock_uid(att))
        };
        std::future::ready(outcome)
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
}
