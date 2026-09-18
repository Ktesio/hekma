//! The event-bus resync helper (story 10-3, FR-33 / AD-14) — the one-call
//! remedy for the bus's documented crash-window at-most-once delivery.
//!
//! ## The problem it closes
//!
//! The 7-2 bus publishes every event right AFTER its durable append succeeds —
//! "never publish what did not commit" — which makes delivery at-most-once in
//! the crash window between the two steps: a process crash there loses that ONE
//! event from the STREAM while the durable logs stay complete. The documented
//! recourse was "re-read the query APIs" — three different log formats a host
//! had to hand-roll into one stream. This module is the engine's own backfill:
//! it reads the COMMITTED truth (the same `instance.log` / `breaches.log` /
//! `usage_events` ledger the query APIs serve) and converts each record to its
//! [`EngineEvent`] bus payload verbatim, so a host heals the window in one
//! call and the caveat reads "recoverable via `resync_events`".
//!
//! ## The contract
//!
//! * **Committed truth only.** A READ-side helper over the same records the
//!   query APIs return — the bus, its publish points, and the subscribe
//!   semantics are untouched. An event that never committed (the VG1
//!   append-failure arm) never appears here either: only durable records
//!   backfill.
//! * **Per-instance.** `name` scopes every read: the instance's transition
//!   log, breach log, and ledger rows. A Fleet-wide backfill is the host
//!   composing per-instance calls.
//! * **Order: exact per family, family-major across families.** Within each
//!   family the batch IS commit order — `instance.log` line order, breach-log
//!   line order, ledger `rowid` order (the same orders the 7-2 suite pins
//!   against the bus). Across families the durable record carries NO global
//!   sequence (commit order was only ever the bus's in-memory publish order,
//!   and the per-family timestamps share whole-second resolution), so the
//!   batch is emitted family-major — transitions, then breaches, then usage —
//!   and does NOT fabricate a cross-family interleaving. How a host combines
//!   the backfill with the live stream is ITS ordering choice, and both
//!   orders are workable with named tradeoffs:
//!   * **Subscribe FIRST, then resync** — recommended for gap-sensitive
//!     hosts. Subscribing first is gap-free by construction: every commit
//!     after the subscribe is delivered live, whatever the backfill does
//!     afterwards. The cost is overlap — the backfill re-delivers what the
//!     live stream also delivered — so the host DEDUPS, per family, against
//!     its own cursor position (skip each family's first `cursor.<field>`
//!     backfilled records, or key on the records' own identity fields).
//!   * **Resync first, then subscribe** — no seam duplicates (the backfill
//!     is precisely the prefix, the live stream precisely the suffix), but
//!     NOT gap-free: a commit+publish landing between `resync_events`
//!     returning and `subscribe()` is delivered by NEITHER. The window is
//!     real; use this order only across a quiescent agent (stopped/paused,
//!     no traffic) or with the window consciously accepted.
//!     No watermark/dedup machinery is built in: the per-family records are
//!     exact, so a host using the recommended subscribe-first order dedups
//!     with its own cursor bookkeeping.
//! * **The snapshot is per-family exact, not cross-family atomic.**
//!   `read_committed` performs THREE sequential reads (transition log, then
//!   breach log, then ledger). A commit landing BETWEEN them yields a batch
//!   whose families are skewed relative to each other — the transition read
//!   may predate a transition the ledger read already sees. Each family's
//!   slice stays exact and cursor-lossless (the per-family cursors never
//!   regress, and the next call picks up exactly the stragglers), but the
//!   batch as a whole is NOT one point-in-time view. A host needing a
//!   coherent cross-family snapshot makes the agent quiescent across the
//!   read.
//! * **Crash-recovery read posture (torn-tail tolerance, SURFACED).** This
//!   helper is called MOST often right after the crash it heals, and a crash
//!   mid-append tears the log's trailing line. Unlike the strict query-API
//!   reads ([`Supervisor::read_events`] hard-errors any malformed line), the
//!   reads here skip ONE unparseable TRAILING line per log — and only when
//!   it carries the torn-append signature: the file's raw text does NOT end
//!   with a newline (the engine appends `line + '\n'`, so a write cut by the
//!   crash always leaves the final newline missing). The skip is never
//!   silent: the batch's `torn_tail_skipped` flag is set, because the
//!   engine's NEXT append fuses onto the torn fragment, so a skipped
//!   trailing line may be carrying a good post-crash record the host did
//!   NOT receive and must know about. Everything else is surfaced as a
//!   typed error: a malformed INTERIOR line (not a race — the wrong file or
//!   a corrupting engine), a newline-terminated unparseable TRAILING line
//!   (not a tear — corruption, or exactly that fused line), and a trailing
//!   line that is VALID JSON of the wrong record shape (the wrong file —
//!   never popped as if it were a tear).
//! * **Cursor-based, idempotent — and never silently re-delivering.**
//!   [`ResyncCursor`] records how many records per family a host has already
//!   consumed; passing it to the next call skips exactly that prefix, so
//!   re-running a resync never re-delivers what the cursor already covers.
//!   A cursor BELOW a family's committed count backfills the suffix; a
//!   cursor ABOVE one (the log truncated or the ledger rotated away) is a
//!   typed ERROR naming the family and both counts — never a silent clamp,
//!   which would either re-deliver from zero or silently skip records,
//!   breaking the contract either way. A host that genuinely wants to start
//!   over resets its cursor to [`ResyncCursor::START`] deliberately. Each
//!   family's log is append-only (transitions/breaches are JSON-Lines
//!   appends; ledger rows are INSERT-only under the no-double-count key),
//!   which is what makes a per-family count a stable position.
//!
//! ## Boundary (what this is NOT)
//!
//! No new event kinds (the payloads are the same three [`EngineEvent`] verse
//! the bus carries), no schema change, no bus/publish-path change, no
//! persistence change. Additive facade API: [`Engine::resync_events`] (async)
//! and [`Blocking::resync_events`](crate::Blocking::resync_events) (sync),
//! documented for hosts in docs/embedding.md.
//!
//! [`Supervisor::read_events`]: super::supervisor::Supervisor::read_events
//! [`Engine::resync_events`]: crate::Engine::resync_events

use serde::{Deserialize, Serialize};

use super::bus::EngineEvent;
use super::event::BudgetBreachEvent;
use super::event::TransitionEvent;
use super::registry::Registry;
use super::usage::{UsageEvent, UsageUpdateEvent};
use super::EngineError;

/// How far into each committed event family a host has already consumed —
/// the resync helper's skip position (story 10-3).
///
/// One count per family (`instance.log` lines, `breaches.log` lines, ledger
/// rows), because the durable record keeps per-family order only — see the
/// module docs for the ordering contract. Counts are positions in APPEND
/// order, and every family's record is append-only, so a cursor returned by
/// one [`ResyncBatch`] is a stable position for the next call.
/// [`ResyncCursor::START`] (all zero) is the entry point: the full backfill.
///
/// Serde-derived so a host can persist its cursor across its own restarts
/// (snake_case, like every AD-14 wire struct).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResyncCursor {
    /// Transition records already consumed (skipped) from `instance.log`.
    pub transitions: u64,
    /// Breach records already consumed (skipped) from `breaches.log`.
    pub breaches: u64,
    /// Ledger rows already consumed (skipped) from the `usage_events` table.
    pub usage: u64,
}

impl ResyncCursor {
    /// The start position: consume EVERYTHING committed (the full backfill a
    /// crash-window heal begins with). Identical to `ResyncCursor::default()`;
    /// spelled as a named constant so a call site reads as intent.
    pub const START: ResyncCursor = ResyncCursor {
        transitions: 0,
        breaches: 0,
        usage: 0,
    };
}

/// One resync batch: the committed events after a cursor position, plus the
/// position to resume from (story 10-3).
///
/// `events` carries ONLY records committed after the cursor the caller passed
/// (an empty vec means the host is already caught up to the committed truth);
/// `cursor` is the position after THIS batch — pass it to the next
/// `resync_events` call and the already-delivered prefix is skipped. The two
/// fields come from one read pass, so `cursor` never names a position beyond
/// what `events` actually carries.
///
/// `torn_tail_skipped` (story 10-3 hardening) surfaces the crash-recovery
/// reader's one tolerated skip: a log's TRAILING line was unparseable AND
/// carried the torn-append signature (the file did not end with a newline),
/// so it was skipped — and because the engine's next append fuses onto a
/// torn fragment, that line may have been carrying a good post-crash record
/// the host did NOT receive. The flag is additive on the wire
/// (`#[serde(default)]` — archived batches without it deserialize as
/// `false`), consistent with the report family's bump policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResyncBatch {
    /// The committed events, transitions then breaches then usage updates
    /// (family-major — see the module docs for the ordering contract), each
    /// payload the EXACT bus wrapper the live stream delivers.
    pub events: Vec<EngineEvent>,
    /// The consume position after this batch — feed it to the next
    /// `resync_events` call for an idempotent continuation.
    pub cursor: ResyncCursor,
    /// A log's trailing line was skipped as a torn append: the events it may
    /// have carried (the engine's next append fuses onto a torn fragment) did
    /// NOT reach `events`. Absent (`false`) on batches produced before this
    /// field existed — the serde default, so archived wire shapes still load.
    #[serde(default)]
    pub torn_tail_skipped: bool,
}

/// Read the COMMITTED event records for `name` past `after` and convert them
/// to bus payloads (the resync helper's engine-side core, story 10-3).
///
/// Three committed-truth reads — the instance's transition log, breach log,
/// and `usage_events` ledger rows, each in its own commit order — skipped to
/// the caller's per-family cursor positions, then wrapped into the SAME
/// [`EngineEvent`] payloads the bus publishes. Torn-tail tolerant per the
/// module docs (ONE skipped unparseable trailing line per log — only with
/// the torn-append signature, and the skip is SURFACED on the batch's
/// `torn_tail_skipped`; every other malformed line is a typed
/// [`EngineError::Log`]).
///
/// A cursor position PAST a family's committed count (the log truncated or
/// the ledger rotated away) is an [`EngineError::Log`] naming the family and
/// both counts — never a silent clamp (which would re-deliver from zero or
/// silently skip records; see the module docs).
///
/// Assumes the name is already validated and the instance exists (the facade
/// checks both) — this is pure read + conversion.
pub(crate) fn read_committed(
    registry: &Registry,
    name: &super::InstanceName,
    after: &ResyncCursor,
) -> Result<ResyncBatch, EngineError> {
    let transitions_path = registry.instance_log_path(name);
    let (transitions, transitions_torn) = read_events_tolerant(&transitions_path, "instance-log")
        .map_err(|detail| EngineError::Log {
        name: name.as_str().to_string(),
        path: transitions_path.to_string_lossy().into_owned(),
        detail,
    })?;
    let breaches_path = registry.instance_breach_log_path(name);
    let (breaches, breaches_torn) = read_breach_events_tolerant(&breaches_path, "breach-log")
        .map_err(|detail| EngineError::Log {
            name: name.as_str().to_string(),
            path: breaches_path.to_string_lossy().into_owned(),
            detail,
        })?;
    let usage = registry
        .usage_rows(name)
        .map_err(super::registry_error_to_engine)?;

    // Truncation-below-cursor is an ERROR, never a silent clamp: a family's
    // committed count below the caller's cursor means the record was
    // truncated or rotated away, and clamping would either re-deliver from
    // zero or silently skip records the cursor was promised — both violate
    // the never-re-deliver contract. Each family and both counts are named
    // so a host can reset its cursor DELIBERATELY if it wants a from-zero
    // backfill.
    reject_truncated_family(
        name,
        &transitions_path,
        "transition",
        after.transitions,
        transitions.len(),
    )?;
    reject_truncated_family(
        name,
        &breaches_path,
        "breach",
        after.breaches,
        breaches.len(),
    )?;
    let ledger_path = registry.state_db_path();
    reject_truncated_family(name, &ledger_path, "usage-ledger", after.usage, usage.len())?;

    let mut batch = assemble(&transitions, &breaches, &usage, after);
    batch.torn_tail_skipped = transitions_torn || breaches_torn;
    Ok(batch)
}

/// The truncation guard (never silent re-delivery): a cursor position past a
/// family's committed count fails the read, naming the family and both
/// counts. `family` is the singular family word ("transition"/"breach"/
/// "usage-ledger") the detail quotes; `path` is the family's record home
/// (the log file, or the state DB for the ledger).
fn reject_truncated_family(
    name: &super::InstanceName,
    path: &std::path::Path,
    family: &str,
    cursor: u64,
    committed: usize,
) -> Result<(), EngineError> {
    if cursor > committed as u64 {
        return Err(EngineError::Log {
            name: name.as_str().to_string(),
            path: path.to_string_lossy().into_owned(),
            detail: format!(
                "the resync cursor's {family} position ({cursor}) is past this family's committed \
                 count ({committed}) — the record was truncated or rotated away; resyncing would \
                 re-deliver or silently skip records, so this fails instead. Reset the cursor to \
                 ResyncCursor::START deliberately if a from-zero backfill is intended"
            ),
        });
    }
    Ok(())
}

/// The pure assembly: skip each family to its cursor position, wrap the
/// remainder into [`EngineEvent`] payloads family-major, and compute the
/// position after the batch. Unit-tested directly so the cursor math (skip +
/// advance) is pinned without any I/O.
///
/// The skip clamps DEFENSIVELY to what exists — but that clamp is never the
/// observable behavior: [`read_committed`] rejects a cursor past a family's
/// committed count (the truncation guard) before assembling, so a caller of
/// the facade always gets the typed error instead of a silently clamped
/// batch. The clamp here only keeps the pure function total.
pub(crate) fn assemble(
    transitions: &[TransitionEvent],
    breaches: &[BudgetBreachEvent],
    usage: &[UsageEvent],
    after: &ResyncCursor,
) -> ResyncBatch {
    let mut events = Vec::new();
    // `usize::try_from(..).unwrap_or(usize::MAX)` — a u64 cursor cannot
    // truncate through a 32-bit `usize` (a wrapped small skip would silently
    // RE-DELIVER the consumed prefix on 32-bit targets); saturate to the
    // max, which the min() then clamps.
    let skip_transitions = usize::try_from(after.transitions)
        .unwrap_or(usize::MAX)
        .min(transitions.len());
    for event in &transitions[skip_transitions..] {
        events.push(EngineEvent::Transition(event.clone()));
    }
    let skip_breaches = usize::try_from(after.breaches)
        .unwrap_or(usize::MAX)
        .min(breaches.len());
    for event in &breaches[skip_breaches..] {
        events.push(EngineEvent::BudgetBreach(event.clone()));
    }
    let skip_usage = usize::try_from(after.usage)
        .unwrap_or(usize::MAX)
        .min(usage.len());
    for row in &usage[skip_usage..] {
        events.push(EngineEvent::UsageUpdate(UsageUpdateEvent::new(row.clone())));
    }
    ResyncBatch {
        events,
        cursor: ResyncCursor {
            transitions: transitions.len() as u64,
            breaches: breaches.len() as u64,
            usage: usage.len() as u64,
        },
        torn_tail_skipped: false,
    }
}

/// Read a JSON-Lines event log with the RESYNC posture (story 10-3, hardened
/// by the Epic-10 adversarial review): an absent file is empty; ONE
/// unparseable TRAILING line is skipped ONLY when it carries the torn-append
/// signature — the file's raw text does NOT end with a newline (the engine
/// appends `line + '\n'`, so a write cut by a crash always leaves the final
/// newline missing) — and the skip is SURFACED via the returned flag, never
/// silent: the engine's NEXT append fuses onto the torn fragment, so a
/// skipped trailing line may be carrying a good post-crash record. Every
/// other malformed line is an error naming its PHYSICAL line number (blank
/// lines never shift the count), mirroring the strict query-API readers:
///
/// * a malformed INTERIOR line — the wrong file or a corrupting engine;
/// * a newline-terminated unparseable TRAILING line — NOT a tear (the
///   tear's signature is the missing final newline): corruption, or exactly
///   that fused post-crash line, either way worth surfacing;
/// * a TRAILING line that is VALID JSON but not THIS log's record shape —
///   the wrong file, never popped as if it were a tear.
fn read_json_lines_tolerant<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    what: &str,
) -> Result<(Vec<T>, bool), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(e) => return Err(e.to_string()),
    };
    // The torn-append signature, read off the RAW text before any line work.
    let newline_terminated = text.ends_with('\n');
    // (physical 0-based line number, line) pairs: the PHYSICAL numbers are
    // what error messages report, so blank lines never shift an index.
    let mut lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .collect();
    let mut torn_tail_skipped = false;
    if let Some((last_idx, last)) = lines.last() {
        if serde_json::from_str::<T>(last).is_err() {
            if serde_json::from_str::<serde_json::Value>(last).is_ok() {
                // Valid JSON, wrong record shape: the WRONG FILE, not a torn
                // append (a tear cuts the write mid-line, which cannot leave
                // well-formed JSON). Error — never skip.
                return Err(format!(
                    "corrupt {what}: line {} is valid JSON but not a {what} record — the wrong file?",
                    last_idx + 1
                ));
            }
            if newline_terminated {
                // Newline-terminated yet unparseable: not a torn append.
                // Corruption, or a torn append the engine's next append
                // FUSED onto — the line may be carrying a good record that
                // cannot be extracted here, so surface it instead of
                // silently popping it.
                return Err(format!(
                    "corrupt {what}: trailing line {} is unparseable and newline-terminated — \
                     not a torn append (corruption, or a fused line a post-crash append left); \
                     repair the log before resyncing",
                    last_idx + 1
                ));
            }
            // The torn append itself: the crash cut the write before the
            // newline. Skip the fragment — and SURFACE the skip (the next
            // append fuses onto this fragment, so whatever record it was
            // carrying did not reach the caller).
            lines.pop();
            torn_tail_skipped = true;
        }
    }
    let mut events = Vec::new();
    for (idx, line) in lines {
        events.push(
            serde_json::from_str(line)
                .map_err(|e| format!("corrupt {what} line {}: {e}", idx + 1))?,
        );
    }
    Ok((events, torn_tail_skipped))
}

/// The transition-log read ([`read_json_lines_tolerant`] over the
/// [`TransitionEvent`] records — the same records the strict
/// [`Supervisor::read_events`] serves, with the resync torn-tail posture).
/// Returns the events plus the torn-tail-skipped flag.
///
/// [`Supervisor::read_events`]: super::supervisor::Supervisor::read_events
fn read_events_tolerant(
    path: &std::path::Path,
    what: &str,
) -> Result<(Vec<TransitionEvent>, bool), String> {
    read_json_lines_tolerant(path, what)
}

/// The breach-log read ([`read_json_lines_tolerant`] over the
/// [`BudgetBreachEvent`] records — the same records the strict
/// [`Supervisor::read_breach_events`] serves, with the resync torn-tail
/// posture). Returns the events plus the torn-tail-skipped flag.
///
/// [`Supervisor::read_breach_events`]: super::supervisor::Supervisor::read_breach_events
fn read_breach_events_tolerant(
    path: &std::path::Path,
    what: &str,
) -> Result<(Vec<BudgetBreachEvent>, bool), String> {
    read_json_lines_tolerant(path, what)
}

#[cfg(test)]
mod tests {
    use super::super::event::TransitionCause;
    use super::super::LifecycleState;
    use super::*;

    fn transition(seq: usize) -> TransitionEvent {
        TransitionEvent::new(
            "probe".to_string(),
            LifecycleState::Running,
            LifecycleState::Stopped,
            TransitionCause::StopGraceful,
            format!("2026-09-10T00:00:{seq:02}Z"),
        )
    }

    fn breach(seq: u64) -> BudgetBreachEvent {
        BudgetBreachEvent::new(
            "probe",
            "run-1",
            super::super::BreachScope::Cumulative,
            90,
            90 + seq,
            super::super::BreachAction::Pause,
            "self-reported",
            format!("2026-09-10T00:00:{seq:02}Z"),
        )
    }

    fn usage(seq: u64) -> UsageEvent {
        UsageEvent {
            instance: "probe".to_string(),
            run_id: super::super::RunId::from_wire("run-1"),
            input_tokens: 10,
            output_tokens: 20,
            metering_source: "self-reported".to_string(),
            sequence: seq,
            occurred_at: format!("2026-09-10T00:00:{seq:02}Z"),
        }
    }

    #[test]
    fn assemble_wraps_every_family_family_major_and_advances_the_cursor() {
        let transitions: Vec<_> = (0..2).map(transition).collect();
        let breaches: Vec<_> = (0..2).map(breach).collect();
        let usage: Vec<_> = (0..2).map(usage).collect();

        let batch = assemble(&transitions, &breaches, &usage, &ResyncCursor::START);
        assert_eq!(batch.events.len(), 6, "every record converts");
        assert_eq!(
            batch.events[0],
            EngineEvent::Transition(transitions[0].clone())
        );
        assert_eq!(
            batch.events[1],
            EngineEvent::Transition(transitions[1].clone())
        );
        assert_eq!(
            batch.events[2],
            EngineEvent::BudgetBreach(breaches[0].clone())
        );
        assert_eq!(
            batch.events[3],
            EngineEvent::BudgetBreach(breaches[1].clone())
        );
        assert_eq!(
            batch.events[4],
            EngineEvent::UsageUpdate(UsageUpdateEvent::new(usage[0].clone()))
        );
        assert_eq!(
            batch.events[5],
            EngineEvent::UsageUpdate(UsageUpdateEvent::new(usage[1].clone()))
        );
        // The cursor names the consumed position per family — the next call
        // with it skips everything (idempotence below).
        assert_eq!(
            batch.cursor,
            ResyncCursor {
                transitions: 2,
                breaches: 2,
                usage: 2
            }
        );
    }

    #[test]
    fn assemble_skips_the_cursor_prefix_per_family() {
        let transitions: Vec<_> = (0..3).map(transition).collect();
        let breaches: Vec<_> = (0..1).map(breach).collect();
        let usage: Vec<_> = (0..2).map(usage).collect();
        let after = ResyncCursor {
            transitions: 2,
            breaches: 0,
            usage: 1,
        };

        let batch = assemble(&transitions, &breaches, &usage, &after);
        // Exactly the unconsumed suffix per family: one transition, one
        // breach, one usage row — nothing re-delivered.
        assert_eq!(
            batch.events,
            vec![
                EngineEvent::Transition(transitions[2].clone()),
                EngineEvent::BudgetBreach(breaches[0].clone()),
                EngineEvent::UsageUpdate(UsageUpdateEvent::new(usage[1].clone())),
            ]
        );
        assert_eq!(batch.cursor.usage, 2);
    }

    #[test]
    fn assemble_clamps_a_cursor_past_the_family_end() {
        // The PURE math stays total: a cursor past a family's end clamps
        // here so nothing panics and nothing goes negative. This clamp is a
        // defensive floor, never the observable behavior — read_committed
        // rejects a truncating cursor with a typed error before assembling.
        let transitions: Vec<_> = (0..1).map(transition).collect();
        let after = ResyncCursor {
            transitions: 99,
            breaches: 99,
            usage: 99,
        };

        let batch = assemble(&transitions, &[], &[], &after);
        assert!(batch.events.is_empty(), "everything is already consumed");
        assert_eq!(
            batch.cursor,
            ResyncCursor {
                transitions: 1,
                breaches: 0,
                usage: 0
            }
        );
    }

    #[test]
    fn assemble_of_nothing_is_an_empty_batch_at_zero() {
        let batch = assemble(&[], &[], &[], &ResyncCursor::START);
        assert_eq!(
            batch,
            ResyncBatch {
                events: Vec::new(),
                cursor: ResyncCursor::START,
                torn_tail_skipped: false,
            }
        );
    }

    #[test]
    fn resync_reads_survive_a_torn_trailing_line_and_surface_the_skip() {
        // The crash-recovery posture: the crash tore the log's trailing
        // append (the raw text does not end with a newline — the write was
        // cut before its '\n'); the read returns the good prefix INSTEAD of
        // failing the heal, and SURFACES the skip so a host never silently
        // loses a record. Both log families share the reader.
        let dir = tempfile::TempDir::new().unwrap();
        let good = serde_json::to_string(&transition(0)).unwrap();
        let path = dir.path().join("instance.log");
        std::fs::write(&path, format!("{good}\n{{\"schema_version\":1,\"inst")).unwrap();
        let (events, torn): (Vec<TransitionEvent>, bool) =
            read_events_tolerant(&path, "instance-log").expect("torn tail tolerated");
        assert_eq!(events, vec![transition(0)]);
        assert!(torn, "the skipped torn tail is surfaced, never silent");

        let good_breach = serde_json::to_string(&breach(0)).unwrap();
        let breach_path = dir.path().join("breaches.log");
        std::fs::write(&breach_path, format!("{good_breach}\n")).unwrap();
        let (breaches, torn): (Vec<BudgetBreachEvent>, bool) =
            read_breach_events_tolerant(&breach_path, "breach-log").expect("complete log parses");
        assert_eq!(breaches, vec![breach(0)]);
        assert!(!torn, "a complete log sets no flag");
    }

    #[test]
    fn resync_reads_still_reject_a_malformed_interior_line() {
        // Torn-tail tolerance is bounded: an unparseable NON-trailing line is
        // not a race — it is the wrong file or a corrupting engine, and the
        // typed error names it (mirrors the strict readers' convention).
        let dir = tempfile::TempDir::new().unwrap();
        let good = serde_json::to_string(&transition(0)).unwrap();
        let path = dir.path().join("instance.log");
        std::fs::write(&path, format!("{{\"not\":\"a transition\"}}\n{good}\n")).unwrap();
        let err = read_events_tolerant(&path, "instance-log").unwrap_err();
        assert!(err.contains("corrupt instance-log line 1"), "{err}");
    }

    #[test]
    fn an_interior_error_reports_the_physical_line_number_despite_blank_lines() {
        // The error's line number is the PHYSICAL file line: a blank line
        // must not shift the index the message reports (a human is tailing
        // the real file).
        let dir = tempfile::TempDir::new().unwrap();
        let good = serde_json::to_string(&transition(0)).unwrap();
        let path = dir.path().join("instance.log");
        // `{good}` is physical line 1, the blank line physical 2, the bad
        // record physical 3 — the old filtered-index reporting said "2".
        std::fs::write(&path, format!("{good}\n\n{{\"x\":1}}\n{good}\n")).unwrap();
        let err = read_events_tolerant(&path, "instance-log").unwrap_err();
        assert!(
            err.contains("corrupt instance-log line 3"),
            "the physical line number is reported (3, not the filtered index 2): {err}"
        );
    }

    #[test]
    fn a_newline_terminated_unparseable_trailing_line_is_an_error_not_a_skip() {
        // NOT a torn append: the engine appends `line + '\n'`, so a tear
        // always leaves the final newline MISSING. A newline-terminated
        // unparseable trailing line is corruption — or the FUSED line a
        // post-crash append left (torn fragment + good record) — and popping
        // it silently would DROP a potentially-good record. Surfaced instead.
        let dir = tempfile::TempDir::new().unwrap();
        let good = serde_json::to_string(&transition(0)).unwrap();
        let torn = serde_json::to_string(&transition(1)).unwrap();
        let path = dir.path().join("instance.log");
        // A torn fragment FUSED with the engine's next append (which wrote
        // its own '\n'): the trailing line is newline-terminated junk.
        let fused = format!("{good}\n{{\"schema_version\":1,\"inst{torn}\n");
        std::fs::write(&path, fused).unwrap();
        let err = read_events_tolerant(&path, "instance-log").unwrap_err();
        assert!(
            err.contains("newline-terminated"),
            "the fused/corrupt trailing line is surfaced, never silently dropped: {err}"
        );
    }

    #[test]
    fn a_valid_json_wrong_shape_trailing_line_is_the_wrong_file_not_a_tear() {
        // A trailing line that parses as JSON but not as THIS log's record
        // shape is the wrong file — it must ERROR, never ride the torn-tail
        // skip (an earlier posture silently popped exactly this shape).
        let dir = tempfile::TempDir::new().unwrap();
        let good = serde_json::to_string(&transition(0)).unwrap();
        let path = dir.path().join("instance.log");
        std::fs::write(&path, format!("{good}\n{{\"nope\":1}}\n")).unwrap();
        let err = read_events_tolerant(&path, "instance-log").unwrap_err();
        assert!(
            err.contains("valid JSON but not a instance-log record"),
            "a wrong-shape trailing line names the wrong-file error: {err}"
        );

        // The same verdict holds without the trailing newline: well-formed
        // JSON of another shape cannot be a tear (a tear cuts mid-line).
        std::fs::write(&path, format!("{good}\n{{\"nope\":1}}")).unwrap();
        let err = read_events_tolerant(&path, "instance-log").unwrap_err();
        assert!(
            err.contains("valid JSON but not a instance-log record"),
            "{err}"
        );
    }

    #[test]
    fn a_cursor_past_a_family_committed_count_is_an_error_naming_family_and_counts() {
        // The truncation guard, end to end through read_committed: a cursor
        // position above a family's committed count means the log was
        // truncated or rotated away — the read FAILS naming the family and
        // both counts instead of silently clamping (which would re-deliver
        // from zero or silently skip records).
        let dir = tempfile::TempDir::new().unwrap();
        let registry = super::super::Registry::open(Some(dir.path().to_path_buf()))
            .expect("open a registry over a hermetic root");
        let name = super::super::InstanceName::new("probe").expect("a valid probe name");

        // One committed transition; the cursor claims five consumed.
        let log_path = registry.instance_log_path(&name);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        let good = serde_json::to_string(&transition(0)).unwrap();
        std::fs::write(&log_path, format!("{good}\n")).unwrap();

        let err = read_committed(
            &registry,
            &name,
            &ResyncCursor {
                transitions: 5,
                breaches: 0,
                usage: 0,
            },
        )
        .expect_err("a transition cursor past the committed count fails");
        assert!(
            matches!(&err, EngineError::Log { detail, .. }
                if detail.contains("transition") && detail.contains("(5)") && detail.contains("(1)")),
            "the error names the family and BOTH counts: {err:?}"
        );

        // The ledger family guards the same way (an empty ledger, cursor 2).
        let err = read_committed(
            &registry,
            &name,
            &ResyncCursor {
                transitions: 0,
                breaches: 0,
                usage: 2,
            },
        )
        .expect_err("a usage cursor past the committed count fails");
        assert!(
            matches!(&err, EngineError::Log { detail, .. }
                if detail.contains("usage-ledger") && detail.contains("(2)") && detail.contains("(0)")),
            "the usage family is named with both counts: {err:?}"
        );

        // An at-or-below cursor proceeds (the truncation guard is a floor,
        // not a ceiling): cursor 1 == committed 1 is fine.
        let batch = read_committed(
            &registry,
            &name,
            &ResyncCursor {
                transitions: 1,
                breaches: 0,
                usage: 0,
            },
        )
        .expect("a cursor AT the committed count is caught up");
        assert!(batch.events.is_empty());
        assert!(!batch.torn_tail_skipped);
    }

    #[test]
    fn resync_reads_treat_an_absent_log_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let (events, torn): (Vec<TransitionEvent>, bool) =
            read_events_tolerant(&dir.path().join("never.log"), "instance-log")
                .expect("absent file is empty");
        assert!(events.is_empty());
        assert!(!torn);
    }
}
