//! The exporter task (AD-21): the bus-drain loop that subscribes read-only
//! to the engine event bus, folds events into the [`MetricState`] through a
//! BOUNDED queue (drop-oldest with a surfaced note), and flushes on the
//! flush interval with bounded POSTs — plus the shutdown handshake that
//! stops the exporter before/with the Control API, attempts one bounded
//! final flush, and never blocks the host's exit past the bound.
//!
//! Never-blocks-supervision discipline (the AD-21 liveness rule, mirrored
//! from the bus contract): a `Lagged(n)` from the bus is a SURFACED drop
//! count (note + `hekma.otlp.dropped`), never an error into supervision; a
//! queue overflow drops the OLDEST event with a surfaced note; a terminal
//! delivery failure surfaces a diagnostic and counts the batch; every POST
//! is timeout-bounded. The exporter holds NO engine lock (`subscribe()` is
//! lock-free by construction) and touches no durable state — the outbox
//! stays the only durable export surface.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::sync::{broadcast, watch};

use hekma_engine::{DiagnosticSink, EngineEvent};

use super::client::{self, OtlpClient, PostOutcome};
use super::payload::{
    unix_nanos_now, MetricState, EXPOSITION_NAME, EXPOSITION_VERSION, MAX_RUNS_PER_INSTANCE,
};
use crate::Notifier;

/// The bounded queue capacity: the engine bus's own
/// [`EVENT_BUS_CAPACITY`](hekma_engine::EVENT_BUS_CAPACITY) shape — a flat,
/// known ceiling; overflow drops the OLDEST event, keeping memory flat under
/// collector outage.
const QUEUE_CAPACITY: usize = 1024;

/// The bounded final-flush budget at shutdown: the export stops, one last
/// cumulative snapshot is attempted, and the host's exit is never held past
/// this bound. `pub` because the serve wiring awaits the exporter's shutdown
/// with THIS bound (P7) — the two must never drift apart as magic numbers.
pub const FINAL_FLUSH_BOUND: Duration = Duration::from_secs(2);

/// The running exporter: the shutdown signal and the task handle — the
/// host's shutdown path awaits this beside the Control API's shutdown.
pub struct OtlpExporter {
    shutdown_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<u64>,
}

/// Everything the task needs — built once at serve time.
pub struct ExporterConfig {
    /// The validated endpoint (the POST URL derives from it).
    pub endpoint: String,
    /// The startup-RESOLVED headers (secret references already turned into
    /// values — they live only inside the client, never in a log).
    pub headers: Vec<(String, String)>,
    /// The flush interval (validated bounds, default 10s).
    pub flush_interval: Duration,
    /// The resource's `service.instance.id` (the state-dir lock identity).
    pub service_instance_id: String,
}

impl OtlpExporter {
    /// Subscribe-side handle is the caller's business (the host subscribes
    /// and hands the receiver in). Spawn the export task on the host's
    /// runtime — MUST be called inside the runtime context (the host wraps
    /// it in `rt.block_on`). Returns the handle the shutdown path awaits.
    pub fn start(
        rx: broadcast::Receiver<EngineEvent>,
        config: ExporterConfig,
        sink: DiagnosticSink,
    ) -> Self {
        let notifier = Notifier::over(sink);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run(rx, config, shutdown_rx, notifier));
        OtlpExporter { shutdown_tx, task }
    }

    /// Stop the exporter: fire the shutdown signal and await the task —
    /// including its bounded final flush — bounded by the caller's bound so
    /// the host's exit is never blocked past it (the AD-21 shutdown rule).
    /// Returns the honest in-flight count the task reported (the points the
    /// final flush did NOT deliver; cumulative sums reset at restart), or
    /// `None` when the task could not be awaited within the bound.
    pub async fn shutdown(self, bound: Duration) -> Option<u64> {
        let _ = self.shutdown_tx.send(true);
        match tokio::time::timeout(bound, self.task).await {
            Ok(Ok(lost)) => Some(lost),
            // Timed out or the task panicked: the export surface is gone,
            // supervision is unaffected; the count is unknown.
            _ => None,
        }
    }
}

/// The task body: drain → queue → flush loop with the cancel-safe shutdown
/// race (the sse.rs pattern: both `recv` and `changed` are cancel-safe, so
/// the `select!` loses nothing on either side). Returns the in-flight count
/// the shutdown path reports honestly.
async fn run(
    mut rx: broadcast::Receiver<EngineEvent>,
    config: ExporterConfig,
    mut shutdown: watch::Receiver<bool>,
    notifier: Notifier,
) -> u64 {
    let client = match OtlpClient::new(&config.endpoint, config.headers) {
        Ok(client) => client,
        Err(message) => {
            // Unreachable in practice (the endpoint is validated at option
            // parse time) — but a surfaced diagnostic beats a silent export
            // stop (AI-18), and the task ends without touching supervision.
            notifier.note(&format!(
                "otlp export DOWN before it started ({message}) — supervision continues"
            ));
            return 0;
        }
    };
    let mut state = MetricState::new(unix_nanos_now(), config.service_instance_id);
    let mut interval = tokio::time::interval(config.flush_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the interval's immediate first tick: the exporter must not
    // flush an (empty) batch the moment it starts.
    interval.tick().await;
    notifier.note(&format!(
        "otlp export enabled ({EXPOSITION_NAME} {EXPOSITION_VERSION}): POSTing metrics \
         to {} every {}s (metrics-only — no agent content, no secrets)",
        client.post_url(),
        config.flush_interval.as_secs()
    ));
    let mut queue: VecDeque<EngineEvent> = VecDeque::with_capacity(QUEUE_CAPACITY);
    // The overflow note's per-flush-cycle flag (P2b): true once the queue-full
    // note has fired this cycle; the flush below resets it — one line per
    // cycle, an exact counter always.
    let mut overflow_noted = false;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                // The host is shutting down: stop BEFORE/WITH the Control
                // API, then run the bounded final flush below.
                break;
            }
            event = rx.recv() => match event {
                Ok(event) => push(&mut queue, event, &mut state, &notifier, &mut overflow_noted),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Surfaced drop count (AI-18): the note AND the
                    // self-metric; the receiver resyncs at the tail and the
                    // loop CONTINUES — never an error into supervision.
                    state.note_dropped_events(n);
                    notifier.note(&format!(
                        "otlp export: the event bus lagged — {n} committed event(s) \
                         were not exported (still visible in the durable logs; the \
                         hekma.otlp.dropped counter grew)"
                    ));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // The engine (and its senders) are gone: no further
                    // events will ever arrive. End the loop — the host is
                    // tearing down anyway.
                    break;
                }
            },
            _ = interval.tick() => {
                drain_bus(&mut rx, &mut queue, &mut state, &notifier, &mut overflow_noted);
                flush(&client, &mut queue, &mut state, &notifier).await;
                // A new flush cycle opens: the next overflow may note again.
                overflow_noted = false;
            }
        }
    }

    // The bounded final flush: drain the bus tail best-effort, then deliver
    // once (retries included) inside the bound — the host's exit is never
    // held past it. Whether the flush LANDED decides how the in-flight
    // count is reported (AI-18: the restart note names the loss).
    drain_bus(
        &mut rx,
        &mut queue,
        &mut state,
        &notifier,
        &mut overflow_noted,
    );
    // Fold the drained tail BEFORE the bound starts (P2a): the fold is pure
    // CPU over a flat queue, and the in-flight count the timeout note
    // reports must INCLUDE the tail — capturing it before the fold would
    // undercount exactly the events the final flush was supposed to carry.
    for event in queue.drain(..) {
        state.observe(&event);
    }
    let in_flight_at_stop = state.in_flight();
    let flush = tokio::time::timeout(
        FINAL_FLUSH_BOUND,
        flush(&client, &mut queue, &mut state, &notifier),
    );
    if flush.await.is_err() {
        notifier.note(&format!(
            "otlp export stopped: the final flush did not complete within the {}ms \
             bound — {in_flight_at_stop} in-flight observation(s) were NOT delivered; \
             cumulative sums reset at restart (documented restart-reset)",
            FINAL_FLUSH_BOUND.as_millis()
        ));
        return in_flight_at_stop;
    }
    let lost = state.in_flight();
    let total_dropped = state.dropped();
    if lost > 0 {
        notifier.note(&format!(
            "otlp export stopped: the final flush did not deliver — {lost} in-flight \
             observation(s) were NOT delivered; cumulative sums reset at restart \
             (documented restart-reset)"
        ));
    } else {
        notifier.note(&format!(
            "otlp export stopped cleanly (lifetime dropped-event counter: \
             {total_dropped}; cumulative sums reset at restart — the documented \
             restart-reset)"
        ));
    }
    lost
}

/// Push one event into the bounded queue, dropping the OLDEST on overflow —
/// surfaced (AI-18) through the self-metric on EVERY drop, but the stderr
/// NOTE at most once per flush cycle (P2b): a sustained collector outage
/// fills the queue and would otherwise print one line per incoming event.
/// `noted_this_cycle` is the cycle's flag — the flush loop resets it after
/// each flush; the counter stays exact either way.
fn push(
    queue: &mut VecDeque<EngineEvent>,
    event: EngineEvent,
    state: &mut MetricState,
    notifier: &Notifier,
    noted_this_cycle: &mut bool,
) {
    if queue.len() >= QUEUE_CAPACITY {
        state.note_dropped_events(1);
        drop(queue.pop_front());
        if !*noted_this_cycle {
            *noted_this_cycle = true;
            notifier.note(
                "otlp export: the export queue is full — the OLDEST event was dropped \
                 (every further overflow this flush cycle is counted in \
                 hekma.otlp.dropped without a repeat note); collector delivery \
                 may be falling behind event volume",
            );
        }
    }
    queue.push_back(event);
}

/// Non-blocking tail drain: fold everything currently buffered on the bus
/// into the queue (through the same bounded push), honoring Lagged markers.
fn drain_bus(
    rx: &mut broadcast::Receiver<EngineEvent>,
    queue: &mut VecDeque<EngineEvent>,
    state: &mut MetricState,
    notifier: &Notifier,
    noted_this_cycle: &mut bool,
) {
    loop {
        match rx.try_recv() {
            Ok(event) => push(queue, event, state, notifier, noted_this_cycle),
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                state.note_dropped_events(n);
                notifier.note(&format!(
                    "otlp export: the event bus lagged — {n} committed event(s) were \
                     not exported (still visible in the durable logs; the \
                     hekma.otlp.dropped counter grew)"
                ));
            }
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => {
                return;
            }
        }
    }
}

/// Fold the queued events into the cumulative state and deliver ONE batch:
/// observe → snapshot → bounded POST with the bounded retry budget. A
/// delivered batch clears the in-flight count; a failed batch surfaces a
/// diagnostic and counts itself; supervision is never touched.
async fn flush(
    client: &OtlpClient,
    queue: &mut VecDeque<EngineEvent>,
    state: &mut MetricState,
    notifier: &Notifier,
) {
    for event in queue.drain(..) {
        state.observe(&event);
    }
    // The run-window eviction note (P5): ONE surfaced note per flush cycle
    // naming each instance's evicted-run count — the window is bounded by
    // design, and the eviction is visible, not silent.
    for (instance, count) in state.take_evicted_run_series() {
        notifier.note(&format!(
            "otlp export: the token-usage run window for {instance} is full — \
             {count} run series beyond the {MAX_RUNS_PER_INSTANCE}-run bound were \
             evicted from the live payload (the durable ledger keeps every run; \
             this is a bounded window, not a drop)"
        ));
    }
    let payload = serde_json::to_string(&state.snapshot(unix_nanos_now()))
        // Unreachable for the plain derive structs; an honest empty-batch
        // skip beats a panic in the export path.
        .unwrap_or_default();
    if payload.is_empty() {
        return;
    }
    match client::deliver(client, payload).await {
        PostOutcome::Delivered => state.mark_delivered(),
        PostOutcome::DeliveredWithRejections {
            rejected_points,
            error_message,
        } => {
            // A 200 whose partialSuccess REJECTED points (P3b): the batch
            // itself landed, so in-flight clears — but the rejected points
            // are counted in the self-metric and named in a note, never
            // silent (a collector that silently loses points is a drop).
            state.mark_delivered();
            state.note_rejected_points(rejected_points);
            notifier.note(&format!(
                "otlp export: the collector accepted the batch but REJECTED \
                 {rejected_points} point(s) (partialSuccess: {error_message}) — the \
                 hekma.otlp.dropped counter grew; check the collector's acceptance \
                 policy for the otlp-metrics v1 families"
            ));
        }
        PostOutcome::Retryable(reason) | PostOutcome::Terminal(reason) => {
            // Terminal-for-the-batch AND exhausted-retryable both land here:
            // the batch is dropped THIS flush; the cumulative state is NOT
            // lost (the next flush re-exports it); the drop is visible.
            state.note_dropped_batch();
            notifier.note(&format!(
                "otlp export: the metrics batch was NOT delivered ({reason}) — the \
                 next flush re-exports the cumulative state; the \
                 hekma.otlp.dropped counter grew; supervision is unaffected"
            ));
        }
    }
}

#[cfg(test)]
mod tests;
