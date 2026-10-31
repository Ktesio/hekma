//! The store writer (story 21-1, spine AD-27): the ONE thread in an engine
//! process that writes state.db.
//!
//! ## Shape
//!
//! The writer owns its own read-write [`SqliteStore`]; the Registry keeps a
//! read-only connection. Every write is a job — a closure run on the
//! writer's connection — sent over a channel:
//!
//! * [`StoreWriter::call`] is the **acked** write: the caller waits for the
//!   job's commit, at most [`ACK_BOUND`] while the job is still queued, then
//!   gets the typed retryable [`StoreError::Busy`] and the job is abandoned
//!   (the writer skips it). Once the writer has taken a job the caller keeps
//!   waiting: from there the work is bounded local SQLite work under a write
//!   lock the batch already holds. So `Busy` always means "not applied".
//! * [`StoreWriter::submit`] is the **unacked** write (no reply): the seam
//!   the shared proxy's per-call records use (story 23.1).
//!
//! ## Batches
//!
//! The writer blocks for one job, then takes whatever is already queued, up
//! to [`BATCH_MAX_EVENTS`]. An acked job never waits for company (it flushes
//! at once, D2); a batch of only unacked jobs waits for more up to
//! [`BATCH_WINDOW`] from its first job. A batch is ONE `BEGIN IMMEDIATE`
//! transaction with a SAVEPOINT per job: a failed job rolls back only its own
//! statements and answers only its own caller. Post-commit hooks registered by
//! a job (the `block` fence raise) run after COMMIT, before the ack. A failed
//! COMMIT rolls the batch back and every taken job gets a typed error.
//!
//! ## Locks
//!
//! The writer takes no engine mutex and sits outside the `Registry →
//! Supervisor` lock order (D2): a caller may wait for an ack while holding
//! both, and the writer never needs either. Until story 21.4 routes every CLI
//! write through one Service Host, each engine process runs its own writer;
//! two processes serialize through SQLite (`BEGIN IMMEDIATE` plus the busy
//! timeout), and guarded writes keep their guards inside the batch.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::sqlite::in_savepoint;
use super::SqliteStore;
use crate::ports::StoreError;

/// How long a caller waits for its job to be taken before the typed
/// retryable [`StoreError::Busy`] (D2: "at most 1 s").
pub(crate) const ACK_BOUND: Duration = Duration::from_secs(1);

/// The most events one batch carries (AD-27's "512 events").
pub(crate) const BATCH_MAX_EVENTS: usize = 512;

/// How long a batch of unacked jobs waits for company (AD-27's "250 ms").
pub(crate) const BATCH_WINDOW: Duration = Duration::from_millis(250);

const PENDING: u8 = 0;
const TAKEN: u8 = 1;
const ABANDONED: u8 = 2;

/// What a job sees while it runs: the writer's store, inside the batch
/// transaction, and a place to register post-commit work.
pub(crate) struct WriterCtx<'a> {
    store: &'a SqliteStore,
    hooks: Vec<Hook>,
}

impl<'a> WriterCtx<'a> {
    /// The writer's store (read-write), inside the open batch transaction.
    pub(crate) fn store(&self) -> &'a SqliteStore {
        self.store
    }

    /// Run `hook` after the batch commits (never if this job or the batch
    /// rolls back), before the job's caller is acked.
    pub(crate) fn after_commit(&mut self, hook: impl FnOnce() + Send + 'static) {
        self.hooks.push(Box::new(hook));
    }
}

type Work = Box<dyn FnOnce(&mut WriterCtx<'_>) -> Result<(), StoreError> + Send>;
type Finish = Box<dyn FnOnce(Result<(), StoreError>) + Send>;
type Hook = Box<dyn FnOnce() + Send>;

struct Job {
    acked: bool,
    weight: usize,
    state: Arc<AtomicU8>,
    work: Work,
    finish: Finish,
}

/// Counters the bench and tests read: batches committed, jobs run, unacked
/// jobs that failed (they have no caller to tell), and the longest time an
/// unacked job waited from submit to commit.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub(crate) struct WriterStats {
    pub batches: AtomicU64,
    pub jobs: AtomicU64,
    pub unacked_failures: AtomicU64,
    pub max_unacked_flush_micros: AtomicU64,
}

/// The handle the Registry holds. Dropping it closes the channel; the writer
/// finishes what is queued and exits, and the drop joins it.
pub(crate) struct StoreWriter {
    tx: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<WriterStats>,
}

impl StoreWriter {
    /// Start the writer thread over an already-opened, migrated store.
    pub(crate) fn spawn(store: SqliteStore) -> Result<Self, StoreError> {
        let (tx, rx) = mpsc::channel::<Job>();
        let stats = Arc::new(WriterStats::default());
        let thread_stats = Arc::clone(&stats);
        let thread = std::thread::Builder::new()
            .name("hekma-store-writer".to_string())
            .spawn(move || run(&store, &rx, &thread_stats))
            .map_err(|e| StoreError::Backend(format!("could not start the store writer: {e}")))?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
            stats,
        })
    }

    /// Run `work` on the writer and wait for its commit (the acked write).
    pub(crate) fn call<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut WriterCtx<'_>) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, StoreError> {
        self.call_weighted(1, work)
    }

    /// [`Self::call`] for a job that counts `weight` events against the
    /// batch bound.
    pub(crate) fn call_weighted<R: Send + 'static>(
        &self,
        weight: usize,
        work: impl FnOnce(&mut WriterCtx<'_>) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, StoreError> {
        let slot: Arc<Mutex<Option<R>>> = Arc::new(Mutex::new(None));
        let work_slot = Arc::clone(&slot);
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<R, StoreError>>(1);
        let state = Arc::new(AtomicU8::new(PENDING));
        let job = Job {
            acked: true,
            weight: weight.max(1),
            state: Arc::clone(&state),
            work: Box::new(move |ctx| {
                let value = work(ctx)?;
                *work_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(value);
                Ok(())
            }),
            finish: Box::new(move |outcome| {
                let result = outcome.and_then(|()| {
                    slot.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .take()
                        .ok_or_else(|| {
                            StoreError::Backend("the store writer lost a job's result".to_string())
                        })
                });
                let _ = reply_tx.send(result);
            }),
        };
        self.send(job)?;
        match reply_rx.recv_timeout(ACK_BOUND) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                if state
                    .compare_exchange(PENDING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    Err(StoreError::Busy {
                        waited_ms: ACK_BOUND.as_millis() as u64,
                    })
                } else {
                    reply_rx.recv().unwrap_or_else(|_| Err(writer_gone()))
                }
            }
            Err(RecvTimeoutError::Disconnected) => Err(writer_gone()),
        }
    }

    /// Queue `work` with no reply (the unacked write). A failure is counted
    /// in [`WriterStats::unacked_failures`]; it has no caller to tell. No
    /// engine path submits unacked yet: the shared proxy's per-call records
    /// (story 23.1) are its first producer; the bench drives it today.
    #[allow(dead_code)]
    pub(crate) fn submit(
        &self,
        work: impl FnOnce(&mut WriterCtx<'_>) -> Result<(), StoreError> + Send + 'static,
    ) -> Result<(), StoreError> {
        let stats = Arc::clone(&self.stats);
        let submitted = Instant::now();
        self.send(Job {
            acked: false,
            weight: 1,
            state: Arc::new(AtomicU8::new(PENDING)),
            work: Box::new(work),
            finish: Box::new(move |outcome| {
                if outcome.is_err() {
                    stats.unacked_failures.fetch_add(1, Ordering::Relaxed);
                }
                let waited = submitted.elapsed().as_micros() as u64;
                stats
                    .max_unacked_flush_micros
                    .fetch_max(waited, Ordering::Relaxed);
            }),
        })
    }

    /// The writer's counters (the bench reads them).
    #[allow(dead_code)]
    pub(crate) fn stats(&self) -> &WriterStats {
        &self.stats
    }

    fn send(&self, job: Job) -> Result<(), StoreError> {
        self.tx
            .as_ref()
            .ok_or_else(writer_gone)?
            .send(job)
            .map_err(|_| writer_gone())
    }
}

impl Drop for StoreWriter {
    fn drop(&mut self) {
        // Close the channel, then wait for the queued jobs to commit.
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn writer_gone() -> StoreError {
    StoreError::Backend("the store writer has stopped; the write was not applied".to_string())
}

/// A fresh copy of a batch-level failure for each job in the batch: `Busy`
/// stays `Busy` (retryable), anything else becomes a backend error naming it.
fn batch_failure(err: &StoreError) -> StoreError {
    match err {
        StoreError::Busy { waited_ms } => StoreError::Busy {
            waited_ms: *waited_ms,
        },
        other => StoreError::Backend(format!(
            "the store writer's batch did not commit, so this write was not applied: {other}"
        )),
    }
}

/// The writer loop: one batch at a time until every handle is dropped.
fn run(store: &SqliteStore, rx: &Receiver<Job>, stats: &WriterStats) {
    while let Some(batch) = next_batch(rx) {
        commit_batch(store, batch, stats);
    }
}

/// Collect the next batch (see the module docs); `None` once the channel
/// closed and drained.
fn next_batch(rx: &Receiver<Job>) -> Option<Vec<Job>> {
    let first = rx.recv().ok()?;
    let started = Instant::now();
    let mut weight = first.weight;
    let mut any_acked = first.acked;
    let mut batch = vec![first];
    while weight < BATCH_MAX_EVENTS {
        let next = if any_acked {
            match rx.try_recv() {
                Ok(job) => job,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        } else {
            let Some(left) = BATCH_WINDOW.checked_sub(started.elapsed()) else {
                break;
            };
            match rx.recv_timeout(left) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        };
        weight += next.weight;
        any_acked |= next.acked;
        batch.push(next);
    }
    Some(batch)
}

/// Run one batch in one transaction and answer every job in it.
fn commit_batch(store: &SqliteStore, batch: Vec<Job>, stats: &WriterStats) {
    let conn = store.conn();
    if let Err(err) = conn.execute_batch("BEGIN IMMEDIATE") {
        let err = super::sqlite::backend(err);
        for job in batch {
            if take(&job) {
                (job.finish)(Err(batch_failure(&err)));
            }
        }
        return;
    }
    let mut ran: Vec<(Finish, Result<(), StoreError>, Vec<Hook>)> = Vec::with_capacity(batch.len());
    let mut lost: Option<String> = None;
    for job in batch {
        if !take(&job) {
            // Abandoned: the caller already returned `Busy`; never apply it.
            continue;
        }
        if let Some(reason) = &lost {
            // The batch transaction is gone (below): never run a job outside
            // it, where its SAVEPOINT would autocommit on its own.
            (job.finish)(Err(StoreError::Backend(reason.clone())));
            continue;
        }
        let mut ctx = WriterCtx {
            store,
            hooks: Vec::new(),
        };
        let work = job.work;
        let result = in_savepoint(conn, "job", || {
            std::panic::catch_unwind(AssertUnwindSafe(|| work(&mut ctx))).unwrap_or_else(|_| {
                Err(StoreError::Backend(
                    "a store writer job panicked; its write was rolled back".to_string(),
                ))
            })
        });
        let hooks = if result.is_ok() {
            ctx.hooks
        } else {
            Vec::new()
        };
        ran.push((job.finish, result, hooks));
        if conn.is_autocommit() {
            // SQLite rolled the whole transaction back on its own (a full
            // disk, an I/O error): every job that ran in it is undone, and
            // nothing may run until the next batch opens a new one.
            lost = Some(
                "the store writer's batch transaction was rolled back by SQLite (a full disk \
                 or an I/O error), so this write was not applied"
                    .to_string(),
            );
        }
    }
    if let Some(reason) = lost {
        for (finish, _result, _hooks) in ran {
            finish(Err(StoreError::Backend(reason.clone())));
        }
        return;
    }
    match conn.execute_batch("COMMIT") {
        Ok(()) => {
            stats.batches.fetch_add(1, Ordering::Relaxed);
            stats.jobs.fetch_add(ran.len() as u64, Ordering::Relaxed);
            for (finish, result, hooks) in ran {
                for hook in hooks {
                    // A panicking hook must not take the writer thread down
                    // (every later write in the process would fail); the
                    // commit stands either way.
                    let _ = std::panic::catch_unwind(AssertUnwindSafe(hook));
                }
                finish(result);
            }
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            let err = super::sqlite::backend(err);
            for (finish, _result, _hooks) in ran {
                finish(Err(batch_failure(&err)));
            }
        }
    }
}

/// Claim a job for this batch; `false` if its caller already abandoned it.
fn take(job: &Job) -> bool {
    job.state
        .compare_exchange(PENDING, TAKEN, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

#[cfg(test)]
mod tests;
