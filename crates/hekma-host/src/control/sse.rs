//! The `GET /v0/events` SSE stream (story 17-3): ONE `Engine::subscribe()`
//! receiver per connected client, each engine event rendered as an SSE
//! `data:` frame whose payload is the engine's own kind-tagged JSON
//! (`{"kind":"transition",...}` — the exact wire shape the resync pull and
//! the engine's bus carry, never a re-shape).
//!
//! The slow-subscriber policy is SURFACED, not silent (AI-18): when this
//! receiver falls more than the engine bus's capacity behind, the
//! `broadcast::Receiver` reports `Lagged(n)` — the stream emits one
//! Control-API-level `{"kind":"lagged","skipped":n}` frame naming the gap
//! and CONTINUES at the current tail (the skipped events stay readable in
//! the durable logs — the per-instance cursor pull is the lossless heal).
//! When the engine (and every sender) is gone, or the API's shutdown signal
//! fires, the stream ENDS — it never holds a connection into the host's
//! teardown (the spec's failure-mode row: a shutdown closes the connection,
//! the engine is unaffected).

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use tokio::sync::{broadcast, watch};

use super::ApiState;

/// The in-stream lag frame's `kind` token (Control-API-level vocabulary —
/// the engine's event kinds are `transition` / `budget_breach` /
/// `usage_update`; `lagged` is THIS surface's honest gap marker).
const LAGGED_KIND: &str = "lagged";

/// One SSE client: subscribe at the CURRENT tail (broadcast semantics — a
/// client sees only events published after its connect; the cursor pull is
/// the from-zero backfill) and translate receiver results into frames until
/// the channel closes OR the API shuts down. The shutdown arm has
/// `wait_for` SEMANTICS (review item 4): the current value is checked
/// FIRST (an ALREADY-closed signal — fired between the handler's watch
/// clone and this stream's first poll — ends the stream immediately), then
/// the next edge races `recv`. Both `recv` and `changed` are cancel-safe,
/// so the `select!` loses nothing on either side.
fn event_stream(
    rx: broadcast::Receiver<hekma_engine::EngineEvent>,
    shutdown: watch::Receiver<bool>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold((rx, shutdown), |(mut rx, mut shutdown)| async move {
        // `wait_for(|ready| *ready)` semantics, split so no borrow crosses
        // the unfold's state move: the CURRENT value answers immediately
        // (and marks it seen), so `changed()` below only waits for a FUTURE
        // edge — together they cover both the already-closed and the
        // fires-later case.
        if *shutdown.borrow_and_update() {
            return None;
        }
        tokio::select! {
            _ = shutdown.changed() => {
                // The API is going down: end the stream NOW so the
                // connection closes inside the graceful window (the abort
                // backstop never has to fire). A sender drop (a host panic)
                // reads the same way — the client's stream ends either way;
                // the engine is untouched.
                None
            }
            event = rx.recv() => match event {
                Ok(event) => {
                    // The engine payload, verbatim (kind-tagged, already
                    // schema-versioned inside). A serialize failure is not
                    // reachable for these plain structs; if it ever were, the
                    // frame stays an honest error marker, never a hang.
                    let payload = serde_json::to_string(&event).unwrap_or_else(|_| {
                        r#"{"kind":"internal","detail":"event serialization failed"}"#
                            .to_string()
                    });
                    Some((Ok(Event::default().data(payload)), (rx, shutdown)))
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // Surfaced gap (AI-18): name the skip, keep streaming.
                    let frame = serde_json::json!({
                        "kind": LAGGED_KIND,
                        "skipped": skipped,
                    })
                    .to_string();
                    Some((Ok(Event::default().data(frame)), (rx, shutdown)))
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // The engine is gone (its senders dropped): end the
                    // stream so a shutdown never strands a client's read.
                    None
                }
            }
        }
    })
}

/// The `GET /v0/events` handler — the SSE response. Keep-alive comments keep
/// an idle stream visible to intermediaries (loopback v0 has none that
/// matter, but a client-side idle read staying warm costs nothing).
pub(crate) async fn live_events(
    State(state): State<ApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.engine.subscribe();
    Sse::new(event_stream(rx, state.shutdown.clone())).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// A shutdown watch that has NOT fired (the streams-under-test end only
    /// on their own trigger, never on this one). The SENDER is returned too
    /// and must stay bound: a dropped sender CLOSES the watch, and
    /// `changed()` would resolve immediately — the "quiet" watch would win
    /// the select race against pending events (the tarpaulin run caught
    /// exactly that flip).
    fn quiet_shutdown() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

    /// The lag frame carries the gap name + count and nothing else — the
    /// in-stream contract a Host branches on.
    #[test]
    fn the_lag_frame_names_the_kind_and_the_skipped_count() {
        let frame = serde_json::json!({ "kind": LAGGED_KIND, "skipped": 7_u64 });
        assert_eq!(frame["kind"], "lagged");
        assert_eq!(frame["skipped"], 7);
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_engine_channel_closes() {
        let (tx, rx) = broadcast::channel::<hekma_engine::EngineEvent>(4);
        let (_shutdown_tx, shutdown_rx) = quiet_shutdown();
        let mut stream = event_stream(rx, shutdown_rx).boxed();
        drop(tx);
        // The only item the closed channel yields is the stream's END — no
        // frame, no hang.
        assert!(StreamExt::next(&mut stream).await.is_none());
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_api_shuts_down() {
        // The channel is ALIVE (the engine is up) — the shutdown signal
        // alone must end the stream (the cooperative close).
        let (tx, rx) = broadcast::channel::<hekma_engine::EngineEvent>(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut stream = event_stream(rx, shutdown_rx).boxed();
        shutdown_tx.send(true).unwrap();
        assert!(StreamExt::next(&mut stream).await.is_none());
        drop(tx);
    }

    #[tokio::test]
    async fn a_stream_subscribed_after_the_shutdown_signal_ends_immediately() {
        // Review item 4's race, pinned at the unit level: the stream is
        // created AFTER `send(true)` — its watch clone already holds `true`,
        // so there is no future EDGE for a `changed()` arm to observe. The
        // `wait_for` semantics must answer from the CURRENT value and end
        // the stream on the first poll, never hang.
        let (tx, rx) = broadcast::channel::<hekma_engine::EngineEvent>(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        shutdown_tx.send(true).unwrap();
        let mut stream = event_stream(rx, shutdown_rx).boxed();
        tokio::time::timeout(std::time::Duration::from_secs(2), async move {
            assert!(StreamExt::next(&mut stream).await.is_none());
        })
        .await
        .expect("an already-closed shutdown must end the stream immediately, not hang");
        drop(tx);
    }

    #[tokio::test]
    async fn the_stream_surfaces_a_lag_as_an_in_stream_gap_frame_and_continues() {
        use hekma_engine::{EngineEvent, LifecycleState, TransitionCause, TransitionEvent};
        // Capacity 1, two events: the second send overflows the buffer and
        // the receiver falls 1 event behind — the stream must NAME the gap
        // (the Control-API `lagged` frame) and CONTINUE at the tail.
        let (tx, rx) = broadcast::channel(1);
        let (_shutdown_tx, shutdown_rx) = quiet_shutdown();
        let mut stream = event_stream(rx, shutdown_rx).boxed();
        let first = EngineEvent::Transition(TransitionEvent::new(
            "laggy",
            LifecycleState::Registered,
            LifecycleState::Starting,
            TransitionCause::Command {
                command: "start".to_string(),
            },
            "2026-10-19T00:00:01Z",
        ));
        let second = EngineEvent::Transition(TransitionEvent::new(
            "laggy",
            LifecycleState::Starting,
            LifecycleState::Running,
            TransitionCause::AdapterReady,
            "2026-10-19T00:00:02Z",
        ));
        tx.send(first).unwrap();
        tx.send(second.clone()).unwrap();
        // The first poll reports the GAP, not a frame.
        let frame = StreamExt::next(&mut stream).await.unwrap().unwrap();
        let rendered = format!("{frame:?}");
        assert!(rendered.contains("lagged"), "{rendered}");
        assert!(rendered.contains("skipped"), "{rendered}");
        // And the stream CONTINUES: the next frame is the tail event's data.
        let frame = StreamExt::next(&mut stream).await.unwrap().unwrap();
        let payload = serde_json::to_string(&second).unwrap();
        assert!(
            format!("{frame:?}")
                .replace("\\\"", "\"")
                .contains(&payload),
            "the stream continues at the tail after the gap: {frame:?}"
        );
    }

    #[tokio::test]
    async fn the_stream_renders_engine_events_as_data_frames_verbatim() {
        use hekma_engine::{EngineEvent, LifecycleState, TransitionCause, TransitionEvent};
        let (tx, rx) = broadcast::channel(4);
        let (_shutdown_tx, shutdown_rx) = quiet_shutdown();
        let mut stream = event_stream(rx, shutdown_rx).boxed();
        let event = EngineEvent::Transition(TransitionEvent::new(
            "probe",
            LifecycleState::Starting,
            LifecycleState::Running,
            TransitionCause::AdapterReady,
            "2026-10-19T00:00:00Z",
        ));
        tx.send(event.clone()).unwrap();
        let frame = StreamExt::next(&mut stream).await.unwrap().unwrap();
        // The frame's data IS the engine's JSON — a `data:` line carrying the
        // kind-tagged payload, no re-shape. (The unit seam renders via Debug —
        // the buffer's quotes are escaped there; the WIRE shape is pinned by
        // the integration suite's real-HTTP SSE reader.)
        let payload = serde_json::to_string(&event).unwrap();
        let rendered = format!("{frame:?}").replace("\\\"", "\"");
        assert!(rendered.contains("data: "), "{rendered}");
        assert!(rendered.contains(&payload), "{rendered}");
    }
}
