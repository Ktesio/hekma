//! Unit tests for the exporter task's pure pieces: the bounded queue's
//! drop-oldest accounting (surfaced note + self-metric) — the async loop's
//! end-to-end behavior is pinned by `crates/hekma-host/tests/otlp_export.rs`
//! against a real in-test collector.

use std::io::Write;
use std::sync::{Arc, Mutex};

use hekma_engine::EngineEvent;

use super::{push, QUEUE_CAPACITY};
use crate::otlp::payload::MetricState;
use crate::Notifier;

/// A sink that records every notice (the serve.rs suite's shape).
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl SharedSink {
    fn notifier() -> (Self, Notifier) {
        let shared = Self(Arc::new(Mutex::new(Vec::new())));
        let sink: hekma_engine::DiagnosticSink = Arc::new(Mutex::new(Box::new(shared.clone())));
        (shared, Notifier::over(sink))
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn transition(seq: u64) -> EngineEvent {
    use hekma_engine::{LifecycleState, TransitionCause, TransitionEvent};
    EngineEvent::Transition(TransitionEvent::new(
        format!("probe-{seq}"),
        LifecycleState::Registered,
        LifecycleState::Starting,
        TransitionCause::command("start"),
        "2026-10-25T00:00:00Z",
    ))
}

#[test]
fn the_queue_holds_capacity_events_before_dropping_anything() {
    let (sink, notifier) = SharedSink::notifier();
    let mut queue = std::collections::VecDeque::new();
    let mut state = MetricState::new(0, "test".to_string());
    let mut noted = false;
    for seq in 0..QUEUE_CAPACITY as u64 {
        push(
            &mut queue,
            transition(seq),
            &mut state,
            &notifier,
            &mut noted,
        );
    }
    assert_eq!(queue.len(), QUEUE_CAPACITY);
    assert_eq!(
        state.dropped(),
        0,
        "at capacity nothing has been dropped yet"
    );
    assert!(!sink.text().contains("OLDEST"), "no note before overflow");
}

#[test]
fn overflow_drops_the_oldest_with_a_surfaced_note_and_a_counter() {
    let (sink, notifier) = SharedSink::notifier();
    let mut queue = std::collections::VecDeque::new();
    let mut state = MetricState::new(0, "test".to_string());
    let mut noted = false;
    for seq in 0..(QUEUE_CAPACITY + 3) as u64 {
        push(
            &mut queue,
            transition(seq),
            &mut state,
            &notifier,
            &mut noted,
        );
    }
    assert_eq!(
        queue.len(),
        QUEUE_CAPACITY,
        "memory stays flat at the bound"
    );
    assert_eq!(state.dropped(), 3, "one drop per overflowed event");
    // AI-18: the overflow is SURFACED, not silent.
    let text = sink.text();
    assert!(text.contains("OLDEST"), "the drop note is emitted:\n{text}");
    assert!(text.contains("dropped"), "the note names the drop:\n{text}");
    // The kept events are the NEWEST (drop-OLDEST).
    let front = match queue.front().unwrap() {
        EngineEvent::Transition(t) => t.instance.clone(),
        other => panic!("unexpected event {other:?}"),
    };
    assert_eq!(front, "probe-3", "the three oldest events were dropped");
}

#[test]
fn the_overflow_note_is_rate_limited_to_once_per_flush_cycle() {
    // P2b: a sustained outage must not print one stderr line per event —
    // the FIRST overflow of a cycle notes; the rest only grow the counter.
    let (sink, notifier) = SharedSink::notifier();
    let mut queue = std::collections::VecDeque::new();
    let mut state = MetricState::new(0, "test".to_string());
    let mut noted = false;
    for seq in 0..(QUEUE_CAPACITY + 50) as u64 {
        push(
            &mut queue,
            transition(seq),
            &mut state,
            &notifier,
            &mut noted,
        );
    }
    assert_eq!(state.dropped(), 50, "the counter stays exact");
    let text = sink.text();
    let note_count = text.matches("OLDEST").count();
    assert_eq!(
        note_count, 1,
        "exactly ONE overflow note per flush cycle:\n{text}"
    );
    // The cycle resets: the next cycle's first overflow notes again.
    noted = false;
    push(
        &mut queue,
        transition(QUEUE_CAPACITY as u64 + 50),
        &mut state,
        &notifier,
        &mut noted,
    );
    let text = sink.text();
    assert_eq!(
        text.matches("OLDEST").count(),
        2,
        "a NEW cycle notes again (still once per cycle):\n{text}"
    );
}
