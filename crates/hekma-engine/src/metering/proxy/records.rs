//! Per-call records (story 23-1, spine AD-26 / AD-27): call ids, the
//! permit pool and the record pump.
//!
//! At admission a call takes a **permit** from a pool of
//! [`RECORD_PERMITS`]; none free within [`PERMIT_WAIT`] → the call is
//! refused retryably before any upstream dial, so nothing is ever spent that
//! cannot be recorded. One permit covers the call's start and close records
//! and is released once its close commits (or is skipped loudly).
//!
//! Records are an unacked stream to the store writer (bounded by its batch
//! window). A failed close job is re-queued with the same call id and record
//! (idempotent: the calls-store primary key and the (run, call id) usage
//! index make a replay a no-op); `Busy` never counts as an attempt; after
//! [`MAX_RECORD_ATTEMPTS`] consecutive failures a record is skipped with a
//! diagnostic naming the call id and run id, and counted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::domain::calls::{CallRecord, CallStart};
use crate::domain::pipeline::{self, EvaluationInbox, ProxiedClose, ProxiedUsage};
use crate::domain::runs::snapshot::SnapshotPublisher;
use crate::ports::StoreError;
use crate::store::writer::WriterHandle;

/// The permit pool's size.
pub(crate) const RECORD_PERMITS: usize = 4096;

/// How long a call waits for a permit before `ledger_unavailable`.
pub(crate) const PERMIT_WAIT: Duration = Duration::from_secs(1);

/// The most consecutive failures of one record before it is skipped.
pub(crate) const MAX_RECORD_ATTEMPTS: u32 = 10;

/// A new call id: `call_` + a ULID ([`mint_ulid`](crate::domain::mint_ulid),
/// the same mint as Run ids). A call id is a dedup key, never a secret.
pub(crate) fn new_call_id() -> String {
    format!("call_{}", crate::domain::mint_ulid())
}

/// Counters the proxy keeps (read by the info surface and the tests).
#[derive(Debug, Default)]
pub(crate) struct RecordStats {
    pub skipped: AtomicU64,
    pub retried: AtomicU64,
}

/// The pump: the writer handle, the permit pool, and what a close job
/// needs to publish.
pub(crate) struct RecordPump {
    pub permits: Arc<Semaphore>,
    pub writer: std::sync::Mutex<Option<WriterHandle>>,
    pub runs: Arc<SnapshotPublisher>,
    pub inbox: Arc<EvaluationInbox>,
    pub stats: Arc<RecordStats>,
    pub diagnostics: Arc<dyn Fn(String) + Send + Sync>,
    /// cfg(test) seam: fail the next N close jobs before they write.
    #[cfg(test)]
    pub fail_closes: Arc<AtomicU64>,
}

impl RecordPump {
    /// Take a permit within [`PERMIT_WAIT`] (`None` → refuse the call).
    pub(crate) async fn permit(&self) -> Option<OwnedSemaphorePermit> {
        tokio::time::timeout(PERMIT_WAIT, Arc::clone(&self.permits).acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
    }

    fn handle(&self) -> Option<WriterHandle> {
        self.writer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Drop the writer handle (graceful stop: after this nothing more is
    /// submitted, so the writer can close).
    pub(crate) fn close(&self) {
        self.writer.lock().unwrap_or_else(|p| p.into_inner()).take();
    }

    /// Submit the call-start record (unacked, best effort: a lost start
    /// only loses the crash-closure row; the close still records the call).
    pub(crate) fn start(&self, start: CallStart) {
        if let Some(handle) = self.handle() {
            let _ = handle.submit_then(move |ctx| pipeline::record_call_start(ctx, &start), |_| {});
        }
    }

    /// Submit the close record with its usage; `permit` is released once
    /// it commits or is skipped.
    pub(crate) fn close_call(
        self: &Arc<Self>,
        record: CallRecord,
        usage: Option<ProxiedUsage>,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        submit_close(Arc::clone(self), ProxiedClose { record, usage }, permit, 0);
    }
}

fn submit_close(
    pump: Arc<RecordPump>,
    close: ProxiedClose,
    permit: Option<OwnedSemaphorePermit>,
    attempts: u32,
) {
    let Some(handle) = pump.handle() else {
        // The proxy is stopping and the writer is closing: the record is
        // lost, loudly.
        (pump.diagnostics)(format!(
            "the metering proxy stopped before call {} (Run {}) was recorded; its usage, if \
             any, was not counted",
            close.record.call_id,
            close.record.run_id.as_str()
        ));
        pump.stats.skipped.fetch_add(1, Ordering::Relaxed);
        drop(permit);
        return;
    };
    let job_close = close.clone();
    let runs = Arc::clone(&pump.runs);
    let inbox = Arc::clone(&pump.inbox);
    #[cfg(test)]
    let fail = Arc::clone(&pump.fail_closes);
    let retry_pump = Arc::clone(&pump);
    let sent = handle.submit_then(
        move |ctx| {
            #[cfg(test)]
            if fail
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(StoreError::Backend(
                    "test seam: the close job fails".to_string(),
                ));
            }
            pipeline::commit_proxied_call(ctx, job_close, &runs, &inbox)
        },
        move |outcome| match outcome {
            Ok(()) => drop(permit),
            Err(err) => {
                let attempts = if matches!(err, StoreError::Busy { .. }) {
                    attempts
                } else {
                    attempts + 1
                };
                if attempts >= MAX_RECORD_ATTEMPTS {
                    (retry_pump.diagnostics)(format!(
                        "call {} (Run {}) could not be recorded after {MAX_RECORD_ATTEMPTS} \
                         attempts and was skipped; its usage was not counted ({err})",
                        close.record.call_id,
                        close.record.run_id.as_str()
                    ));
                    retry_pump.stats.skipped.fetch_add(1, Ordering::Relaxed);
                    drop(permit);
                } else {
                    retry_pump.stats.retried.fetch_add(1, Ordering::Relaxed);
                    submit_close(Arc::clone(&retry_pump), close, permit, attempts);
                }
            }
        },
    );
    if sent.is_err() {
        // The writer is gone: nothing more can be recorded.
        pump.stats.skipped.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_ids_are_ulids_with_the_prefix_and_unique() {
        let a = new_call_id();
        let b = new_call_id();
        assert!(a.starts_with("call_"));
        assert_eq!(a.len(), 31);
        assert_ne!(a, b);
        assert!(a[5..].bytes().all(|byte| CROCKFORD.contains(&byte)));
    }
}
