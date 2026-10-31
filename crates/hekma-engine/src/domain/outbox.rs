//! The engine-GLOBAL event outbox (story 17-4, the AD-14 amendment) — one
//! durable append-only journal at the state base carrying the transition,
//! breach, and usage families under ONE monotonically increasing `seq`.
//!
//! ## Why it exists
//!
//! A Host on an ephemeral disk loses the engine's event record when the
//! container dies. The per-family `resync_events` helper (story 10-3) heals
//! the bus's crash window per instance, but it has no global order and no
//! acknowledgement: a Host re-pulling after a restart cannot cheaply say "I
//! already have everything through here". The outbox is that cursor-able,
//! cross-instance, acknowledgeable surface: every committed event is ALSO
//! appended to one JSON-Lines journal (`EnginePaths::outbox_journal`) under a
//! single increasing sequence, the Host pulls bounded batches by cursor
//! ([`ExportBatch`]), and acknowledges a seq to advance a durable consumer
//! cursor (`EnginePaths::outbox_ack`). Un-acked events survive for re-pull;
//! per-family `resync_events` is UNCHANGED (the heal path stays).
//!
//! ## The contract
//!
//! * **Committed truth only.** The journal is written at the THREE existing
//!   commit sites (transition append, breach append, usage-ledger commit),
//!   each AFTER its family commit succeeded and BEFORE the bus publish — the
//!   same "never publish what did not commit" ordering the bus keeps. An
//!   event that never committed never appears here.
//! * **The outbox-append failure NEVER fails the family commit.** The family
//!   stores stay the truth; a failed append rolls the seq counter back, emits
//!   a surfaced stderr diagnostic (AI-18), and the operation proceeds —
//!   `resync_events` remains the heal path for whatever the journal missed.
//! * **Order: seq order ≈ commit order.** All three sites run under the
//!   supervisor lock, so in ONE engine process the seqs land in commit order.
//!   Two engines on one state dir (the host lock only guards `hekma serve`)
//!   each hold their own counter and can interleave — a documented caveat,
//!   not solved here; a reader serves the journal's physical order, which in
//!   that window is the only honest global order there is.
//! * **Restart-survivable seq.** The next seq is re-derived from the journal
//!   tail at engine open, so seqs keep increasing across restarts (a cursor a
//!   Host held stays valid).
//! * **Bounded, honestly (AI-18).** The journal is compacted at append/ack
//!   time (a check-before-write approximation of the log families' rotation
//!   discipline, capped at `LOG_ROTATE_MAX_BYTES × LOG_ROTATE_GENERATIONS`):
//!   the ACKED prefix is dropped first, then — only if still over cap — the
//!   oldest UN-ACKED entries, NEVER the very last entry (so the journal's
//!   high-water seq and `retained_from` survive). Any un-acked eviction is
//!   SURFACED: a stderr diagnostic at eviction time, `retained_from` on every
//!   batch, and a typed below-floor refusal naming both counts — never a
//!   silent clamp, which would either re-deliver or silently skip records.
//! * **Cursor semantics mirror the resync contract.** The default pull
//!   resumes from the durable ack cursor; an explicit cursor re-pulls; a
//!   cursor below the retained floor or past the committed tail is a typed
//!   [`EngineError::Log`] carrying a STABLE marker sentence (the Control API
//!   lifts those to 409 `cursor_invalid`) — never a clamp. Ack is monotonic
//!   (`max(current, given)`); a regressive ack is a no-op success; an ack
//!   past the tail is the same refusal as a pull past the tail.
//! * **Crash-recovery read posture (torn-tail tolerance, SURFACED).** Reads
//!   skip ONE unparseable TRAILING line only when it carries the torn-append
//!   signature (the file's raw text does not end with a newline) and surface
//!   the skip on the batch (`torn_tail_skipped`), mirroring `domain::resync`.
//!   The APPEND path additionally REPAIRS a torn fragment (truncates back to
//!   the last newline) so the journal never fuses records — the resync
//!   readers' "next append fuses onto the torn fragment" hazard cannot arise
//!   here, and the repair is invisible to a tolerant read (the fragment is
//!   gone before the read).
//!
//! ## Boundary (what this is NOT)
//!
//! No new event kinds (the payload is the EXACT [`EngineEvent`] verse the bus
//! carries), no schema change to any family, no bus/publish-path change, and
//! no change to the per-family logs or `resync_events`. Additive facade API:
//! [`crate::Engine::export_events`] (async) and
//! [`crate::Blocking::export_events`] (sync), plus their ack siblings.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::bus::EngineEvent;
use super::registry::Registry;
use super::EngineError;
use crate::ports::RunStore;
use crate::ports::{LOG_ROTATE_GENERATIONS, LOG_ROTATE_MAX_BYTES};

/// The outbox payload family's schema version (story 17-4, AD-14's
/// `schema_version`-on-payloads rule). Additive evolution only while the
/// surface is v0-unstable.
pub const OUTBOX_SCHEMA_VERSION: u32 = 1;

/// The outbox journal's byte cap: the log families' own rotation budget
/// ("10MB × 3", `LOG_ROTATE_MAX_BYTES × LOG_ROTATE_GENERATIONS`) applied to
/// the ONE global journal — the spec's retention bound. Checked BEFORE each
/// append (and at ack), so the file may transiently sit a little over this
/// exact count (the accepted check-before-write approximation, same as the
/// log families').
pub const OUTBOX_MAX_BYTES: u64 = LOG_ROTATE_MAX_BYTES * LOG_ROTATE_GENERATIONS as u64;

/// The default batch size for an export pull whose caller passed no `max`.
pub const EXPORT_BATCH_DEFAULT_MAX: u32 = 500;

/// The hard ceiling on `max` — a Host cannot request an unbounded batch even
/// by passing a huge `max`; the engine clamps here.
pub const EXPORT_BATCH_MAX_CAP: u32 = 10_000;

/// How far into the outbox a Host has already consumed AND acknowledged —
/// the export pull's resume position (story 17-4).
///
/// `seq` is the highest event sequence the Host holds; the next pull serves
/// everything AFTER it. [`OutboxCursor::START`] (seq 0) is the entry point:
/// the full backfill from the journal's very first retained record. The FIRST
/// committed event carries seq 1, so START is a valid position, not a special
/// "nothing consumed" sentinel below the floor.
///
/// Serde-derived (snake_case, like every AD-14 wire struct) so a Host can
/// persist its own in-flight cursor between pulls; the DURABLE cursor the
/// default pull resumes from lives in the engine's ack file
/// (`EnginePaths::outbox_ack`), advanced by `ack_export`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxCursor {
    /// The highest event sequence already consumed.
    pub seq: u64,
}

impl OutboxCursor {
    /// The start position: consume EVERYTHING the journal retains. Identical
    /// to `OutboxCursor::default()`; spelled as a named constant so a call
    /// site reads as intent.
    pub const START: OutboxCursor = OutboxCursor { seq: 0 };
}

/// One entry of the outbox journal (one JSON-Lines record): a committed event
/// under its global sequence. The payload is the EXACT [`EngineEvent`] bus
/// wrapper — no new event kinds (the `resync.rs` boundary holds).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    /// The record family's schema version ([`OUTBOX_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The GLOBAL sequence: monotonically increasing across the transition,
    /// breach, and usage families (first committed event = 1). Stable across
    /// restarts (re-derived from the journal tail at engine open).
    pub seq: u64,
    /// The committed event, the exact payload the live bus published.
    pub event: EngineEvent,
}

/// One export batch: the committed events after a cursor position, plus the
/// position to resume from (story 17-4). `events` carries entries with seq
/// strictly greater than the request cursor, in seq order (bounded batch);
/// `cursor` is the position after THIS batch — feed it to the next pull (or
/// acknowledge it) and the delivered prefix is skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportBatch {
    /// The outbox family's schema version ([`OUTBOX_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The retained entries after the request cursor, in seq order (empty =
    /// the caller is caught up to the committed truth).
    pub events: Vec<OutboxEntry>,
    /// The consume position after this batch — the next pull's default.
    pub cursor: OutboxCursor,
    /// The journal's trailing line was skipped as a torn append (the crash-
    /// recovery posture, surfaced exactly like the resync batch's flag).
    #[serde(default)]
    pub torn_tail_skipped: bool,
    /// Interior blank/whitespace-only journal lines skipped by this read
    /// (`#[serde(default)]` — additive evolution: an older Host tolerates a
    /// newer engine's field). Zero on a healthy journal; nonzero is a
    /// surfaced partial-corruption hint (see [`JournalView::
    /// blank_lines_skipped`]), never a silent drop.
    #[serde(default)]
    pub blank_lines_skipped: u64,
    /// The seq of the journal's FIRST retained entry (1 when the journal is
    /// empty) — the retention floor. A request cursor below `retained_from`
    /// minus one is refused (the entries it would skip were rotated away);
    /// this field is how a Host learns the floor without a failed round-trip.
    pub retained_from: u64,
}

/// The STABLE marker sentences the refusals carry (one construction site each,
/// below). The Control API's error mapper matches these SUBSTRINGS and lifts
/// them to 409 `cursor_invalid` — the same coupling pattern as the resync
/// truncation sentence. If a sentence is ever reworded, the mapper's unit pins
/// and the end-to-end conformance tests go red together.
pub const BELOW_FLOOR_SENTENCE: &str = "below the journal's retained floor";
pub const PAST_TAIL_SENTENCE: &str = "past the journal's committed tail";

/// Build the below-floor refusal detail (the pull's guard): the cursor's
/// window would include evicted entries. The EXACT rule (the supervisor's
/// ratified wording): a cursor at `retained_from - 1` or above is servable
/// LOSSLESSLY (its window starts at the first retained entry); a cursor
/// strictly below that would silently skip rotated-away entries and is
/// refused. Names BOTH counts so a Host can reset deliberately.
fn below_floor_detail(cursor: u64, retained_from: u64) -> String {
    format!(
        "the export cursor ({cursor}) is {BELOW_FLOOR_SENTENCE} ({retained_from}) — the \
         entries it would skip were rotated away (the lossless rule: cursors from \
         {servable} up are still served, anything lower would silently skip rotated-away \
         records); re-pull from the cursor the journal still retains, or reset it \
         deliberately to accept the loss",
        servable = retained_from.saturating_sub(1)
    )
}

/// Build the past-tail refusal detail (shared by the pull's above-tail guard
/// and the ack-past-tail refusal): the journal never committed a record this
/// far — the record was truncated away or the cursor is bogus.
fn past_tail_detail(position: u64, tail: u64, what: &str) -> String {
    format!(
        "the export {what} ({position}) is {PAST_TAIL_SENTENCE} ({tail}) — the record was \
         truncated or rotated away, and honoring it would silently skip records; reset the \
         cursor deliberately if a re-pull from the journal's retention floor is intended"
    )
}

/// The read-side view of one journal read: the retained entries in physical
/// (journal) order, the torn-tail flag, the interior-blank count, and the
/// derived retention facts.
#[derive(Debug)]
pub(crate) struct JournalView {
    pub entries: Vec<OutboxEntry>,
    pub torn_tail_skipped: bool,
    /// Interior blank/whitespace-only journal lines SKIPPED by this read.
    /// A blank line is harmless structurally (the parse simply finds no
    /// record there), but it is also a plausible partial-corruption or
    /// external-editor signature — so it is COUNTED and surfaced on every
    /// export batch, never silently dropped (one reader, one posture:
    /// tail damage is loud, interior blanks are counted-and-surfaced).
    pub blank_lines_skipped: u64,
    /// The first retained entry's seq, or 1 when the journal holds nothing.
    pub retained_from: u64,
    /// The last retained entry's seq, or 0 when the journal holds nothing.
    pub tail: u64,
}

/// Read the outbox journal with the RESYNC torn-tail posture: an absent file
/// is empty; ONE unparseable TRAILING line is skipped only when it carries the
/// torn-append signature (the raw text does not end with a newline) and the
/// skip is SURFACED; every other malformed line is an error naming its
/// physical line number. A valid-JSON wrong-shape trailing line is the wrong
/// file — an error, never popped as a tear.
///
/// Entries are served in PHYSICAL order; the seq sequence is VERIFIED here
/// (review finding 2): a duplicate or decreasing seq between consecutive
/// entries is a typed error naming both seqs — never a silent twin delivery.
/// One engine process cannot produce it (the counter is monotonic under the
/// supervisor lock), so seeing one means TWO WRITERS touched the journal
/// (the documented cross-process caveat is real, observed, and refused
/// loudly here — it would otherwise corrupt ack math and evict
/// never-delivered entries at compaction).
pub(crate) fn read_journal_tolerant(path: &Path) -> Result<JournalView, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalView {
                entries: Vec::new(),
                torn_tail_skipped: false,
                blank_lines_skipped: 0,
                retained_from: 1,
                tail: 0,
            })
        }
        Err(e) => return Err(e.to_string()),
    };
    let newline_terminated = text.ends_with('\n');
    let mut blank_lines_skipped: u64 = 0;
    let mut lines: Vec<(usize, &str)> = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            // Counted-and-surfaced (see `JournalView::blank_lines_skipped`):
            // never a silent drop, never a hard error either — the records
            // around it parse fine, so the journal is still serviceable.
            blank_lines_skipped += 1;
            continue;
        }
        lines.push((idx, line));
    }
    let mut torn_tail_skipped = false;
    if let Some((last_idx, last)) = lines.last() {
        if serde_json::from_str::<OutboxEntry>(last).is_err() {
            if serde_json::from_str::<serde_json::Value>(last).is_ok() {
                // Valid JSON, wrong record shape: the WRONG FILE, not a tear.
                return Err(format!(
                    "corrupt outbox journal: line {} is valid JSON but not an outbox record \
                     — the wrong file?",
                    last_idx + 1
                ));
            }
            if newline_terminated {
                // Newline-terminated yet unparseable: corruption, or a fused
                // line — surfaced, never silently popped (the resync rule).
                // The remediation is NAMED (review finding 5): quarantine the
                // journal, then re-sync; the next engine open re-derives its
                // sequence from the journal's absence.
                return Err(format!(
                    "corrupt outbox journal: trailing line {} is unparseable and \
                     newline-terminated — not a torn append (corruption, or a fused line a \
                     post-crash append left); to recover: stop the engine (or host), \
                     quarantine the journal by renaming the file (the next open re-derives \
                     its event sequence from the journal's absence), and re-sync the \
                     families with resync_events — see https://hekma.ktesio.dev/docs/control-api/",
                    last_idx + 1
                ));
            }
            lines.pop();
            torn_tail_skipped = true;
        }
    }
    let mut entries: Vec<OutboxEntry> = Vec::with_capacity(lines.len());
    let mut prev_seq: Option<u64> = None;
    for (idx, line) in lines {
        let entry: OutboxEntry = serde_json::from_str(line)
            .map_err(|e| format!("corrupt outbox journal line {}: {e}", idx + 1))?;
        // The read-time duplicate/decreasing-seq guard (review finding 2):
        // one engine process cannot mint a non-increasing seq (the counter
        // is monotonic under the supervisor lock), so a violation means two
        // writers interleaved on this journal. Refuse LOUDLY — delivering a
        // twin, or letting the ack math double-count a reused range, would
        // corrupt exactly the delivery guarantees this file exists for.
        if let Some(prev) = prev_seq {
            if entry.seq <= prev {
                return Err(format!(
                    "corrupt outbox journal: line {} carries seq {} after seq {} — a \
                     duplicate or decreasing sequence, which one engine process cannot \
                     produce (two engines wrote this journal concurrently); quarantine \
                     the journal by renaming it (the next open re-derives the sequence \
                     from its absence) and re-sync the families with resync_events — \
                     see https://hekma.ktesio.dev/docs/control-api/",
                    idx + 1,
                    entry.seq,
                    prev
                ));
            }
        }
        prev_seq = Some(entry.seq);
        entries.push(entry);
    }
    let retained_from = entries.first().map_or(1, |e| e.seq);
    let tail = entries.last().map_or(0, |e| e.seq);
    Ok(JournalView {
        entries,
        torn_tail_skipped,
        blank_lines_skipped,
        retained_from,
        tail,
    })
}

/// The PURE export selection (unit-tested without I/O): from a journal view,
/// select the entries with seq strictly greater than `after` (bounded by
/// `max`), and compute the batch. A cursor below the retained floor (it would
/// silently skip rotated-away entries) or above the committed tail is an
/// `Err` carrying the STABLE refusal sentence — never a clamp.
pub(crate) fn select_batch(
    view: &JournalView,
    after: u64,
    max: usize,
) -> Result<ExportBatch, String> {
    if after > view.tail {
        return Err(past_tail_detail(after, view.tail, "cursor"));
    }
    if after + 1 < view.retained_from {
        return Err(below_floor_detail(after, view.retained_from));
    }
    let events: Vec<OutboxEntry> = view
        .entries
        .iter()
        .filter(|entry| entry.seq > after)
        .take(max)
        .cloned()
        .collect();
    let cursor_seq = events.last().map_or(after, |entry| entry.seq);
    Ok(ExportBatch {
        schema_version: OUTBOX_SCHEMA_VERSION,
        events,
        cursor: OutboxCursor { seq: cursor_seq },
        torn_tail_skipped: view.torn_tail_skipped,
        blank_lines_skipped: view.blank_lines_skipped,
        retained_from: view.retained_from,
    })
}

/// The PURE ack decision (unit-tested without I/O): `given` past the
/// committed tail is the same refusal as a pull past the tail; anything at or
/// below the tail is accepted (the caller stores `max(current, given)` — a
/// regressive ack is a no-op success by construction).
pub(crate) fn check_ack(given: u64, tail: u64) -> Result<(), String> {
    if given > tail {
        return Err(past_tail_detail(given, tail, "ack"));
    }
    Ok(())
}

/// Read the durable ack cursor (`EnginePaths::outbox_ack`): `None` = no ack
/// file yet (a fresh state dir — the default pull backfills from seq 0).
pub(crate) fn read_ack(path: &Path) -> Result<Option<u64>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    #[derive(Deserialize)]
    struct AckDocument {
        seq: u64,
    }
    let doc: AckDocument = serde_json::from_str(&text)
        .map_err(|e| format!("corrupt outbox ack file {}: {e}", path.display()))?;
    Ok(Some(doc.seq))
}

/// The facade-facing export core (story 21-1: served from state.db's
/// `outbox` table): resolve the request cursor (explicit, or the durable
/// ack), check it against the retained floor and the committed tail, and
/// read the bounded batch. Bounds and page come from ONE read snapshot, so a
/// concurrent eviction can never slide the floor between the check and the
/// read (a cursor that passed the check is served losslessly). `max == 0`
/// takes [`EXPORT_BATCH_DEFAULT_MAX`]; anything above
/// [`EXPORT_BATCH_MAX_CAP`] is clamped. Errors ride [`EngineError::Log`]
/// naming state.db — the refusal details carry the STABLE marker sentences
/// the Control API maps to 409 `cursor_invalid`.
pub(crate) fn export_core(
    registry: &Registry,
    after: Option<OutboxCursor>,
    max: u32,
) -> Result<ExportBatch, EngineError> {
    let max = if max == 0 {
        EXPORT_BATCH_DEFAULT_MAX as usize
    } else {
        (max.min(EXPORT_BATCH_MAX_CAP)) as usize
    };
    let (bounds, page) = registry
        .reader()
        .snapshot(|store| {
            let bounds = store.outbox_bounds()?;
            let after_seq = after.map_or(bounds.ack.unwrap_or(0), |cursor| cursor.seq);
            let page = store.outbox_page(after_seq, max)?;
            Ok((bounds, (after_seq, page)))
        })
        .map_err(|err| outbox_error(registry, err.to_string()))?;
    let (after_seq, page) = page;
    let view = JournalView {
        entries: page.items,
        torn_tail_skipped: false,
        blank_lines_skipped: 0,
        retained_from: bounds.retained_from,
        tail: bounds.tail,
    };
    select_batch(&view, after_seq, max).map_err(|detail| outbox_error(registry, detail))
}

/// The facade-facing ack core (story 17-4; since story 21-1 one store-writer
/// job): MONOTONICALLY advance the durable ack cursor to `max(current,
/// given)` and return the persisted value — a re-ack or a backward ack is the
/// current cursor (idempotent), never a rewind. The target is validated
/// against the committed tail inside the same transaction (`check_ack`): a
/// cursor PAST the tail names the stable [`PAST_TAIL_SENTENCE`] refusal (the
/// Control API maps it to 409 `cursor_invalid`). The writer serializes the
/// ack against every append's eviction, so compaction never decides against a
/// moving cursor.
pub(crate) fn ack_core(registry: &Registry, given: u64) -> Result<u64, EngineError> {
    let outcome = registry
        .writer()
        .call(move |ctx| {
            let store = ctx.store();
            let bounds = store.outbox_bounds()?;
            let current = bounds.ack.unwrap_or(0);
            let target = current.max(given);
            if let Err(detail) = check_ack(target, bounds.tail) {
                return Ok(Err(detail));
            }
            // No-op acks write nothing (review finding 20): only a real
            // ADVANCE touches the row.
            if target > current {
                store.set_outbox_ack(target)?;
            }
            Ok(Ok(target))
        })
        .map_err(|err| outbox_error(registry, err.to_string()))?;
    outcome.map_err(|detail| outbox_error(registry, detail))
}

/// A typed outbox error naming state.db (the outbox's home since 21-1).
fn outbox_error(registry: &Registry, detail: String) -> EngineError {
    EngineError::Log {
        name: "<outbox>".to_string(),
        path: registry.state_db_path().to_string_lossy().into_owned(),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event::TransitionEvent;
    use crate::domain::{LifecycleState, TransitionCause};

    fn transition(n: usize) -> TransitionEvent {
        TransitionEvent::new(
            "probe",
            LifecycleState::Starting,
            LifecycleState::Running,
            TransitionCause::AdapterReady,
            format!("2026-10-19T00:00:{n:02}Z"),
        )
    }

    fn entry(seq: u64) -> OutboxEntry {
        OutboxEntry {
            schema_version: OUTBOX_SCHEMA_VERSION,
            seq,
            event: EngineEvent::Transition(transition(seq as usize)),
        }
    }

    fn view(entries: &[u64]) -> JournalView {
        let entries: Vec<OutboxEntry> = entries.iter().map(|seq| entry(*seq)).collect();
        let retained_from = entries.first().map_or(1, |e| e.seq);
        let tail = entries.last().map_or(0, |e| e.seq);
        JournalView {
            entries,
            torn_tail_skipped: false,
            blank_lines_skipped: 0,
            retained_from,
            tail,
        }
    }

    /// Write one journal line the way the pre-21-1 engine appended it (the
    /// import's input format).
    fn append_line(journal: &std::path::Path, entry: &OutboxEntry) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(entry).unwrap()).unwrap();
    }

    fn batch_event(batch: &ExportBatch, idx: usize) -> u64 {
        batch.events[idx].seq
    }

    // ---- the pure selection ----

    #[test]
    fn select_batch_serves_the_suffix_in_order_and_advances_the_cursor() {
        let view = view(&[1, 2, 3, 4, 5]);
        let batch = select_batch(&view, 2, 10).expect("a mid-journal cursor serves");
        assert_eq!(batch.events.len(), 3);
        assert_eq!(batch_event(&batch, 0), 3);
        assert_eq!(batch.cursor, OutboxCursor { seq: 5 });
        assert_eq!(batch.retained_from, 1);
        assert!(!batch.torn_tail_skipped);
        assert_eq!(batch.schema_version, OUTBOX_SCHEMA_VERSION);
    }

    #[test]
    fn select_batch_bounds_to_max_and_the_cursor_names_the_last_served_entry() {
        let view = view(&[1, 2, 3, 4, 5]);
        let batch = select_batch(&view, 0, 2).expect("bounded batch");
        assert_eq!(batch.events.len(), 2);
        assert_eq!(
            batch.cursor,
            OutboxCursor { seq: 2 },
            "cursor == last served"
        );
        // The NEXT pull with that cursor serves exactly the rest.
        let next = select_batch(&view, batch.cursor.seq, 10).unwrap();
        assert_eq!(next.events.len(), 3);
        assert_eq!(next.cursor, OutboxCursor { seq: 5 });
    }

    #[test]
    fn select_batch_caught_up_is_an_empty_batch_at_the_same_cursor() {
        let view = view(&[1, 2, 3]);
        let batch = select_batch(&view, 3, 10).unwrap();
        assert!(batch.events.is_empty());
        assert_eq!(batch.cursor, OutboxCursor { seq: 3 });
    }

    #[test]
    fn select_batch_of_an_empty_journal_is_an_empty_batch_at_start() {
        let view = view(&[]);
        assert_eq!(view.retained_from, 1, "an empty journal's floor is 1");
        assert_eq!(view.tail, 0);
        let batch = select_batch(&view, 0, 10).unwrap();
        assert!(batch.events.is_empty());
        assert_eq!(batch.retained_from, 1);
    }

    #[test]
    fn a_cursor_past_the_tail_is_refused_with_the_stable_sentence() {
        let view = view(&[1, 2, 3]);
        let err = select_batch(&view, 4, 10).unwrap_err();
        assert!(
            err.contains(PAST_TAIL_SENTENCE),
            "the above-tail refusal carries the stable sentence: {err}"
        );
        assert!(
            err.contains("(4)") && err.contains("(3)"),
            "both counts: {err}"
        );
    }

    #[test]
    fn a_cursor_below_the_retained_floor_is_refused_with_the_stable_sentence() {
        // Rotation kept seqs 5.. only. A cursor of 3 would silently skip 4.
        let view = view(&[5, 6, 7]);
        assert_eq!(view.retained_from, 5);
        let err = select_batch(&view, 3, 10).unwrap_err();
        assert!(
            err.contains(BELOW_FLOOR_SENTENCE),
            "the below-floor refusal carries the stable sentence: {err}"
        );
        assert!(
            err.contains("(3)") && err.contains("(5)"),
            "both counts: {err}"
        );

        // The boundary: a cursor of 4 (floor − 1) is SERVABLE — everything
        // after 4 is exactly what the journal retains.
        let batch = select_batch(&view, 4, 10).expect("floor-minus-one is servable");
        assert_eq!(batch.events.len(), 3);
    }

    // ---- the pure ack ----

    #[test]
    fn check_ack_accepts_everything_at_or_below_the_tail() {
        assert_eq!(check_ack(0, 5).unwrap(), ());
        assert_eq!(check_ack(5, 5).unwrap(), ());
        // An empty journal: only seq 0 is acceptable (the no-op).
        assert_eq!(check_ack(0, 0).unwrap(), ());
    }

    #[test]
    fn check_ack_past_the_tail_is_refused_with_the_stable_sentence() {
        let err = check_ack(6, 5).unwrap_err();
        assert!(
            err.contains(PAST_TAIL_SENTENCE),
            "the ack refusal carries the SAME stable sentence as the pull refusal: {err}"
        );
    }

    // ---- the pure compaction plan ----

    // ---- the journal I/O ----

    #[test]
    fn append_and_read_round_trip_in_seq_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        for seq in 1..=3u64 {
            append_line(&journal, &entry(seq));
        }
        let view = read_journal_tolerant(&journal).expect("the journal reads back");
        let seqs: Vec<u64> = view.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert!(!view.torn_tail_skipped);
        assert_eq!(view.retained_from, 1);
        assert_eq!(view.tail, 3);
    }

    #[test]
    fn a_torn_tail_read_is_surfaced() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        append_line(&journal, &entry(1));
        append_line(&journal, &entry(2));
        let text = std::fs::read_to_string(&journal).unwrap();
        std::fs::write(&journal, &text[..text.len() - 5]).unwrap();
        let view = read_journal_tolerant(&journal).expect("torn tail tolerated");
        assert_eq!(view.entries.len(), 1, "the complete prefix survives");
        assert!(view.torn_tail_skipped, "the skip is surfaced, never silent");
    }

    #[test]
    fn read_treats_an_absent_journal_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let view = read_journal_tolerant(&dir.path().join("never.jsonl")).unwrap();
        assert!(view.entries.is_empty());
        assert_eq!(view.retained_from, 1);
        assert_eq!(view.tail, 0);
    }

    #[test]
    fn a_valid_json_wrong_shape_trailing_line_is_the_wrong_file_not_a_tear() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        append_line(&journal, &entry(1));
        let text = std::fs::read_to_string(&journal).unwrap();
        std::fs::write(&journal, format!("{text}{{\"nope\":1}}\n")).unwrap();
        let err = read_journal_tolerant(&journal).unwrap_err();
        assert!(err.contains("valid JSON but not an outbox record"), "{err}");
    }

    #[test]
    fn a_newline_terminated_unparseable_trailing_line_is_an_error_not_a_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        append_line(&journal, &entry(1));
        let text = std::fs::read_to_string(&journal).unwrap();
        let torn = "not json at all";
        std::fs::write(&journal, format!("{text}{torn}\n")).unwrap();
        let err = read_journal_tolerant(&journal).unwrap_err();
        assert!(err.contains("newline-terminated"), "{err}");
    }

    #[test]
    fn an_interior_error_reports_the_physical_line_number() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        append_line(&journal, &entry(1));
        append_line(&journal, &entry(2));
        let text = std::fs::read_to_string(&journal).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let mangled = format!("{}\n{{\"not\":\"an entry\"}}\n{}", lines[0], lines[1]);
        std::fs::write(&journal, mangled).unwrap();
        let err = read_journal_tolerant(&journal).unwrap_err();
        assert!(
            err.contains("line 2"),
            "the physical line number is reported: {err}"
        );
    }

    // ---- the ack file ----

    #[test]
    fn a_corrupt_ack_file_is_an_error_not_a_silent_reset() {
        let dir = tempfile::TempDir::new().unwrap();
        let ack = dir.path().join("outbox-ack.json");
        std::fs::write(&ack, "not json").unwrap();
        let err = read_ack(&ack).unwrap_err();
        assert!(err.contains("corrupt outbox ack"), "{err}");
    }

    // ---- the facade core ----

    // ---- the I/O error arms: every typed EngineError::Log constructor ----
    //
    // The fault injection is the codebase's established idiom (the
    // paths-suite atomic-write tests): a DIRECTORY occupies the file path,
    // so the read/open fails with a non-NotFound OS error. Each surfaced
    // error must name its file and carry the OS detail — a silent empty
    // read would serve a Host a false caught-up batch, and a silent ack
    // reset would rewind a consumer cursor.

    #[test]
    fn an_unreadable_journal_is_an_error_not_an_empty_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        std::fs::create_dir(&journal).unwrap();
        let err = read_journal_tolerant(&journal).unwrap_err();
        assert!(!err.is_empty(), "the OS detail is preserved: {err}");
    }

    #[test]
    fn an_unreadable_ack_file_is_an_error_not_a_fresh_cursor() {
        let dir = tempfile::TempDir::new().unwrap();
        let ack = dir.path().join("outbox-ack.json");
        std::fs::create_dir(&ack).unwrap();
        let err = read_ack(&ack).unwrap_err();
        assert!(!err.is_empty(), "the OS detail is preserved: {err}");
    }

    // ---- review-finding pins (17-4 review pass) ----

    #[test]
    fn a_duplicate_or_decreasing_seq_is_a_typed_error_naming_both_seqs() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        // Two engines writing one journal (the cross-process caveat): the
        // second writer's restart re-minted seq 1. The reader refuses — a
        // silent twin delivery would corrupt ack math and evict
        // never-delivered entries at compaction.
        let twin = format!(
            "{}\n{}\n",
            serde_json::to_string(&entry(1)).unwrap(),
            serde_json::to_string(&entry(1)).unwrap()
        );
        std::fs::write(&journal, twin).unwrap();
        let err = read_journal_tolerant(&journal).unwrap_err();
        assert!(err.contains("duplicate or decreasing sequence"), "{err}");
        assert!(err.contains("two engines wrote this journal"), "{err}");
        // A DECREASING seq is the same refusal.
        let decreasing = format!(
            "{}\n{}\n",
            serde_json::to_string(&entry(5)).unwrap(),
            serde_json::to_string(&entry(2)).unwrap()
        );
        std::fs::write(&journal, decreasing).unwrap();
        let err = read_journal_tolerant(&journal).unwrap_err();
        assert!(err.contains("duplicate or decreasing sequence"), "{err}");
    }

    // ---- the state.db outbox (story 21-1) ----

    fn registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = Registry::open(Some(dir.path().to_path_buf())).unwrap();
        (dir, registry)
    }

    fn append_db(registry: &Registry, n: u64, cap: u64) -> crate::ports::OutboxAppend {
        registry
            .writer()
            .call(move |ctx| {
                ctx.store()
                    .append_outbox(&EngineEvent::Transition(transition(n as usize)), cap)
            })
            .unwrap()
    }

    #[test]
    fn export_core_resumes_from_the_durable_ack_cursor_by_default() {
        let (_dir, registry) = registry();
        for n in 1..=3 {
            assert_eq!(append_db(&registry, n, OUTBOX_MAX_BYTES).seq, n);
        }
        assert_eq!(ack_core(&registry, 2).unwrap(), 2);
        let batch = export_core(&registry, None, 0).expect("default pull resumes");
        let seqs: Vec<u64> = batch.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3], "everything through the ack is skipped");
        assert_eq!(batch.cursor, OutboxCursor { seq: 3 });
        assert!(!batch.torn_tail_skipped);
        assert_eq!(batch.blank_lines_skipped, 0);
    }

    #[test]
    fn export_core_with_an_explicit_cursor_re_pulls_and_max_bounds_the_batch() {
        let (_dir, registry) = registry();
        for n in 1..=3 {
            append_db(&registry, n, OUTBOX_MAX_BYTES);
        }
        let batch = export_core(&registry, Some(OutboxCursor { seq: 0 }), 2).unwrap();
        assert_eq!(batch.events.len(), 2, "the explicit max bounds the batch");
        assert_eq!(batch.cursor, OutboxCursor { seq: 2 });
        let all = export_core(&registry, Some(OutboxCursor::START), 0).unwrap();
        assert_eq!(all.events.len(), 3, "max 0 takes the default");
    }

    #[test]
    fn export_core_refuses_past_the_tail_and_below_the_floor() {
        let (_dir, registry) = registry();
        let one = serde_json::to_string(&EngineEvent::Transition(transition(1)))
            .unwrap()
            .len() as u64;
        append_db(&registry, 1, OUTBOX_MAX_BYTES);
        append_db(&registry, 2, OUTBOX_MAX_BYTES);
        // A toy cap: the third append evicts the oldest un-acked row.
        let third = append_db(&registry, 3, 3 * one - 1);
        assert_eq!(third.seq, 3);
        assert_eq!(third.dropped_unacked, 1, "the cap evicted seq 1, un-acked");
        let above = export_core(&registry, Some(OutboxCursor { seq: 9 }), 0).unwrap_err();
        assert!(
            matches!(&above, EngineError::Log { detail, .. } if detail.contains(PAST_TAIL_SENTENCE)),
            "{above:?}"
        );
        let below = export_core(&registry, Some(OutboxCursor { seq: 0 }), 0).unwrap_err();
        assert!(
            matches!(&below, EngineError::Log { detail, .. }
                if detail.contains(BELOW_FLOOR_SENTENCE)),
            "{below:?}"
        );
        let served = export_core(&registry, Some(OutboxCursor { seq: 1 }), 0).unwrap();
        assert_eq!(served.retained_from, 2);
        assert_eq!(
            served.events.len(),
            2,
            "a cursor at the floor - 1 is lossless"
        );
    }

    #[test]
    fn the_acked_prefix_is_evicted_first_and_silently() {
        let (_dir, registry) = registry();
        for n in 1..=3 {
            append_db(&registry, n, OUTBOX_MAX_BYTES);
        }
        ack_core(&registry, 2).unwrap();
        let one = serde_json::to_string(&EngineEvent::Transition(transition(1)))
            .unwrap()
            .len() as u64;
        let append = append_db(&registry, 4, 3 * one);
        assert_eq!(append.dropped_acked, 2, "the whole acked prefix goes");
        assert_eq!(append.dropped_unacked, 0);
        let batch = export_core(&registry, Some(OutboxCursor { seq: 2 }), 0).unwrap();
        let seqs: Vec<u64> = batch.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3, 4]);
    }

    #[test]
    fn the_outbox_never_evicts_its_last_entry() {
        let (_dir, registry) = registry();
        append_db(&registry, 1, OUTBOX_MAX_BYTES);
        append_db(&registry, 2, OUTBOX_MAX_BYTES);
        let append = append_db(&registry, 3, 1);
        assert_eq!(append.dropped_unacked, 1, "only the older row can go");
        let batch = export_core(&registry, Some(OutboxCursor { seq: 1 }), 0).unwrap();
        let seqs: Vec<u64> = batch.events.iter().map(|e| e.seq).collect();
        assert_eq!(
            seqs,
            vec![2, 3],
            "the last existing row survives a 1-byte cap"
        );
        assert_eq!(batch.retained_from, 2);
    }

    #[test]
    fn ack_is_monotonic_and_refuses_past_the_tail() {
        let (_dir, registry) = registry();
        assert_eq!(
            ack_core(&registry, 0).unwrap(),
            0,
            "a zero ack on an empty outbox"
        );
        append_db(&registry, 1, OUTBOX_MAX_BYTES);
        append_db(&registry, 2, OUTBOX_MAX_BYTES);
        assert_eq!(ack_core(&registry, 2).unwrap(), 2);
        assert_eq!(
            ack_core(&registry, 1).unwrap(),
            2,
            "a backward ack is a no-op"
        );
        let err = ack_core(&registry, 3).unwrap_err();
        assert!(
            matches!(&err, EngineError::Log { detail, .. } if detail.contains(PAST_TAIL_SENTENCE)),
            "{err:?}"
        );
    }
}
