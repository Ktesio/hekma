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

use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::bus::EngineEvent;
use super::EngineError;
use crate::paths::write_atomically;
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

/// REPAIR a torn trailing fragment before an append (story 17-4): if the
/// journal's raw text does not end with a newline, a previous crash cut its
/// last write mid-line, and appending now would FUSE the new record onto the
/// fragment (the resync readers would then surface it as a corrupt fused
/// line). Truncate back to (and including) the last newline — dropping only
/// the torn fragment — so the journal stays line-structured. An absent or
/// clean file is a no-op. The dropped fragment was a partial record the
/// tolerant readers already skip; nothing parseable is lost.
///
/// Returns whether a repair TRUNCATED the file (the caller's byte-size
/// cache must re-derive in that case). O(1) on the clean-tail fast path —
/// only the last byte is read — so the hot append does not pay a
/// whole-journal read per record (review finding 7); the torn path reads a
/// bounded trailing window (falling back to a whole-file scan only for a
/// pathological fragment larger than the window).
pub(crate) fn repair_torn_tail(path: &Path) -> Result<bool, String> {
    use std::io::{Read, Seek, SeekFrom};
    // `Some(cut)` = a torn tail that must be truncated at byte `cut`;
    // `None` = clean (absent, empty, or newline-terminated) — no repair.
    let cut = (|| -> Result<Option<u64>, String> {
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            // Names the path: on Windows a directory at the journal path
            // fails HERE (ERROR_ACCESS_DENIED) — before the regular-file
            // guard below ever runs — and a bare OS detail names nothing.
            Err(e) => {
                return Err(format!(
                    "could not open the outbox journal for repair: {}: {e}",
                    path.display()
                ))
            }
        };
        let meta = file.metadata().map_err(|e| {
            format!(
                "could not stat the outbox journal for repair: {}: {e}",
                path.display()
            )
        })?;
        // The journal path must be a REGULAR file. `File::open` happily
        // opens a directory, and when that directory is empty its
        // `st_size` is 0 on filesystems that size empty directories at
        // zero (btrfs) — so the `len == 0` clean shortcut below returned
        // "clean" without a single read ever erroring (the unreadable-
        // journal red only showed on btrfs boxes; ext4/tmpfs dirs carry a
        // nonzero size and fail later at the EISDIR read). A FIFO would
        // hang the reads instead of erroring at all. The unreadable-
        // journal contract: surface it, never serve a false caught-up
        // read.
        if !meta.is_file() {
            return Err(format!(
                "the outbox journal is not a regular file: {}",
                path.display()
            ));
        }
        let len = meta.len();
        if len == 0 {
            return Ok(None);
        }
        // Fast path: a newline-terminated journal has no torn tail — one
        // byte decides it, and the hot append path never reads the body.
        file.seek(SeekFrom::Start(len - 1))
            .map_err(|e| e.to_string())?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last).map_err(|e| e.to_string())?;
        if last[0] == b'\n' {
            return Ok(None);
        }
        // Torn tail: find the cut inside a bounded trailing window.
        const WINDOW: u64 = 64 * 1024;
        let window_len = len.min(WINDOW);
        let window_start = len - window_len;
        file.seek(SeekFrom::Start(window_start))
            .map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; window_len as usize];
        file.read_exact(&mut buf).map_err(|e| e.to_string())?;
        let cut_in_buf = buf
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |idx| idx + 1);
        if cut_in_buf > 0 || window_len == len {
            // A newline inside the window (or the window IS the whole file:
            // no newline anywhere → cut to 0).
            return Ok(Some(window_start + cut_in_buf as u64));
        }
        // No newline in the window and the file is bigger: the fragment
        // itself is larger than the window (pathological) — fall back to a
        // whole-file scan so the cut stays exact.
        drop(file);
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Ok(Some(text.rfind('\n').map_or(0, |idx| idx as u64 + 1)))
    })()?;
    let Some(cut) = cut else {
        return Ok(false);
    };
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("could not open the outbox journal for repair: {e}"))?;
    writer
        .set_len(cut)
        .map_err(|e| format!("could not repair the outbox journal's torn tail: {e}"))?;
    Ok(true)
}

/// What one compaction pass did (all counts SURFACED by the caller).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CompactionOutcome {
    /// How many ACKED entries (seq ≤ ack cursor) were dropped — safe, silent
    /// by design (that is what ack means).
    pub dropped_acked: u64,
    /// How many UN-ACKED entries were dropped to get under the cap — the
    /// honest-degradation case; the caller emits a diagnostic naming the loss.
    pub dropped_unacked: u64,
}

/// The PURE compaction decision (unit-tested without I/O): given the entries
/// and their byte costs, the ack cursor, the incoming line's size, and the
/// cap, decide which PREFIX of entries to drop. Rules, in order:
/// 1. If current + incoming fits the cap, drop nothing.
/// 2. Drop the ACKED prefix first (seq ≤ ack) — acked means consumed.
/// 3. If STILL over, drop the oldest UN-ACKED entries too — but NEVER the
///    very last entry (the journal's high-water seq and `retained_from`
///    survive even under an acking-absent Host's flood).
///
/// Returns the keep-from index into `byte_costs` plus both drop counts.
pub(crate) fn compaction_plan(
    byte_costs: &[u64],
    seqs: &[u64],
    ack: Option<u64>,
    incoming_bytes: u64,
    cap: u64,
) -> (usize, CompactionOutcome) {
    let current: u64 = byte_costs.iter().sum();
    let mut outcome = CompactionOutcome::default();
    if current + incoming_bytes <= cap || byte_costs.is_empty() {
        return (0, outcome);
    }
    let mut freed = 0u64;
    let mut keep_from = 0usize;
    // Phase 1: the acked prefix is free to drop — but NEVER the very last
    // entry (review finding 3): a journal emptied by compaction, followed by
    // a crash before the next append, would restart the seq sequence at 1 on
    // the next open and silently fork the delivery range. Keeping the final
    // entry preserves the high-water seq through any crash window.
    while keep_from + 1 < seqs.len() {
        let is_acked = ack.is_some_and(|ack| seqs[keep_from] <= ack);
        if !is_acked {
            break;
        }
        freed += byte_costs[keep_from];
        outcome.dropped_acked += 1;
        keep_from += 1;
    }
    if freed >= current + incoming_bytes - cap {
        return (keep_from, outcome);
    }
    // Phase 2: still over — drop un-acked entries from the front, but never
    // the very last entry (keep at least one; the journal's tail survives).
    while keep_from + 1 < seqs.len() && freed < current + incoming_bytes - cap {
        freed += byte_costs[keep_from];
        outcome.dropped_unacked += 1;
        keep_from += 1;
    }
    (keep_from, outcome)
}

/// Append one entry to the journal, compacting first when the cap would be
/// crossed (the check-before-write approximation). Returns the compaction
/// outcome (the caller surfaces any un-acked eviction) AND the journal's
/// post-append byte size (the caller's O(1) size cache — review finding 7).
/// The append is ONE `write_all` of `line + '\n'` in append mode — a crash
/// cuts it to a torn fragment the next append repairs (see
/// [`repair_torn_tail`]) and tolerant readers skip.
///
/// `ack` is the CURRENT durable ack cursor (read before this call); `cap` is
/// [`OUTBOX_MAX_BYTES`] in production and a parameter so tests can drive the
/// eviction path at toy sizes. `cached_bytes` is the caller's CACHED journal
/// size (`None` = stat the file): the hot path needs no whole-journal read —
/// `repair_torn_tail` decides from one byte, the cap check compares cached
/// size + incoming, and only an actual compaction pays the O(journal) read.
/// A `Some` cache is re-derived automatically when the repair truncated (its
/// return says so) — the cache is trusted ONLY between the caller's own
/// writes (the supervisor holds the lock across both).
pub(crate) fn append_with_retention(
    journal: &Path,
    ack: Option<u64>,
    entry: &OutboxEntry,
    cap: u64,
    cached_bytes: Option<u64>,
) -> Result<(CompactionOutcome, u64), String> {
    // Torn fragment first: never fuse a new record onto a torn tail.
    let repaired = repair_torn_tail(journal)?;
    let mut current_bytes = if repaired || cached_bytes.is_none() {
        // The repair may have truncated (stale cache), or the caller had no
        // cache: stat the file (O(1)); an absent file is an empty journal.
        std::fs::metadata(journal).map(|m| m.len()).unwrap_or(0)
    } else {
        cached_bytes.unwrap_or_default()
    };
    let line = serde_json::to_string(entry)
        .map_err(|e| format!("could not serialize the outbox entry: {e}"))?;
    let incoming_bytes = line.len() as u64 + 1;
    let mut outcome = CompactionOutcome::default();
    // Compact only when the cap would be crossed.
    if current_bytes + incoming_bytes > cap {
        let view = read_journal_tolerant(journal)?;
        let byte_costs: Vec<u64> = view
            .entries
            .iter()
            .map(|e| serde_json::to_string(e).map_or(0, |l| l.len() as u64 + 1))
            .collect();
        let seqs: Vec<u64> = view.entries.iter().map(|e| e.seq).collect();
        let (keep_from, plan) = compaction_plan(&byte_costs, &seqs, ack, incoming_bytes, cap);
        if keep_from > 0 {
            let kept: Vec<OutboxEntry> = view.entries[keep_from..].to_vec();
            let mut rewritten = String::new();
            for kept_entry in &kept {
                let kept_line = serde_json::to_string(kept_entry)
                    .map_err(|e| format!("could not serialize the outbox entry: {e}"))?;
                rewritten.push_str(&kept_line);
                rewritten.push('\n');
            }
            write_atomically(journal, rewritten.as_bytes())
                .map_err(|e| format!("could not compact the outbox journal: {e}"))?;
            outcome = plan;
            current_bytes = rewritten.len() as u64;
        }
    }
    if let Some(parent) = journal.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create the outbox directory: {e}"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal)
        .map_err(|e| format!("could not open the outbox journal: {e}"))?;
    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("could not append to the outbox journal: {e}"))?;
    // The authoritative post-append size (O(1) stat — kills accumulation
    // drift; a stat failure degrades to the computed accounting).
    let new_size = std::fs::metadata(journal)
        .map(|m| m.len())
        .unwrap_or(current_bytes + incoming_bytes);
    Ok((outcome, new_size))
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

/// Persist the durable ack cursor atomically (`write_atomically` — the
/// crash-safety contract every durable engine write keeps).
pub(crate) fn write_ack(path: &Path, seq: u64) -> Result<(), String> {
    let body = format!("{{\"schema_version\":{OUTBOX_SCHEMA_VERSION},\"seq\":{seq}}}\n");
    write_atomically(path, body.as_bytes())
        .map_err(|e| format!("could not persist the outbox ack cursor: {e}"))
}

/// The facade-facing export core: read the journal, resolve the request
/// cursor (explicit, or the durable ack), select the batch. `max == 0` takes
/// [`EXPORT_BATCH_DEFAULT_MAX`]; anything above [`EXPORT_BATCH_MAX_CAP`] is
/// clamped to the cap. Errors ride [`EngineError::Log`] naming the journal —
/// the refusal details carry the STABLE marker sentences the Control API maps
/// to 409 `cursor_invalid`.
pub(crate) fn export_core(
    journal: &Path,
    ack_file: &Path,
    after: Option<OutboxCursor>,
    max: u32,
) -> Result<ExportBatch, EngineError> {
    let journal_path = journal.to_string_lossy().into_owned();
    let after_seq = match after {
        Some(cursor) => cursor.seq,
        None => read_ack(ack_file)
            .map_err(|detail| EngineError::Log {
                name: "<outbox>".to_string(),
                path: ack_file.to_string_lossy().into_owned(),
                detail,
            })?
            .unwrap_or(0),
    };
    let view = read_journal_tolerant(journal).map_err(|detail| EngineError::Log {
        name: "<outbox>".to_string(),
        path: journal_path.clone(),
        detail,
    })?;
    let max = if max == 0 {
        EXPORT_BATCH_DEFAULT_MAX as usize
    } else {
        (max.min(EXPORT_BATCH_MAX_CAP)) as usize
    };
    select_batch(&view, after_seq, max).map_err(|detail| EngineError::Log {
        name: "<outbox>".to_string(),
        path: journal_path,
        detail,
    })
}

/// The facade-facing ack core (story 17-4): MONOTONICALLY advance the
/// durable ack cursor to `max(current, given)` and return the persisted
/// value — a re-ack or a backward ack is the current cursor (idempotent),
/// never a rewind. The target is validated against the journal's committed
/// tail (`check_ack`): a cursor PAST the tail names the stable
/// [`PAST_TAIL_SENTENCE`] refusal (the Control API maps it to 409
/// `cursor_invalid`) — an ack is a claim about entries that exist. The
/// caller holds the SUPERVISOR lock, so this cannot race an append's
/// compaction (a compaction deciding what to keep against a MOVING ack
/// cursor would be the one unsound interleaving).
pub(crate) fn ack_core(journal: &Path, ack_file: &Path, given: u64) -> Result<u64, EngineError> {
    let ack_path = ack_file.to_string_lossy().into_owned();
    let current = read_ack(ack_file)
        .map_err(|detail| EngineError::Log {
            name: "<outbox>".to_string(),
            path: ack_path.clone(),
            detail,
        })?
        .unwrap_or(0);
    let target = current.max(given);
    let view = read_journal_tolerant(journal).map_err(|detail| EngineError::Log {
        name: "<outbox>".to_string(),
        path: journal.to_string_lossy().into_owned(),
        detail,
    })?;
    check_ack(target, view.tail).map_err(|detail| EngineError::Log {
        name: "<outbox>".to_string(),
        path: ack_path.clone(),
        detail,
    })?;
    // No-op acks do not touch the disk (review finding 20): a re-ack at or
    // below the current cursor — including an ack of 0 on a fresh journal,
    // where `current` is the absence-derived 0 — is already durable; only a
    // real ADVANCE rewrites the file. Rewriting the same value would be pure
    // churn on the Host's polling path.
    if target > current {
        write_ack(ack_file, target).map_err(|detail| EngineError::Log {
            name: "<outbox>".to_string(),
            path: ack_path,
            detail,
        })?;
    }
    Ok(target)
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

    fn costs(entries: &[usize]) -> Vec<u64> {
        entries.iter().map(|n| *n as u64).collect()
    }

    #[test]
    fn compaction_is_skipped_while_the_cap_holds() {
        let (keep_from, outcome) = compaction_plan(&costs(&[10, 10]), &[1, 2], Some(0), 5, 100);
        assert_eq!(keep_from, 0);
        assert_eq!(outcome, CompactionOutcome::default());
    }

    #[test]
    fn compaction_drops_the_acked_prefix_first() {
        // 10 + 10 + incoming 5 = 25 > cap 18: drop the acked prefix (10) → 15 ≤ 18.
        let (keep_from, outcome) = compaction_plan(&costs(&[10, 10]), &[1, 2], Some(1), 5, 18);
        assert_eq!(keep_from, 1);
        assert_eq!(outcome.dropped_acked, 1);
        assert_eq!(
            outcome.dropped_unacked, 0,
            "acked drops are free, not losses"
        );
    }

    #[test]
    fn compaction_evicts_unacked_entries_only_when_still_over_the_cap() {
        // Nothing acked, cap forces an un-acked eviction.
        let (keep_from, outcome) = compaction_plan(&costs(&[10, 10, 10]), &[1, 2, 3], None, 5, 12);
        assert!(keep_from > 0);
        assert_eq!(outcome.dropped_acked, 0);
        assert_eq!(outcome.dropped_unacked, keep_from as u64);
    }

    #[test]
    fn compaction_never_drops_the_very_last_entry() {
        // One huge entry over the cap alone, nothing acked: it STAYS.
        let (keep_from, outcome) = compaction_plan(&costs(&[100]), &[1], None, 5, 50);
        assert_eq!(keep_from, 0, "the last entry survives even over cap");
        assert_eq!(outcome.dropped_unacked, 0);
    }

    #[test]
    fn compaction_of_an_empty_journal_is_a_no_op() {
        let (keep_from, outcome) = compaction_plan(&[], &[], Some(3), 5, 1);
        assert_eq!(keep_from, 0);
        assert_eq!(outcome, CompactionOutcome::default());
    }

    // ---- the journal I/O ----

    #[test]
    fn append_and_read_round_trip_in_seq_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        for seq in 1..=3u64 {
            let _ = append_with_retention(&journal, None, &entry(seq), OUTBOX_MAX_BYTES, None)
                .expect("append succeeds");
        }
        let view = read_journal_tolerant(&journal).expect("the journal reads back");
        let seqs: Vec<u64> = view.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert!(!view.torn_tail_skipped);
        assert_eq!(view.retained_from, 1);
        assert_eq!(view.tail, 3);
    }

    #[test]
    fn append_repairs_a_torn_tail_instead_of_fusing() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let _ = append_with_retention(&journal, None, &entry(1), OUTBOX_MAX_BYTES, None).unwrap();
        let _ = append_with_retention(&journal, None, &entry(2), OUTBOX_MAX_BYTES, None).unwrap();
        // Simulate a crash mid-append: the LAST record's write was cut
        // mid-line (chop its tail off, leaving it newline-less). Entry 2's
        // torn write never committed — the repair drops it; the point is
        // the journal stays line-structured, not that torn bytes survive.
        let text = std::fs::read_to_string(&journal).unwrap();
        std::fs::write(&journal, &text[..text.len() - 10]).unwrap();
        // The next append repairs before writing.
        let _ = append_with_retention(&journal, None, &entry(3), OUTBOX_MAX_BYTES, None).unwrap();
        let view = read_journal_tolerant(&journal).expect("repaired journal reads clean");
        let seqs: Vec<u64> = view.entries.iter().map(|e| e.seq).collect();
        assert_eq!(
            seqs,
            vec![1, 3],
            "the torn fragment was dropped, not fused onto the new record"
        );
        assert!(!view.torn_tail_skipped);
    }

    #[test]
    fn a_torn_tail_read_is_surfaced() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let _ = append_with_retention(&journal, None, &entry(1), OUTBOX_MAX_BYTES, None).unwrap();
        let _ = append_with_retention(&journal, None, &entry(2), OUTBOX_MAX_BYTES, None).unwrap();
        let text = std::fs::read_to_string(&journal).unwrap();
        std::fs::write(&journal, &text[..text.len() - 5]).unwrap();
        let view = read_journal_tolerant(&journal).expect("torn tail tolerated");
        assert_eq!(view.entries.len(), 1, "the complete prefix survives");
        assert!(view.torn_tail_skipped, "the skip is surfaced, never silent");
    }

    #[test]
    fn append_compacts_the_acked_prefix_at_the_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        // Toy cap: each entry line is well over 100 bytes here, so three
        // entries cross a 400-byte cap; seqs 1-2 are acked.
        let _ = append_with_retention(&journal, Some(0), &entry(1), 400, None).unwrap();
        let _ = append_with_retention(&journal, Some(0), &entry(2), 400, None).unwrap();
        let _ = append_with_retention(&journal, Some(2), &entry(3), 400, None).unwrap();
        let view = read_journal_tolerant(&journal).unwrap();
        let seqs: Vec<u64> = view.entries.iter().map(|e| e.seq).collect();
        assert!(
            seqs.len() < 3 || seqs[0] > 1,
            "the acked prefix was eligible for compaction: {seqs:?}"
        );
    }

    #[test]
    fn append_evicts_unacked_entries_at_the_cap_and_never_the_last_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        // Cap 300 bytes; each line ~180-200. Nothing is acked: the FIRST
        // entry must be evicted (surfaced), never the last.
        let (outcome1, _) = append_with_retention(&journal, None, &entry(1), 300, None).unwrap();
        assert_eq!(outcome1.dropped_unacked, 0);
        let (outcome2, _) = append_with_retention(&journal, None, &entry(2), 300, None).unwrap();
        let view = read_journal_tolerant(&journal).unwrap();
        let seqs: Vec<u64> = view.entries.iter().map(|e| e.seq).collect();
        if outcome2.dropped_unacked > 0 {
            assert_eq!(seqs, vec![2], "the oldest un-acked entry was evicted");
        } else {
            assert_eq!(seqs, vec![1, 2], "under the toy cap this append fit");
        }
        assert_eq!(seqs.last(), Some(&2), "the LAST entry always survives");
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
        let _ = append_with_retention(&journal, None, &entry(1), OUTBOX_MAX_BYTES, None).unwrap();
        let text = std::fs::read_to_string(&journal).unwrap();
        std::fs::write(&journal, format!("{text}{{\"nope\":1}}\n")).unwrap();
        let err = read_journal_tolerant(&journal).unwrap_err();
        assert!(err.contains("valid JSON but not an outbox record"), "{err}");
    }

    #[test]
    fn a_newline_terminated_unparseable_trailing_line_is_an_error_not_a_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let _ = append_with_retention(&journal, None, &entry(1), OUTBOX_MAX_BYTES, None).unwrap();
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
        let _ = append_with_retention(&journal, None, &entry(1), OUTBOX_MAX_BYTES, None).unwrap();
        let _ = append_with_retention(&journal, None, &entry(2), OUTBOX_MAX_BYTES, None).unwrap();
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
    fn the_ack_cursor_round_trips_atomically_and_reads_absent_as_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let ack = dir.path().join("outbox-ack.json");
        assert_eq!(read_ack(&ack).unwrap(), None, "absent = no ack yet");
        write_ack(&ack, 42).unwrap();
        assert_eq!(read_ack(&ack).unwrap(), Some(42));
        write_ack(&ack, 7).unwrap();
        assert_eq!(
            read_ack(&ack).unwrap(),
            Some(7),
            "the file is overwritten verbatim; the max() lives at the caller"
        );
        // The residue discipline: an atomic write leaves no temp litter.
        let residue: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(residue.is_empty(), "no temp residue: {residue:?}");
    }

    #[test]
    fn a_corrupt_ack_file_is_an_error_not_a_silent_reset() {
        let dir = tempfile::TempDir::new().unwrap();
        let ack = dir.path().join("outbox-ack.json");
        std::fs::write(&ack, "not json").unwrap();
        let err = read_ack(&ack).unwrap_err();
        assert!(err.contains("corrupt outbox ack"), "{err}");
    }

    // ---- the facade core ----

    #[test]
    fn export_core_resumes_from_the_durable_ack_cursor_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let ack = dir.path().join("outbox-ack.json");
        for seq in 1..=3u64 {
            let _ =
                append_with_retention(&journal, None, &entry(seq), OUTBOX_MAX_BYTES, None).unwrap();
        }
        write_ack(&ack, 2).unwrap();
        let batch = export_core(&journal, &ack, None, 0).expect("default pull resumes");
        let seqs: Vec<u64> = batch.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3], "everything through the ack is skipped");
        assert_eq!(batch.cursor, OutboxCursor { seq: 3 });
    }

    #[test]
    fn export_core_with_an_explicit_cursor_re_pulls_and_max_zero_takes_the_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let ack = dir.path().join("outbox-ack.json");
        for seq in 1..=3u64 {
            let _ =
                append_with_retention(&journal, None, &entry(seq), OUTBOX_MAX_BYTES, None).unwrap();
        }
        let batch = export_core(&journal, &ack, Some(OutboxCursor { seq: 0 }), 2).unwrap();
        assert_eq!(batch.events.len(), 2, "the explicit max bounds the batch");
        assert_eq!(batch.cursor, OutboxCursor { seq: 2 });
    }

    #[test]
    fn export_core_maps_the_refusals_to_typed_log_errors_with_stable_sentences() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let ack = dir.path().join("outbox-ack.json");
        for seq in 1..=2u64 {
            let _ =
                append_with_retention(&journal, None, &entry(seq), OUTBOX_MAX_BYTES, None).unwrap();
        }
        let above = export_core(&journal, &ack, Some(OutboxCursor { seq: 9 }), 0).unwrap_err();
        assert!(
            matches!(&above, EngineError::Log { detail, .. }
                if detail.contains(PAST_TAIL_SENTENCE)),
            "{above:?}"
        );
        // Stage a rotated-away floor: keep only seq 2 in the file.
        let text = std::fs::read_to_string(&journal).unwrap();
        let last_line = text.lines().last().unwrap();
        std::fs::write(&journal, format!("{last_line}\n")).unwrap();
        let below = export_core(&journal, &ack, Some(OutboxCursor { seq: 0 }), 0).unwrap_err();
        assert!(
            matches!(&below, EngineError::Log { detail, .. }
                if detail.contains(BELOW_FLOOR_SENTENCE)),
            "{below:?}"
        );
    }

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
    fn repair_surfaces_an_unreadable_journal_instead_of_appending_blindly() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        std::fs::create_dir(&journal).unwrap();
        let err = repair_torn_tail(&journal).unwrap_err();
        assert!(!err.is_empty(), "the OS detail is preserved: {err}");
        assert!(
            err.contains("outbox.jsonl"),
            "the error names the non-file journal path: {err}"
        );
    }

    #[test]
    fn an_unreadable_ack_file_is_an_error_not_a_fresh_cursor() {
        let dir = tempfile::TempDir::new().unwrap();
        let ack = dir.path().join("outbox-ack.json");
        std::fs::create_dir(&ack).unwrap();
        let err = read_ack(&ack).unwrap_err();
        assert!(!err.is_empty(), "the OS detail is preserved: {err}");
    }

    #[test]
    fn export_core_surfaces_an_unreadable_ack_file_as_a_typed_log_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let ack = dir.path().join("outbox-ack.json");
        std::fs::create_dir(&ack).unwrap();
        let err = export_core(&journal, &ack, None, 0).unwrap_err();
        assert!(
            matches!(&err, EngineError::Log { path, detail, .. }
                if path.contains("outbox-ack.json") && !detail.is_empty()),
            "the ack file is named with its detail: {err:?}"
        );
    }

    #[test]
    fn export_core_surfaces_an_unreadable_journal_as_a_typed_log_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let ack = dir.path().join("outbox-ack.json");
        std::fs::create_dir(&journal).unwrap();
        let err = export_core(&journal, &ack, Some(OutboxCursor::START), 0).unwrap_err();
        assert!(
            matches!(&err, EngineError::Log { path, detail, .. }
                if path.contains("outbox.jsonl") && !detail.is_empty()),
            "the journal is named with its detail: {err:?}"
        );
    }

    #[test]
    fn ack_core_surfaces_an_unreadable_ack_file_as_a_typed_log_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let ack = dir.path().join("outbox-ack.json");
        std::fs::create_dir(&ack).unwrap();
        let err = ack_core(&journal, &ack, 1).unwrap_err();
        assert!(
            matches!(&err, EngineError::Log { path, detail, .. }
                if path.contains("outbox-ack.json") && !detail.is_empty()),
            "{err:?}"
        );
    }

    #[test]
    fn ack_core_surfaces_an_unreadable_journal_as_a_typed_log_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let ack = dir.path().join("outbox-ack.json");
        std::fs::create_dir(&journal).unwrap();
        let err = ack_core(&journal, &ack, 0).unwrap_err();
        assert!(
            matches!(&err, EngineError::Log { path, detail, .. }
                if path.contains("outbox.jsonl") && !detail.is_empty()),
            "{err:?}"
        );
    }

    // ---- review-finding pins (17-4 review pass) ----

    #[test]
    fn an_interior_blank_line_is_counted_and_surfaced_not_silent() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let ack = dir.path().join("outbox-ack.json");
        let _ = append_with_retention(&journal, None, &entry(1), OUTBOX_MAX_BYTES, None).unwrap();
        // Interior blank + whitespace-only lines (an external edit or a
        // partial-corruption signature): never a silent drop, never a hard
        // error — counted on the batch.
        let text = std::fs::read_to_string(&journal).unwrap();
        std::fs::write(&journal, format!("{text}\n   \n")).unwrap();
        let _ = append_with_retention(&journal, None, &entry(2), OUTBOX_MAX_BYTES, None).unwrap();
        let batch = export_core(&journal, &ack, Some(OutboxCursor::START), 0).unwrap();
        assert_eq!(batch.events.len(), 2, "both records still serve");
        assert_eq!(
            batch.blank_lines_skipped, 2,
            "the interior blanks are counted on the batch"
        );
    }

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

    #[test]
    fn compaction_never_empties_the_journal_even_when_everything_is_acked() {
        // Review finding 3: phase 1 (the acked prefix) must stop at the
        // last entry — a journal emptied by compaction, followed by a crash
        // before the next append, would restart seqs at 1 on the next open
        // and silently fork the delivery range.
        let plan = compaction_plan(
            &[10, 10, 10],
            &[1, 2, 3],
            Some(3), // everything acked
            100,     // incoming
            25,      // cap: needs 85+ freed, but only two entries may drop
        );
        assert_eq!(plan.0, 2, "phase 1 stops before the last entry");
        assert_eq!(plan.1.dropped_acked, 2);
        assert_eq!(plan.1.dropped_unacked, 0);
        // End-to-end at the file level: all-acked journal + a cap-crossing
        // append leaves ONE entry (the high-water seq survives).
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        for seq in 1..=2u64 {
            let _ = append_with_retention(&journal, None, &entry(seq), 10_000, None).unwrap();
        }
        let (outcome, _) = append_with_retention(&journal, Some(2), &entry(3), 100, None).unwrap();
        let view = read_journal_tolerant(&journal).unwrap();
        assert_eq!(view.tail, 3, "the high-water seq survives any crash");
        assert_eq!(
            view.retained_from, 2,
            "the acked prefix was dropped up to (never past) the last entry"
        );
        assert_eq!(outcome.dropped_acked, 1);
    }

    #[test]
    fn an_ack_that_does_not_advance_the_cursor_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let ack = dir.path().join("outbox-ack.json");
        let _ = append_with_retention(&journal, None, &entry(1), OUTBOX_MAX_BYTES, None).unwrap();
        let _ = append_with_retention(&journal, None, &entry(2), OUTBOX_MAX_BYTES, None).unwrap();
        // Anchor: a real advance writes the file.
        assert_eq!(ack_core(&journal, &ack, 1).unwrap(), 1);
        assert!(ack.exists(), "the first real advance persists the cursor");
        // A re-ack BELOW the cursor is idempotent AND churn-free: make the
        // file read-only — a write attempt would FAIL, so success proves no
        // write happened (cross-OS: both platforms honor the read-only bit).
        let mut perms = std::fs::metadata(&ack).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ack, perms).unwrap();
        let persisted = ack_core(&journal, &ack, 0).expect("regressive ack is a no-op");
        assert_eq!(persisted, 1, "the cursor stays at its durable value");
        // A fresh journal's ack of 0 writes nothing at all (no file is born).
        let fresh_ack = dir.path().join("fresh-ack.json");
        let _ = append_with_retention(&journal, None, &entry(3), OUTBOX_MAX_BYTES, None).unwrap();
        assert_eq!(ack_core(&journal, &fresh_ack, 0).unwrap(), 0);
        assert!(
            !fresh_ack.exists(),
            "an ack of 0 on a fresh journal must not create the ack file"
        );
    }

    #[test]
    fn the_cached_size_path_stays_correct_when_the_cache_is_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let journal = dir.path().join("outbox.jsonl");
        let _ = append_with_retention(&journal, None, &entry(1), 10_000, None).unwrap();
        // Stale-LOW cache (says the journal is empty): the append still
        // lands, and the RETURNED size is the file's real truth.
        let (_, size) = append_with_retention(&journal, None, &entry(2), 10_000, Some(0)).unwrap();
        let real = std::fs::metadata(&journal).unwrap().len();
        assert_eq!(size, real, "the returned size is the file's truth");
        // Stale-HIGH cache (over-triggers the compaction check): the plan
        // reads REAL costs and drops nothing (the journal actually fits).
        let (outcome, _) =
            append_with_retention(&journal, None, &entry(3), 10_000, Some(u32::MAX as u64))
                .unwrap();
        assert_eq!(outcome, CompactionOutcome::default());
        let view = read_journal_tolerant(&journal).unwrap();
        assert_eq!(view.entries.len(), 3, "nothing was dropped");
    }
}
