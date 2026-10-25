//! Unit tests for the exporter task: the bounded queue's drop-oldest
//! accounting (surfaced note + self-metric) plus the loop-level arms driven
//! directly against [`OtlpExporter::start`] on hand-built event-bus
//! receivers — the lagged bus, the closed-bus final fold, the refused and
//! stalled collectors at the bounded final flush, the partialSuccess
//! rejection (P3b), and the run-window eviction note (P5). The serve-time
//! end-to-end matrix stays in `crates/hekma-host/tests/otlp_export.rs`
//! against a real host; these legs pin the exact loop arms deterministically
//! instead of by racing a host's shutdown.
//!
//! OS-neutral by construction: no OS-conditional compilation anywhere — the
//! collectors are plain `std::net` loopback listeners and every event is a
//! hand-built engine value (no child processes).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hekma_engine::EngineEvent;

use super::{push, ExporterConfig, OtlpExporter, FINAL_FLUSH_BOUND, QUEUE_CAPACITY};
use crate::otlp::payload::MetricState;
use crate::Notifier;

/// A sink that records every notice (the serve.rs suite's shape).
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl SharedSink {
    fn notifier() -> (Self, Notifier) {
        let (shared, sink) = Self::sink();
        (shared, Notifier::over(sink))
    }

    /// The raw engine sink (the loop legs hand it straight to
    /// [`OtlpExporter::start`], which wraps its own notifier).
    fn sink() -> (Self, hekma_engine::DiagnosticSink) {
        let shared = Self(Arc::new(Mutex::new(Vec::new())));
        let sink: hekma_engine::DiagnosticSink = Arc::new(Mutex::new(Box::new(shared.clone())));
        (shared, sink)
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

/// A committed usage update for one run of `instance` (the engine's own
/// sample-event shape; `cached_tokens: Some(0)` keeps every token arm real).
fn usage(instance: &str, run: &str, seq: u64) -> EngineEvent {
    use hekma_engine::{RunId, UsageEvent, UsageUpdateEvent};
    EngineEvent::UsageUpdate(UsageUpdateEvent::new(UsageEvent {
        instance: instance.to_string(),
        run_id: RunId::from_wire(run),
        input_tokens: 10,
        output_tokens: 20,
        cached_tokens: Some(0),
        metering_source: "self-reported".to_string(),
        sequence: seq,
        occurred_at: "2026-10-25T00:00:00Z".to_string(),
    }))
}

#[test]
fn the_queue_holds_capacity_events_before_dropping_anything() {
    let (sink, notifier) = SharedSink::notifier();
    let mut queue = VecDeque::new();
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
    let mut queue = VecDeque::new();
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
    let mut queue = VecDeque::new();
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

// ---------------------------------------------------------------------------
// The loop arms, driven directly: every leg starts the real task over a
// hand-built bus receiver whose sender is DROPPED after the events are
// sent — `broadcast` then delivers the buffered events in order before it
// reports the closed bus, so each leg's path through the select loop is
// deterministic on every OS (no racing a host's shutdown).
// ---------------------------------------------------------------------------

/// The exporter config for the loop legs: a LONG flush interval (300s) so
/// the interval tick never fires mid-test — every delivery below is the
/// shutdown handshake's bounded final flush, exactly the surface under pin.
fn loop_config(endpoint: String) -> ExporterConfig {
    ExporterConfig {
        endpoint,
        headers: Vec::new(),
        flush_interval: Duration::from_secs(300),
        service_instance_id: "task-loop-test".to_string(),
    }
}

/// One recorded POST at a driven collector.
#[derive(Clone, Debug)]
struct Recorded {
    body: String,
}

/// Read ONE HTTP/1.1 request (headers + Content-Length body) off `stream`.
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return None,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return None;
    }
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_ok() {
        Some(String::from_utf8_lossy(&body).to_string())
    } else {
        None
    }
}

/// A collector that answers every POST with `status` and `body`.
fn answering_collector(status: &str, body: &str) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the answering collector");
    let addr = listener.local_addr().unwrap();
    let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded_thread = Arc::clone(&recorded);
    let status = status.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream);
            let recorded = Arc::clone(&recorded_thread);
            let status = status.clone();
            let body = body.clone();
            std::thread::spawn(move || {
                if let Some(text) = request {
                    recorded.lock().unwrap().push(Recorded { body: text });
                }
                let response = format!(
                    "HTTP/1.1 {status} ANSWER\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    (format!("http://{addr}"), recorded)
}

/// A collector that accepts the connection, reads the POST, and then never
/// answers — the stalled-collector shape the final-flush bound exists for.
/// The serving thread parks in a blocking read until the process ends.
fn stalled_collector() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stalled collector");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                // Drain the request bytes, then hold the socket open in a
                // blocking read — no response is ever written.
                let mut buffer = [0u8; 4096];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => continue,
                    }
                }
            });
        }
    });
    format!("http://{addr}")
}

/// A port where nothing listens anymore (the e2e suite's refused shape).
fn refused_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

/// An exporter over a bus that already carries `events`, with the sender
/// dropped — the task observes every event, then `Closed`, then runs the
/// bounded final flush.
async fn exporter_over(
    events: Vec<EngineEvent>,
    capacity: usize,
    endpoint: String,
    sink: hekma_engine::DiagnosticSink,
) -> OtlpExporter {
    let (tx, rx) = tokio::sync::broadcast::channel::<EngineEvent>(capacity);
    for event in events {
        let _ = tx.send(event);
    }
    drop(tx);
    OtlpExporter::start(rx, loop_config(endpoint), sink)
}

/// Let the spawned task run to its natural quiescence (every buffered event
/// observed, the closed bus noticed, the final flush in flight) before the
/// shutdown handshake runs.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// The defensive client-construction arm: an endpoint the parse-time
/// validation would refuse (https:// is the v1 deferral) still surfaces a
/// DOWN note instead of a silent export stop (AI-18).
#[tokio::test]
async fn an_endpoint_the_client_rejects_surfaces_export_down_and_returns_zero() {
    let (sink_handle, sink) = SharedSink::sink();
    let exporter = exporter_over(
        Vec::new(),
        16,
        "https://collector.example".to_string(),
        sink,
    )
    .await;
    settle().await;
    let lost = exporter.shutdown(Duration::from_secs(5)).await;
    assert_eq!(
        lost,
        Some(0),
        "nothing was ever observed, so nothing is lost"
    );
    let text = sink_handle.text();
    assert!(
        text.contains("otlp export DOWN before it started"),
        "the refusal is surfaced, not silent:\n{text}"
    );
    assert!(
        text.contains("supervision continues"),
        "the note names the liveness rule:\n{text}"
    );
}

/// The lagged bus (AI-18): a capacity-4 bus fed 10 events reports Lagged(6)
/// first — the drop count is surfaced with the self-metric — then the four
/// RETAINED events are folded and delivered by the bounded final flush, and
/// the closed bus ends the loop with a clean stop.
#[tokio::test]
async fn a_lagged_bus_surfaces_the_drop_count_then_folds_and_delivers_the_retained_tail() {
    let (endpoint, recorded) = answering_collector("200 OK", "");
    let (sink_handle, sink) = SharedSink::sink();
    let events: Vec<EngineEvent> = (0..10).map(transition).collect();
    let exporter = exporter_over(events, 4, endpoint, sink).await;
    settle().await;
    let lost = exporter.shutdown(Duration::from_secs(5)).await;
    assert_eq!(lost, Some(0), "the retained tail was delivered whole");

    let text = sink_handle.text();
    assert!(
        text.contains("the event bus lagged — 6 committed event(s)"),
        "the missed count is surfaced exactly:\n{text}"
    );
    assert!(
        text.contains("otlp export stopped cleanly"),
        "a delivered final flush reports the clean stop:\n{text}"
    );

    // The delivered batch carries the RETAINED (newest four) transitions —
    // the fold really happened, not an empty snapshot.
    let requests = recorded.lock().unwrap().clone();
    assert!(
        requests
            .iter()
            .any(|r| r.body.contains("hekma.agent.state")),
        "the final POST carries the folded agent-state series: {:?}",
        requests.iter().map(|r| r.body.as_str()).collect::<Vec<_>>()
    );
}

/// The refused collector at the final flush: the fold completes, delivery
/// is TERMINAL (connection refused never becomes retryable), and the
/// in-flight count is reported honestly as NOT delivered (AI-18) — the
/// handshake still returns within its bound.
#[tokio::test]
async fn a_refused_collector_at_shutdown_reports_the_undelivered_in_flight_count() {
    let (sink_handle, sink) = SharedSink::sink();
    let exporter = exporter_over(vec![transition(0)], 16, refused_endpoint(), sink).await;
    settle().await;
    let lost = exporter.shutdown(Duration::from_secs(5)).await;
    assert_eq!(
        lost,
        Some(1),
        "the single observed event is the honest loss"
    );
    let text = sink_handle.text();
    assert!(
        text.contains("the final flush did not deliver — 1 in-flight"),
        "the loss is named with its count:\n{text}"
    );
    assert!(
        text.contains("restart-reset"),
        "the note names the documented reset semantics:\n{text}"
    );
}

/// The stalled final flush: a collector that never answers trips the task's
/// OWN [`FINAL_FLUSH_BOUND`] — the bound, not the client's POST timeout, is
/// what releases the host's exit — and the note names both the bound and
/// the undelivered count.
#[tokio::test]
async fn a_stalled_final_flush_is_released_by_the_task_bound_and_reports_the_loss() {
    let (sink_handle, sink) = SharedSink::sink();
    let events = vec![transition(0), transition(1)];
    let exporter = exporter_over(events, 16, stalled_collector(), sink).await;
    settle().await;
    // The caller's bound is generous: the TASK's own bound must fire first.
    let lost = exporter.shutdown(Duration::from_secs(10)).await;
    assert_eq!(lost, Some(2), "both observed events are the reported loss");
    let text = sink_handle.text();
    assert!(
        text.contains(&format!(
            "the final flush did not complete within the {}ms bound — 2 in-flight",
            FINAL_FLUSH_BOUND.as_millis()
        )),
        "the bound fires and is named in the note:\n{text}"
    );
}

/// The caller-side bound: when the task cannot finish within the CALLER's
/// bound, the handshake returns `None` — the count is unknown, and the
/// host's exit was still never held past the bound.
#[tokio::test]
async fn a_shutdown_bound_shorter_than_the_final_flush_reports_none() {
    let (sink_handle, sink) = SharedSink::sink();
    let exporter = exporter_over(vec![transition(0)], 16, stalled_collector(), sink).await;
    settle().await;
    let lost = exporter.shutdown(Duration::from_millis(100)).await;
    assert_eq!(lost, None, "an unawaitable task reports an unknown count");
    // The task keeps running its bounded final flush detached; nothing more
    // to assert here — the None IS the contract under pin.
    let _ = sink_handle;
}

/// The partialSuccess rejection (P3b): a 200 whose body rejects points
/// still clears in-flight (the batch LANDED), but the rejected points are
/// counted and named — a collector that silently loses points is a drop.
#[tokio::test]
async fn a_partial_success_rejection_clears_in_flight_and_names_the_rejected_points() {
    let (endpoint, _recorded) = answering_collector(
        "200 OK",
        r#"{"partialSuccess":{"rejectedMetricPoints":7,"errorMessage":"policy: family not accepted"}}"#,
    );
    let (sink_handle, sink) = SharedSink::sink();
    let exporter = exporter_over(vec![transition(0)], 16, endpoint, sink).await;
    settle().await;
    let lost = exporter.shutdown(Duration::from_secs(5)).await;
    assert_eq!(lost, Some(0), "the batch landed — in-flight clears");
    let text = sink_handle.text();
    assert!(
        text.contains("REJECTED 7 point(s)"),
        "the rejected count is surfaced:\n{text}"
    );
    assert!(
        text.contains("policy: family not accepted"),
        "the collector's error message is named verbatim:\n{text}"
    );
    assert!(
        text.contains("otlp export stopped cleanly"),
        "a landed batch is a clean stop even with rejections:\n{text}"
    );
}

/// The run-window eviction note (P5): [`MAX_RUNS_PER_INSTANCE`] + 2 distinct
/// runs for one instance evict exactly 2 series at the fold — the note
/// names the instance, the count, and the bound, once, at the flush.
#[tokio::test]
async fn a_full_run_window_evicts_the_oldest_series_and_names_them_at_the_flush() {
    use crate::otlp::payload::MAX_RUNS_PER_INSTANCE;
    let (endpoint, _recorded) = answering_collector("200 OK", "");
    let (sink_handle, sink) = SharedSink::sink();
    let events: Vec<EngineEvent> = (0..MAX_RUNS_PER_INSTANCE as u64 + 2)
        .map(|seq| usage("evictor", &format!("run-evict-{seq}"), seq))
        .collect();
    let exporter = exporter_over(events, 128, endpoint, sink).await;
    settle().await;
    let lost = exporter.shutdown(Duration::from_secs(5)).await;
    assert_eq!(
        lost,
        Some(0),
        "evictions are a window, not drops — all delivered"
    );
    let text = sink_handle.text();
    assert!(
        text.contains("run series beyond the 64-run bound were evicted"),
        "the eviction note names the bound:\n{text}"
    );
    assert!(
        text.contains("the token-usage run window for evictor is full — 2 run series"),
        "exactly TWO series (66 observed − 64 kept) are named:\n{text}"
    );
}
