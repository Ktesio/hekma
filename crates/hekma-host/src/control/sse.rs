//! The `GET /v0/events` SSE stream (story 17-3; ids, Selector filter and
//! resume since story 21-5): ONE engine receiver per connected client over
//! the SEQUENCED bus ring, each committed event rendered as an SSE frame
//! carrying `id: <outbox seq>` — the durable position `export_events`
//! serves — with an optional server-side `?selector=` filter and
//! `Last-Event-ID` resume.
//!
//! ## The resume contract (story 21-5)
//!
//! * Every event frame carries `id: <outbox seq>`; the in-stream `lagged`
//!   gap marker carries NONE (it is a gap, not an event).
//! * A reconnect with `Last-Event-ID: N` is validated BEFORE streaming: a
//!   seq below the outbox's retained floor answers `409
//!   cursor-below-retention`, one past the committed tail `409
//!   cursor-past-tail` — typed, never a clamp (the export contract's
//!   rule). Then the handler SUBSCRIBES FIRST, replays the outbox rows
//!   with `seq > N` through the active Selector filter, and hands over to
//!   live delivery dropping live seqs at or below the replayed tail — no
//!   duplicate, no gap at the seam.
//! * `?selector=key=value[,…]` evaluates each event's subject Run against
//!   its CURRENT labels (snapshot first, one read-only store read when the
//!   Run is absent from it — ended Runs keep matching until compaction).
//!   Events naming no Run (`policy`, `breach-default-upgraded`) are
//!   delivered only WITHOUT a Selector. Honest limitation, by design: the
//!   filter sees current labels, not labels-at-event-time (nothing stores
//!   the latter).
//!
//! The slow-subscriber policy stays SURFACED, not silent (AI-18): a
//! receiver that falls more than the engine bus's capacity behind observes
//! the Control-API-level `{"kind":"lagged","skipped":n}` frame naming the
//! gap and the stream CONTINUES at the current tail. A Selector-filter
//! read that fails surfaces a `{"kind":"filter-error",…}` frame and drops
//! that one event — never a silent skip, never a dead stream. When the
//! engine (and every sender) is gone, or the API's shutdown signal fires,
//! the stream ENDS.

use std::convert::Infallible;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use serde::Deserialize;
use tokio::sync::broadcast;

#[cfg(test)]
use tokio::sync::watch;

use super::error::ApiError;
use super::ApiState;

/// The in-stream lag frame's `kind` token (Control-API-level vocabulary —
/// the engine's event kinds are `transition` / `budget_breach` /
/// `usage_update`; `lagged` is THIS surface's honest gap marker).
const LAGGED_KIND: &str = "lagged";

/// The Selector-filter read-failure marker's `kind` token (AI-18: the
/// event is dropped, the drop is NAMED — never a silent skip, never a dead
/// stream).
const FILTER_ERROR_KIND: &str = "filter-error";

/// The `?selector=` query of the events stream (story 21-5).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct EventsStreamQuery {
    /// `key=value[,key=value…]`: only events whose subject Run currently
    /// carries every term (bound or caller) are delivered.
    pub selector: Option<String>,
}

/// One SSE frame decision the stream loop emits.
enum Frame {
    /// An event with its outbox seq (the `id:`). Boxed: `EngineEvent` is
    /// the large variant, and the stream yields one frame at a time.
    Sequenced(u64, Box<hekma_engine::EngineEvent>),
    /// The lagged gap marker (no id).
    Lagged(u64),
    /// A Selector-filter read failed; the named event was dropped.
    FilterError(String),
}

/// The `GET /v0/events` handler — the SSE response (ids, Selector filter,
/// `Last-Event-ID` resume since story 21-5). Keep-alive comments keep an
/// idle stream visible to intermediaries.
pub(crate) async fn live_events(
    State(state): State<ApiState>,
    Query(query): Query<EventsStreamQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let selector = match &query.selector {
        None => None,
        Some(raw) => Some(raw.parse::<hekma_engine::Selector>().map_err(|err| {
            ApiError::invalid_query_param("invalid-selector", format!("selector: {err}"))
        })?),
    };
    // Validate the resume cursor BEFORE streaming (the never-clamp rule):
    // below the retained floor or past the committed tail is a typed 409.
    let replay = match headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
    {
        None => None,
        Some(raw) => {
            let seq = raw.parse::<u64>().map_err(|err| {
                ApiError::invalid_query_param(
                    "invalid-cursor",
                    format!("Last-Event-ID must be an event id (a whole number): {err}"),
                )
            })?;
            let mut entries = Vec::new();
            let mut after = seq;
            loop {
                let batch = state
                    .engine
                    .export_events(
                        Some(hekma_engine::OutboxCursor { seq: after }),
                        hekma_engine::EXPORT_BATCH_MAX_CAP,
                    )
                    .await
                    .map_err(|err| match err {
                        hekma_engine::EngineError::Log { detail, .. }
                            if detail.contains(hekma_engine::BELOW_FLOOR_SENTENCE) =>
                        {
                            ApiError::conflict_with_code("cursor-below-retention", detail)
                        }
                        hekma_engine::EngineError::Log { detail, .. }
                            if detail.contains(hekma_engine::PAST_TAIL_SENTENCE) =>
                        {
                            ApiError::conflict_with_code("cursor-past-tail", detail)
                        }
                        other => other.into(),
                    })?;
                let count = batch.events.len();
                after = batch.cursor.seq;
                entries.extend(batch.events);
                if count < hekma_engine::EXPORT_BATCH_MAX_CAP as usize {
                    break;
                }
            }
            Some(entries)
        }
    };
    // Subscribe FIRST (before replaying) so the seam has no gap: everything
    // committed after this point buffers on the receiver while the replay
    // drains; the handover drops live seqs at or below the replayed tail.
    let rx = state.engine.subscribe_sequenced();
    Ok(Sse::new(event_stream(
        state,
        rx,
        selector,
        replay.map(Vec::into_iter),
    ))
    .keep_alive(KeepAlive::default()))
}

/// Whether `event` passes `selector` — the subject Run's CURRENT labels
/// (snapshot first, one read-only store read when absent). `Err` names a
/// failed read (surfaced as a marker frame, the event dropped).
async fn passes_selector(
    state: &ApiState,
    selector: &hekma_engine::Selector,
    event: &hekma_engine::EngineEvent,
) -> Result<bool, String> {
    let labels = state
        .engine
        .event_subject_labels(event)
        .await
        .map_err(|err| err.to_string())?;
    Ok(labels
        .map(|labels| selector.matches(&labels, hekma_engine::SelectorScope::Any))
        .unwrap_or(false))
}

/// The stream: replay (when resuming), then live. The unfold state carries
/// the phase; each poll produces at most one frame (skipped events —
/// seq-dedup at the seam, non-matching Selectors — simply loop).
#[allow(clippy::type_complexity)]
fn event_stream(
    state: ApiState,
    rx: broadcast::Receiver<(u64, hekma_engine::EngineEvent)>,
    selector: Option<hekma_engine::Selector>,
    replay: Option<std::vec::IntoIter<hekma_engine::OutboxEntry>>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let shutdown = state.shutdown.clone();
    futures_util::stream::unfold(
        (state, rx, shutdown, selector, replay, 0u64),
        |(state, mut rx, mut shutdown, selector, mut replay, replay_tail)| async move {
            // `wait_for` semantics: the CURRENT shutdown value answers
            // immediately, `changed()` only future edges.
            if *shutdown.borrow_and_update() {
                return None;
            }
            let frame = loop {
                if let Some(entries) = replay.as_mut() {
                    if let Some(entry) = entries.next() {
                        match &selector {
                            Some(selector) => {
                                match passes_selector(&state, selector, &entry.event).await {
                                    Ok(true) => {
                                        break Frame::Sequenced(
                                            entry.seq,
                                            Box::new(entry.event.clone()),
                                        )
                                    }
                                    Ok(false) => continue,
                                    Err(detail) => break Frame::FilterError(detail),
                                }
                            }
                            None => {
                                break Frame::Sequenced(entry.seq, Box::new(entry.event.clone()))
                            }
                        }
                    }
                }
                replay = None;
                tokio::select! {
                    _ = shutdown.changed() => {
                        // The API is going down: end the stream inside the
                        // graceful window; the engine is untouched.
                        return None;
                    }
                    received = rx.recv() => match received {
                        Ok((seq, event)) => {
                            // Seq-dedup at the seam: everything the replay
                            // already delivered is dropped, never re-sent.
                            if seq <= replay_tail {
                                continue;
                            }
                            match &selector {
                                Some(selector) => match passes_selector(&state, selector, &event)
                                    .await
                                {
                                    Ok(true) => break Frame::Sequenced(seq, Box::new(event)),
                                    Ok(false) => continue,
                                    Err(detail) => break Frame::FilterError(detail),
                                },
                                None => break Frame::Sequenced(seq, Box::new(event)),
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            break Frame::Lagged(skipped);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // The engine is gone: end the stream so a
                            // shutdown never strands a client's read.
                            return None;
                        }
                    }
                }
            };
            // The tail advances only over DELIVERED (or replayed) frames —
            // a lag burst re-syncs at the current tail either way.
            let next_tail = match &frame {
                Frame::Sequenced(seq, _) => (*seq).max(replay_tail),
                _ => replay_tail,
            };
            Some((
                Ok(render(frame)),
                (state, rx, shutdown, selector, None, next_tail),
            ))
        },
    )
}

/// Render one frame decision as an SSE event.
fn render(frame: Frame) -> Event {
    match frame {
        Frame::Sequenced(seq, event) => {
            let payload = serde_json::to_string(&event).unwrap_or_else(|_| {
                r#"{"kind":"internal","detail":"event serialization failed"}"#.to_string()
            });
            Event::default().id(seq.to_string()).data(payload)
        }
        Frame::Lagged(skipped) => {
            let frame = serde_json::json!({ "kind": LAGGED_KIND, "skipped": skipped });
            // A gap marker, not an event: NO id (nothing was committed).
            Event::default().data(frame.to_string())
        }
        Frame::FilterError(detail) => {
            let frame = serde_json::json!({ "kind": FILTER_ERROR_KIND, "detail": detail });
            // The dropped event's seq must not become a resume point that
            // claims delivery — no id on the marker.
            Event::default().data(frame.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

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
        let (tx, rx) = broadcast::channel::<(u64, hekma_engine::EngineEvent)>(4);
        let (state, _shutdown_tx) = quiet_state();
        let mut stream = event_stream(state, rx, None, None).boxed();
        drop(tx);
        // The only item the closed channel yields is the stream's END — no
        // frame, no hang.
        assert!(StreamExt::next(&mut stream).await.is_none());
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_api_shuts_down() {
        // The channel is ALIVE (the engine is up) — the shutdown signal
        // alone must end the stream (the cooperative close).
        let (tx, rx) = broadcast::channel::<(u64, hekma_engine::EngineEvent)>(4);
        let (state, shutdown_tx) = quiet_state();
        let mut stream = event_stream(state, rx, None, None).boxed();
        shutdown_tx.send(true).unwrap();
        assert!(StreamExt::next(&mut stream).await.is_none());
        drop(tx);
    }

    #[tokio::test]
    async fn a_stream_subscribed_after_the_shutdown_signal_ends_immediately() {
        // The wait_for race, pinned at the unit level: the stream is
        // created AFTER `send(true)` — its watch clone already holds `true`,
        // so there is no future EDGE for a `changed()` arm to observe.
        let (tx, rx) = broadcast::channel::<(u64, hekma_engine::EngineEvent)>(4);
        let (state, shutdown_tx) = quiet_state();
        shutdown_tx.send(true).unwrap();
        let mut stream = event_stream(state, rx, None, None).boxed();
        tokio::time::timeout(std::time::Duration::from_secs(2), async move {
            assert!(StreamExt::next(&mut stream).await.is_none());
        })
        .await
        .expect("an already-closed shutdown must end the stream immediately, not hang");
        drop(tx);
    }

    #[tokio::test]
    async fn the_stream_carries_ids_and_lagged_carries_none() {
        use hekma_engine::{EngineEvent, LifecycleState, TransitionCause, TransitionEvent};
        let (tx, rx) = broadcast::channel::<(u64, hekma_engine::EngineEvent)>(4);
        let (state, _shutdown_tx) = quiet_state();
        let mut stream = event_stream(state, rx, None, None).boxed();
        let event = EngineEvent::Transition(TransitionEvent::new(
            "probe",
            LifecycleState::Registered,
            LifecycleState::Starting,
            TransitionCause::Command {
                command: "start".to_string(),
            },
            "2026-11-11T00:00:00Z",
        ));
        tx.send((41, event.clone())).unwrap();
        let frame = StreamExt::next(&mut stream).await.unwrap().unwrap();
        let rendered = format!("{frame:?}");
        assert!(rendered.contains("id: 41"), "{rendered}");
        let payload = serde_json::to_string(&event).unwrap();
        assert!(
            rendered.replace("\\\"", "\"").contains(&payload),
            "the payload rides verbatim: {rendered}"
        );
        drop(tx);
    }

    /// A quiet [`ApiState`] plus its shutdown sender (see
    /// [`test_support::quiet_api_state`]).
    fn quiet_state() -> (ApiState, watch::Sender<bool>) {
        super::super::test_support::quiet_api_state()
    }
}
