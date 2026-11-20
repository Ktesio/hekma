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
//!   rule). The handler SUBSCRIBES FIRST, then reads the outbox rows with
//!   `seq > N` (one bounded batch at a time, as the client drains them —
//!   never the whole backlog in memory) through the active Selector
//!   filter, and hands over to live delivery dropping live seqs at or
//!   below the last seq the replay READ — no duplicate, no gap at the
//!   seam. A later replay batch that fails (the floor moved past it)
//!   surfaces a `{"kind":"replay-error",…}` frame and ENDS the stream: a
//!   reconnect then gets the typed `409`, never a silent gap.
//! * Without `Last-Event-ID` nothing is replayed and nothing is deduped:
//!   every live event is delivered as published.
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

/// The replay-failure marker's `kind` token: a later replay batch could
/// not be read (the outbox floor moved past it); the stream ends after it.
const REPLAY_ERROR_KIND: &str = "replay-error";

/// One SSE frame decision the stream loop emits.
enum Frame {
    /// An event with its outbox seq (the `id:`). Boxed: `EngineEvent` is
    /// the large variant, and the stream yields one frame at a time.
    Sequenced(u64, Box<hekma_engine::EngineEvent>),
    /// The lagged gap marker (no id).
    Lagged(u64),
    /// A Selector-filter read failed; the named event was dropped.
    FilterError(String),
    /// A later replay batch failed; the stream ends after this frame.
    ReplayError(String),
}

/// The lazy outbox replay of a `Last-Event-ID` resume (story 21-5): one
/// bounded export batch buffered at a time, the next read only when the
/// client has drained the last — never the whole backlog in memory.
struct Replay {
    /// The current batch's undelivered entries.
    buffered: std::vec::IntoIter<hekma_engine::OutboxEntry>,
    /// The last seq the replay has READ (the next batch reads after it;
    /// once the replay ends, live seqs at or below it are already
    /// delivered).
    after: u64,
    /// Whether the last batch came back full (another may follow).
    more: bool,
    /// The batch size.
    batch: u32,
}

impl Replay {
    /// Read the FIRST batch after `after` — the connect-time validation:
    /// below the retained floor or past the committed tail is the typed
    /// `409` (the never-clamp rule).
    async fn start(state: &ApiState, after: u64, batch: u32) -> Result<Self, ApiError> {
        let mut replay = Self {
            buffered: Vec::new().into_iter(),
            after,
            more: true,
            batch,
        };
        replay.fill(state).await.map_err(|err| match err {
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
        Ok(replay)
    }

    /// Read the next batch into the buffer.
    async fn fill(&mut self, state: &ApiState) -> Result<(), hekma_engine::EngineError> {
        let batch = state
            .engine
            .export_events(
                Some(hekma_engine::OutboxCursor { seq: self.after }),
                self.batch,
            )
            .await?;
        self.more = batch.events.len() >= self.batch as usize;
        self.after = self.after.max(batch.cursor.seq);
        self.buffered = batch.events.into_iter();
        Ok(())
    }

    /// The next replayed entry: `Ok(None)` once the outbox is drained,
    /// `Err` naming a later batch that could not be read.
    async fn next(
        &mut self,
        state: &ApiState,
    ) -> Result<Option<hekma_engine::OutboxEntry>, String> {
        loop {
            if let Some(entry) = self.buffered.next() {
                return Ok(Some(entry));
            }
            if !self.more {
                return Ok(None);
            }
            self.fill(state).await.map_err(|err| err.to_string())?;
        }
    }
}

/// Subscribe to the sequenced bus, THEN read the replay's first batch — in
/// this order, so every event committed after the subscribe is on the
/// receiver and every event committed before the read is in the replay
/// (the seam dedups the overlap by seq). `seam` runs between the two steps:
/// production passes a no-op; the tests commit an event there.
async fn subscribe_then_replay<F>(
    state: &ApiState,
    last_event_id: Option<u64>,
    batch: u32,
    seam: F,
) -> Result<
    (
        broadcast::Receiver<(u64, hekma_engine::EngineEvent)>,
        Option<Replay>,
    ),
    ApiError,
>
where
    F: std::future::Future<Output = ()>,
{
    let rx = state.engine.subscribe_sequenced();
    seam.await;
    let replay = match last_event_id {
        None => None,
        Some(after) => Some(Replay::start(state, after, batch).await?),
    };
    Ok((rx, replay))
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
    let last_event_id = match headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
    {
        None => None,
        Some(raw) => Some(raw.parse::<u64>().map_err(|err| {
            ApiError::invalid_query_param(
                "invalid-cursor",
                format!("Last-Event-ID must be an event id (a whole number): {err}"),
            )
        })?),
    };
    // Subscribe FIRST, then validate and read the resume cursor's first
    // batch (below the retained floor or past the committed tail is a
    // typed 409 BEFORE streaming — the never-clamp rule).
    let (rx, replay) = subscribe_then_replay(
        &state,
        last_event_id,
        hekma_engine::EXPORT_BATCH_MAX_CAP,
        std::future::ready(()),
    )
    .await?;
    Ok(Sse::new(event_stream(state, rx, selector, replay)).keep_alive(KeepAlive::default()))
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

/// The stream's state between polls.
struct StreamState {
    state: ApiState,
    rx: broadcast::Receiver<(u64, hekma_engine::EngineEvent)>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    selector: Option<hekma_engine::Selector>,
    /// The replay still draining (a resume only).
    replay: Option<Replay>,
    /// Set once a resume's replay has drained: the last seq it READ. Live
    /// seqs at or below it were already replayed and are dropped. `None`
    /// on a plain stream — nothing is deduped there.
    live_floor: Option<u64>,
    /// The stream ends at the next poll (after a replay-error frame).
    ended: bool,
}

/// Whether `event` passes the stream's Selector (always, without one).
async fn admit(
    state: &ApiState,
    selector: &Option<hekma_engine::Selector>,
    seq: u64,
    event: hekma_engine::EngineEvent,
) -> Option<Frame> {
    match selector {
        Some(selector) => match passes_selector(state, selector, &event).await {
            Ok(true) => Some(Frame::Sequenced(seq, Box::new(event))),
            Ok(false) => None,
            Err(detail) => Some(Frame::FilterError(detail)),
        },
        None => Some(Frame::Sequenced(seq, Box::new(event))),
    }
}

/// The stream: replay (when resuming), then live. The unfold state carries
/// the phase; each poll produces at most one frame (skipped events —
/// seq-dedup at the seam, non-matching Selectors — simply loop).
fn event_stream(
    state: ApiState,
    rx: broadcast::Receiver<(u64, hekma_engine::EngineEvent)>,
    selector: Option<hekma_engine::Selector>,
    replay: Option<Replay>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let shutdown = state.shutdown.clone();
    futures_util::stream::unfold(
        StreamState {
            state,
            rx,
            shutdown,
            selector,
            replay,
            live_floor: None,
            ended: false,
        },
        |mut stream| async move {
            // `wait_for` semantics: the CURRENT shutdown value answers
            // immediately, `changed()` only future edges.
            if stream.ended || *stream.shutdown.borrow_and_update() {
                return None;
            }
            let frame = loop {
                if let Some(replay) = stream.replay.as_mut() {
                    match replay.next(&stream.state).await {
                        Ok(Some(entry)) => {
                            match admit(&stream.state, &stream.selector, entry.seq, entry.event)
                                .await
                            {
                                Some(frame) => break frame,
                                None => continue,
                            }
                        }
                        Ok(None) => {
                            stream.live_floor = Some(replay.after);
                            stream.replay = None;
                        }
                        Err(detail) => {
                            stream.replay = None;
                            stream.ended = true;
                            break Frame::ReplayError(detail);
                        }
                    }
                }
                tokio::select! {
                    _ = stream.shutdown.changed() => {
                        // The API is going down: end the stream inside the
                        // graceful window; the engine is untouched.
                        return None;
                    }
                    received = stream.rx.recv() => match received {
                        Ok((seq, event)) => {
                            // Seq-dedup at the seam (a resume only):
                            // everything the replay read is dropped, never
                            // re-sent.
                            if stream.live_floor.is_some_and(|floor| seq <= floor) {
                                continue;
                            }
                            match admit(&stream.state, &stream.selector, seq, event).await {
                                Some(frame) => break frame,
                                None => continue,
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
            Some((Ok(render(frame)), stream))
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
        Frame::ReplayError(detail) => {
            let frame = serde_json::json!({ "kind": REPLAY_ERROR_KIND, "detail": detail });
            // No id: nothing past the client's last delivered id was sent.
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

    #[tokio::test]
    async fn a_multi_entry_replay_delivers_every_entry_in_seq_order_before_live() {
        // The seam's no-gap rule, pinned at the unit level: a resume with
        // MORE than one replayed entry must deliver ALL of them (in seq
        // order) before any live event — the replay queue is state, not a
        // one-shot. (A previous version dropped the queue after the first
        // frame; the integration test's single-entry replay hid it.)
        use hekma_engine::{EngineEvent, LifecycleState, TransitionCause, TransitionEvent};
        let (tx, rx) = broadcast::channel::<(u64, hekma_engine::EngineEvent)>(4);
        let (state, _shutdown_tx) = quiet_state();
        let replay: Vec<hekma_engine::OutboxEntry> = [10u64, 11, 12]
            .into_iter()
            .map(|seq| hekma_engine::OutboxEntry {
                schema_version: 1,
                seq,
                event: EngineEvent::Transition(TransitionEvent::new(
                    "probe",
                    LifecycleState::Registered,
                    LifecycleState::Starting,
                    TransitionCause::Command {
                        command: format!("start-{seq}"),
                    },
                    "2026-11-12T00:00:00Z",
                )),
            })
            .collect();
        let replay = Replay {
            buffered: replay.into_iter(),
            after: 12,
            more: false,
            batch: 10,
        };
        let mut stream = event_stream(state, rx, None, Some(replay)).boxed();
        for expected in [10u64, 11, 12] {
            let frame = StreamExt::next(&mut stream).await.unwrap().unwrap();
            let rendered = format!("{frame:?}");
            assert!(rendered.contains(&format!("id: {expected}")), "{rendered}");
            assert!(
                rendered.contains(&format!("start-{expected}")),
                "the replayed payload rides verbatim: {rendered}"
            );
        }
        // The replay exhausted: the live tail delivers next, deduped
        // against nothing yet (the replayed tail was 12).
        tx.send((
            13,
            EngineEvent::Transition(TransitionEvent::new(
                "probe",
                LifecycleState::Starting,
                LifecycleState::Running,
                TransitionCause::AdapterReady,
                "2026-11-12T00:00:01Z",
            )),
        ))
        .unwrap();
        let frame = StreamExt::next(&mut stream).await.unwrap().unwrap();
        let rendered = format!("{frame:?}");
        assert!(rendered.contains("id: 13"), "{rendered}");
        drop(tx);
    }

    /// A `{"kind":"…"}` probe transition (`n` rides the command name, so a
    /// frame names which event it carries).
    fn probe(n: u64) -> hekma_engine::EngineEvent {
        use hekma_engine::{EngineEvent, LifecycleState, TransitionCause, TransitionEvent};
        EngineEvent::Transition(TransitionEvent::new(
            "probe",
            LifecycleState::Registered,
            LifecycleState::Starting,
            TransitionCause::Command {
                command: format!("start-{n}"),
            },
            "2026-11-12T00:00:00Z",
        ))
    }

    /// The next frame's debug rendering, bounded (never a hung test).
    async fn next_frame<S>(stream: &mut S) -> String
    where
        S: Stream<Item = Result<Event, Infallible>> + Unpin,
    {
        let frame =
            tokio::time::timeout(std::time::Duration::from_secs(10), StreamExt::next(stream))
                .await
                .expect("a frame within the bound")
                .expect("the stream is open")
                .unwrap();
        format!("{frame:?}")
    }

    /// The `id:` a rendered frame carries, if any.
    fn frame_id(rendered: &str) -> Option<u64> {
        let start = rendered.find("id: ")? + 4;
        let digits: String = rendered[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    }

    /// Commit one Policy (one committed, published event) through `state`'s
    /// writable engine.
    async fn commit_policy(state: &ApiState, name: &str) {
        state
            .engine
            .create_policy(
                hekma_engine::PolicyChange {
                    name: name.to_string(),
                    selector: Some("team=core".parse().unwrap()),
                    set: Default::default(),
                    clear: Vec::new(),
                    rates: Default::default(),
                },
                false,
            )
            .await
            .unwrap();
    }

    /// Every committed outbox seq after `after`.
    async fn outbox_seqs(state: &ApiState, after: u64) -> Vec<u64> {
        state
            .engine
            .export_events(Some(hekma_engine::OutboxCursor { seq: after }), 10_000)
            .await
            .unwrap()
            .events
            .iter()
            .map(|entry| entry.seq)
            .collect()
    }

    /// Resume from 0 with `batch`-sized replay reads while an event commits
    /// between the subscribe and the first replay read: every committed
    /// event arrives exactly once, in order, then live continues.
    async fn resume_across_the_seam(batch: u32) {
        let (state, _shutdown_tx) = super::super::test_support::writable_api_state();
        commit_policy(&state, "before-one").await;
        commit_policy(&state, "before-two").await;
        let (rx, replay) =
            subscribe_then_replay(&state, Some(0), batch, commit_policy(&state, "at-the-seam"))
                .await
                .unwrap();
        let expected = outbox_seqs(&state, 0).await;
        assert!(expected.len() >= 3, "{expected:?}");
        let mut stream = event_stream(state.clone(), rx, None, replay).boxed();
        let mut delivered = Vec::new();
        let mut seam_seen = false;
        for _ in 0..expected.len() {
            let rendered = next_frame(&mut stream).await;
            seam_seen |= rendered.contains("at-the-seam");
            delivered.push(frame_id(&rendered).expect("an event frame carries its id"));
        }
        assert_eq!(
            delivered, expected,
            "batch {batch}: every committed event, once, in order"
        );
        assert!(seam_seen, "batch {batch}: the seam's event was delivered");
        // Nothing else is pending: the live copy of the seam's event (and
        // of every replayed event) was deduped.
        commit_policy(&state, "after").await;
        let rendered = next_frame(&mut stream).await;
        assert!(rendered.contains("after"), "{rendered}");
        assert_eq!(
            frame_id(&rendered),
            outbox_seqs(&state, 0).await.last().copied()
        );
    }

    #[tokio::test]
    async fn an_event_committed_between_subscribe_and_replay_is_delivered_once() {
        // The seam: the resume subscribes FIRST and reads the replay
        // second, so an event committed between the two is on the receiver
        // AND in the replay — delivered exactly once, never lost. (Reading
        // the replay before subscribing loses it: in neither.) One batch
        // holds the whole replay here, so the seam is the only path.
        resume_across_the_seam(hekma_engine::EXPORT_BATCH_MAX_CAP).await;
    }

    #[tokio::test]
    async fn a_resume_reads_the_replay_lazily_in_bounded_batches() {
        // Two-entry batches: the replay drains across several reads (each
        // read only once the client has drained the last), with the same
        // exactly-once seam.
        resume_across_the_seam(2).await;
    }

    #[tokio::test]
    async fn a_failed_later_replay_batch_is_named_and_ends_the_stream() {
        // The replay reads lazily; a batch that cannot be read after the
        // stream started (here: past the tail) surfaces a replay-error
        // frame with no id and ENDS the stream — never a silent gap.
        let (state, _shutdown_tx) = super::super::test_support::writable_api_state();
        let (_tx, rx) = broadcast::channel::<(u64, hekma_engine::EngineEvent)>(4);
        let replay = Replay {
            buffered: Vec::new().into_iter(),
            after: 1_000_000,
            more: true,
            batch: 2,
        };
        let mut stream = event_stream(state, rx, None, Some(replay)).boxed();
        let rendered = next_frame(&mut stream).await;
        assert!(rendered.contains("replay-error"), "{rendered}");
        assert_eq!(frame_id(&rendered), None, "the marker carries no id");
        let end = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            StreamExt::next(&mut stream),
        )
        .await
        .expect("the stream ends, it does not hang");
        assert!(end.is_none(), "the stream ended after the marker");
    }

    #[tokio::test]
    async fn the_lagged_frame_carries_no_id() {
        // A receiver that falls behind the ring sees the gap marker — with
        // NO id (it is a gap, not an event) — then the stream continues.
        let (tx, rx) = broadcast::channel::<(u64, hekma_engine::EngineEvent)>(1);
        let (state, _shutdown_tx) = quiet_state();
        let mut stream = event_stream(state, rx, None, None).boxed();
        for seq in [1u64, 2, 3] {
            tx.send((seq, probe(seq))).unwrap();
        }
        let rendered = next_frame(&mut stream).await;
        assert!(rendered.contains("lagged"), "{rendered}");
        assert_eq!(frame_id(&rendered), None, "{rendered}");
        let rendered = next_frame(&mut stream).await;
        assert_eq!(
            frame_id(&rendered),
            Some(3),
            "the stream continues: {rendered}"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn a_plain_stream_delivers_every_live_event_even_out_of_seq_order() {
        // No Last-Event-ID: nothing was replayed, so nothing is deduped — a
        // live event whose seq is below one already delivered is still
        // delivered (only a resume's replayed range is dropped).
        let (tx, rx) = broadcast::channel::<(u64, hekma_engine::EngineEvent)>(4);
        let (state, _shutdown_tx) = quiet_state();
        let mut stream = event_stream(state, rx, None, None).boxed();
        tx.send((5, probe(5))).unwrap();
        tx.send((3, probe(3))).unwrap();
        assert_eq!(frame_id(&next_frame(&mut stream).await), Some(5));
        assert_eq!(frame_id(&next_frame(&mut stream).await), Some(3));
        drop(tx);
    }

    #[tokio::test]
    async fn a_resume_drops_live_copies_of_replayed_seqs_only() {
        // The resume's dedup floor is the last seq the replay READ: a live
        // copy at or below it is dropped, everything above it delivered.
        let (tx, rx) = broadcast::channel::<(u64, hekma_engine::EngineEvent)>(8);
        let (state, _shutdown_tx) = quiet_state();
        let replay = Replay {
            buffered: vec![hekma_engine::OutboxEntry {
                schema_version: 1,
                seq: 7,
                event: probe(7),
            }]
            .into_iter(),
            after: 7,
            more: false,
            batch: 10,
        };
        let mut stream = event_stream(state, rx, None, Some(replay)).boxed();
        for seq in [6u64, 7, 8] {
            tx.send((seq, probe(seq))).unwrap();
        }
        assert_eq!(frame_id(&next_frame(&mut stream).await), Some(7));
        assert_eq!(frame_id(&next_frame(&mut stream).await), Some(8));
        drop(tx);
    }

    /// A quiet [`ApiState`] plus its shutdown sender (see
    /// [`test_support::quiet_api_state`]).
    fn quiet_state() -> (ApiState, watch::Sender<bool>) {
        super::super::test_support::quiet_api_state()
    }
}
