//! The per-Run hold queues (story 23-4, spine AD-26).
//!
//! A call whose Run (or an ancestor) holds a hold-class reason takes a
//! ticket in its Run's queue and waits. A parked call re-reads the
//! published snapshot on a short tick — the snapshot is the truth; there
//! is no separate wake path to miss — and leaves only when the Run is no
//! longer held AND its ticket is at the front, so released calls start in
//! arrival order. Each queue holds at most `cap` calls; past that, the
//! caller answers the retryable hold-full refusal. A ticket leaves its
//! queue when dropped (a released call, or a dropped connection).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::domain::RunId;

/// How often a parked call re-reads the snapshot.
pub(crate) const RECHECK: std::time::Duration = std::time::Duration::from_millis(250);

/// The default most calls one Run may have parked.
pub const DEFAULT_MAX_HELD_PER_RUN: usize = 32;

#[derive(Debug, Default)]
struct Queue {
    /// Tickets in arrival order, with each one's start time.
    order: VecDeque<(u64, String)>,
    next: u64,
}

/// Every Run's hold queue.
#[derive(Debug)]
pub(crate) struct HoldQueues {
    cap: usize,
    queues: Mutex<HashMap<RunId, Queue>>,
    /// Runs whose held figures changed since the last report.
    dirty: Mutex<Vec<RunId>>,
    /// Tokio's notify for "a ticket left": waiting callers re-check at once.
    changed: tokio::sync::Notify,
}

impl HoldQueues {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            queues: Mutex::new(HashMap::new()),
            dirty: Mutex::new(Vec::new()),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn mark(&self, run_id: &RunId) {
        let mut dirty = self.dirty.lock().unwrap_or_else(|p| p.into_inner());
        if !dirty.contains(run_id) {
            dirty.push(run_id.clone());
        }
    }

    /// Take a ticket in `run_id`'s queue, or `None` when it is full.
    pub(crate) fn enter(self: &Arc<Self>, run_id: &RunId, started_at: &str) -> Option<Ticket> {
        let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
        let queue = queues.entry(run_id.clone()).or_default();
        if queue.order.len() >= self.cap {
            return None;
        }
        let id = queue.next;
        queue.next += 1;
        queue.order.push_back((id, started_at.to_string()));
        drop(queues);
        self.mark(run_id);
        Some(Ticket {
            queues: Arc::clone(self),
            run_id: run_id.clone(),
            id,
        })
    }

    /// The Run's parked count and its oldest parked call's start.
    pub(crate) fn stats(&self, run_id: &RunId) -> (usize, Option<String>) {
        let queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
        queues.get(run_id).map_or((0, None), |queue| {
            (
                queue.order.len(),
                queue.order.front().map(|(_, at)| at.clone()),
            )
        })
    }

    /// The Runs whose figures changed since the last call, with their
    /// figures now.
    pub(crate) fn take_reports(&self) -> Vec<(RunId, usize, Option<String>)> {
        let dirty = std::mem::take(&mut *self.dirty.lock().unwrap_or_else(|p| p.into_inner()));
        dirty
            .into_iter()
            .map(|run_id| {
                let (count, oldest) = self.stats(&run_id);
                (run_id, count, oldest)
            })
            .collect()
    }

    /// Wait for a ticket to leave somewhere, or the re-check tick.
    pub(crate) async fn wait(&self) {
        let _ = tokio::time::timeout(RECHECK, self.changed.notified()).await;
    }
}

/// A parked call's place in its Run's queue; leaving on drop.
#[derive(Debug)]
pub(crate) struct Ticket {
    queues: Arc<HoldQueues>,
    run_id: RunId,
    id: u64,
}

impl Ticket {
    /// Whether this ticket is at the front of its queue.
    pub(crate) fn is_front(&self) -> bool {
        let queues = self.queues.queues.lock().unwrap_or_else(|p| p.into_inner());
        queues
            .get(&self.run_id)
            .and_then(|queue| queue.order.front())
            .is_some_and(|(id, _)| *id == self.id)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut queues = self.queues.queues.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(queue) = queues.get_mut(&self.run_id) {
            queue.order.retain(|(id, _)| *id != self.id);
            if queue.order.is_empty() {
                queues.remove(&self.run_id);
            }
        }
        drop(queues);
        self.queues.mark(&self.run_id);
        self.queues.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_queue_in_arrival_order_up_to_the_cap_and_leave_on_drop() {
        let queues = Arc::new(HoldQueues::new(2));
        let run = RunId::from_wire("run_A");
        let first = queues.enter(&run, "t1").unwrap();
        let second = queues.enter(&run, "t2").unwrap();
        assert!(queues.enter(&run, "t3").is_none(), "the cap");
        assert!(first.is_front() && !second.is_front());
        assert_eq!(queues.stats(&run), (2, Some("t1".to_string())));
        drop(first);
        assert!(second.is_front());
        assert_eq!(queues.stats(&run), (1, Some("t2".to_string())));
        let reports = queues.take_reports();
        assert_eq!(reports, vec![(run.clone(), 1, Some("t2".to_string()))]);
        assert!(queues.take_reports().is_empty());
        drop(second);
        assert_eq!(queues.stats(&run), (0, None));
        // Another Run's queue is its own.
        let other = RunId::from_wire("run_B");
        let _b = queues.enter(&other, "t").unwrap();
        assert_eq!(queues.stats(&run), (0, None));
        assert_eq!(HoldQueues::new(0).cap, 1);
    }
}
