//! The lifecycle supervisor (spine AD-1, AD-4, AD-12/AD-14/AD-15 seeds).
//!
//! The supervisor owns the running Agent Instances' process handles in memory
//! for the current engine lifetime and drives every lifecycle transition:
//!
//! 1. apply the transition table ([`next_state`](super::transition::next_state)),
//! 2. spawn / stop via the per-OS
//!    [`ProcessBackend`](crate::ports::ProcessBackend) (selected in
//!    `backends/mod.rs`; the supervisor names only the trait + cfg-selected
//!    aliases — it is cfg-free),
//! 3. persist the new state via the [`Registry`], and
//! 4. emit the [`TransitionEvent`] (append to the per-instance log + return it).
//!
//! ## Cross-lifetime supervision (AD-5, story 1-6: IMPLEMENTED)
//!
//! The running-handle map lives for THIS engine's lifetime, but the write-ahead
//! spawn records (AD-5) persist across lifetimes. Story 1-6 IMPLEMENTS orphan
//! adoption: [`Supervisor::adopt_orphans`] (called from [`Engine::open`]) reads
//! every persisted [`SpawnRecord`] and re-attaches to a still-live process whose
//! start-time fingerprint matches (`backend.adopt`), re-populating the handle map
//! so `stop`/`pause`/`poll` work on it again; a record whose process is gone (or
//! whose PID was reused) reconciles to `failed`. So a process started by a prior
//! engine that CRASHED is now re-adopted (or honestly failed) — the single-
//! lifetime boundary is lifted for the durable-record case.
//!
//! ## Crash detection + Restart Policy (AD-5/AD-15, story 1-6)
//!
//! [`Supervisor::poll_once`] is the reaper: it polls every held handle via the
//! EXISTING `backend.poll` and, on an unrequested `Exited` for an instance the
//! store still shows `running`/`paused`, applies the EVENT-driven `running →
//! failed` edge (a [`TransitionCause::Crashed`]) and consults the per-instance
//! [`RestartPolicy`] to decide whether to schedule a restart (returning a
//! [`RestartPlan`] the engine cadence times). The reaper + restart executor stay
//! SYNC + cfg-free; the engine owns the poll interval and the backoff timer.
//!
//! ## What "an event" is here (AD-14 seed)
//!
//! Each transition RECORDS a [`TransitionEvent`] to the per-instance log and
//! returns it (observable to tests / embedders). Since story 7-2 the supervisor
//! ALSO PUBLISHES each committed event onto the bounded event bus (the
//! `domain::bus` module, FR-33) — at the SAME commit points where the logs
//! append, so bus order == durable append order. The bus is additive to the
//! log/query surface, which stays the machine-authoritative record.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hemaka_adapter_api::{Capability, ConfigMapping, OsId, SupportLevel};

use crate::adapter::{self, ConfigApplyError, LaunchResolveError};
use crate::backends;
use crate::metering::{ListenerError, ObservedListener};
use crate::ports::{
    assemble_usage_event, BackendError, LogCapture, MemoryBackingKind, ObservedUsageSource,
    ParsedUsage, ProcessBackend, ProcessStatus, SelfReportedUsageSource, SpawnRecord, SpawnSpec,
    UsageSource, KILL_CONFIRM_TIMEOUT, LOG_ROTATE_GENERATIONS,
};
use crate::time::now_rfc3339;

use super::budget::{BreachAction, BreachDecision, BreachScope, BudgetEvaluator};
use super::bus::{EngineEvent, EventBus};
use super::config::{self, ConfigLayer};
use super::cost::{CostEvaluator, EstimateLabel, Micros};
use super::error::EngineError;
use super::event::{
    BreachDimension, BudgetBreachEvent, LogLine, LogStream, TransitionCause, TransitionEvent,
};
use super::instance::AgentInstance;
use super::lifecycle::LifecycleState;
use super::name::InstanceName;
use super::registry::Registry;
use super::restart::{is_crash_loop, BackoffSchedule, RestartPolicy, MAX_CONSECUTIVE_FAILURES};
use super::transition::{next_state, LifecycleCommand};
use super::usage::{RecordOutcome, RunId, UsageUpdateEvent};

/// The default graceful-shutdown window before a stop escalates to a forced kill
/// (AC3). Per-instance configurable via [`Supervisor::stop`]'s `window` argument;
/// this is the conservative fallback when the caller passes `None`.
pub const DEFAULT_STOP_WINDOW: Duration = Duration::from_secs(30);

/// How long to watch a freshly spawned process for an immediate failure before
/// declaring it `running` (the readiness definition, `[ASSUMPTION]`).
///
/// "Adapter ready" this story = "the process spawned and did not die during this
/// short startup window". A process that exits (especially non-zero) within it is
/// treated as a launch failure (AC2 "immediate non-zero exit during startup").
/// Kept small so `start` stays snappy; the fake test agent's `--exit-fast` path
/// exits well inside it.
const READINESS_WINDOW: Duration = Duration::from_millis(300);

/// How often the readiness watch polls the freshly spawned process.
const READINESS_POLL: Duration = Duration::from_millis(10);

/// AI-12: how many CONSECUTIVE `backend.poll` errors on one held handle the
/// crash reaper tolerates as "transient, treat as still-alive" before it stops
/// trusting that reading and treats the handle as a crash signal. At the engine
/// cadence (~250ms per reaper tick) this bounds a permanently un-pollable
/// handle at roughly 2.5s of silent non-detection — instead of FOREVER (the old
/// `Err(_) => None` swallowed every error, so a handle the backend could never
/// again poll was never crash-detected). A single-digit error burst (a
/// transient syscall hiccup) stays below it and keeps the historical
/// tolerate-and-retry behavior.
const MAX_CONSECUTIVE_POLL_ERRORS: u32 = 10;

/// AI-12 (loop 1): how many characters of the LAST poll error's text the
/// persistent-poll-failure crash cause carries. Bounded so a pathological
/// error string cannot bloat the event log; enough to name the actual why
/// (e.g. the injected fault text, an OS errno message, a procfs failure).
const POLL_ERROR_CAUSE_MAX_CHARS: usize = 200;

/// AI-12 (loop 2): how many CONSECUTIVE environmental ticks the corroboration
/// guard tolerates before it stops granting blanket immunity. Corroboration
/// needs readable peers; two persistently broken (or flaky) handles erroring
/// together every tick would otherwise defeat crash detection FOREVER — the
/// exact hole AI-12 closed. At ~250ms per reaper tick, 40 ticks ≈ 10s of
/// continuous multi-handle failure, after which the per-handle streak path
/// resumes (and trips `MAX_CONSECUTIVE_POLL_ERRORS` ticks later) with a
/// diagnostic naming the escalation.
const MAX_CONSECUTIVE_ENVIRONMENTAL_TICKS: u32 = 40;

/// AI-41 (loop 2): how many consecutive failed drain passes the MidRun cursor
/// may stay parked at the SAME byte offset before the block is skipped with a
/// diagnostic. Bounds both the park (a permanently poisoned row cannot wedge
/// the cursor forever) and the diagnostic noise (≤ one diagnostic per attempt
/// per offset, then the skip note).
const USAGE_PARK_MAX_ATTEMPTS: u32 = 3;

/// Story 12-4 AMENDMENT (review loop 1): how many minted events the observed
/// drain's pending park buffer may hold at once. During a store outage EVERY
/// reaper tick (~250ms cadence) mints MORE events than it can commit, so
/// without a cap the parked buffer would grow for as long as the outage lasts
/// — unbounded engine memory for an unbounded outage. When a park would exceed
/// this cap, the OLDEST parked events are DROPPED (billing honesty: the ledger
/// keeps its order-faithful sequence) and the loss is announced LOUDLY with the
/// count — surfaced-not-silent, never a quiet truncation. The value is generous
/// (each parked event is two token counts + a sequence ordinal — a few dozen
/// bytes), so the cap only bites in a genuine, sustained outage; the 3-attempt
/// SKIP bound above is the per-event counterpart of this whole-buffer bound.
const OBSERVED_PARK_MAX_EVENTS: usize = 1024;

/// A scheduled restart of a crashed instance (story 1-6, AC4). Returned by
/// [`Supervisor::poll_once`] for each crashed `on-failure` instance that has not
/// hit the crash-loop threshold; the engine cadence sleeps [`RestartPlan::delay`]
/// then calls [`Supervisor::restart`] with the plan's `attempt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartPlan {
    /// The instance to restart.
    pub name: InstanceName,
    /// The consecutive restart attempt number (1-based) this plan represents.
    pub attempt: u32,
    /// The backoff to wait before performing the restart.
    pub delay: Duration,
}

/// The Restart Policy outcome for a just-crashed instance (internal to the
/// reaper). Carries the crash cause to record in the event log — enriched with
/// the policy conclusion on a terminal outcome so the failed cause survives after
/// the write-ahead record is cleared — and, when a restart is scheduled, the
/// [`RestartPlan`] the engine cadence should time.
struct RestartDecision {
    /// The crash cause detail to record on the `running → failed` event.
    crash_cause: String,
    /// The restart to schedule, or `None` on a terminal (`never`/crash-loop) outcome.
    plan: Option<RestartPlan>,
}

/// Truncate a diagnostic string for inclusion in a crash cause (AI-12, loop 1):
/// the operator gets the actual why (the last poll error's text), bounded so a
/// pathological message cannot bloat the event log. Pure — unit-tested.
fn truncate_for_cause(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max_chars).collect();
    cut.push('…');
    cut
}

/// One held handle's SAME-TICK poll result (AI-12, loop 1): every handle is
/// polled once per reaper pass BEFORE any crash handling, so the pass can
/// corroborate — a poll error shared by MULTIPLE handles in one tick is an
/// environmental condition, not a per-handle fault.
enum PollOutcome {
    /// `Ok(ProcessStatus::Alive)`.
    Alive,
    /// `Ok(ProcessStatus::Exited { code })`.
    Exited(Option<i32>),
    /// `Err(_)` — carries the error for the streak decision + the crash cause.
    Errored(BackendError),
}

/// The AI-12 verdict for ONE reaper poll of one held handle — what this pass's
/// `backend.poll` result means for crash detection.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PollVerdict {
    /// A clean `Alive` read: still alive — and the handle's consecutive
    /// poll-error streak RESETS.
    Alive,
    /// The process exited with the given (possibly unknown) code — crash input,
    /// exactly as before AI-12.
    Exited(Option<i32>),
    /// A poll error whose streak stays BELOW [`MAX_CONSECUTIVE_POLL_ERRORS`] —
    /// tolerated as transient (the historical behavior for a short error burst);
    /// the next pass re-checks. The streak increments.
    TransientError,
    /// A poll error that reached [`MAX_CONSECUTIVE_POLL_ERRORS`] — the handle can
    /// no longer be trusted as "still alive": CRASH INPUT (the instance is
    /// reconciled to `failed` with a cause naming the persistent poll failure),
    /// never a silent `None` forever.
    PersistentError,
}

/// Decide what one reaper poll means (AI-12). Pure — no I/O, no locks — so the
/// streak policy is unit-testable without a backend. `previous_streak` is the
/// handle's consecutive `backend.poll` error count BEFORE this pass; the returned
/// pair is the verdict and the streak to store for the next pass.
///
/// * `Ok(Alive)` → [`PollVerdict::Alive`], streak reset to 0;
/// * `Ok(Exited)` → [`PollVerdict::Exited`], streak reset to 0 (the handle is
///   leaving the map anyway);
/// * `Err(_)`, `previous_streak + 1 < MAX` → [`PollVerdict::TransientError`]
///   (streak `previous_streak + 1`);
/// * `Err(_)`, `previous_streak + 1 >= MAX` → [`PollVerdict::PersistentError`].
fn poll_verdict(
    previous_streak: u32,
    poll: Result<ProcessStatus, BackendError>,
) -> (PollVerdict, u32) {
    match poll {
        Ok(ProcessStatus::Alive) => (PollVerdict::Alive, 0),
        Ok(ProcessStatus::Exited { code }) => (PollVerdict::Exited(code), 0),
        Err(_) => {
            let streak = previous_streak.saturating_add(1);
            if streak >= MAX_CONSECUTIVE_POLL_ERRORS {
                (PollVerdict::PersistentError, streak)
            } else {
                (PollVerdict::TransientError, streak)
            }
        }
    }
}

/// The crash input the reaper acts on for one held handle — an observed exit
/// (with its code, `None` when the backend cannot report one) or, new under
/// AI-12, a handle that went permanently un-pollable.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CrashInput {
    /// `backend.poll` reported a real exit.
    Exited(Option<i32>),
    /// `MAX_CONSECUTIVE_POLL_ERRORS` consecutive poll errors — treated as a
    /// crash signal even though no exit was observed. `sole_handle` records
    /// that the failing handle was the ONLY held handle (AI-12, loop 2): its
    /// errors could never be corroborated against peers, so the cause must say
    /// a single-handle fleet cannot distinguish a per-handle fault from a
    /// platform-wide one.
    PersistentPollFailure { sole_handle: bool },
}

/// How [`Supervisor::drain_usage_for`] (and, story 12-4, the observed sibling
/// [`Supervisor::drain_observed_for`]) treats a drain — the difference is
/// whether a failure can be retried by a later pass: MidRun parks and retries
/// (bounded), Terminal announces the loss (no next pass). For the log-tail
/// drain it ALSO decides whether a final newline-less line is consumed now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrainMode {
    /// The process is (believed) still alive — the reaper cadence. Consume only up
    /// to the last newline; a partial final line may still be completed, so it waits
    /// for the next pass.
    MidRun,
    /// The process is DEAD (drain-on-stop / drain-on-reap) — no more bytes will ever
    /// append. Consume the WHOLE tail, INCLUDING a final newline-less line, so a last
    /// usage line flushed without a trailing `\n` is not stranded and lost when the
    /// next Run's cursor anchors past it.
    Terminal,
}

/// What a single [`Supervisor::drain_usage_for`] pass should do with the captured
/// log, decided purely from `(bytes, cursor, mode)` (story 3-1 — the H1 terminal-
/// tail rule + the M2 shrink guard, unit-testable without a process handle).
#[derive(Clone, Debug, PartialEq, Eq)]
enum DrainPlan {
    /// The log shrank below the cursor (a truncate/rotation — M2). Snap the cursor
    /// to `new_cursor` (the new length) and ingest NOTHING — never re-read from 0
    /// under the same live `run_id` (that would double-count → an inflated bill).
    Shrunk { new_cursor: u64 },
    /// No complete unit to consume this pass (an empty tail, or a MidRun tail with no
    /// newline yet). Leave the cursor where it is.
    Nothing,
    /// Consume `bytes[range]` and set the cursor to `new_cursor`.
    Consume {
        range: std::ops::Range<usize>,
        new_cursor: u64,
    },
}

/// Decide what one drain pass reads (spine AD-7; story 3-1 H1/M2). Pure — no I/O.
///
/// * Shrink (M2): `cursor > len` ⇒ [`DrainPlan::Shrunk`] (snap to `len`, ingest
///   nothing) — the anti-double-count fallback for a truncated/rotated log.
/// * Otherwise consume the tail `bytes[cursor..]`:
///   - [`DrainMode::Terminal`] consumes the WHOLE tail (the process is dead; a
///     final newline-less usage line must land now or be lost — H1).
///   - [`DrainMode::MidRun`] consumes only up to the last `\n` (a live process may
///     still complete a partial final line on a later pass); no newline ⇒ nothing.
fn plan_drain(bytes: &[u8], cursor: u64, mode: DrainMode) -> DrainPlan {
    let len = bytes.len() as u64;
    if cursor > len {
        return DrainPlan::Shrunk { new_cursor: len };
    }
    let start = cursor as usize;
    let tail = &bytes[start..];
    let consumable = match mode {
        DrainMode::Terminal => tail.len(),
        DrainMode::MidRun => match tail.iter().rposition(|b| *b == b'\n') {
            Some(pos) => pos + 1, // include the newline
            None => 0,            // no complete line yet — nothing to consume
        },
    };
    if consumable == 0 {
        return DrainPlan::Nothing;
    }
    DrainPlan::Consume {
        range: start..start + consumable,
        new_cursor: cursor + consumable as u64,
    }
}

/// The outcome of ONE incremental read of an instance's agent-output log for a
/// usage drain (AI-63) — the input `drain_usage_for` feeds to the UNCHANGED
/// [`plan_drain`].
///
/// **Why this exists (the billing-critical stall it fixes):** the metered
/// `agent.log` is NEVER rotated (Epic 4 shipped rotation only for the off-lock
/// attributed `output.log`, not this file). The previous `drain_usage_for` did
/// `std::fs::read(&path)` — reading the ENTIRE file into memory — on EVERY
/// crash-reaper tick (~250ms, per running instance), while BOTH global locks
/// (Registry + Supervisor) were held. For a long-running agent the file grows
/// without bound, so that whole-file read grows without bound and every fleet
/// operation stalls longer the longer the engine runs. [`read_usage_tail`] reads
/// ONLY `[cursor, len)` — the bytes appended since the last drain — which is all
/// [`plan_drain`] ever looked at anyway (it inspects only `bytes[cursor..]` and
/// `bytes.len()`); the already-consumed `[0, cursor)` prefix was pure waste.
#[derive(Debug, PartialEq, Eq)]
enum UsageTail {
    /// The log could not be opened / stat'd / read this pass — a best-effort
    /// skip, BYTE-FOR-BYTE the old `let Ok(bytes) = std::fs::read(..) else
    /// { return }`: the cursor is left untouched and the next pass retries (the
    /// DB is the source of truth). Ingest nothing.
    Unavailable,
    /// The file is SHORTER than the cursor (a truncate/rotation — the M2 guard).
    /// The SAME decision as [`DrainPlan::Shrunk`]: snap the cursor to `new_cursor`
    /// (the new length) and ingest nothing this pass — NEVER re-read from 0 under
    /// the same live `run_id` (that re-ingests already-counted lines → a
    /// double-count → an INFLATED bill). Detected HERE rather than in
    /// [`plan_drain`] because a shrink is exactly the case where `len - cursor`
    /// would underflow, so it must be caught before computing how many tail bytes
    /// to read.
    Shrunk {
        /// The file's new (shorter) length — the value the cursor snaps to.
        new_cursor: u64,
    },
    /// `bytes` is exactly the on-disk region `[cursor, len)` — BYTE-FOR-BYTE what
    /// the old code's `bytes[cursor..]` whole-file slice held, obtained WITHOUT
    /// reading (or allocating) the already-consumed `[0, cursor)` prefix. Fed
    /// straight to [`plan_drain`] with a 0 base (see [`Supervisor::drain_usage_for`]).
    Tail {
        /// The tail bytes `[cursor, len)`; empty when nothing new was appended.
        bytes: Vec<u8>,
    },
}

/// Read ONLY the new tail (`[cursor, len)`) of the agent-output log at `path`
/// for a usage drain (AI-63) — the incremental replacement for the previous
/// whole-file `std::fs::read`. This is PURE I/O; the CONSUMPTION decision stays
/// in the unchanged, adversarially-reviewed [`plan_drain`] (Epic 3, spine AD-7).
///
/// Mirrors [`crate::ports`]'s off-lock `tail_new_lines` (the proven
/// attributed-log tailer) — open, stat the length, seek to the cursor, read only
/// `len - cursor` bytes, guard the shrink case:
/// * open fails (a missing/unreadable file) ⇒ [`UsageTail::Unavailable`] — the
///   same best-effort skip the old `std::fs::read` `Err(_)` arm made;
/// * `len < cursor` ⇒ [`UsageTail::Shrunk`] — the M2 guard, snap forward + ingest
///   nothing, matching [`DrainPlan::Shrunk`] exactly;
/// * `len == cursor` ⇒ an EMPTY [`UsageTail::Tail`] (the reaper's common case:
///   nothing appended since the last tick) — [`plan_drain`] then returns
///   [`DrainPlan::Nothing`], exactly as the old whole-file path did at end-of-log,
///   and no `seek`/`read` syscall is issued;
/// * otherwise seek to `cursor` and read EXACTLY `len - cursor` bytes — the tail.
///
/// **Why the tail is byte-identical to the old whole-file slice:** it is
/// literally the same on-disk region `[cursor, len)` of the same file.
/// [`plan_drain`] only ever inspected `bytes[cursor..]` and `bytes.len()`; the
/// `[0, cursor)` prefix it never touched is precisely what this skips reading.
/// So the block later handed to `usage_source.drain` and the resulting
/// `usage_cursor` are UNCHANGED for every input — see [`Supervisor::drain_usage_for`]
/// for the (tail-relative range + absolute cursor) coordinate translation.
///
/// **Snapshot semantics:** the read is capped to the length stat'd at entry
/// (`read_exact` of exactly `len - cursor` bytes), so any bytes the live agent
/// appends AFTER the stat are simply left for the next pass — a stable per-pass
/// snapshot, exactly like the old `std::fs::read` captured whatever existed at
/// its call. The rare shrink BETWEEN the stat and the read makes `read_exact`
/// fall short ⇒ [`UsageTail::Unavailable`] (skip, retry next pass; the cursor is
/// untouched, so no miscount).
///
/// **No per-pass byte/line cap (deliberate — unlike `tail_new_lines`):** see
/// [`Supervisor::drain_usage_for`]'s docs for why capping this read would risk a
/// billing regression (the Terminal drain is single-shot, so a capped remainder
/// would be permanently stranded → an under-count, reintroducing H1).
fn read_usage_tail(path: &Path, cursor: u64) -> UsageTail {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return UsageTail::Unavailable;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return UsageTail::Unavailable;
    };
    if len < cursor {
        // M2 shrink guard — matches plan_drain's `cursor > len` branch.
        return UsageTail::Shrunk { new_cursor: len };
    }
    let want = len - cursor; // no underflow: `len >= cursor` guaranteed above.
    if want == 0 {
        // Nothing appended since the last drain — the reaper's common case.
        // Skip the seek/read entirely; plan_drain on an empty tail is `Nothing`.
        return UsageTail::Tail { bytes: Vec::new() };
    }
    if file.seek(SeekFrom::Start(cursor)).is_err() {
        return UsageTail::Unavailable;
    }
    let mut buf = vec![0u8; want as usize];
    if file.read_exact(&mut buf).is_err() {
        // A transient read hiccup (or a shrink racing the stat above) — skip this
        // pass, cursor untouched, retry next pass. No bytes ingested ⇒ no miscount.
        return UsageTail::Unavailable;
    }
    UsageTail::Tail { bytes: buf }
}

/// What one `Supervisor::read_agent_log_since` poll should do, decided purely
/// from `(bytes, cursor)` (story 4-2, Task 5, AC-D/AC-H/AC-G). MIRRORS (does
/// NOT literally reuse) [`plan_drain`]'s shrink-guard + "consume only up to
/// the last complete newline" shape — deliberately kept as an INDEPENDENT
/// pure function rather than a shared generalization: this is Epic 4's READ
/// path, `plan_drain` is Epic 3's adversarially-reviewed BILLING ingestion
/// path (story 3-1/AD-7), and coupling them would put a change to one at risk
/// of silently affecting the other's already-hardened behavior (a
/// genericization was evaluated and deliberately NOT taken — Task 1's Dev
/// Notes).
#[derive(Clone, Debug, PartialEq, Eq)]
enum FollowPlan {
    /// The file is SHORTER than the cursor — a rotation happened since the
    /// last poll. Snap the cursor to `new_cursor` (the file's new length) and
    /// deliver nothing new THIS pass; the caller detects the snap-back
    /// (`new_cursor < cursor`) and prints the one-line rotation notice
    /// (Task 6) — never a claim of completeness across the boundary.
    Shrunk { new_cursor: u64 },
    /// Consume `bytes[range]` (a whole number of COMPLETE lines only — a
    /// trailing partial line, if any, waits for the next poll, exactly like
    /// `plan_drain`'s MidRun tail rule) and advance the cursor to
    /// `new_cursor`.
    Consume {
        range: std::ops::Range<usize>,
        new_cursor: u64,
    },
}

/// Decide what one `read_agent_log_since` poll reads. Pure — no I/O.
fn plan_follow(bytes: &[u8], cursor: u64) -> FollowPlan {
    let len = bytes.len() as u64;
    if cursor > len {
        return FollowPlan::Shrunk { new_cursor: len };
    }
    let start = cursor as usize;
    let tail = &bytes[start..];
    let consumable = match tail.iter().rposition(|b| *b == b'\n') {
        Some(pos) => pos + 1,
        None => 0,
    };
    FollowPlan::Consume {
        range: start..start + consumable,
        new_cursor: cursor + consumable as u64,
    }
}

/// The parked OBSERVED events pending a ledger commit (story 12-4) — the
/// observed channel's analog of the self-reported drain's byte cursor. The
/// queue has no byte offset, so the parked identity is the minted events
/// THEMSELVES: the buffer carries the exact [`ParsedUsage`] values that failed
/// to commit (NEVER re-minted on retry — re-minting would hand out fresh
/// `sequence` ordinals and break the dedup key stability the retry depends
/// on), with the FRONT event's minted `sequence` as the attempt-counting
/// identity (the analog of the self-reported "same offset" check: a different
/// front resets the streak).
#[derive(Debug, Clone)]
struct ObservedPending {
    /// The minted `sequence` of the FRONT parked event — the attempt-counting
    /// identity (a different front resets the bounded-retry streak).
    front_sequence: u64,
    /// The exact minted events awaiting commit, in original (queue) order.
    /// Capped at [`OBSERVED_PARK_MAX_EVENTS`]: a longer tail drops its OLDEST
    /// events with a loud overflow diagnostic (the 12-4 AMENDMENT bound —
    /// never unbounded memory during a store outage, never a silent loss).
    events: Vec<ParsedUsage>,
}

/// The in-memory supervision state for ONE running Agent Instance (story 3-1).
///
/// Beyond the process [`Handle`](backends::Handle) the supervisor has always held,
/// this carries the metering context ingestion needs during the instance's Run:
/// the current [`RunId`] (minted at `starting`, spine AD-7), the declared metering
/// source (its wire string, stamped on every ingested [`UsageEvent`]), and a byte
/// CURSOR into the per-instance agent-output log so each reaper pass ingests only
/// the NEWLY-captured tail (never re-reading — and never re-attributing a prior
/// Run's lines under a fresh Run id after a stop→start). It lives for THIS engine
/// lifetime alongside the handle, exactly like the handle map it replaced.
struct Supervised {
    /// The backend-owned process handle (group/job control).
    handle: backends::Handle,
    /// The current Run this instance is in (spine AD-7) — minted at `starting`.
    run_id: RunId,
    /// The declared Metering Source wire string (`self-reported` / `engine-observed`),
    /// stamped on every [`UsageEvent`] ingested during this Run.
    metering_source: String,
    /// Byte offset already consumed from the agent-output log — the ingestion read
    /// cursor. Advanced past each block the drain reads, so lines are ingested at
    /// most once from the capture (the DB dedup is the second, authoritative guard).
    usage_cursor: u64,
    /// AI-41 (loop 2): the MIDRUN park bound — `Some((parked_cursor, attempts))`
    /// while a store error keeps the cursor parked at `parked_cursor`; `attempts`
    /// counts consecutive failed drain passes AT THAT OFFSET (any different
    /// offset resets it). Past [`USAGE_PARK_MAX_ATTEMPTS`] the block is skipped
    /// with a loud diagnostic, so a permanently poisoned row cannot wedge the
    /// cursor (and silently strand every later usage event for the Run) forever.
    usage_park_attempts: Option<(u64, u32)>,
    /// Story 12-4: the OBSERVED channel's park — `Some((pending, attempts))`
    /// while a store error keeps minted-but-uncommitted observed events parked
    /// (the analog of `usage_park_attempts`, which is the SELF-REPORTED
    /// channel's park). `pending.events` are the exact [`ParsedUsage`] values
    /// awaiting commit (retried as-is — never re-minted, so the dedup keys are
    /// stable); `attempts` counts consecutive failed passes at the SAME front
    /// event (a different front resets the streak); past
    /// [`USAGE_PARK_MAX_ATTEMPTS`] the poisoned front event is SKIPPED with a
    /// loud diagnostic and the rest keep counting. Dropped with the instance
    /// at the terminal transition (a terminal failure announces the loss).
    observed_park: Option<(ObservedPending, u32)>,
    /// The per-Run breach LATCH (story 3-2 idempotence fix; story 3-3 keyed by
    /// dimension): the set of `(dimension, scope)` pairs that have ALREADY fired a
    /// breach for THIS Run. Enforcement (`enforce_budget`) runs on EVERY committed
    /// usage event, but a breach must fire **at most once per (dimension, scope) per
    /// Run** — otherwise every post-crossing event re-records a `BudgetBreachEvent`
    /// and re-fires the action (unbounded duplicate records for `warn`; redundant
    /// records for `pause`/`stop`). A pair is inserted the first time it trips; a
    /// subsequent event whose pair is already latched short-circuits BOTH the record
    /// and the action.
    ///
    /// STORY 3-3 — DIMENSION KEY: the latch key is `(BreachDimension, BreachScope)`
    /// so a TOKEN breach and a DOLLAR breach of the SAME scope latch INDEPENDENTLY —
    /// each fires once per Run (a run can legitimately trip both its token ceiling
    /// and its dollar cap; the action is identical, so both fire once each). The
    /// per-run and cumulative scopes still latch independently within each dimension.
    /// The latch lives on `Supervised`, so it RESETS automatically when a new Run
    /// starts — a fresh `Supervised` (built at `starting`, where the `run_id` is
    /// freshly minted) begins empty, giving "at most one breach per (dimension,
    /// scope) per Run".
    breached_scopes: std::collections::HashSet<(BreachDimension, BreachScope)>,
    /// The per-instance loopback forward listener for an `engine-observed` instance
    /// (story 3-4), or `None` for a `self-reported` instance (whose start path is
    /// UNCHANGED). Held for the Run; DROPPED at the terminal transition (which
    /// aborts its accept-loop task — teardown bounded to the Run, no orphan
    /// listeners, NFR-1). A restart opens a NEW listener under the new Run.
    observed_listener: Option<ObservedListener>,
    /// The `engine-observed` source (story 3-4): the per-Run monotonic `sequence`
    /// minter for observed completions (the agent supplies no ordinal). Fresh per
    /// Run (built here with the freshly-minted `run_id`), so the ordinal resets per
    /// Run — preserving the `UNIQUE(instance_id, run_id, sequence)` dedup invariant.
    /// Present only for an `engine-observed` instance (a `self-reported` instance
    /// leaves it `None` and drives the log-tail `drain_usage_for` instead).
    observed_source: Option<ObservedUsageSource>,
    /// Set when a PRIOR [`Supervisor::stop`] call on this handle's `stop_inner`
    /// pass got [`BackendError::StopUnconfirmed`] back from the backend (fix
    /// pass, review of #80 follow-up — the CRITICAL finding): SIGKILL was sent
    /// but death could not be confirmed within [`KILL_CONFIRM_TIMEOUT`], most
    /// likely because the process is stuck in an OS-level uninterruptible I/O
    /// wait. Defaults `false` for a freshly started OR adopted instance (an
    /// ordinary stop attempt never sets it). Lets BOTH a RETRY `stop()` call
    /// (see `stop_inner`'s docs) and the crash reaper (`poll_once`) recognize
    /// "this handle's death is pending reconciliation" — via a cheap,
    /// NON-BLOCKING liveness poll, never a repeat of the whole bounded
    /// SIGTERM/SIGKILL/confirm sequence — distinctly from an ORDINARY
    /// in-flight stop or an externally-forced `stopping` row (neither of
    /// which ever sets this flag), so this fix pass changes behavior ONLY
    /// for the specific scenario it targets.
    stop_unconfirmed: bool,
    /// Whether this handle was ADOPTED (re-acquired by
    /// [`Supervisor::adopt_orphans`] from a prior engine session) rather than
    /// spawned by THIS engine (AI-13). An adopted handle is not the engine's
    /// child, so its exit code is unrecoverable — `Exited { code: None }` for an
    /// adopted process means "code UNAVAILABLE", and the crash cause must say so
    /// instead of asserting a signal termination it cannot prove. `false` for a
    /// freshly spawned process (whose `code: None` genuinely means "terminated by
    /// a signal" — `try_wait` had the authoritative `ExitStatus`).
    adopted: bool,
}

/// A host-provided diagnostic sink (story 10-2): the writer every engine
/// diagnostic routes to when one is installed, instead of the default stderr.
///
/// The engine names the WIDE, thread-safe shape — `Arc<Mutex<Box<dyn Write +
/// Send>>>` — so a host can clone the `Arc` and share ONE sink across several
/// engines (or engine + non-engine components) in one process. The engine's
/// two AD-12 diagnostics (the DC-10 memory-delivery notice and the
/// enforcement breadcrumb) emit through it when one is installed; with no
/// sink the diagnostics go to stderr exactly as they always have (the default
/// path is byte-identical to the pre-sink behavior).
///
/// Each diagnostic arrives as ONE full line — the message text as it appears
/// on stderr today, `[hemaka] ` prefixed, `\n` terminated — so a sink
/// receiving a diagnostic receives the exact bytes the default stderr path
/// would have emitted. Write failures are swallowed (the diagnostics are
/// best-effort by contract, AD-12 — a broken or closed host writer must never
/// fail or crash supervision), and a write PANIC in the host's `Write` impl
/// is caught and swallowed the same way — a host bug must never unwind
/// through the engine's supervisor critical section (that would poison the
/// supervisor mutex on its way out); the same sink keeps receiving later
/// diagnostics. The writer is invoked while the SUPERVISOR lock is held
/// (both emission sites are supervisor paths), so a sink's `Write` impl must
/// not re-enter the engine — a call that took the supervisor lock would
/// deadlock; routing the line onward inside the writer's own lock is fine.
///
/// Installing is ONE-WAY: `install_diagnostics` (reached via
/// [`Engine::with_diagnostics`] / [`Blocking::with_diagnostics`]) REPLACES
/// the current sink — it never removes one, so there is no uninstall back to
/// the stderr default. A host that wants the default back re-opens the
/// engine ([`Engine::open`](crate::Engine::open)), or installs its own writer
/// that emits to the process's stderr.
///
/// Install via [`Engine::open_with_diagnostics`](crate::Engine::open_with_diagnostics)
/// (airtight — the sink is in place before orphan adoption and before the
/// crash-detection reaper starts) or post-open via
/// [`Engine::with_diagnostics`](crate::Engine::with_diagnostics) /
/// [`Blocking::with_diagnostics`](crate::Blocking::with_diagnostics)
/// (install or rotate at any later point; rotation flushes the outgoing
/// writer before the swap so buffered bytes are not silently lost).
pub type DiagnosticSink = Arc<Mutex<Box<dyn Write + Send>>>;

/// The lifecycle supervisor: owns running process handles + drives transitions.
///
/// Constructed empty by [`Engine::open`](crate::Engine::open). Holds ONE
/// [`ProcessBackend`](crate::ports::ProcessBackend) (the current OS's), a map of
/// the instances it currently supervises (each with its process handle + metering
/// context, story 3-1), the self-reported [`UsageSource`](crate::ports::UsageSource)
/// ingestion adapter, and the [`BackoffSchedule`] the restart executor uses
/// (production 1s×2 cap 60s; tests inject a scaled one).
pub struct Supervisor {
    backend: backends::Backend,
    running: HashMap<InstanceName, Supervised>,
    usage_source: SelfReportedUsageSource,
    backoff: BackoffSchedule,
    /// The event bus (story 7-2, FR-33): publishes at the three commit points
    /// (transition append, breach append, usage-ingestion commit) so a
    /// subscriber observes exactly the committed truth in commit order. The
    /// engine holds its own clone ([`Supervisor::event_bus`]) so
    /// `subscribe()` never takes the supervisor lock.
    events: EventBus,
    /// The engine's tokio runtime handle (story 3-4), used to SPAWN the loopback
    /// forward listener's accept loop for an `engine-observed` instance. The
    /// supervisor's sync start path runs on the blocking pool, so it cannot use
    /// `Handle::current`; the engine threads its handle in via
    /// [`Supervisor::with_runtime`]. `None` (the [`Supervisor::new`]/
    /// [`Supervisor::with_backoff`] default) means "no runtime to spawn a
    /// listener" — an `engine-observed` start then fails fast with a clear error
    /// (only the sync unit tests, which never start an observed instance, use the
    /// handle-less constructors).
    runtime: Option<tokio::runtime::Handle>,
    /// The host-provided diagnostic sink (story 10-2), when one is installed.
    /// `None` (every constructor's default) keeps the historical behavior: the
    /// two AD-12 diagnostics go to stderr. Both emission sites run while the
    /// supervisor lock is held (the start / enforcement paths), so the sink's
    /// own `Mutex` is contended only by the rare diagnostics — never a hot
    /// path — and installs/rotations serialize with emissions correctly.
    diagnostics: Option<DiagnosticSink>,
    /// AI-12: the per-instance CONSECUTIVE `backend.poll` error count, kept
    /// across reaper passes so a PERSISTENT poll failure can trip
    /// [`MAX_CONSECUTIVE_POLL_ERRORS`] and be treated as a crash signal instead
    /// of being swallowed forever as "still-alive". A clean poll (or the
    /// handle's removal) clears the entry; a singleton error streak restarts
    /// from zero.
    poll_error_streaks: HashMap<InstanceName, u32>,
    /// AI-12 (loop 1): the LAST poll error's text per handle (truncated at
    /// [`POLL_ERROR_CAUSE_MAX_CHARS`]), so the persistent-poll-failure crash
    /// cause can carry the actual why — not just the bare count. Updated on
    /// every poll error (handle-specific OR environmental), read when a streak
    /// trips, cleared wherever the streak clears.
    poll_last_errors: HashMap<InstanceName, String>,
    /// AI-12 (loop 2): how many CONSECUTIVE ticks the corroboration guard has
    /// classified as environmental. Every non-environmental tick resets it.
    /// Past [`MAX_CONSECUTIVE_ENVIRONMENTAL_TICKS`] the guard stops granting
    /// blanket immunity (per-handle streak credit resumes), so a persistently
    /// erroring pair cannot keep crash detection defeated forever.
    consecutive_environmental_ticks: u32,
    /// AI-12 (loop 1) — cfg(test) FAULT-INJECTION SEAM at the backend poll
    /// boundary: a pid listed here fails every `backend.poll` with an injected
    /// [`BackendError::Control`], driving `poll_once` down its persistent-
    /// poll-failure path in the lib wiring tests without staging a real
    /// procfs/sysctl outage. Placement NOTE: the seam fronts the BACKEND's
    /// poll (the supervisor only ever sees the port), but its state lives on
    /// the supervisor — the embed-clean audit forbids global cells in the
    /// engine, and per-supervisor state is test-isolated by construction
    /// (each test's faults die with its own supervisor). Never compiled
    /// outside the lib's own test builds.
    #[cfg(test)]
    poll_fault_pids: std::collections::HashSet<u32>,
    /// AI-9 (loop 2) — cfg(test) FAULT-INJECTION SEAM at the backend signal
    /// boundary: an instance name listed here fails every `signal_backend` for
    /// it with an injected [`BackendError::Control`], driving the post-commit
    /// signal-failure branch of `suspend_or_resume` (the transition has already
    /// committed, so the ledger and the live process diverge) in the lib wiring
    /// tests without staging a real kill/pgid outage. Placement NOTE: the seam
    /// fronts the BACKEND's pause/resume (the supervisor only ever sees the
    /// port), but its state lives on the supervisor — same rationale as
    /// `poll_fault_pids` above (the embed-clean audit forbids global cells, and
    /// per-supervisor state is test-isolated by construction). Never compiled
    /// outside the lib's own test builds.
    #[cfg(test)]
    signal_fault_names: std::collections::HashSet<InstanceName>,
}

impl Supervisor {
    /// Construct an empty supervisor with the current OS's process backend and
    /// the PRODUCTION backoff schedule (1s base, ×2, 60s cap — spine AD-15).
    ///
    /// NO runtime handle (story 3-4) — so this cannot start an `engine-observed`
    /// listener. Production uses [`Supervisor::with_runtime`] (the engine threads
    /// its runtime handle in); this handle-less form remains for the sync unit
    /// tests that only exercise self-reported / lifecycle paths.
    pub fn new() -> Self {
        Self {
            backend: backends::current(),
            running: HashMap::new(),
            usage_source: SelfReportedUsageSource::new(),
            backoff: BackoffSchedule::production(),
            events: EventBus::new(),
            runtime: None,
            diagnostics: None,
            poll_error_streaks: HashMap::new(),
            poll_last_errors: HashMap::new(),
            consecutive_environmental_ticks: 0,
            #[cfg(test)]
            poll_fault_pids: std::collections::HashSet::new(),
            #[cfg(test)]
            signal_fault_names: std::collections::HashSet::new(),
        }
    }

    /// Construct an empty supervisor with the PRODUCTION backoff schedule AND the
    /// engine's tokio runtime handle (story 3-4) — the production constructor the
    /// engine uses. The handle lets an `engine-observed` start SPAWN its loopback
    /// forward listener's accept loop on the engine runtime (the supervisor's sync
    /// start path runs on the blocking pool, so `Handle::current` is unavailable;
    /// a `Handle` spawns onto its runtime from any thread).
    pub fn with_runtime(runtime: tokio::runtime::Handle) -> Self {
        Self {
            backend: backends::current(),
            running: HashMap::new(),
            usage_source: SelfReportedUsageSource::new(),
            backoff: BackoffSchedule::production(),
            events: EventBus::new(),
            runtime: Some(runtime),
            diagnostics: None,
            poll_error_streaks: HashMap::new(),
            poll_last_errors: HashMap::new(),
            consecutive_environmental_ticks: 0,
            #[cfg(test)]
            poll_fault_pids: std::collections::HashSet::new(),
            #[cfg(test)]
            signal_fault_names: std::collections::HashSet::new(),
        }
    }

    /// Construct an empty supervisor with a custom backoff schedule (TEST
    /// injection, so the crash-loop / backoff legs run in milliseconds without
    /// weakening the production constants). Production always uses
    /// [`Supervisor::with_runtime`]. NO runtime handle — the lib tests using this
    /// never start an `engine-observed` instance.
    #[cfg(test)]
    pub(crate) fn with_backoff(backoff: BackoffSchedule) -> Self {
        Self {
            backend: backends::current(),
            running: HashMap::new(),
            usage_source: SelfReportedUsageSource::new(),
            backoff,
            events: EventBus::new(),
            runtime: None,
            diagnostics: None,
            poll_error_streaks: HashMap::new(),
            poll_last_errors: HashMap::new(),
            consecutive_environmental_ticks: 0,
            #[cfg(test)]
            poll_fault_pids: std::collections::HashSet::new(),
            #[cfg(test)]
            signal_fault_names: std::collections::HashSet::new(),
        }
    }

    /// A clone of this supervisor's event bus (story 7-2) — the engine holds it
    /// so [`Engine::subscribe`](crate::Engine::subscribe) hands out receivers
    /// WITHOUT taking the supervisor lock. `broadcast::Sender::clone` shares
    /// the same channel, so publishes through either clone reach every
    /// receiver.
    pub(crate) fn event_bus(&self) -> EventBus {
        self.events.clone()
    }

    /// Publish one committed event onto the bus (story 7-2).
    ///
    /// Called EXCLUSIVELY from the three commit points, immediately AFTER the
    /// durable append/commit succeeded. Every caller runs while the supervisor
    /// lock is held — a CALLER-ENFORCED obligation (the bus itself does not
    /// serialize; see the `domain::bus` module's ordering invariant) — so
    /// publishes are serialized in durable-append order: the per-instance FIFO
    /// guarantee. Publishing cannot fail supervision: a send with no receivers
    /// (or any send error) is swallowed by the bus.
    fn publish(&self, event: EngineEvent) {
        self.events.publish(event);
    }

    /// Install (or replace) the host-provided diagnostic sink (story 10-2).
    /// Crate-internal: hosts reach it through
    /// [`Engine::open_with_diagnostics`](crate::Engine::open_with_diagnostics)
    /// / [`Engine::with_diagnostics`](crate::Engine::with_diagnostics) /
    /// [`Blocking::with_diagnostics`](crate::Blocking::with_diagnostics).
    /// Installing replaces any earlier sink (a host rotating a log file
    /// installs the new writer over the old one); the change takes effect for
    /// every later diagnostic. Serialized with emissions by the supervisor
    /// mutex. Installing is ONE-WAY — this replaces, never removes; there is
    /// no uninstall back to the stderr default (see the
    /// [`DiagnosticSink`] docs).
    ///
    /// Rotation flushes the OUTGOING writer before the swap: a buffering
    /// host writer must not silently lose its already-emitted diagnostics
    /// because its bytes never made it out of the host's buffer. The flush
    /// is best-effort (an error or a panic in the outgoing writer's flush is
    /// swallowed, exactly like an emission write — never fails or crashes
    /// the rotation).
    pub(crate) fn install_diagnostics(&mut self, sink: DiagnosticSink) {
        if let Some(previous) = self.diagnostics.take() {
            let mut writer = previous
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = writer.flush();
            }));
        }
        self.diagnostics = Some(sink);
    }

    /// Emit ONE engine diagnostic (story 10-2) — the engine's ONLY diagnostic
    /// emission choke point. With a sink installed the line goes to the sink;
    /// with none it goes to STDERR, byte-identical to the pre-sink behavior
    /// (same `[hemaka] `-prefixed wording, one line, AD-12's "diagnostics ride
    /// the engine log / stderr, NEVER `kt` stdout").
    ///
    /// The `[hemaka] ` marker prefix and the terminating `\n` are added HERE,
    /// in one place, so the sink receives the exact bytes the stderr default
    /// would have emitted — same message text as today, one line per
    /// diagnostic.
    ///
    /// Best-effort by contract: a sink/stderr write failure is swallowed — a
    /// broken or closed host writer must never fail, block, or crash
    /// supervision (no diagnostic is the durable record of anything; the
    /// records live in the logs/ledger). A host writer's write/flush PANIC
    /// is caught with `catch_unwind` and swallowed the same way: an uncaught
    /// panic here would unwind through this supervisor-lock critical section
    /// and POISON the supervisor mutex, turning every later engine call into
    /// a panic. Because the panic never escapes this scope, the sink's own
    /// mutex never poisons either, and the same sink receives the next
    /// diagnostic. (The panic message itself still prints via the process's
    /// panic hook — the host's own bug surfacing on its own stderr is honest;
    /// silencing it would require installing a process-global hook, which
    /// the embed-clean audit forbids.)
    ///
    /// MUST be called while the SUPERVISOR lock is held — and that is
    /// load-bearing beyond the no-re-entry rule below: the `self.diagnostics`
    /// field read here is an ORDINARY, non-atomic read, and
    /// [`Supervisor::install_diagnostics`] swaps that field under the SAME
    /// supervisor mutex. Holding the lock across the read+write is what
    /// makes an install/rotation serialize with an emission (a diagnostic is
    /// never torn across two sinks, never races a rotation mid-write); a
    /// caller that read the field without the supervisor lock would race a
    /// concurrent `with_diagnostics`. Both emission sites are supervisor
    /// paths, so the precondition holds by construction — keep it that way.
    ///
    /// The sink's `Write` impl must not re-enter the engine — a call that
    /// took the supervisor lock would deadlock.
    fn emit_diagnostic(&self, message: &str) {
        let mut line = String::with_capacity(message.len() + 10);
        line.push_str("[hemaka] ");
        line.push_str(message);
        line.push('\n');
        match &self.diagnostics {
            Some(sink) => {
                let mut writer = sink
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let bytes = line.as_bytes();
                // Swallow BOTH failure modes (io error, panic) and FLUSH:
                // a buffering host writer must not silently lose the line
                // inside its own buffer after a successful `write_all`.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = writer.write_all(bytes);
                    let _ = writer.flush();
                }));
            }
            None => {
                let _ = std::io::stderr().write_all(line.as_bytes());
            }
        }
    }

    /// Start a registered / previously stopped / FAILED Agent Instance
    /// (AC1/AC2; AC3 restart-from-failed via the 1-6 transition row).
    ///
    /// Thin wrapper over [`Supervisor::start_inner`] with no restart context (a
    /// fresh operator `start`): the `starting → running` transition records a
    /// plain [`TransitionCause::AdapterReady`], and the write-ahead spawn record's
    /// restart count is RESET to 0 (a clean run resets the count, AC4).
    pub fn start(&mut self, registry: &Registry, name: &str) -> Result<AgentInstance, EngineError> {
        self.start_inner(registry, name, None, false)
    }

    /// Start a registered Agent Instance DETACHED (story 12-1): the spawned
    /// child's handle is DISARMED at spawn so the child survives THIS engine
    /// session's exit, and the next engine open re-adopts it through the
    /// EXISTING write-ahead fingerprint path (AD-5 — nothing about the record,
    /// the fingerprint, or the adoption machinery changes).
    ///
    /// Ratified v1 REFUSALS + HONEST WINDOW (AI-20 option b; the epic's
    /// enforcement-window honesty is a hard AC, not a nicety):
    /// * An `engine-observed` instance is refused BEFORE any side effect (no
    ///   transition, no listener, no spawn): the loopback forward listener
    ///   would die with this command, stranding the agent's model traffic on a
    ///   dead port — the same loud strand adoption already surfaces. The error
    ///   names why + the remediation (start without `--detach`).
    /// * A detached self-reported instance is supervised ONLY while THIS
    ///   command runs: between commands there is NO crash detection, NO budget
    ///   enforcement, and NO event delivery. The caller (the CLI) must state
    ///   that window on `--help` and in its stderr notice.
    /// * The child is spawned with stdin NULL regardless of the declared
    ///   interaction level (a pipe's write end would die with this CLI — an
    ///   EPIPE strand); after re-adoption `send` fails with the ordinary
    ///   adopted-instance interaction error.
    pub fn start_detached(
        &mut self,
        registry: &Registry,
        name: &str,
    ) -> Result<AgentInstance, EngineError> {
        self.start_inner(registry, name, None, true)
    }

    /// The shared start path (AC1/AC2 + the 1-6 write-ahead record commit).
    ///
    /// `restart`:
    /// * `None` — a fresh `start` (operator or first launch). The
    ///   `starting → running` cause is [`TransitionCause::AdapterReady`]; the
    ///   spawn record's restart count is RESET to 0.
    /// * `Some((attempt, waited))` — a Restart Policy restart (from
    ///   [`Supervisor::restart`]). The `starting → running` cause is
    ///   [`TransitionCause::Restarted`] recording the consecutive `attempt` +
    ///   the backoff `waited`; the record keeps that count.
    ///
    /// Order (so a rejection leaves NO spurious state change):
    /// 1. look up + validate `Start` against the transition table (AC4),
    /// 2. resolve the launch spec (a bad/native-only adapter rejects here),
    /// 3. persist `registered/stopped/failed → starting` + emit,
    /// 4. spawn; a spawn failure → `starting → failed` (diagnostic preserved),
    /// 5. readiness watch: an immediate death → `failed` (AC2),
    /// 6. **commit the write-ahead spawn record** (AD-5: `{pid, fingerprint}` +
    ///    policy + count) BEFORE the instance is treated as supervised — "no
    ///    spawn without its record committed first",
    /// 7. persist `starting → running` + emit, store the handle, return.
    fn start_inner(
        &mut self,
        registry: &Registry,
        name: &str,
        restart: Option<(u32, Duration)>,
        detach: bool,
    ) -> Result<AgentInstance, EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = registry.lookup(&name).map_err(registry_to_engine)?;

        // (1) Transition gate (AC4). Rejects (e.g. start on running) before any
        // side effect and BEFORE we touch the backend.
        let starting = next_state(instance.state, LifecycleCommand::Start)?;

        // (2) Resolve the launch spec (may reject: native-only / bad manifest),
        // still before any persisted state change.
        let (kind, manifest_path, persisted_launch) = registry
            .adapter_launch_facts(&name)
            .map_err(registry_to_engine)?;
        // Prefer the launch SNAPSHOTTED at registration — this removes the fragile
        // start-time manifest re-read that dropped `args` on hosted CI runners
        // (the agent spawned with the right binary but ZERO args). Fall back to
        // re-reading the manifest ONLY when the snapshot carries no launch: a
        // native adapter (→ NativeHasNoLaunch, preserved) or an instance
        // registered before the launch was persisted (legacy snapshot). The
        // fallback RE-NEGOTIATES the contract version (retro #161): the file on
        // disk may have drifted since registration, and a manifest edited to a
        // foreign major must fail the start, not bypass the 6-6 load gate.
        let mut launch = match persisted_launch {
            Some(launch) => launch,
            None => adapter::resolve_start_launch(&kind, manifest_path.as_deref())
                .map_err(|e| launch_to_engine(&name, e))?,
        };

        // Read the declared Metering Source (story 3-1) from the persisted adapter
        // snapshot — stamped on every UsageEvent ingested during this Run. Read here
        // (a pure snapshot read) before any side effect; a corrupt snapshot surfaces
        // the same way the launch-facts read above would.
        let metering_source = registry
            .metering_source(&name)
            .map_err(registry_to_engine)?;

        // Story 12-1 — the DETACHED REFUSAL for engine-observed instances,
        // BEFORE ANY SIDE EFFECT (no `starting` transition, no loopback
        // listener, no spawn — every fallible step below still lies ahead, so
        // the instance keeps its prior state). A detached observed instance is
        // structurally broken: the loopback forward listener lives inside THIS
        // engine process, so a detached start would inject a `base_url` whose
        // server dies the moment the CLI exits — the agent's model calls would
        // then hit a dead port (the loud stranded-listener condition adoption
        // already announces). Refuse with the why + the remediation; a
        // self-reported instance detaches fine (no listener to strand).
        if detach && metering_source == "engine-observed" {
            return Err(EngineError::DetachRefused {
                name: name.as_str().to_string(),
                detail: "the instance meters via the engine-observed channel, whose loopback \
                         forward listener lives inside the starting command — detaching would \
                         leave the agent pointed at a listener that dies with the command. \
                         Start it without --detach (the in-command supervision keeps the \
                         listener alive), or switch the adapter's metering source to \
                         self-reported."
                    .to_string(),
            });
        }

        // Read the effective (current-OS) Capability::Interaction level (story
        // 4.1 fix pass, HIGH finding — review of #79) to decide whether THIS
        // spawn should pipe stdin at all. The story's original implementation
        // piped UNCONDITIONALLY for every process; an adversarial audit showed
        // this can hang an adapter that declares no interaction support: a
        // process that blocks reading stdin at startup (a common "sniff for
        // piped input" real-CLI idiom) never sees EOF, because the engine
        // holds the pipe's write end open for the process's whole supervised
        // lifetime and nothing ever writes to it unless `send` is called — the
        // child hangs forever yet is reported `running` (readiness here is
        // just "the process didn't exit immediately"), a silent deadlock with
        // no error signal anywhere. Mirrors how the rest of this codebase
        // gates BEHAVIOR (not just callability) on declared capabilities
        // (e.g. pause's SIGSTOP-vs-noop branching). Read here (a pure
        // snapshot read, mirroring `metering_source` above) before any side
        // effect, so a corrupt snapshot rejects the start cleanly like every
        // other pre-transition read.
        let interaction_level = registry
            .effective_support(&name, Capability::Interaction)
            .map_err(registry_to_engine)?;
        let pipe_stdin = matches!(
            interaction_level,
            SupportLevel::Guaranteed | SupportLevel::BestEffort
        );

        // (2b) Map the resolved unified config into the adapter's NATIVE mechanism
        // (story 2-2, FR-12) — still before any persisted state change, so a
        // config/mapping failure rejects the start cleanly (no spurious state
        // change, no half-launched process). Resolve the instance's effective
        // config (2-1's four-layer fold; empty invocation overrides for a plain
        // start — the parameter is threaded so a future `start --set k=v` supplies
        // it without an API change), the adapter's declared mapping (manifest
        // `[config]` or the native code-declared table), then apply: known keys
        // land in their declared native target (env → launch.env; flag →
        // launch.args; file → a rendered file in the Agent Home), and `agent.*`
        // pass-through leaves are delivered VERBATIM (AC6). The Agent Home already
        // exists (created at registration); file targets render into it here.
        let home = registry.agent_home(&name);
        let mut effective = registry
            .effective_config(&name, crate::domain::ConfigLayer::empty())
            .map_err(|e| config_to_engine(&name, e))?;
        // (2b-memory-spoof) The reserved `memory.dir` key is a DELIVERY
        // MECHANISM, never operator configuration (story 5-1's CORRECTION; docs:
        // "the operator never set this key"). Strip any hand-set value from the
        // operator layers — mirroring the reserved-identity `name` drop — so it
        // can reach neither the mapping application nor the snapshot. Without
        // this, an operator-supplied value would flow through whenever NO
        // backing is attached (the engine override layer is absent then) and
        // masquerade as engine-delivered memory. Only the invocation-override
        // layer built further below may supply this key.
        let _ = effective.remove(super::config::MEMORY_DIR_KEY);
        let mapping = adapter::resolve_config_mapping(&kind, manifest_path.as_deref())
            .map_err(|e| launch_to_engine(&name, e))?;

        // (2b-memory) MANAGED MEMORY BACKING (story 5-1, spine AD-11). Read the
        // attached backing (one DB read) and — for a `filesystem` kind — ensure
        // the managed directory exists: ONE idempotent `create_dir_all`, no
        // recursion, no copy/seed/restore of CONTENTS ever (DC-7 — byte-identical
        // survival comes from non-interference; this is also the AD-17 bounded-work
        // rule: identical cost to `ensure_log_dir` below). Both happen HERE, in
        // the pre-transition block: every fallible step precedes any state change,
        // so a failure rejects the start with no spurious transition. This is the
        // defensive SELF-HEAL — attach already created it; a manual delete must
        // not wedge future starts.
        let memory_backing = registry.memory_backing(&name).map_err(registry_to_engine)?;
        let memory_dir = memory_backing
            .as_ref()
            .filter(|backing| backing.kind == MemoryBackingKind::Filesystem)
            .map(|_| registry.agent_memory_dir(&name));
        if let Some(dir) = &memory_dir {
            // Strict UTF-8 BEFORE anything else: this path is DELIVERED to the
            // agent at the reserved key (`invocation_overrides` stringifies it),
            // and a lossy coercion there would hand the agent a mangled path
            // while every local check still passed. A non-UTF-8 state-dir path
            // is effectively impossible for a sane install; if one shows up, it
            // fails LOUD here, pre-transition, with no side effect.
            if dir.to_str().is_none() {
                return Err(EngineError::Log {
                    name: name.as_str().to_string(),
                    path: dir.to_string_lossy().into_owned(),
                    detail: "the managed memory directory path is not valid UTF-8, so it \
                             cannot be delivered safely at the reserved 'memory.dir' key"
                        .to_string(),
                });
            }
            // Symlink refusal mirrors the attach-side guard (registry's
            // ensure_managed_memory_dir): never follow a link out of the Agent
            // Home — e.g. one planted between attach and start.
            if std::fs::symlink_metadata(dir)
                .map(|m| m.is_symlink())
                .unwrap_or(false)
            {
                return Err(EngineError::Log {
                    name: name.as_str().to_string(),
                    path: dir.to_string_lossy().into_owned(),
                    detail: "the managed memory directory path is a symlink; refusing to \
                             follow it"
                        .to_string(),
                });
            }
            std::fs::create_dir_all(dir).map_err(|e| EngineError::Log {
                name: name.as_str().to_string(),
                path: dir.to_string_lossy().into_owned(),
                detail: format!("could not ensure the managed memory directory: {e}"),
            })?;
        }

        // (2b-observed) ENGINE-OBSERVED metering (story 3-4, AC-A/AC6): for an
        // `engine-observed` instance, START the loopback forward listener HERE
        // (before the mapping application + the `starting` transition, so a listener
        // failure rejects the start cleanly with NO state change — mirroring the
        // secret/snapshot failures), then INJECT its loopback `http://127.0.0.1:<port>`
        // address as a `metering.base_url` INVOCATION-OVERRIDE so the adapter's
        // EXISTING config-mapping (2-2) delivers it into the agent's native mechanism
        // (e.g. env `OPENAI_BASE_URL`). The address is ENGINE-computed (the engine is
        // the sole authority — AC-B); the adapter merely receives it. A `self-reported`
        // instance leaves `observed_listener` None and its start path UNCHANGED. The
        // held listener is moved into `Supervised` on success; on any later start
        // failure its `Drop` aborts the accept-loop task (RAII teardown, no leak).
        let observed_listener =
            self.start_observed_listener(&name, &metering_source, &effective)?;
        // The effective config the MAPPING applies: for an observed instance it
        // carries the engine-injected loopback base_url (story 3-4), and for a
        // filesystem-backed instance the engine-computed managed memory dir at the
        // reserved `memory.dir` key (story 5-1) — both INVOCATION overrides (the
        // strongest layer, AD-9), so a hand-set lower-layer value cannot win. The
        // SNAPSHOT (2c) below stays on the plain `effective` (the operator config),
        // so NEITHER injected value is persisted as "what applied" — honest
        // provenance (3-4's rule; 5-1's CORRECTION extends it: `memory.dir` is a
        // delivery mechanism, not operator configuration).
        let mut mapping_effective = match invocation_overrides(
            observed_listener.as_ref().map(ObservedListener::base_url),
            memory_dir.as_deref(),
        ) {
            Some(layer) => registry
                .effective_config(&name, layer)
                .map_err(|e| config_to_engine(&name, e))?,
            None => effective.clone(),
        };
        // (2b-memory-spoof, the override branch — story 11-3, A1) The re-fold
        // above re-derives the config from EVERY layer, so a hand-set reserved
        // key RESURRECTS here: the base strip above removed it from
        // `effective`, but this fresh fold never saw that strip. A hand-set
        // `memory.dir` must not ride the re-fold into the mapping application.
        // When the engine injected NO managed dir (no filesystem backing), any
        // `memory.dir` in the fold is exactly such a hand-set resurrection —
        // strip it, exactly like the base path. When the engine DID inject the
        // dir, its own override leaf is the fold's winner (the invocation layer
        // is the strongest, AD-9) and MUST survive the strip — a hand-set
        // lower-layer value cannot beat it, so stripping there would only
        // break the engine's own delivery.
        if memory_dir.is_none() {
            let _ = mapping_effective.remove(super::config::MEMORY_DIR_KEY);
        }

        // (2b-memory-delivery) DC-10 honesty (AD-11 Delivery clause): when a
        // `filesystem` backing is attached but the resolved mapping declares NO
        // target for the reserved key, say so ONCE through the diagnostic
        // emission (AD-12: stderr by default, the host's story-10-2 sink when
        // installed) — naming the instance, the managed path, and the fact
        // that the agent will not receive it. The start still SUCCEEDS: the
        // directory guarantee holds regardless, and refusing an
        // otherwise-healthy agent because its adapter maps no memory key would
        // be a regression. Deliberately NOT generalized to other unmapped
        // keys (story 2-2 Decision 6 stands; memory is special only because
        // the operator took an explicit attach action and is owed the truth
        // about its effect). Pure decision fn (unit-tested); this is the only
        // emission site, routed through `emit_diagnostic` (story 10-2).
        if let Some(notice) = memory_delivery_notice(memory_dir.as_deref(), &mapping, &name) {
            self.emit_diagnostic(&notice);
        }

        // (2b-secret) Resolve every `secret:NAME` leaf into a SecretString BEFORE
        // the mapping application (story 2-4, spine AD-10, AC-A/AC9). This is where
        // display and delivery DIVERGE: `effective`'s `display()`-based surfaces
        // (the snapshot at (2c), `config get`) stay MASKED, but the resolved
        // cleartext flows into `apply_config_mapping` so the ADAPTER gets a usable
        // key. Resolution (env → the 0600 secrets file) runs here, still before any
        // persisted state change, so an unresolved/ill-permissioned secret REJECTS
        // the start cleanly (no half-launch, mirroring the config-apply + snapshot
        // failures) — a typed `EngineError::Secret` that NEVER echoes a value.
        let secrets = registry
            .resolve_secrets(&mapping_effective)
            .map_err(|e| secret_to_engine(&name, e))?;
        // (AI-27 shadow capture) The pre-apply launch env, snapshotted BEFORE
        // the mapping application mutates `launch.env` (whatever the persisted
        // registration snapshot carried): an env-targeted mapping whose name
        // already exists here OVERWRITES that variable (documented precedence —
        // config wins, last-write-wins, untouched), and the value diff below
        // makes that shadow VISIBLE instead of silent (story 11-2).
        let base_env: std::collections::BTreeMap<String, String> = launch.env.clone();
        let mapping_report = adapter::apply_config_mapping(
            &mut launch,
            &mapping,
            &mapping_effective,
            &secrets,
            &home,
        )
        .map_err(|e| config_apply_to_engine(&name, e))?;

        // (AI-27 shadow visibility) ONE diagnostic naming every launch env var
        // the config mapping overwrote — deliberately NOT worded "base-launch":
        // the pre-apply `launch.env` is whatever the persisted registration
        // snapshot carried (the manifest `[lifecycle.start]` env or the
        // code-declared launch), a set this diagnostic must not over-claim. The
        // start still SUCCEEDS — the precedence is unchanged (the config value
        // won in `launch.env`) and has always been the documented behavior; only
        // the silence was the bug. Routed through `emit_diagnostic` (story 10-2:
        // stderr by default, the host's sink when installed), like the
        // memory-delivery notice above. The message is formatted into a local
        // first, exactly like that notice (the embed-clean audit pins the inline
        // `emit_diagnostic(&format!(` shape to the ONE breadcrumb route).
        let shadowed = shadowed_env_keys(&base_env, &launch.env);
        if !shadowed.is_empty() {
            let shadow_notice = format!(
                "{}: the config mapping overwrote launch environment variable(s) {} — \
                 the config value wins for this start (documented precedence). Rename the \
                 env target or unset the config key if the original value was intended.",
                name.as_str(),
                shadowed.join(", "),
            );
            self.emit_diagnostic(&shadow_notice);
        }

        // (AI-39 runtime) ONE warn-only diagnostic naming the config keys whose
        // resolved SECRET cleartext was delivered into FLAG targets — i.e. the
        // keys now on the process argv, world-readable cross-user. This is the
        // runtime half of the steering the set-time warning starts (story 11-2):
        // the argv boundary itself stays ACCEPTED (documented; no rejection
        // semantics were ever ratified) — the diagnostic rides the same
        // `emit_diagnostic` channel as the notices above, so operators (and the
        // existing audit trail) see it on every start that delivers one. The
        // message is formatted into a local first (see the shadow notice above).
        if !mapping_report.secret_flag_keys.is_empty() {
            let flag_notice = format!(
                "{}: secret-carrying config key(s) [{}] resolve into FLAG targets — their \
                 cleartext is passed on the agent's command line, where argv is readable \
                 by other local users (ps, /proc/<pid>/cmdline). Prefer an env or file \
                 target for these keys.",
                name.as_str(),
                mapping_report.secret_flag_keys.join(", "),
            );
            self.emit_diagnostic(&flag_notice);
        }

        // (2c) Persist the effective-config snapshot into the Agent Home (story
        // 2-3, spine AD-9 "start resolves to an EffectiveConfig snapshot persisted
        // in the Agent Home, every value tagged with its source layer" + AD-6
        // "effective-config snapshots are files inside the Agent Home"). The
        // resolved `effective` is already in hand from (2b); write it HERE, right
        // after the mapping application and BEFORE the `starting` transition below,
        // so a snapshot-write failure rejects the start cleanly (NO state change —
        // exactly mirroring how the config-apply failure at (2b) rejects before the
        // transition). The snapshot is a PROMISED AD-9 artifact (a Host/debugging
        // record of "what will apply on next start"), not a best-effort nicety, so
        // its failure is a typed start error. Because RESTART also flows through
        // this path (story 1-6), the snapshot is refreshed on restart too (AC7:
        // OVERWRITTEN every successful start/restart, never a stale resolution). It
        // is NOT written at registration (there is no "effective at start" until a
        // start happens) and NOT deleted at stop.
        registry
            .write_effective_config_snapshot(&name, &effective)
            .map_err(snapshot_to_engine)?;

        // Read the per-instance Restart Policy so the write-ahead record carries
        // it (AD-15 per-instance configurable). Read once, before any side effect.
        let policy = registry
            .effective_restart_policy(&name)
            .map_err(registry_to_engine)?;

        // The spawned agent's stdout/stderr go to a SEPARATE agent.log, never the
        // engine's JSON-Lines transition-event log (instance.log) — otherwise the
        // agent's plain-text output would corrupt the structured event log.
        let agent_log_path = registry.agent_output_log_path(&name);
        // Ensure the log directory exists (AD-12 seed) so spawn can redirect
        // stdout/stderr into it and we can append transition events.
        self.ensure_log_dir(registry, &name)?;

        // Anchor the usage-ingestion cursor at the agent-output log length BEFORE
        // the spawn (story 3-1). This Run's own output is appended AFTER this point,
        // so ingestion reads ALL of it — while a PRIOR Run's already-captured lines
        // (a stop→start reuses the same append-only agent.log) stay BEHIND the cursor
        // and are never re-ingested under this fresh Run id. Capturing it HERE (not
        // after the readiness watch below) is essential: a fast agent emits its first
        // usage lines within the ~300ms readiness window, so a cursor set post-
        // readiness would skip them — the ingestion bug this prevents.
        let usage_cursor = self.agent_log_len(registry, &name);

        // (3) registered/stopped/failed → starting.
        self.transition(
            registry,
            &name,
            instance.state,
            starting,
            TransitionCause::command(LifecycleCommand::Start.as_str()),
        )?;

        let spec = SpawnSpec {
            exec: launch.exec.clone(),
            args: launch.args,
            env: launch.env,
            working_dir: home,
            log_file: Some(agent_log_path),
            // Story 4-2 (AC-E): capture is unconditional, computed from the
            // SAME Registry path authority as `log_file` (never gated on
            // `pipe_stdin`/`Capability::Interaction` — that gate governs only
            // the stdin *write* direction).
            attributed_log_path: Some(registry.attributed_output_log_path(&name)),
            // Fix pass (review of #80): the crash-immune raw STDERR capture,
            // computed from the SAME path authority, paired 1:1:1 with
            // `log_file`/`attributed_log_path` (all three Some together).
            stderr_log_file: Some(registry.agent_stderr_log_path(&name)),
            instance_name: name.as_str().to_string(),
            // Story 12-1: a DETACHED spawn is never given a stdin pipe — the
            // pipe's write end would be held by this soon-to-exit CLI, and a
            // child writing to (or a later engine sending into) a dead peer's
            // pipe is an EPIPE strand. `Stdio::null()` gives the child an
            // immediate, honest EOF instead; after re-adoption `send` fails
            // with the ordinary adopted-instance interaction error (an adopted
            // handle has no recoverable pipe either), which is exactly the
            // behavior the spec's I/O matrix pins.
            pipe_stdin: pipe_stdin && !detach,
            // The spawn-time disarm flag (see the port's `SpawnSpec::detach`
            // docs): the Unix handle skips its Drop killpg; the Windows spawn
            // never creates/assigns the kill-on-close Job Object.
            detach,
        };

        // (4) Spawn. A spawn failure lands the instance in `failed` with the
        // diagnostic preserved and no zombie (the backend spawned nothing / reaps).
        let mut handle = match self.backend.spawn(&spec) {
            Ok(handle) => handle,
            Err(err) => return Err(self.fail_launch(registry, &name, &err)),
        };

        // (5) Readiness watch: a process that dies immediately (especially
        // non-zero) during startup is a launch failure (AC2). Watch briefly;
        // `watch_startup` returns the exit code (if it died) or `None` (ready).
        if let Some(exit_code) = self.watch_startup(&mut handle) {
            let detail = match exit_code {
                Some(c) => format!("exited immediately during startup with code {c}"),
                None => "exited immediately during startup".to_string(),
            };
            // Reap already done by poll; nothing survives.
            return Err(self.fail_launch_detail(registry, &name, detail));
        }

        // (6) Commit the write-ahead spawn record (AD-5) BEFORE the instance is
        // treated as supervised — "no spawn without its record committed first".
        // A fresh start resets the restart count to 0; a restart keeps its
        // attempt count. The fingerprint is the PID-reuse guard for later
        // orphan adoption. A record-commit failure fails the start (leaving the
        // instance `failed`) — we must not run an unrecorded supervised process.
        let restart_count = restart.map(|(attempt, _)| attempt).unwrap_or(0);
        let record = SpawnRecord {
            name: name.clone(),
            fingerprint: self.backend.fingerprint(&handle),
            restart_policy: policy,
            restart_count,
            last_known_cause: None,
            // Story 12-1 AMENDMENT (review loop 1): detached-ness RIDES THE
            // RECORD — it is a cross-lifetime property, so every later
            // `adopt_orphans` reads it and re-holds the handle DISARMED. This
            // is the durable-detach promise across N commands, not just the
            // first.
            detach,
        };
        if let Err(e) = registry.write_spawn_record(&record) {
            // Persisting the record failed: kill the just-spawned process and
            // land the instance in `failed` so we never leave an unrecorded
            // process behind (AD-5 safety).
            //
            // Review round 2: `drop(handle)` alone only tears the process down
            // when the handle is ATTACHED (drop = group/job kill). A DETACHED
            // handle is DISARMED — its Drop deliberately kills nothing — so a
            // detached start whose record commit fails would LEAK a live,
            // unrecorded process. The teardown is therefore explicit on the
            // detach path: a short graceful window through the backend stop
            // (which still works on a detached handle — the pgid/job is
            // unchanged), then the drop releases whatever remains.
            //
            // AUDIT of the other post-spawn error paths in this function (why
            // this is the ONLY fix site): between `spawn` and this commit the
            // sole fallible step is `watch_startup`, whose failure branch runs
            // only when the process ALREADY exited (reaped by the poll —
            // nothing survives, detached or not); every `?` after the commit
            // happens once the record is DURABLE, so a dropped detached
            // handle there leaves a recorded (adoptable) process, not an
            // unrecorded one.
            if detach {
                let _ = self.backend.stop(&mut handle, Duration::from_millis(500));
            }
            drop(handle);
            return Err(self.fail_launch_detail(
                registry,
                &name,
                format!("could not commit the write-ahead spawn record: {e}"),
            ));
        }

        // (7) starting → running (adapter ready, or a Restart Policy restart).
        let ready_cause = match restart {
            Some((attempt, waited)) => {
                TransitionCause::restarted(attempt, waited.as_millis() as u64)
            }
            None => TransitionCause::AdapterReady,
        };
        // Story 4-2, Task 4: `handle` already exists (spawned above) and
        // carries a live `log_capture` (capture is unconditional, AC-E), but
        // it is not YET in `self.running` (inserted below) — so the default
        // `self.transition(...)`'s `self.running`-based lookup would miss
        // it. Pass the capture explicitly so the `starting → running` line
        // lands in the attributed capture too.
        self.transition_with_log_capture(
            registry,
            &name,
            starting,
            LifecycleState::Running,
            ready_cause,
            self.backend.log_capture(&handle),
        )?;
        // Mint the fresh Run id for this `starting`→terminal span (spine AD-7). Each
        // `starting` — operator start OR restart (story 1-6) — mints a distinct id
        // (AC-B), so a restarted instance opens a NEW Run whose per-run totals never
        // bleed in the previous Run's usage. The ingestion cursor was anchored at the
        // pre-spawn log length (above), so this Run ingests all of its own output.
        let run_id = RunId::mint();
        // Story 3-4: an `engine-observed` instance holds its listener + a fresh
        // per-Run observed `sequence` minter (built here with the just-minted
        // run_id, so the ordinal resets per Run — the AD-7 Run boundary + the dedup
        // invariant). A `self-reported` instance leaves both `None` (its log-tail
        // drain is unchanged).
        let observed_source = observed_listener
            .as_ref()
            .map(|_| ObservedUsageSource::new());
        // A fresh Run starts from a ZERO poll-error streak (AI-12): a stale entry
        // from this name's PRIOR handle must never pre-load the new one.
        self.clear_poll_error_streak(&name);
        self.running.insert(
            name.clone(),
            Supervised {
                handle,
                run_id,
                metering_source,
                usage_cursor,
                usage_park_attempts: None,
                // Story 12-4: a fresh Run starts with no parked observed events.
                observed_park: None,
                // A fresh Run starts with an EMPTY breach latch (story 3-2): the
                // run_id was just minted, so no scope has fired for it yet. This is
                // how the latch RESETS per Run — a persistently-over-cumulative agent
                // that stops and starts again gets a new Run + a clean latch, so it
                // can fire one cumulative breach in the new Run too.
                breached_scopes: std::collections::HashSet::new(),
                observed_listener,
                observed_source,
                // A fresh start's stop attempt has not happened yet.
                stop_unconfirmed: false,
                // A fresh start (operator or restart) spawned this process itself.
                adopted: false,
            },
        );

        registry.lookup(&name).map_err(registry_to_engine)
    }

    /// Perform ONE Restart Policy restart of a crashed instance (story 1-6, AC4).
    ///
    /// Called by the engine cadence AFTER it has waited the backoff
    /// [`RestartPlan::delay`]. Re-runs the start path (`failed → starting →
    /// running`) recording a [`TransitionCause::Restarted`] with the consecutive
    /// `attempt` + the `waited` backoff, and keeps the persisted restart count at
    /// `attempt`.
    ///
    /// Interaction with a concurrent `stop`: `restart` re-runs the start path, so
    /// its transition gate is `next_state(state, Start)`. That gate only accepts
    /// `failed` (or registered/stopped); if the instance was already restarted to
    /// `running` by an EARLIER plan, or an operator stopped it back to `stopped`,
    /// the gate rejects and this restart is a harmless no-op. NOTE: during the
    /// backoff WINDOW the instance is `failed`, and `next_state(Failed, Stop)` is
    /// an `InvalidTransition` — so an operator cannot `stop` a mid-backoff
    /// instance to pre-empt this restart (there is no `failed → stopping` edge
    /// this story; adding one is out of scope). The restart therefore proceeds; a
    /// stop is only effective once the instance is `running` again.
    pub fn restart(
        &mut self,
        registry: &Registry,
        name: &str,
        attempt: u32,
        waited: Duration,
    ) -> Result<AgentInstance, EngineError> {
        // Review round 2: a RESTART of a DETACHED agent must stay detached.
        // The write-ahead record survives the crash (only a clean stop clears
        // it), so it is the durable source of the spawn's detached-ness — read
        // it BEFORE `start_inner` rewrites the record. Dropping the flag here
        // (the pre-patch hardcoded `false`) would silently restart the agent
        // ATTACHED and downgrade the record, killing the agent at this
        // engine's clean exit — the exact surprise `--detach` promises away.
        // A missing/unreadable record falls back to the supervised (attached)
        // default, matching how the pre-12-1 records read.
        let instance_name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let detach = registry
            .spawn_record(&instance_name)
            .ok()
            .flatten()
            .map(|record| record.detach)
            .unwrap_or(false);
        self.start_inner(registry, name, Some((attempt, waited)), detach)
    }

    /// Stop a running Agent Instance (AC3/AC4).
    ///
    /// Transitions `running → stopping`, requests graceful shutdown via the
    /// backend and escalates to a forced kill after `window` (default
    /// [`DEFAULT_STOP_WINDOW`]) if needed, records the escalation in the instance
    /// log, then `stopping → stopped`. No process of the instance survives (the
    /// backend kills the whole group/job) — in the NORMAL case.
    ///
    /// **Bounded death confirmation (fix pass, review of #80 follow-up — the
    /// CRITICAL finding):** after escalating to a forced kill, the backend
    /// CONFIRMS death bounded to [`crate::ports::KILL_CONFIRM_TIMEOUT`] (see
    /// its docs for the mechanism: a fast writer can exhaust disk and enter
    /// an OS-level uninterruptible I/O wait immune to every signal, including
    /// the one just sent). If confirmation is not reached within that bound,
    /// this returns [`EngineError::StopUnconfirmed`] instead of continuing to
    /// block — the instance stays `stopping` (never a false `stopped`), and
    /// the handle is RETAINED (not dropped) so the situation can be
    /// reconciled later.
    ///
    /// **No compounding on retry:** a SUBSEQUENT `stop` call against an
    /// instance still `stopping` with a retained (unconfirmed) handle does
    /// NOT re-run the whole SIGTERM/graceful-window/SIGKILL/confirm sequence
    /// — it performs a single cheap, NON-BLOCKING liveness poll instead
    /// (`ProcessBackend::poll`, never `ProcessBackend::stop`). If the process
    /// has since actually exited (the OS condition cleared), this
    /// SELF-HEALS: it completes the stuck `stopping → stopped` transition
    /// right here. If it is still alive, this fails fast with the SAME
    /// honest [`EngineError::StopUnconfirmed`], with no new signal and no new
    /// wait. (The crash-detection reaper's own poll, `poll_once`, performs
    /// the identical reconciliation if it observes the exit first — whichever
    /// happens first, the row does not stay permanently stuck.)
    pub fn stop(
        &mut self,
        registry: &Registry,
        name: &str,
        window: Option<Duration>,
    ) -> Result<AgentInstance, EngineError> {
        self.stop_inner(registry, name, window, None)
    }

    /// Stop driven by a budget BREACH (story 3-2). Identical to [`Supervisor::stop`]
    /// (graceful → forced escalation, story 1-4) except the `running → stopping`
    /// edge carries the [`TransitionCause::BudgetExceeded`] cause instead of a plain
    /// `stop` command, so the lifecycle log explains WHY. The terminal
    /// `stopping → stopped` edge keeps its graceful/forced cause (the escalation
    /// detail). Takes `&InstanceName` (the caller already validated it).
    fn stop_with_cause(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        cause: TransitionCause,
    ) -> Result<AgentInstance, EngineError> {
        self.stop_inner(registry, name.as_str(), None, Some(cause))
    }

    /// The shared stop driver (story 1-4 + story 3-2 cause override).
    ///
    /// `cause_override`: when `Some`, replaces the `running → stopping` cause
    /// (a budget stop records `BudgetExceeded`); `None` uses the plain `stop`
    /// command cause (an operator `kt agent stop` is unchanged). The terminal edge
    /// always records the graceful/forced escalation cause regardless.
    fn stop_inner(
        &mut self,
        registry: &Registry,
        name: &str,
        window: Option<Duration>,
        cause_override: Option<TransitionCause>,
    ) -> Result<AgentInstance, EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = registry.lookup(&name).map_err(registry_to_engine)?;

        // Fix pass (review of #80 follow-up — the CRITICAL finding): a RETRY
        // `stop` against an instance already `stopping` whose handle is
        // marked `stop_unconfirmed` means a PRIOR pass through THIS function
        // already sent SIGKILL but could not confirm death within
        // KILL_CONFIRM_TIMEOUT (`EngineError::StopUnconfirmed`) — most likely
        // because the process is stuck in an OS-level uninterruptible I/O
        // wait. The transition gate below (`next_state`) has no
        // `(Stopping, Stop)` row, so an unmodified retry would either reject
        // with a generic, non-self-healing `InvalidTransition`, or (if that
        // gate were bypassed) re-run the WHOLE SIGTERM/graceful-window/
        // SIGKILL/confirm sequence for an outcome we can already suspect is
        // unchanged — exactly the compounding wait this fix pass closes.
        // Instead: a single cheap, NON-BLOCKING poll (`ProcessBackend::poll`,
        // never `ProcessBackend::stop`) decides the outcome — self-heals if
        // the process has since actually died (the OS condition cleared), or
        // fails fast with the SAME honest error if it is still alive, with NO
        // new signal and NO new wait. Gated specifically on `stop_unconfirmed`
        // (not merely "state is `stopping`") so this new branch changes
        // behavior ONLY for the scenario it targets — an externally-forced
        // `stopping` row with no real stop attempt behind it (as
        // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`
        // exercises) takes the ORIGINAL, unchanged path below.
        if instance.state == LifecycleState::Stopping {
            let stuck = match self.running.get_mut(&name) {
                Some(supervised) if supervised.stop_unconfirmed => {
                    let status = self
                        .backend
                        .poll(&mut supervised.handle)
                        .map_err(|source| EngineError::Backend {
                            name: name.as_str().to_string(),
                            source,
                        })?;
                    let log_capture = self.backend.log_capture(&supervised.handle);
                    Some((status, log_capture))
                }
                _ => None,
            };
            if let Some((status, log_capture)) = stuck {
                if !status.is_exited() {
                    // Still stuck: fail fast, honestly, with no new blocking.
                    return Err(EngineError::StopUnconfirmed {
                        name: name.as_str().to_string(),
                        timeout_secs: KILL_CONFIRM_TIMEOUT.as_secs(),
                    });
                }
                // Self-healing: the process has now actually exited (the OS
                // condition that made confirmation time out has cleared).
                // Complete the stuck `stopping -> stopped` transition exactly
                // as the ordinary path below would have on confirmed death.
                self.clear_poll_error_streak(&name);
                self.running.remove(&name);
                registry
                    .clear_spawn_record(&name)
                    .map_err(registry_to_engine)?;
                self.transition_with_log_capture(
                    registry,
                    &name,
                    LifecycleState::Stopping,
                    LifecycleState::Stopped,
                    TransitionCause::stop_forced(
                        "SIGKILL was sent by an earlier stop attempt; the process's death was \
                         confirmed on a later reconciliation (it may have been stuck in an \
                         OS-level I/O wait that has since cleared)",
                    ),
                    log_capture,
                )?;
                return registry.lookup(&name).map_err(registry_to_engine);
            }
        }

        // Transition gate (AC4): stop on stopped / registered / … rejects here
        // with the uniform InvalidTransition, before touching any process.
        let stopping = next_state(instance.state, LifecycleCommand::Stop)?;

        let window = window.unwrap_or(DEFAULT_STOP_WINDOW);
        self.ensure_log_dir(registry, &name)?;

        // running → stopping (a story-3-2 budget stop overrides the cause).
        self.transition(
            registry,
            &name,
            instance.state,
            stopping,
            cause_override
                .unwrap_or_else(|| TransitionCause::command(LifecycleCommand::Stop.as_str())),
        )?;

        // Drain any final self-reported usage the agent emitted before the stop, so
        // the last batch of a Run is not lost to the race between "agent printed it"
        // and "we killed the process" (story 3-1). TERMINAL drain: the process is
        // about to be gone, so a final newline-less usage line is consumed to
        // end-of-log rather than stranded (H1). Best-effort — a drain hiccup never
        // blocks the stop.
        self.drain_usage_for(registry, &name, DrainMode::Terminal);
        // Drain any final ENGINE-OBSERVED usage still queued before the listener is
        // torn down (story 3-4): a completion the proxy parsed just before the stop
        // must land, not be lost when the `Supervised` (and its listener) is dropped
        // below. TERMINAL mode (story 12-4): a commit failure here announces the
        // loss — there is no next pass — and any parked buffer dies with the
        // instance.
        self.drain_observed_for(registry, &name, DrainMode::Terminal);

        // Ask the backend to stop the process (group/job). If we have no handle
        // for it (the row says running but this engine holds no handle AND orphan
        // adoption found no live process), the desired end state "no process of
        // the instance survives" already holds, so we treat it as a graceful
        // stop. With story 1-6 adoption, a handle for a still-live process
        // started by a PRIOR engine IS re-held (via `adopt_orphans`), so a
        // cross-restart stop now really terminates it.
        // Story 4-2, Task 4 (fix pass, review of #80): capture the
        // log_capture HERE, before `running.remove` drops the handle below
        // — the default `self.transition(...)` lookup (via `self.running`)
        // would find NOTHING by the time the terminal transition below
        // runs. By the time `backend.stop` (below) returns, the process is
        // provably dead (its raw capture files can never grow again), and
        // `send_engine_line`'s inline catch-up folds in every remaining
        // byte of agent output BEFORE the "-> stopped" line, so the engine
        // line still lands correctly ordered after it, regardless of
        // whether the process handle's `Drop` (which also signals the
        // background tailer thread to stop) has run yet.
        let (outcome, log_capture) = match self.running.get_mut(&name) {
            Some(supervised) => {
                let log_capture = self.backend.log_capture(&supervised.handle);
                let outcome = self.backend.stop(&mut supervised.handle, window).map_err(
                    |source| match source {
                        // Fix pass (review of #80 follow-up — the CRITICAL
                        // finding): mark the handle so a RETRY `stop` (or the
                        // crash reaper's own poll) recognizes this EXACT
                        // scenario and reconciles it with a cheap,
                        // non-blocking poll instead of re-running the whole
                        // bounded SIGTERM/SIGKILL/confirm sequence (see
                        // `stop_inner`'s retry-branch docs above and
                        // `poll_once`'s docs). This `?` skips
                        // `self.running.remove` below, so the handle is
                        // RETAINED, never silently dropped — the instance
                        // stays `stopping` (the terminal transition below is
                        // never reached), an honest, non-terminal state.
                        BackendError::StopUnconfirmed { timeout_secs } => {
                            supervised.stop_unconfirmed = true;
                            EngineError::StopUnconfirmed {
                                name: name.as_str().to_string(),
                                timeout_secs,
                            }
                        }
                        other => EngineError::Backend {
                            name: name.as_str().to_string(),
                            source: other,
                        },
                    },
                )?;
                (outcome, log_capture)
            }
            None => (crate::ports::StopOutcome { forced: false }, None),
        };
        // AI-63 follow-on (billing under-count at STOP, owner-approved): a FINAL
        // TERMINAL rescue drain AFTER `backend.stop` has CONFIRMED the process
        // dead and BEFORE `self.running.remove` below drops the handle + cursor.
        //
        // The pre-kill drain (above, before `backend.stop`) runs while the agent is
        // still ALIVE, so a usage line the agent flushes in the window between that
        // drain and the kill would otherwise be stranded: the cursor never advances
        // past it and the handle is removed right after with no further drain (a
        // permanent UNDER-count). Reaching HERE means death is CONFIRMED — the
        // `BackendError::StopUnconfirmed` case `?`-returned ABOVE without removing
        // the handle (it is retained for later reconciliation), so this rescue drain
        // NEVER runs on a still-live process. With the process provably dead,
        // `agent.log` is STABLE (it can never grow again), so this Terminal drain
        // reads it to its now-final EOF, capturing exactly the tail the pre-kill
        // drain missed — with NO unbounded-growth / under-lock-stall concern (the
        // file is finite and final).
        //
        // No double-count: `drain_usage_for` is cursor-based — the pre-kill drain
        // advanced `usage_cursor` to what it consumed, so this pass ingests ONLY
        // bytes that arrived AFTER it (the two drains are disjoint by cursor; the DB
        // dedup is a backstop, not the primary guard). The `Supervised` entry MUST
        // still be in `self.running` for its cursor/run_id/metering_source to be
        // read — hence strictly BEFORE `self.running.remove`. This mirrors the crash
        // reaper's proven drain-AFTER-observed-exit (see `poll_once`). Best-effort,
        // like the pre-kill drain — a drain hiccup never blocks the stop.
        self.drain_usage_for(registry, &name, DrainMode::Terminal);
        // Drop the handle (also closes the Job / releases the child on Windows) and
        // the Run's metering context — the Run ends at this terminal transition.
        self.clear_poll_error_streak(&name);
        self.running.remove(&name);

        // Clear the write-ahead spawn record (AD-5): a cleanly-stopped instance
        // must NOT be later adopted or reconciled-to-failed as an orphan. Cleared
        // BEFORE the terminal transition so the durable record leads the state.
        registry
            .clear_spawn_record(&name)
            .map_err(registry_to_engine)?;

        // stopping → stopped, recording whether escalation happened (AC3).
        let cause = if outcome.forced {
            TransitionCause::stop_forced(format!(
                "graceful window ({}s) elapsed; escalated to a forced kill of the process group/job",
                window.as_secs()
            ))
        } else {
            TransitionCause::StopGraceful
        };
        self.transition_with_log_capture(
            registry,
            &name,
            stopping,
            LifecycleState::Stopped,
            cause,
            log_capture,
        )?;

        registry.lookup(&name).map_err(registry_to_engine)
    }

    /// Pause a running Agent Instance with honest, per-OS semantics (story 1-5,
    /// AC1/AC2/AC3/AC5 — the "surfaced not silent" HONESTY command).
    ///
    /// Order mirrors [`Supervisor::stop`] — including the persist-FIRST ordering
    /// (AI-9: the transition commits before the signal, so a persist failure can
    /// never leave the process suspended while the ledger says otherwise) —
    /// except the middle step DISPATCHES on the effective (current-OS) pause
    /// `SupportLevel` read from the persisted snapshot (AC5), rather than always
    /// calling the backend:
    /// 1. name → [`InstanceName`]; look up the instance,
    /// 2. transition gate `next_state(state, Pause)?` — an invalid transition
    ///    (e.g. pause on `stopped`/`paused`) rejects HERE with the uniform
    ///    [`LifecycleError::InvalidTransition`] (AC4), before any side effect or
    ///    level read,
    /// 3. read the effective pause level (AC5) and dispatch:
    ///    * **Guaranteed** → persist `running→paused` + a plain
    ///      [`TransitionCause::Command`] (`"pause"`) — no qualifier — THEN
    ///      `backend.pause(handle)` (real SIGSTOP suspension on Unix). AI-8: when
    ///      NO in-memory handle is held (nothing can be signalled), the recorded
    ///      cause is the honest [`TransitionCause::PauseBestEffort`] qualifier
    ///      naming the missing handle instead of a plain command that would read
    ///      as a real suspension,
    ///    * **BestEffort** → persist `running→paused` + a
    ///      [`TransitionCause::PauseBestEffort`] qualifier (the machine-readable
    ///      half of "surfaced not silent"); the process may keep running,
    ///    * **Unsupported** → FAIL FAST with
    ///      [`EngineError::CapabilityUnsupported`], NO transition, NO backend
    ///      call, NOTHING persisted (AC3).
    pub fn pause(&mut self, registry: &Registry, name: &str) -> Result<AgentInstance, EngineError> {
        self.suspend_or_resume(registry, name, LifecycleCommand::Pause, None)
    }

    /// Pause driven by a budget BREACH (story 3-2 AC6). Identical to
    /// [`Supervisor::pause`] — honoring the adapter pause Capability Declaration
    /// EXACTLY (guaranteed suspends; best-effort transitions with the honest
    /// posture; UNSUPPORTED fails fast, NO fake pause, NO silent escalation) —
    /// except the resulting `running → paused` transition carries the
    /// [`TransitionCause::BudgetExceeded`] cause instead of a plain `pause` command,
    /// so the lifecycle log itself explains WHY (the standalone breach event is the
    /// AD-14 subscription payload). Takes `&InstanceName` (the caller already has
    /// the validated name inside the ingestion path).
    fn pause_with_cause(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        cause: TransitionCause,
    ) -> Result<AgentInstance, EngineError> {
        self.suspend_or_resume(
            registry,
            name.as_str(),
            LifecycleCommand::Pause,
            Some(cause),
        )
    }

    /// Resume a paused Agent Instance (story 1-5, AC1/AC2).
    ///
    /// The symmetric counterpart of [`Supervisor::pause`]: the transition gate is
    /// `next_state(state, Resume)?` (`paused → running`; anything else rejects
    /// with the uniform invalid-transition, AC4), and the dispatch is on the same
    /// effective pause level:
    /// * **Guaranteed** → `backend.resume(handle)` (SIGCONT), then `paused→running`
    ///   + a plain `resume` command cause,
    /// * **BestEffort** → `paused→running` + a [`TransitionCause::ResumeBestEffort`]
    ///   qualifier,
    /// * **Unsupported** → fail fast with the DEDICATED
    ///   [`EngineError::ResumeUnsupported`] (AI-7 — NOT the bare pause-unsupported
    ///   error): the instance is already `paused` (the gate above guarantees it),
    ///   so a diagnostic that merely says "pause is unsupported" would strand the
    ///   operator with no way forward. The dedicated variant names the state + the
    ///   adapter's pause declaration and gives the escape hatch — `stop` works
    ///   without pause support (it never consults the pause level), so
    ///   `stop` + `start` is a real recovery, and a `resume` on an OS where the
    ///   declaration supports pause works too. NO state change, NO signal, NO
    ///   fake success. Not normally reachable within one declaration (a `paused`
    ///   row implies pause was allowed at some point), but real via
    ///   declaration/OS drift between the pause and the resume.
    pub fn resume(
        &mut self,
        registry: &Registry,
        name: &str,
    ) -> Result<AgentInstance, EngineError> {
        self.suspend_or_resume(registry, name, LifecycleCommand::Resume, None)
    }

    /// Shared pause/resume driver (the three-level dispatch), keyed on `command`
    /// (`Pause` or `Resume`). Kept as one method so the pause and resume paths
    /// cannot drift: the transition gate, the level read, and the three-way
    /// dispatch are identical; only the target state and the cause differ.
    ///
    /// `cause_override` (story 3-2): when `Some`, it REPLACES the default cause on
    /// the resulting transition for the GUARANTEED + BEST-EFFORT paths — a
    /// budget-driven pause records [`TransitionCause::BudgetExceeded`] instead of a
    /// plain `pause` command / a best-effort qualifier, so the lifecycle log
    /// explains WHY. `None` preserves the story-1-5 causes exactly (an operator
    /// `kt agent pause` is unchanged). The UNSUPPORTED fail-fast is identical
    /// regardless (no transition, nothing persisted — the override is moot).
    fn suspend_or_resume(
        &mut self,
        registry: &Registry,
        name: &str,
        command: LifecycleCommand,
        cause_override: Option<TransitionCause>,
    ) -> Result<AgentInstance, EngineError> {
        debug_assert!(
            matches!(command, LifecycleCommand::Pause | LifecycleCommand::Resume),
            "suspend_or_resume only handles Pause/Resume"
        );
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = registry.lookup(&name).map_err(registry_to_engine)?;

        // (1) Transition gate (AC4): pause on stopped/paused, resume on running,
        // etc. reject HERE with the uniform InvalidTransition, before any level
        // read or side effect.
        let new_state = next_state(instance.state, command)?;

        // (2) Read the effective (current-OS) pause level from the persisted
        // snapshot (AC5). Projected at read time onto OsId::current(); NOT
        // re-derived from the manifest, NOT frozen at register time.
        let level = registry
            .effective_support(&name, Capability::Pause)
            .map_err(registry_to_engine)?;
        let os = OsId::current();

        // (3) Dispatch on the level.
        match level {
            // FAIL FAST (AC3): no transition, no backend call, nothing persisted.
            // AI-7: a RESUME under an Unsupported PAUSE declaration gets its OWN
            // diagnostic (not the bare pause-unsupported error): the instance is
            // already `paused` (the transition gate above guarantees it), so
            // telling the operator "cannot pause" strands them. The error names
            // the state + the declaration and gives the path forward (stop works
            // without pause support). The PAUSE arm keeps the original
            // CapabilityUnsupported fail-fast verbatim (AC3).
            SupportLevel::Unsupported if command == LifecycleCommand::Resume => {
                Err(EngineError::ResumeUnsupported {
                    name: name.as_str().to_string(),
                    os: os.as_str().to_string(),
                    level: level.as_str().to_string(),
                })
            }
            SupportLevel::Unsupported => Err(EngineError::CapabilityUnsupported {
                name: name.as_str().to_string(),
                capability: Capability::Pause.as_str().to_string(),
                os: os.as_str().to_string(),
                level: level.as_str().to_string(),
            }),
            // GUARANTEED (AC1): a real suspension via the backend, then a plain
            // command-cause transition (no qualifier — it is a true suspension). A
            // story-3-2 budget pause overrides the cause with BudgetExceeded.
            //
            // AI-8 + AI-9 (order mirrors `stop_inner`): the HONEST cause is
            // decided BEFORE anything is signalled or persisted — a guaranteed
            // command with no in-memory handle signals nothing, so its cause is
            // the best-effort qualifier naming the missing handle, never a plain
            // command that would read as a real suspension — and the transition
            // (persist + log) lands FIRST, then the signal, so a failed persist
            // can never leave the process suspended while the ledger says
            // otherwise (the durable state leads, exactly like stop).
            SupportLevel::Guaranteed => {
                self.ensure_log_dir(registry, &name)?;
                let has_handle = self.running.contains_key(&name);
                let cause = match (cause_override.clone(), has_handle) {
                    // The handle is held: a story-3-2 budget pause overrides the
                    // cause with BudgetExceeded; a plain command keeps its plain
                    // command cause (a true suspension).
                    (Some(cause), true) => cause,
                    (None, true) => TransitionCause::command(command.as_str()),
                    // AI-8 (loop 1): NOTHING is held to signal — record the
                    // honest best-effort posture with the reason (the missing
                    // handle), never a plain command that would read as a real
                    // suspension. A `Some(cause_override)` (e.g. BudgetExceeded)
                    // does NOT win here either: a budget pause that suspended
                    // nothing must not read as a performed suspension, so the
                    // override is WRAPPED as the qualifier's detail (the breach
                    // event itself already carries the budget record).
                    (override_cause, false) => {
                        let detail = match override_cause {
                            Some(cause) => format!(
                                "no live process handle is held in this engine session for \
                                 '{name}', so the guaranteed {} signalled nothing; the transition \
                                 is recorded best-effort — the requested override was:{}",
                                command.as_str(),
                                cause_suffix(&cause),
                            ),
                            None => format!(
                                "no live process handle is held in this engine session for \
                                 '{name}', so the guaranteed {} signalled nothing; the transition \
                                 is recorded best-effort",
                                command.as_str(),
                            ),
                        };
                        match command {
                            LifecycleCommand::Pause => TransitionCause::pause_best_effort(detail),
                            _ => TransitionCause::resume_best_effort(detail),
                        }
                    }
                };
                // AI-9: persist FIRST (the durable state leads; a transition
                // failure aborts BEFORE any signal, so the ledger can never claim
                // `paused` around a suspension that did not happen — nor the
                // reverse), THEN signal the held process.
                self.transition(registry, &name, instance.state, new_state, cause)?;
                // AI-9 (loop 1): the transition COMMITTED — if the signal now
                // fails, the ledger and the live process DIVERGE (the row says
                // paused/running while the process did not transition). The
                // divergence must never be silent (mirrors `stop_inner`'s
                // honesty): emit the breadcrumb naming instance + committed
                // state + signal error + the real recovery, then surface the
                // error as before.
                if let Err(err) = self.signal_backend(&name, command) {
                    // AI-9 (loop 2): the remediation must be budget-safe. When
                    // the failed pause was breach-driven, the per-Run breach
                    // latch is ALREADY spent — advising `resume` would leave an
                    // over-budget agent running for the rest of the Run with no
                    // re-enforcement — so that case recommends `stop` only.
                    let breach_driven =
                        matches!(cause_override, Some(TransitionCause::BudgetExceeded { .. }));
                    let remediation = match (command, breach_driven) {
                        // Row says `paused`, process still running: `resume`
                        // realigns the ledger (the SIGCONT is a harmless no-op
                        // on a running process); `stop` ends it — but for a
                        // breach-driven pause, `stop` is the ONLY safe advice.
                        (LifecycleCommand::Pause, true) => format!(
                            "kt agent stop {name} (the pause was budget-driven and the \
                             per-Run breach latch is spent — resuming would leave the \
                             over-budget run unenforced)"
                        ),
                        (LifecycleCommand::Pause, false) => {
                            format!(
                                "kt agent resume {name} to realign the ledger, or \
                                 kt agent stop {name} to end the instance"
                            )
                        }
                        // Row says `running`, process still suspended: only
                        // `stop` applies (a resume is now the invalid
                        // transition; stop's escalation reaches a stopped
                        // process where SIGTERM cannot).
                        (LifecycleCommand::Resume, _) => {
                            format!(
                                "kt agent stop {name} (its escalation reaches a suspended process)"
                            )
                        }
                        (_, _) => format!("kt agent stop {name}"),
                    };
                    let signal_failure = format!(
                        "{}: the committed {} transition says '{}', but the signal failed: {} — \
                         the ledger and the live process may diverge; recovery: {remediation}",
                        name.as_str(),
                        command.as_str(),
                        new_state.as_str(),
                        err,
                    );
                    self.emit_diagnostic(&signal_failure);
                    return Err(err);
                }
                registry.lookup(&name).map_err(registry_to_engine)
            }
            // BEST-EFFORT (AC2): transition + a VISIBLE qualifier cause, never a
            // silent success. No backend suspension is guaranteed here (on Unix a
            // best-effort declaration is unusual, but we still do NOT SIGSTOP — the
            // declared level is the contract; the qualifier is the honesty). A
            // story-3-2 budget pause overrides the cause with BudgetExceeded (the
            // best-effort posture is captured in the standalone breach event + a
            // diagnostic, so the lifecycle cause stays the honest WHY).
            SupportLevel::BestEffort => {
                self.ensure_log_dir(registry, &name)?;
                let cause = cause_override.clone().unwrap_or_else(|| {
                    let detail = format!(
                        "{} is best-effort for '{}' on {} (adapter-cooperative); the process may keep running",
                        Capability::Pause.as_str(),
                        name.as_str(),
                        os.as_str(),
                    );
                    match command {
                        LifecycleCommand::Pause => TransitionCause::pause_best_effort(detail),
                        _ => TransitionCause::resume_best_effort(detail),
                    }
                });
                self.transition(registry, &name, instance.state, new_state, cause)?;
                registry.lookup(&name).map_err(registry_to_engine)
            }
        }
    }

    /// Signal the running process for a GUARANTEED pause/resume, via the in-memory
    /// handle map (same `self.running.get_mut(&name)` pattern as `stop`).
    ///
    /// Cross-lifetime honesty (AD-5, story 1-6: adoption re-holds handles): with
    /// orphan adoption, a still-live process started by a PRIOR engine is
    /// re-acquired at [`Engine::open`] (via [`Supervisor::adopt_orphans`]), so its
    /// handle IS in the map and this path really signals it. The no-handle branch
    /// now only occurs when the row says `running`/`paused` but adoption found NO
    /// live process — a state adoption would already have reconciled to `failed`;
    /// so a lingering no-handle case is a best-effort no-op (nothing to signal).
    /// AI-8: the CALLER records that honesty in the transition cause (the
    /// best-effort qualifier naming the missing handle — decided in
    /// [`Supervisor::suspend_or_resume`] BEFORE the persist, via the
    /// `contains_key` probe) — this method still returns `Ok` (the desired end
    /// state trivially holds; nothing to signal), never a fake plain-command
    /// success in the ledger. A real held (spawned or adopted) process IS
    /// signalled.
    fn signal_backend(
        &mut self,
        name: &InstanceName,
        command: LifecycleCommand,
    ) -> Result<(), EngineError> {
        let Some(supervised) = self.running.get_mut(name) else {
            return Ok(());
        };
        // AI-9 (loop 2) wiring seam (cfg(test)): the armed instance's signal
        // fails with an injected error even though its transition has already
        // committed — the fault-injection front for the backend's pause/resume
        // (see the `signal_fault_names` field docs). Consulted AFTER the
        // no-handle probe so an armed name with nothing held keeps the honest
        // "no handle = harmless no-op" semantics above.
        #[cfg(test)]
        if self.signal_fault_names.contains(name) {
            return Err(EngineError::Backend {
                name: name.as_str().to_string(),
                source: BackendError::Control {
                    op: match command {
                        LifecycleCommand::Pause => "pause",
                        _ => "resume",
                    },
                    detail: "injected cfg(test) signal fault (AI-9 post-commit seam)".to_string(),
                },
            });
        }
        let result = match command {
            LifecycleCommand::Pause => self.backend.pause(&mut supervised.handle),
            _ => self.backend.resume(&mut supervised.handle),
        };
        result.map_err(|source| EngineError::Backend {
            name: name.as_str().to_string(),
            source,
        })
    }

    /// Send text input to a running Agent Instance's native input channel
    /// (story 4.1, FR-24, spine AD-12) — the v1 interaction surface. For
    /// every adapter that can actually run today (native mock or manifest),
    /// "the native input channel" is the spawned child's OS stdin pipe (both
    /// backends pipe it unconditionally at spawn, Task 1); this needs ZERO
    /// per-kind branching, so one method serves both (AC-A).
    ///
    /// Unlike [`Supervisor::suspend_or_resume`], `send` is NOT itself a state
    /// transition (AD-15's transition table has no `send` entry): no
    /// `next_state` call, no [`TransitionEvent`]. The dispatch order:
    ///
    /// 1. name-resolve (`NotFound` unchanged),
    /// 2. **AC-C**: the instance MUST be [`LifecycleState::Running`] —
    ///    anything else fails with [`EngineError::NotRunning`], checked
    ///    BEFORE the capability read (mirrors "transition gate before any
    ///    side effect"),
    /// 3. **AC-B**: read the effective (current-OS) `Capability::Interaction`
    ///    level — `Unsupported` FAILS FAST with
    ///    [`EngineError::CapabilityUnsupported`] (the already-generic
    ///    machinery, reused verbatim — same shape pause already produces),
    ///    no I/O attempted,
    /// 4. **AC-D**: `Guaranteed` and `BestEffort` take the IDENTICAL action —
    ///    unlike pause/resume there is no OS-conditional difference in
    ///    writing bytes to a pipe, so a declared `best-effort` is purely an
    ///    adapter-author honesty signal, not a different code path. A
    ///    missing handle, or one with no live stdin pipe (an ADOPTED
    ///    instance has no recoverable pipe — see
    ///    [`crate::ports::ProcessBackend::has_stdin`]'s docs), is a HARD
    ///    ERROR ([`EngineError::InteractionUnavailable`]): unlike
    ///    [`Supervisor::signal_backend`]'s "no handle = harmless no-op" (a
    ///    suspend/resume of an already-gone process trivially satisfies its
    ///    own desired end state), there is no equivalent "desired end state"
    ///    for text that was never delivered — a silent success would violate
    ///    FR-24's "honest failure" framing, and this must NEVER be
    ///    misattributed to `CapabilityUnsupported` (the declaration is
    ///    truthful; it is this engine session's reach that is limited).
    ///    **Fix pass addition (review of #79):** a handle whose PRIOR write
    ///    already timed out ([`crate::ports::ProcessBackend::stdin_timed_out`])
    ///    fails fast with [`EngineError::InteractionTimedOut`] here too — a
    ///    cheap, no-I/O check, never a repeat doomed write.
    /// 5. **AC-F**: append exactly one trailing `\n` if `text` doesn't
    ///    already end with one, then write + flush via
    ///    [`crate::ports::ProcessBackend::write_stdin`] — BOUNDED to
    ///    [`crate::ports::STDIN_WRITE_TIMEOUT`] (fix pass, the CRITICAL
    ///    finding: the original unbounded write could freeze the ENTIRE
    ///    engine, since this call runs while the caller already holds the
    ///    single, engine-wide supervisor lock — see `write_stdin`'s docs). A
    ///    timeout maps to [`EngineError::InteractionTimedOut`]; any OTHER
    ///    [`BackendError`] maps to [`EngineError::Backend`] — the SAME
    ///    generic mapping `signal_backend` already uses for pause/resume.
    pub fn send_input(
        &mut self,
        registry: &Registry,
        name: &str,
        text: &str,
    ) -> Result<(), EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = registry.lookup(&name).map_err(registry_to_engine)?;

        // (1) AC-C: send is not a transition, so this is a dedicated
        // pre-flight state check — before any capability read or I/O.
        if instance.state != LifecycleState::Running {
            return Err(EngineError::NotRunning {
                name: name.as_str().to_string(),
                state: instance.state.as_str().to_string(),
            });
        }

        // (2) AC-B: reuse the already-generic capability-unsupported
        // fail-fast machinery verbatim.
        let level = registry
            .effective_support(&name, Capability::Interaction)
            .map_err(registry_to_engine)?;
        let os = OsId::current();
        if level == SupportLevel::Unsupported {
            return Err(EngineError::CapabilityUnsupported {
                name: name.as_str().to_string(),
                capability: Capability::Interaction.as_str().to_string(),
                os: os.as_str().to_string(),
                level: level.as_str().to_string(),
            });
        }

        // (3) AC-D: Guaranteed and BestEffort collapse to the SAME action
        // below (no OS-conditional difference in delivering bytes to a
        // pipe). A missing handle, or one with no live stdin pipe (an
        // adopted instance), is a HARD error — never a silent success.
        let Some(supervised) = self.running.get_mut(&name) else {
            return Err(EngineError::InteractionUnavailable {
                name: name.as_str().to_string(),
                detail: "no live process handle is held in this engine session".to_string(),
            });
        };
        // Fix pass (CRITICAL finding, review of #79): a cheap, no-I/O check
        // FIRST — a handle whose prior write already exceeded the bounded
        // timeout is PERMANENTLY broken for the rest of this engine session
        // (see `write_stdin`'s docs). Checked before `has_stdin` (which would
        // also read `false` here) so the more precise, honest diagnostic
        // wins: "we had a pipe and it stopped draining" is a materially
        // different fact from "no pipe was ever recoverable", and the CLI's
        // remediation differs (restart to get a fresh channel either way, but
        // the cause is not the same).
        if self.backend.stdin_timed_out(&supervised.handle) {
            return Err(EngineError::InteractionTimedOut {
                name: name.as_str().to_string(),
                timeout_secs: crate::ports::STDIN_WRITE_TIMEOUT.as_secs(),
            });
        }
        if !self.backend.has_stdin(&supervised.handle) {
            return Err(EngineError::InteractionUnavailable {
                name: name.as_str().to_string(),
                detail: "no live stdin pipe is held for this instance in this engine session \
                         (an adopted instance has no recoverable pipe; durable cross-invocation \
                         interaction needs a persistent engine session, planned for Epic 7/v1.x)"
                    .to_string(),
            });
        }

        // (4) AC-F: append exactly one trailing newline if absent, so a
        // line-oriented agent (`BufRead::read_line`) receives a complete
        // line.
        let mut bytes = text.as_bytes().to_vec();
        if !text.ends_with('\n') {
            bytes.push(b'\n');
        }
        // Fix pass (CRITICAL finding): this write is now BOUNDED to
        // `STDIN_WRITE_TIMEOUT` (`write_stdin`'s new contract) rather than
        // the story's original unbounded `write_all` — still runs while
        // `self` (the supervisor) is held under the caller's lock, exactly
        // like `stop`'s existing bounded graceful-window wait; a deliberate,
        // ACCEPTED, BOUNDED tradeoff, not the unbounded-freeze problem this
        // fix closes.
        match self.backend.write_stdin(&mut supervised.handle, &bytes) {
            Ok(()) => Ok(()),
            Err(BackendError::StdinTimedOut { timeout_secs }) => {
                Err(EngineError::InteractionTimedOut {
                    name: name.as_str().to_string(),
                    timeout_secs,
                })
            }
            Err(source) => Err(EngineError::Backend {
                name: name.as_str().to_string(),
                source,
            }),
        }
    }

    /// The current [`RunId`] for a supervised instance (story 3-1), or `None` if
    /// this engine holds no live handle for it (never started this lifetime, or
    /// already stopped/crashed). The Fleet read uses it to scope the current-Run
    /// token totals; a `None` simply means "no active Run" (current-run totals are
    /// zero). Held in memory alongside the process handle for this engine lifetime.
    pub fn current_run_id(&self, name: &InstanceName) -> Option<RunId> {
        self.running.get(name).map(|s| s.run_id.clone())
    }

    /// Clear one instance's consecutive poll-error streak (AI-12). Called at
    /// EVERY [`Supervisor::poll_once`] `running.remove` site and on every fresh
    /// handle insert (start / adopt), so a removed — or replaced — handle can
    /// never bequeath a stale error streak to the instance's next Run.
    fn clear_poll_error_streak(&mut self, name: &InstanceName) {
        self.poll_error_streaks.remove(name);
        self.poll_last_errors.remove(name);
    }

    /// Arm the cfg(test) poll-fault seam for `pid`: every `backend.poll` of
    /// the handle with this pid fails with an injected error until this
    /// supervisor is dropped. Lib-test only.
    #[cfg(test)]
    pub(crate) fn arm_poll_fault(&mut self, pid: u32) {
        assert!(
            self.poll_fault_pids.insert(pid),
            "poll fault already armed for pid {pid}"
        );
    }

    /// The injected poll error for `pid`, when the seam is armed (cfg(test)).
    #[cfg(test)]
    fn injected_poll_fault(&self, pid: u32) -> Option<BackendError> {
        self.poll_fault_pids
            .contains(&pid)
            .then(|| BackendError::Control {
                op: "poll",
                detail: "injected cfg(test) poll fault (AI-12 wiring seam)".to_string(),
            })
    }

    /// Arm the cfg(test) signal-fault seam for `name`: every `signal_backend`
    /// for this instance fails with an injected error until this supervisor is
    /// dropped. Lib-test only.
    #[cfg(test)]
    pub(crate) fn arm_signal_fault(&mut self, name: InstanceName) {
        assert!(
            self.signal_fault_names.insert(name),
            "signal fault already armed for this instance"
        );
    }

    /// Read the recorded [`TransitionEvent`]s for an instance from its log
    /// (observation helper for tests / embedders; the AD-14 seed, NOT the 7-2
    /// bus). Returns an empty vec if the log does not exist yet.
    pub fn read_events(
        registry: &Registry,
        name: &str,
    ) -> Result<Vec<TransitionEvent>, EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let path = registry.instance_log_path(&name);
        read_events_from(&path).map_err(|detail| EngineError::Log {
            name: name.as_str().to_string(),
            path: path.to_string_lossy().into_owned(),
            detail,
        })
    }

    /// One-shot full read of every currently-retained ATTRIBUTED output line
    /// for an instance (story 4-2, AC-A/AC-G) — reads the rotated generations
    /// OLDEST-to-newest (`.2`, `.1`, current — skipping any that do not exist
    /// yet), concatenates, and parses each JSON-Lines [`LogLine`] record in
    /// ON-DISK APPEND ORDER (the sole ordering authority; NEVER re-sorted by
    /// `at` — AC-G, since `now_rfc3339`'s whole-second resolution makes
    /// same-second lines common). This reads the NEW, SEPARATE
    /// `logs/output.log[.N]` file (CRITICAL SCOPING #3) — never `agent.log`,
    /// which stays byte-identical and untouched for Epic 3's
    /// `drain_usage_for`.
    ///
    /// DELIBERATE IMPROVEMENT over the `read_events`/`read_breach_events`
    /// precedent above (which never check the registry for the instance's
    /// existence at all — harmless there, since neither is exposed via any
    /// `kt` command): `read_agent_log` is the FIRST CLI-facing consumer of
    /// this shape (`kt agent logs`, Task 6), where silently showing "no
    /// output" for a mistyped name would be genuinely confusing UX
    /// (indistinguishable from "the agent just hasn't said anything yet").
    /// So this DOES check the registry first: a truly UNREGISTERED name
    /// fails [`EngineError::NotFound`] (matching every other CLI-facing
    /// command — `show`/`send`/`pause` all do this); a REGISTERED-but-never-
    /// started instance still falls through to an honest empty vec (mirrors
    /// `read_events_from`'s "missing file → empty" precedent).
    ///
    /// Fix pass (M1, review of #80): ALSO returns the byte-cursor position
    /// (into the CURRENT generation, matching
    /// [`Supervisor::read_agent_log_since`]'s cursor shape exactly) this
    /// read reached — computed from the SAME bytes this call parsed, never a
    /// second, separately-timed read. `kt agent logs --follow` (the sole
    /// production caller) primes its poll loop's cursor from this value
    /// directly, instead of a SEPARATE `read_agent_log_since(name, 0)` call
    /// whose returned lines it used to discard — that discarding call read
    /// up to a slightly LATER point in time than this one-shot dump, so
    /// anything emitted in the gap between the two reads was silently lost
    /// before `--follow` ever started polling. Returning the cursor here
    /// closes that gap: there is only ever ONE read establishing both the
    /// dump and the resume point.
    pub fn read_agent_log(
        registry: &Registry,
        name: &str,
    ) -> Result<(Vec<LogLine>, u64), EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        registry.lookup(&name).map_err(registry_to_engine)?;

        let mut lines = Vec::new();
        // Oldest generation first (LOG_ROTATE_GENERATIONS - 1 down to 1),
        // then the current generation last — append order overall.
        for generation in (1..LOG_ROTATE_GENERATIONS).rev() {
            let path = registry.attributed_output_log_generation_path(&name, generation);
            read_log_lines_from(&path, &mut lines).map_err(|detail| EngineError::Log {
                name: name.as_str().to_string(),
                path: path.to_string_lossy().into_owned(),
                detail,
            })?;
        }
        let current = registry.attributed_output_log_path(&name);
        // Read the CURRENT generation's raw text ONCE so its exact byte
        // length (the cursor) and its parsed lines come from the identical
        // bytes — never a second, later, potentially-inconsistent read.
        let current_text = match std::fs::read_to_string(&current) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(EngineError::Log {
                    name: name.as_str().to_string(),
                    path: current.to_string_lossy().into_owned(),
                    detail: e.to_string(),
                })
            }
        };
        let cursor = current_text.len() as u64;
        parse_log_lines(&current_text, &mut lines).map_err(|detail| EngineError::Log {
            name: name.as_str().to_string(),
            path: current.to_string_lossy().into_owned(),
            detail,
        })?;
        Ok((lines, cursor))
    }

    /// A CURSOR-based follow read for `kt agent logs --follow`'s poll loop
    /// (story 4-2, AC-B/AC-C/AC-H, AD-13). `cursor` is a byte offset into the
    /// CURRENT generation ONLY (mirrors `agent_log_len`/`plan_drain`'s
    /// existing cursor shape) — distinct from `read_agent_log`'s
    /// concatenated multi-generation view, so a caller must not mix cursors
    /// from the two methods. Returns `(new_lines, next_cursor)` — plain
    /// request/response (AD-13), never a `Stream`-typed API (see the story's
    /// Dev Notes on why: this keeps the existing async/blocking pairing with
    /// zero new API shape).
    ///
    /// On a detected SHRINK (the current generation's length is now LESS
    /// than `cursor` — a rotation happened since the last poll), the cursor
    /// snaps to the new length and this returns `(vec![], new_len)` — the
    /// CALLER detects the signal itself by comparing the returned cursor to
    /// the one it just passed in (`next_cursor < cursor`) and prints one
    /// honest notice (Task 6); `read_agent_log` WITHOUT `--follow` always
    /// re-reads everything currently retained, so this never loses data
    /// permanently — only a possible (rare) live-tail gap at the rotation
    /// boundary.
    pub fn read_agent_log_since(
        registry: &Registry,
        name: &str,
        cursor: u64,
    ) -> Result<(Vec<LogLine>, u64), EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        registry.lookup(&name).map_err(registry_to_engine)?;

        let path = registry.attributed_output_log_path(&name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(EngineError::Log {
                    name: name.as_str().to_string(),
                    path: path.to_string_lossy().into_owned(),
                    detail: e.to_string(),
                })
            }
        };
        match plan_follow(&bytes, cursor) {
            FollowPlan::Shrunk { new_cursor } => Ok((Vec::new(), new_cursor)),
            FollowPlan::Consume { range, new_cursor } => {
                let mut lines = Vec::new();
                if !range.is_empty() {
                    parse_log_lines(&String::from_utf8_lossy(&bytes[range]), &mut lines).map_err(
                        |detail| EngineError::Log {
                            name: name.as_str().to_string(),
                            path: path.to_string_lossy().into_owned(),
                            detail,
                        },
                    )?;
                }
                Ok((lines, new_cursor))
            }
        }
    }

    /// The crash-detection reaper pass (story 1-6, AC-A / AC3 / AC5).
    ///
    /// Polls every held handle via the EXISTING `backend.poll` and reacts to an
    /// unrequested exit: for each instance the store still shows `running` or
    /// `paused` (a `stopping` in flight means an operator stop is under way — NOT
    /// a crash, so it is skipped), applies the EVENT-driven `running → failed`
    /// edge with a [`TransitionCause::Crashed`] (AC5), removes the handle, and
    /// consults the per-instance [`RestartPolicy`] (AD-15):
    /// * [`RestartPolicy::Never`] — leave `failed`; record the crash cause; NO
    ///   restart plan.
    /// * [`RestartPolicy::OnFailure`] — increment the consecutive restart count;
    ///   if it hit the crash-loop threshold ([`is_crash_loop`]) leave `failed`
    ///   with the crash-loop reason and NO plan; otherwise persist the new count
    ///   and return a [`RestartPlan`] with the backoff delay for that attempt.
    ///
    /// Returns the [`RestartPlan`]s the engine cadence should time. SYNC +
    /// cfg-free (the engine calls it via `spawn_blocking` on an interval); it
    /// performs NO sleeping itself. Idempotent per exit: once an instance is
    /// moved to `failed` and its handle removed, a later pass will not see it in
    /// `self.running` again.
    ///
    /// **Persistent poll errors are crash input (AI-12):** a `backend.poll` error
    /// is tolerated as transient only while its PER-INSTANCE consecutive streak
    /// stays below [`MAX_CONSECUTIVE_POLL_ERRORS`] (the
    /// [`poll_verdict`] pure decision). A clean `Alive` read resets the streak,
    /// and every handle removal clears it; an error streak that reaches the
    /// threshold is treated exactly like an observed exit — the instance lands
    /// `failed` with a cause naming the persistent poll failure plus the LAST
    /// error's text (truncated; loop 1) and the Restart Policy applies — instead
    /// of the old silent `Err(_) => None` that could hide a dead handle FOREVER.
    ///
    /// **Systemic guard (AI-12, loop 1):** every held handle is polled ONCE per
    /// tick, up front. An error on MORE THAN ONE handle in the same tick is
    /// corroborated as environmental (a procfs/sysctl-style outage): no streak
    /// increment, one diagnostic, handles stay alive — a fleet-wide poll outage
    /// must never mass-crash running agents through kill-on-drop. Only a handle
    /// erroring ALONE (its peers read fine) accumulates crash-input credit.
    pub fn poll_once(&mut self, registry: &Registry) -> Vec<RestartPlan> {
        // First, INGEST self-reported usage from every running instance's captured
        // output (story 3-1): the reaper is the natural cadence for draining the
        // agent-output log into the Usage Ledger while an instance is `running`.
        // Best-effort per instance; a drain hiccup never blocks crash detection.
        self.drain_usage_all(registry);
        // Then INGEST engine-observed usage (story 3-4): drain each observed
        // instance's listener queue (the counts the loopback proxy parsed out of the
        // agent's model traffic) into the SAME `ingest_usage` choke point, minting
        // the per-Run `sequence`. This reaper cadence (~250ms) lands observed usage
        // well within the AD-7/FR-19 flush bound (≤5s) of call completion. Best-
        // effort per instance, exactly like the self-reported drain.
        self.drain_observed_all(registry);

        // Snapshot the currently-held names (we mutate self.running as we react).
        let names: Vec<InstanceName> = self.running.keys().cloned().collect();
        let mut plans = Vec::new();

        // PHASE 1 — poll EVERY held handle in the SAME tick (AI-12, loop 1).
        // Liveness reads happen up front, BEFORE any crash handling, so the pass
        // can CORROBORATE: a poll error that shows up on MULTIPLE handles in one
        // tick is a backend/environment-wide condition (a procfs/sysctl-style
        // outage), never a per-handle fault — and tripping every streak then
        // would mass-crash the fleet on kill-on-drop handles (the
        // graceful-degradation gate forbids it). The chosen guard is same-tick
        // cross-handle corroboration (the error-classification alternative was
        // evaluated: the port carries no environment-vs-handle distinction an
        // OS backend could honestly report, so corroboration is the provable
        // shape). A single handle erroring alone stays on the streak path: an
        // un-pollable HANDLE amid readable peers is exactly the crash signal
        // AI-12 exists to surface.
        let mut outcomes: Vec<(InstanceName, PollOutcome)> = Vec::with_capacity(names.len());
        for name in &names {
            // AI-12 wiring seam (cfg(test)): the armed pid's poll fails with an
            // injected error — the fault-injection front for the backend's poll
            // (see the `poll_fault_pids` field docs). The pid read + the seam
            // probe run on short immutable borrows BEFORE the mutable handle
            // borrow below.
            #[cfg(test)]
            {
                let pid = self
                    .running
                    .get(name)
                    .map(|supervised| self.backend.pid(&supervised.handle));
                if let Some(err) = pid.and_then(|pid| self.injected_poll_fault(pid)) {
                    outcomes.push((name.clone(), PollOutcome::Errored(err)));
                    continue;
                }
            }
            let Some(supervised) = self.running.get_mut(name) else {
                continue;
            };
            let outcome = match self.backend.poll(&mut supervised.handle) {
                Ok(ProcessStatus::Alive) => PollOutcome::Alive,
                Ok(ProcessStatus::Exited { code }) => PollOutcome::Exited(code),
                Err(err) => PollOutcome::Errored(err),
            };
            outcomes.push((name.clone(), outcome));
        }

        // PHASE 2 — same-tick corroboration (AI-12 amendment a). Count how many
        // DISTINCT handles errored this tick; more than one ⇒ environmental.
        let errored = outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, PollOutcome::Errored(_)))
            .count();
        let mut environmental_tick = errored > 1;
        if environmental_tick {
            // AI-12 (loop 2): blanket environmental immunity is bounded. A pair
            // of handles that errors together EVERY tick (one broken, one flaky
            // — or a real outage that outlives the cap) must not keep crash
            // detection defeated forever, so past the cap the per-handle streak
            // path resumes and trips normally a few ticks later. The escalation
            // diagnostic fires ONCE, on the transition tick.
            self.consecutive_environmental_ticks =
                self.consecutive_environmental_ticks.saturating_add(1);
            if self.consecutive_environmental_ticks > MAX_CONSECUTIVE_ENVIRONMENTAL_TICKS {
                environmental_tick = false;
                if self.consecutive_environmental_ticks == MAX_CONSECUTIVE_ENVIRONMENTAL_TICKS + 1 {
                    let escalation = format!(
                        "environmental poll failure has persisted for \
                         {MAX_CONSECUTIVE_ENVIRONMENTAL_TICKS} consecutive ticks — no longer \
                         treated as a transient environment-wide condition; per-handle \
                         crash-input credit resumes (an un-pollable handle will be \
                         crash-detected again)"
                    );
                    self.emit_diagnostic(&escalation);
                }
            }
        } else {
            self.consecutive_environmental_ticks = 0;
        }
        if environmental_tick {
            // Record each error's text (a later, genuinely per-handle streak may
            // still want it in a crash cause) but grant NO crash-input credit:
            // every streak stays where it is and every handle stays alive.
            for (name, outcome) in &outcomes {
                if let PollOutcome::Errored(err) = outcome {
                    self.poll_last_errors.insert(
                        name.clone(),
                        truncate_for_cause(&err.to_string(), POLL_ERROR_CAUSE_MAX_CHARS),
                    );
                }
            }
            let errors: Vec<String> = outcomes
                .iter()
                .filter_map(|(name, outcome)| match outcome {
                    PollOutcome::Errored(err) => Some(format!(
                        "{}: {}",
                        name.as_str(),
                        truncate_for_cause(&err.to_string(), POLL_ERROR_CAUSE_MAX_CHARS)
                    )),
                    _ => None,
                })
                .collect();
            let environmental = format!(
                "environmental poll failure: {errored} of {} held handles failed backend.poll \
                 in the same tick — a backend/environment-wide condition, not a per-handle \
                 fault; treating every one as transient (no crash-input credit, no streak \
                 increment, handles stay alive). Errors: {}",
                outcomes.len(),
                errors.join("; "),
            );
            self.emit_diagnostic(&environmental);
        }

        // PHASE 3 — per-name handling (unchanged crash semantics, now fed by the
        // corroborated outcomes). `held_handles` feeds the sole-handle caveat:
        // a handle that trips with no peers could never be corroborated.
        let held_handles = names.len();
        for (name, outcome) in outcomes {
            // What the reaper treats as the crash input: a real observed exit
            // (with the code the backend reported, `None` if unknown), or — new
            // under AI-12 — a persistent poll failure (no exit code exists; the
            // recorded cause says so). Everything else keeps polling.
            let crash = match outcome {
                PollOutcome::Alive => {
                    self.clear_poll_error_streak(&name);
                    continue;
                }
                PollOutcome::Exited(code) => {
                    self.clear_poll_error_streak(&name);
                    CrashInput::Exited(code)
                }
                // Environmental tick: this error was already corroborated as
                // environment-wide above (diagnostic emitted, text recorded) —
                // grant no crash-input credit and keep the handle alive.
                PollOutcome::Errored(_) if environmental_tick => continue,
                PollOutcome::Errored(err) => {
                    // Record the error's text FIRST (AI-12b): whatever the
                    // verdict, a later persistent trip must carry this why.
                    self.poll_last_errors.insert(
                        name.clone(),
                        truncate_for_cause(&err.to_string(), POLL_ERROR_CAUSE_MAX_CHARS),
                    );
                    // A clean Alive read clears the handle's consecutive
                    // poll-error streak; an error increments it and, once it
                    // reaches MAX_CONSECUTIVE_POLL_ERRORS, becomes crash input
                    // instead of the old silent `Err(_) => None` that swallowed
                    // every error forever. Reap on exit is done inside `poll`.
                    let (verdict, streak) = poll_verdict(
                        self.poll_error_streaks.get(&name).copied().unwrap_or(0),
                        Err(err),
                    );
                    match verdict {
                        PollVerdict::TransientError => {
                            self.poll_error_streaks.insert(name.clone(), streak);
                            continue;
                        }
                        PollVerdict::PersistentError => {
                            self.poll_error_streaks.insert(name.clone(), streak);
                            CrashInput::PersistentPollFailure {
                                sole_handle: held_handles == 1,
                            }
                        }
                        // Unreachable by construction: `poll_verdict` maps an
                        // `Err` input to one of the two error verdicts only.
                        PollVerdict::Alive | PollVerdict::Exited(_) => {
                            unreachable!("an Err poll cannot yield a clean verdict")
                        }
                    }
                }
            };
            // The process exited (or the handle went permanently un-pollable):
            // drain any usage it emitted right before dying, so a final batch is
            // not lost between "agent printed it" and this reap. TERMINAL drain —
            // the process is dead, so consume a final newline-less usage line to
            // end-of-log instead of stranding it (H1). On the poll-failure path
            // the process is not PROVEN dead, but the handle is about to be
            // dropped (which kills the group), so this is the last chance to
            // capture the flushed tail; a truncated mid-write line fails the
            // sentinel parse and is skipped, and the DB dedup key backstops the
            // rest.
            self.drain_usage_for(registry, &name, DrainMode::Terminal);
            // Drain any final ENGINE-OBSERVED usage still queued before the crashed
            // instance's listener is torn down (story 3-4): a completion parsed just
            // before the crash must land, not be lost when the `Supervised` is
            // removed below. TERMINAL mode (story 12-4): a commit failure here
            // announces the loss — no next pass.
            self.drain_observed_for(registry, &name, DrainMode::Terminal);

            // Read the store state: only an instance the store still shows
            // running/paused is an UNREQUESTED crash. A `stopping` (operator
            // stop) or any other state is not a crash — drop the (now-dead)
            // handle without a `failed` transition.
            let state = match registry.lookup(&name) {
                Ok(inst) => inst.state,
                // The row is gone (removed concurrently) — just drop the handle.
                Err(_) => {
                    self.clear_poll_error_streak(&name);
                    self.running.remove(&name);
                    continue;
                }
            };
            if !matches!(state, LifecycleState::Running | LifecycleState::Paused) {
                // Requested stop (or already-terminal) — not a crash.
                //
                // Fix pass (review of #80 follow-up — the CRITICAL finding,
                // self-healing requirement): if this handle's PRIOR stop
                // attempt sent SIGKILL but could not confirm death within
                // KILL_CONFIRM_TIMEOUT (`stop_unconfirmed` — set ONLY by
                // that specific path, see `stop_inner`'s docs) and the store
                // still shows `stopping`, THIS poll's own observed `Exited`
                // is the reconciliation event the stuck stop() call itself
                // could not wait for: finalize `stopping -> stopped` here
                // rather than silently dropping the handle, so the row does
                // not stay permanently stuck even if no operator ever
                // retries `stop` manually. This is DELIBERATELY narrower
                // than "any exit while stopping" — an ordinary in-flight
                // (non-stuck) stop() call ALWAYS finalizes this transition
                // itself upon its own return, so only the
                // stuck-then-abandoned case needs the reaper's help; every
                // OTHER "not a crash" exit (mirrored by
                // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`,
                // which never sets `stop_unconfirmed`) keeps its EXISTING,
                // unchanged silent-drop behavior.
                let stuck_stopping = state == LifecycleState::Stopping
                    && self.running.get(&name).is_some_and(|s| s.stop_unconfirmed);
                if stuck_stopping {
                    let log_capture = self
                        .running
                        .get(&name)
                        .and_then(|s| self.backend.log_capture(&s.handle));
                    self.clear_poll_error_streak(&name);
                    self.running.remove(&name);
                    if registry.clear_spawn_record(&name).is_ok() {
                        let _ = self.transition_with_log_capture(
                            registry,
                            &name,
                            LifecycleState::Stopping,
                            LifecycleState::Stopped,
                            TransitionCause::stop_forced(
                                "SIGKILL was sent by an earlier stop attempt; the \
                                 crash-detection reaper confirmed the process's death on a \
                                 later poll (it may have been stuck in an OS-level I/O wait \
                                 that has since cleared)",
                            ),
                            log_capture,
                        );
                    }
                    continue;
                }
                self.clear_poll_error_streak(&name);
                self.running.remove(&name);
                continue;
            }

            // A crash. Consult the Restart Policy FIRST (so a terminal outcome —
            // `never` or crash-loop — can enrich the recorded crash cause), then
            // apply running/paused → failed with that detail (AC5).
            //
            // Story 4-2, Task 4: capture the log_capture BEFORE removing the
            // entry below — same reasoning as `stop_inner`'s terminal
            // transition (the default `self.transition(...)` lookup would
            // otherwise miss it).
            //
            // AI-13: read the `adopted` flag BEFORE the remove below — an adopted
            // handle is not the engine's child, so a `code: None` exit means "the
            // exit code is UNAVAILABLE", and the cause must say so instead of
            // asserting a signal termination it cannot prove.
            let adopted = self.running.get(&name).is_some_and(|s| s.adopted);
            let crash_log_capture = self
                .running
                .get(&name)
                .and_then(|s| self.backend.log_capture(&s.handle));
            // AI-12b: capture the last poll error's text BEFORE the bookkeeping
            // clear below (the cause build needs it).
            let last_poll_error = self.poll_last_errors.get(&name).cloned();
            self.clear_poll_error_streak(&name);
            self.running.remove(&name);
            let base_detail = match crash {
                // AI-12: the handle went permanently un-pollable — the recorded
                // cause names the persistent poll failure, never a fabricated
                // exit. Loop 1: it also carries the LAST poll error's text
                // (truncated) so the operator gets the actual why.
                CrashInput::PersistentPollFailure { sole_handle } => {
                    let last = last_poll_error
                        .as_deref()
                        .unwrap_or("no error text recorded");
                    // AI-12 (loop 2): a lone held handle's errors could never be
                    // cross-checked against peers, so the cause says the
                    // single-handle caveat out loud instead of asserting a
                    // per-handle fault it cannot prove.
                    let corroboration = if sole_handle {
                        " — this was the ONLY held handle, so the error could not be \
                         corroborated against peers; if it recurs across restarts, check the \
                         platform's process-table source (procfs/sysctl) before blaming the \
                         agent"
                    } else {
                        ""
                    };
                    format!(
                        "persistent poll failure: {MAX_CONSECUTIVE_POLL_ERRORS} consecutive \
                         backend.poll errors — the handle's liveness could no longer be read, \
                         so it is not trusted as alive; last error: {last}{corroboration}"
                    )
                }
                CrashInput::Exited(Some(c)) => {
                    format!("process exited unexpectedly with code {c}")
                }
                // AI-13: an adopted process's exit code is unrecoverable on
                // UNIX (it is not this engine's child — only a parent gets an
                // ExitStatus), so THIS arm is the Unix-shaped case: the cause
                // says the code is unavailable and why, instead of asserting a
                // signal termination it cannot prove. On WINDOWS this arm is
                // reachable only when the code is GENUINELY unreadable: the
                // adopted handle's poll (backends/windows reap_if_exited)
                // reads the real exit code via GetExitCodeProcess, so a
                // Windows adopted exit normally lands in the `Exited(Some)`
                // arm above carrying its true code (the story-11-5 closure of
                // the 11-1 Windows-half defer).
                CrashInput::Exited(None) if adopted => "process exited unexpectedly (exit \
                 code unavailable — adopted process is not this engine's child)"
                    .to_string(),
                CrashInput::Exited(None) => {
                    "process exited unexpectedly (terminated by signal)".to_string()
                }
            };
            let decision = self.plan_restart(registry, &name, &base_detail);
            if self.ensure_log_dir(registry, &name).is_err() {
                // If we cannot even prepare the log dir, still persist the state
                // so the durable state leads; skip the event append best-effort.
            }
            // The recorded crash cause carries the full story: the exit detail,
            // plus (on a terminal outcome) the policy conclusion (crash-loop, or
            // "policy is never — not restarting"). This is what `instance_status`
            // falls back to for the failed cause once the terminal record is
            // cleared (AC9).
            if self
                .transition_with_log_capture(
                    registry,
                    &name,
                    state,
                    LifecycleState::Failed,
                    TransitionCause::crashed(decision.crash_cause.clone()),
                    crash_log_capture,
                )
                .is_err()
            {
                // Persisting the crash transition failed; leave the record for a
                // later reconcile and move on (do not panic the reaper).
                continue;
            }

            if let Some(plan) = decision.plan {
                plans.push(plan);
            }
        }
        plans
    }

    /// Decide the Restart Policy action for a just-crashed instance (AC4).
    ///
    /// Reads the per-instance record (policy + current consecutive count) and
    /// returns a [`RestartDecision`]: the crash cause to record in the event log
    /// (enriched with the policy conclusion on a terminal outcome) and, when a
    /// restart is scheduled, the [`RestartPlan`]. Side effects (all best-effort —
    /// a store hiccup is never a panic):
    /// * `on-failure`, below the crash-loop threshold → increment the persisted
    ///   restart count; the plan carries the backoff delay for that attempt.
    /// * `on-failure`, at the crash-loop threshold ([`is_crash_loop`]) → TERMINAL:
    ///   CLEAR the write-ahead record (F-Low-2: no needless adopt-attempt against
    ///   a dead/reused PID on a later open) and enrich the crash cause with the
    ///   crash-loop reason; no plan.
    /// * `never` → TERMINAL: clear the write-ahead record and note the policy in
    ///   the crash cause; no plan.
    fn plan_restart(
        &self,
        registry: &Registry,
        name: &InstanceName,
        crash_detail: &str,
    ) -> RestartDecision {
        let record = registry.spawn_record(name).ok().flatten();
        let policy = record
            .as_ref()
            .map(|r| r.restart_policy)
            .unwrap_or_default();
        let current = record.as_ref().map(|r| r.restart_count).unwrap_or(0);

        if !policy.restarts_on_crash() {
            // `never`: TERMINAL. Settle the record so a later open does not
            // adopt-attempt a dead PID; the crash cause names the policy.
            self.settle_terminal_record(registry, name, policy);
            return RestartDecision {
                crash_cause: format!("{crash_detail}; restart policy is 'never' — not restarting"),
                plan: None,
            };
        }

        let next = current.saturating_add(1);
        if is_crash_loop(next) {
            // Crash loop: TERMINAL. Settle the record (F-Low-2), leave `failed`
            // with the reason STATED in the crash cause.
            self.settle_terminal_record(registry, name, policy);
            return RestartDecision {
                crash_cause: format!(
                    "{crash_detail}; crash-loop: {} consecutive failures reached — \
                     not restarting, inspect the agent and start it manually",
                    MAX_CONSECUTIVE_FAILURES,
                ),
                plan: None,
            };
        }

        // Schedule a restart: persist the incremented count + the crash cause,
        // and return the plan with the backoff delay for this attempt.
        let _ = registry.set_restart_count(name, next, Some(crash_detail));
        let delay = self.backoff.delay_for(next);
        RestartDecision {
            crash_cause: crash_detail.to_string(),
            plan: Some(RestartPlan {
                name: name.clone(),
                attempt: next,
                delay,
            }),
        }
    }

    /// Settle the write-ahead record on a TERMINAL `failed` outcome (F-Low-2).
    ///
    /// Drops the record's LIVE fingerprint (so a later [`Supervisor::adopt_orphans`]
    /// does NOT adopt-attempt the dead/reused PID — the reconcile skips a pid-0
    /// record, exactly like a policy-only config seed), while RE-SEEDING the
    /// per-instance policy so `kt agent show` still reports the active Restart
    /// Policy for the failed instance (AC9). Concretely: clear the record, then
    /// re-persist the policy as a pid-0 seed. The failed CAUSE is not kept in the
    /// record — it rides in the event log, which `instance_status` falls back to.
    /// Best-effort (a store hiccup here is never a panic).
    fn settle_terminal_record(
        &self,
        registry: &Registry,
        name: &InstanceName,
        policy: RestartPolicy,
    ) {
        let _ = registry.clear_spawn_record(name);
        let _ = registry.set_restart_policy(name, policy);
    }

    /// Adopt orphaned processes on engine start (story 1-6, AC-B / AC7 / AI-7 /
    /// AI-8) — the HONEST cross-lifetime reconcile.
    ///
    /// Reads EVERY write-ahead [`SpawnRecord`] (AD-5) and, for each, asks the
    /// backend to re-acquire a live process matching the fingerprint
    /// (`backend.adopt`):
    /// * `Some(handle)` — a live process whose start-time matches: ADOPT it
    ///   (re-hold the handle so `stop`/`pause`/`poll` work again); the persisted
    ///   state stays as-is (`running`/`paused` — AI-7: a live paused process is
    ///   re-held so a later `resume` works).
    /// * `None` — no live match (PID gone, or reused by a different process):
    ///   reconcile HONESTLY to `failed` with an "orphan not found" cause + the
    ///   last-known cause (AI-8: never leave a phantom `running`/`paused` row),
    ///   and clear the record.
    ///
    /// Called from [`Engine::open`]. Best-effort per record: a single
    /// adopt/persist failure does not abort the whole reconcile; it leaves that
    /// record for the next open. Returns the number of processes adopted (for
    /// diagnostics/tests).
    pub fn adopt_orphans(&mut self, registry: &Registry) -> usize {
        let records = match registry.list_spawn_records() {
            Ok(records) => records,
            Err(_) => return 0,
        };
        let mut adopted = 0;
        for record in records {
            let name = record.name.clone();
            // A pid-0 record is a policy-only config SEED (set via
            // `set_restart_policy` before the instance was ever started), NOT a
            // supervised process — skip it (it names no real process to adopt or
            // fail, and clearing it would wipe the persisted policy).
            if record.fingerprint.pid == 0 {
                continue;
            }
            // The record's detach flag (story 12-1 AMENDMENT) decides HOW the
            // handle is re-held: a detached spawn's handle comes back DISARMED
            // (its Drop does not kill), so this engine's clean exit leaves the
            // agent alive for the command after it; a supervised spawn's
            // handle keeps the story 1-6 kill-on-drop guarantee.
            match self.backend.adopt(&record.fingerprint, record.detach) {
                Ok(Some(handle)) => {
                    // Live match: re-hold the handle. State stays as persisted
                    // (running/paused). AI-7: a paused process is now resumable.
                    //
                    // Metering across a crash/adoption (story 3-1, documented
                    // assumption): the pre-crash Run id lived only in the crashed
                    // engine's memory, so the adopted instance opens a NEW Run and
                    // begins ingestion at the CURRENT end of its agent-output log
                    // (skipping pre-crash lines). This keeps per-run totals honest for
                    // the post-adoption span without re-attributing (or double-
                    // counting) the old Run's already-captured usage; the DB dedup key
                    // includes the run id, so even an overlapping sequence is safe.
                    let run_id = RunId::mint();
                    let usage_cursor = self.agent_log_len(registry, &name);
                    let metering_source = registry.metering_source(&name).unwrap_or_else(|err| {
                        // AI-46 (review loop 1): a registry read hiccup must
                        // not SILENCE the stranded-listener diagnostic —
                        // defaulting to `self-reported` here would skip the
                        // one announcement an actually-observed orphan
                        // needs. Announce the ambiguity loudly, then use
                        // the neutral fallback for bookkeeping.
                        let unclear = format!(
                            "{}: the adopted instance's metering source could not be \
                                 read ({err}); if it is engine-observed, its injected \
                                 'metering.base_url' points at the PREVIOUS engine's dead \
                                 loopback listener — stop the instance and start it again \
                                 to re-anchor the listener",
                            name.as_str(),
                        );
                        self.emit_diagnostic(&unclear);
                        "self-reported".to_string()
                    });
                    // Clone the Run context into `Supervised` — the AI-44
                    // enforcement call below borrows the same values afterwards.
                    self.clear_poll_error_streak(&name);
                    self.running.insert(
                        name.clone(),
                        Supervised {
                            handle,
                            run_id: run_id.clone(),
                            metering_source: metering_source.clone(),
                            usage_cursor,
                            usage_park_attempts: None,
                            // Story 12-4: an adopted instance starts with no parked
                            // observed events (its prior engine's park died with it).
                            observed_park: None,
                            // The adopted instance opens a NEW Run (the pre-crash
                            // run_id died with the crashed engine), so its breach latch
                            // starts empty too (story 3-2).
                            breached_scopes: std::collections::HashSet::new(),
                            // ENGINE-OBSERVED across a crash/adoption (story 3-4,
                            // tracked follow-up — NOT just a metering gap): the pre-crash
                            // listener died with the crashed engine, but the already-
                            // running agent's `base_url` STILL points at that now-DEAD
                            // loopback port. So the adopted agent's MODEL TRAFFIC ITSELF
                            // breaks — its completion calls hit the dead port and fail
                            // with a connection-refused error (not merely un-metered).
                            // This fails LOUD (a transport error the agent surfaces),
                            // never a corrupt/silent-wrong output. We cannot rebind the
                            // old port to a fresh listener here (the agent chose no port;
                            // the OS did), so we leave it un-observed with no listener;
                            // RECOVERY is an operator stop→start, which relaunches the
                            // agent pointed at a fresh listener. The full fix (re-launch
                            // an adopted observed instance / a stable per-instance listener
                            // port / the Epic-7 daemon owning the listener) is a tracked
                            // follow-up, not done here. A self-reported instance's
                            // log-tail drain is unaffected (it needs no listener).
                            observed_listener: None,
                            observed_source: None,
                            // An adopted instance's stop attempt has not
                            // happened yet in THIS engine session.
                            stop_unconfirmed: false,
                            // AI-13: this handle was re-acquired, not spawned.
                            // On UNIX its exit code is unrecoverable (the
                            // adopted-exit crash cause says so); on WINDOWS
                            // the adopted handle's poll still reads the real
                            // exit code via GetExitCodeProcess
                            // (backends/windows reap_if_exited), so only a
                            // genuinely unreadable code falls to the
                            // unavailable-code cause there.
                            adopted: true,
                        },
                    );
                    adopted += 1;
                    // AI-46 (story 11-3): an adopted ENGINE-OBSERVED instance is
                    // stranded — the paragraph on `observed_listener: None`
                    // above documents it, but until now the engine said it
                    // NOWHERE an operator could hear. The engine cannot rewrite
                    // the already-running child's injected `base_url` (the env
                    // went in at the previous engine's spawn), so the honest
                    // fix is to ANNOUNCE the condition: name the instance, the
                    // stranded observed listener, and the stop→start
                    // remediation. The diagnostic names the CONDITION without
                    // the dead port number: the spawn record carries no launch
                    // facts (the write-ahead record is
                    // {fingerprint, policy, count, cause} only), and the
                    // registration snapshot's launch predates the start-time
                    // injection, so no base_url host/port is recoverable here.
                    // Semantics are untouched: the instance stays marked
                    // un-observed exactly as below, and no process is
                    // relaunched (adoption keeps-them-running).
                    if metering_source == "engine-observed" {
                        let strand = format!(
                            "{}: adopted an engine-observed instance; its injected \
                             'metering.base_url' still points at the PREVIOUS engine's \
                             loopback forward listener, which died with that engine — this \
                             engine holds NO listener for the adopted process, so its model \
                             calls hit the dead port and fail with connection errors \
                             (stranded observed listener). The already-injected environment \
                             of a running process cannot be rewritten. Remediation: stop \
                             the instance and start it again; the fresh start binds a new \
                             listener and re-injects a live base_url.",
                            name.as_str(),
                        );
                        self.emit_diagnostic(&strand);
                    }
                    // AI-44: re-evaluate budgets for the JUST-ADOPTED instance
                    // NOW, before returning — the durable ledger survived the
                    // engine crash, so an instance already past its ceiling must
                    // be enforced at startup, not left running unconstrained
                    // until the NEXT usage event happens to arrive (which may be
                    // never for a quiet agent). This is the SAME AD-7
                    // enforcement stage `ingest_usage` runs: a live config read,
                    // the committed per-run + cumulative ledger totals, the pure
                    // evaluators, record-first-then-act. The fresh Run's breach
                    // latch is empty (inserted above), so a surviving breach
                    // fires exactly once here; the action (default pause) goes
                    // through the normal lifecycle path with the handle just
                    // re-held. Best-effort by contract: an enforcement error is
                    // a diagnostic, never an adoption failure.
                    self.enforce_budget(registry, &name, &run_id, &metering_source);
                }
                Ok(None) => {
                    // No live match — reconcile to `failed` HONESTLY (AI-8).
                    self.reconcile_orphan_failed(registry, &record);
                }
                Err(_) => {
                    // A backend adopt error is treated as "cannot confirm live" —
                    // reconcile to failed rather than leave a phantom row (AI-8).
                    self.reconcile_orphan_failed(registry, &record);
                }
            }
        }
        adopted
    }

    /// Reconcile a non-adopted orphan record to `failed` (AI-8): the process is
    /// gone (or unconfirmable), so a persisted `running`/`paused` row must NOT be
    /// left implying supervision that does not exist. Records a `Crashed` cause
    /// naming the orphan + the last-known cause, then clears the record. If the
    /// current state is already terminal (`failed`/`stopped`) we only clear the
    /// stale record. Best-effort — a persist failure leaves the record for the
    /// next open.
    fn reconcile_orphan_failed(&self, registry: &Registry, record: &SpawnRecord) {
        let name = &record.name;
        let state = match registry.lookup(name) {
            Ok(inst) => inst.state,
            Err(_) => {
                // Row gone — just drop the stale record.
                let _ = registry.clear_spawn_record(name);
                return;
            }
        };
        if matches!(state, LifecycleState::Running | LifecycleState::Paused) {
            let last = record
                .last_known_cause
                .as_deref()
                .unwrap_or("no prior cause recorded");
            let detail = format!(
                "orphan not found on engine restart (process pid {} is gone or was reused); \
                 last known: {last}",
                record.fingerprint.pid,
            );
            let _ = self.ensure_log_dir(registry, name);
            let _ = self.transition(
                registry,
                name,
                state,
                LifecycleState::Failed,
                TransitionCause::crashed(detail),
            );
        }
        // Clear the stale record either way (its process is gone).
        let _ = registry.clear_spawn_record(name);
    }

    // ---- internals ----

    /// Apply one transition: persist the new state, then append the event to the
    /// per-instance log. Persist-before-log so the durable state leads; a log
    /// append failure surfaces (the escalation record is load-bearing for AC3).
    ///
    /// Story 4-2, Task 4: ALSO best-effort-projects the transition into the
    /// unified attributed-output stream as an `engine`-attributed [`LogLine`]
    /// — a human-readable mirror of the SAME fact `instance.log` (above,
    /// unchanged, machine-authoritative) just recorded. The output-capture
    /// handle is looked up via [`Supervisor::log_capture_for`] (the
    /// instance's CURRENT `self.running` entry, when one exists); fix pass
    /// (review of #80): [`LogCapture::send_engine_line`] catches up any
    /// pending agent-out/agent-err content FIRST, so the engine line lands
    /// after whatever agent output already existed at this moment rather
    /// than racing the background tailer thread's own poll schedule.
    fn transition(
        &self,
        registry: &Registry,
        name: &InstanceName,
        prior: LifecycleState,
        new: LifecycleState,
        cause: TransitionCause,
    ) -> Result<TransitionEvent, EngineError> {
        let log_capture = self.log_capture_for(name);
        self.transition_with_log_capture(registry, name, prior, new, cause, log_capture)
    }

    /// Like [`Supervisor::transition`], but the `engine`-attributed
    /// [`LogCapture`] is supplied EXPLICITLY rather than looked up via
    /// `self.running` — needed at the three call sites (`start_inner`'s
    /// `starting → running`, `stop_inner`'s `stopping → stopped`, and
    /// `poll_once`'s crash `→ failed`) where the just-spawned/about-to-be-
    /// torn-down handle is not (or no longer) present in `self.running` at
    /// the exact moment of the call, even though a live capture pipeline
    /// still exists (captured by the caller a few lines earlier, before the
    /// map mutation that would otherwise hide it).
    fn transition_with_log_capture(
        &self,
        registry: &Registry,
        name: &InstanceName,
        prior: LifecycleState,
        new: LifecycleState,
        cause: TransitionCause,
        log_capture: Option<LogCapture>,
    ) -> Result<TransitionEvent, EngineError> {
        registry.set_state(name, new).map_err(registry_to_engine)?;
        let event = TransitionEvent::new(name.as_str(), prior, new, cause, now_rfc3339());
        append_event(&registry.instance_log_path(name), &event).map_err(|detail| {
            EngineError::Log {
                name: name.as_str().to_string(),
                path: registry
                    .instance_log_path(name)
                    .to_string_lossy()
                    .into_owned(),
                detail,
            }
        })?;
        // Story 7-2: the append COMMITTED — publish onto the event bus. After
        // the durable append (never before: a subscriber never sees an
        // uncommitted event), before the best-effort text mirror below, so the
        // bus order is exactly the durable-log order. Ordering obligation:
        // this runs under the supervisor lock (see the `domain::bus`
        // caller-enforced invariant).
        self.publish(EngineEvent::Transition(event.clone()));
        if let Some(capture) = log_capture {
            let text = engine_transition_line_text(&event);
            capture.send_engine_line(LogLine::new(
                name.as_str(),
                LogStream::Engine,
                text,
                event.at.clone(),
            ));
        }
        Ok(event)
    }

    /// The output-capture handle this engine session currently holds for
    /// `name`, if any (story 4-2, Task 4) — `None` when the instance has no
    /// `self.running` entry (not started this session, already torn down, or
    /// adopted with no recoverable capture pipeline).
    fn log_capture_for(&self, name: &InstanceName) -> Option<LogCapture> {
        self.running
            .get(name)
            .and_then(|s| self.backend.log_capture(&s.handle))
    }

    /// Land a spawn failure in `failed` with the backend diagnostic preserved
    /// (AC2), returning the [`EngineError::LaunchFailed`] to surface.
    fn fail_launch(
        &self,
        registry: &Registry,
        name: &InstanceName,
        err: &BackendError,
    ) -> EngineError {
        self.fail_launch_detail(registry, name, err.to_string())
    }

    /// Land a launch failure in `failed` with `detail` preserved (AC2).
    ///
    /// Records the `starting → failed` transition (cause = launch-error, detail
    /// verbatim) and returns [`EngineError::LaunchFailed`]. If persisting the
    /// failed state itself errors, that store error is surfaced instead (it is
    /// the more fundamental problem).
    fn fail_launch_detail(
        &self,
        registry: &Registry,
        name: &InstanceName,
        detail: String,
    ) -> EngineError {
        if let Err(e) = self.transition(
            registry,
            name,
            LifecycleState::Starting,
            LifecycleState::Failed,
            TransitionCause::launch_error(detail.clone()),
        ) {
            return e;
        }
        EngineError::LaunchFailed {
            name: name.as_str().to_string(),
            detail,
        }
    }

    /// Start the loopback forward listener for an `engine-observed` instance
    /// (story 3-4, AC-A/AC-B/AC6), or return `Ok(None)` for a `self-reported`
    /// instance (whose start path is UNCHANGED). Runs at `starting`, BEFORE any
    /// persisted state change, so a failure rejects the start cleanly.
    ///
    /// For an `engine-observed` instance it: (1) resolves the operator-configured
    /// real upstream provider URL (`metering.upstream_base_url`) from `effective`;
    /// (2) requires the engine runtime handle (the listener's accept loop runs on
    /// it) — absent → a clear error (only the handle-less unit-test supervisor lacks
    /// it, and it never starts an observed instance); (3) binds `127.0.0.1:0`
    /// (loopback ONLY — AC-B) and spawns the accept loop. Every failure maps to a
    /// TRAFFIC-FREE [`EngineError::ObservedMetering`] (no body/header/key — 2-4
    /// no-leak). The returned [`ObservedListener`] is moved into `Supervised`; its
    /// `base_url` is what the caller injects via the config-mapping (AC6).
    fn start_observed_listener(
        &self,
        name: &InstanceName,
        metering_source: &str,
        effective: &crate::domain::EffectiveConfig,
    ) -> Result<Option<ObservedListener>, EngineError> {
        // Only an `engine-observed` instance runs a listener. `self-reported`
        // (and any other) leaves it None — its start path is byte-unchanged.
        if metering_source != "engine-observed" {
            return Ok(None);
        }
        // The operator MUST configure the real upstream provider URL (there is
        // nowhere to forward otherwise). Absent → a clear start error naming the key.
        let upstream = config::resolve_upstream_base_url(effective).ok_or_else(|| {
            EngineError::ObservedMetering {
                name: name.as_str().to_string(),
                detail: format!(
                    "no upstream provider URL configured; set `{}` to the agent's real \
                     OpenAI-compatible endpoint",
                    crate::domain::METERING_UPSTREAM_BASE_URL_KEY
                ),
            }
        })?;
        // The listener's accept loop runs on the engine runtime; the sync start path
        // (on the blocking pool) cannot use `Handle::current`, so the engine threads
        // its handle in (`with_runtime`). A handle-less supervisor cannot observe.
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| EngineError::ObservedMetering {
                name: name.as_str().to_string(),
                detail: "the engine has no runtime handle to run the loopback listener \
                     (engine-observed metering requires the async engine)"
                    .to_string(),
            })?;
        // Bind loopback + spawn. A ListenerError is TRAFFIC-FREE by construction
        // (bind/upstream-shape only — never a body/header/key), so mapping it into
        // the detail cannot leak a secret (2-4 rigor).
        let listener = ObservedListener::start(runtime, upstream).map_err(|e: ListenerError| {
            EngineError::ObservedMetering {
                name: name.as_str().to_string(),
                detail: e.to_string(),
            }
        })?;
        Ok(Some(listener))
    }

    /// Watch a freshly spawned process for [`READINESS_WINDOW`]. Returns
    /// `Some(exit_code)` if the process died within the window (a launch failure,
    /// AC2 — the inner `Option<i32>` is the OS exit code, `None` if killed by a
    /// signal with no code), or `None` if it stayed alive the whole window
    /// (ready). Reaps on exit (no zombie).
    fn watch_startup(&self, handle: &mut backends::Handle) -> Option<Option<i32>> {
        let deadline = std::time::Instant::now() + READINESS_WINDOW;
        loop {
            match self.backend.poll(handle) {
                Ok(ProcessStatus::Exited { code }) => return Some(code),
                Ok(ProcessStatus::Alive) => {}
                // A poll error during startup is treated as still-alive; the next
                // stop/poll will surface a real problem. Don't fail the start on a
                // transient poll hiccup.
                Err(_) => {}
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(READINESS_POLL);
        }
    }

    /// Ensure the per-instance log directory exists (AD-12 seed).
    fn ensure_log_dir(&self, registry: &Registry, name: &InstanceName) -> Result<(), EngineError> {
        let dir = registry.instance_log_dir(name);
        std::fs::create_dir_all(&dir).map_err(|e| EngineError::Log {
            name: name.as_str().to_string(),
            path: dir.to_string_lossy().into_owned(),
            detail: e.to_string(),
        })
    }

    // ---- Self-reported usage ingestion → the ONE ledger-commit choke point ----
    //      (story 3-1, spine AD-6/AD-7/AD-12)

    /// The current byte length of an instance's agent-output log, or 0 if it does
    /// not exist yet. Used to set the ingestion cursor at a Run's start so a new
    /// Run never re-reads a prior Run's already-captured lines.
    fn agent_log_len(&self, registry: &Registry, name: &InstanceName) -> u64 {
        std::fs::metadata(registry.agent_output_log_path(name))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Drain self-reported usage from EVERY currently-running instance (the reaper
    /// cadence). Best-effort per instance — one instance's drain failure never
    /// blocks another's or crash detection.
    ///
    /// This is the MID-RUN cadence: the process is (believed) still alive, so a
    /// half-written final line is left for the next pass ([`DrainMode::MidRun`]).
    fn drain_usage_all(&mut self, registry: &Registry) {
        let names: Vec<InstanceName> = self.running.keys().cloned().collect();
        for name in names {
            self.drain_usage_for(registry, &name, DrainMode::MidRun);
        }
    }

    /// Drain the NEWLY-captured tail of one instance's agent-output log, ingesting
    /// each well-formed usage sentinel line through the commit choke point
    /// ([`Supervisor::ingest_usage`]), and advance the read cursor.
    ///
    /// Reads from the per-instance cursor to the file's end (only the bytes written
    /// since the last drain), parses usage lines via the self-reported
    /// [`UsageSource`](crate::ports::UsageSource), and records each. A read error
    /// (log gone / unreadable) is a best-effort skip — the DB is the source of
    /// truth, and the next pass retries. Malformed usage lines are skipped inside
    /// the parser (a diagnostic, never fatal — AD-12).
    ///
    /// **AI-41 (billing honesty): the cursor only advances past DURABLE bytes.**
    /// Every parsed event is ingested FIRST; the cursor moves past the consumed
    /// block only when every event committed (or was a recognized duplicate
    /// replay). A store error parks the cursor where it was — the failed event is
    /// retried on the next drain, and already-committed neighbors in the same
    /// block re-drift safely into the DB dedup key (`DuplicateReplay`, never a
    /// double-count). The pre-fix advance-then-ingest order silently dropped any
    /// event whose INSERT failed.
    ///
    /// The `mode` decides how the TAIL is treated (story 3-1 under-count fix, H1):
    /// * [`DrainMode::MidRun`] — the process may still be mid-`writeln!`, so only
    ///   bytes UP TO the last newline are consumed; a partial trailing line waits
    ///   for the next drain (it lands whole then).
    /// * [`DrainMode::Terminal`] — the process is DEAD (drain-on-stop / drain-on-
    ///   reap); no more bytes will ever append, so a final usage line flushed
    ///   WITHOUT a trailing newline is consumed to end-of-log rather than stranded
    ///   (which the next Run's cursor would skip past → a permanent under-count).
    ///
    /// Log-shrink guard (M2): if the file is shorter than the cursor (a truncate /
    /// rotation — nothing in-tree does this yet; Epic 4 owns rotation), we do NOT
    /// re-read from 0 under the same live `run_id` (that would re-ingest already-
    /// counted lines → a double-count, an INFLATED bill). We instead treat it as an
    /// anomaly: advance the cursor to the new length and ingest nothing this pass.
    /// Proper rotation handling is deferred to Epic 4.
    fn drain_usage_for(&mut self, registry: &Registry, name: &InstanceName, mode: DrainMode) {
        // Only running/adopted instances have a cursor + metering context.
        let (cursor, run_id, metering_source) = match self.running.get(name) {
            Some(s) => (s.usage_cursor, s.run_id.clone(), s.metering_source.clone()),
            None => return,
        };
        let path = registry.agent_output_log_path(name);
        // AI-63 (billing-critical stall fix): read ONLY the tail written since the
        // last drain (`[cursor, len)`) — NEVER the whole never-rotated `agent.log`.
        // `read_usage_tail` returns the SAME bytes the old whole-file read's
        // `bytes[cursor..]` slice held (and catches the M2 shrink, where
        // `len - cursor` would underflow), so the BILLING decision below —
        // `plan_drain` on those exact tail bytes — is byte-identical to before.
        // See `read_usage_tail`'s docs for the full equivalence + snapshot proof.
        let tail = match read_usage_tail(&path, cursor) {
            // Read error: best-effort skip, cursor untouched, retry next pass —
            // identical to the old `let Ok(bytes) = std::fs::read(..) else { return }`.
            UsageTail::Unavailable => return,
            // M2 shrink guard: snap the cursor to the file's new (shorter) length
            // and ingest nothing — identical to the old `DrainPlan::Shrunk { .. }`
            // arm (never re-read from 0 under the same live `run_id` → no
            // double-count → no inflated bill).
            UsageTail::Shrunk { new_cursor } => {
                if let Some(s) = self.running.get_mut(name) {
                    s.usage_cursor = new_cursor;
                }
                return;
            }
            UsageTail::Tail { bytes } => bytes,
        };
        // `tail` == the old code's `bytes[cursor..]`. Feeding it to `plan_drain`
        // with a 0 base makes the SAME `(bytes, cursor, mode)` decision on the same
        // bytes, just in tail-relative coordinates: the returned `range` slices
        // `tail` directly (0-based), and the returned count is added to `cursor` to
        // recover the ABSOLUTE cursor. The MidRun (up-to-last-newline) and Terminal
        // (whole tail, incl. a newline-less final line — H1) rules are computed
        // purely from these bytes, so BOTH are preserved unchanged.
        match plan_drain(&tail, 0, mode) {
            DrainPlan::Consume {
                range,
                new_cursor: consumed,
            } => {
                let block = String::from_utf8_lossy(&tail[range]);
                let parsed = self.usage_source.drain(&block);
                // AI-41 (billing honesty): ingest FIRST; advance the cursor past
                // the consumed block ONLY when every parsed event committed (or
                // was a recognized duplicate replay). The pre-fix code advanced
                // the cursor BEFORE ingesting, so a store error silently DROPPED
                // the event(s) behind it — usage that never reached the ledger
                // and was never retried. Now a store error PARKS the cursor
                // where it was: the failed event is retried on the next drain,
                // and any already-committed neighbor in the same block re-drifts
                // safely into the DB dedup key (`DuplicateReplay` — never a
                // double-count; see the design note on AI-41's dedup safety).
                let mut committed_count = 0usize;
                for usage in &parsed {
                    if self
                        .ingest_usage(registry, name, &run_id, &metering_source, usage, mode)
                        .is_ok()
                    {
                        committed_count += 1;
                    } else {
                        // Park at the FIRST store error — later events in this
                        // block are not even attempted, so they cannot leapfrog
                        // the failed one.
                        break;
                    }
                }
                // `cursor + consumed` equals the old whole-file path's
                // `new_cursor` (which was `cursor + consumed`) exactly —
                // reached only when every event behind the advance is durable.
                if committed_count == parsed.len() {
                    if let Some(s) = self.running.get_mut(name) {
                        s.usage_cursor = cursor + consumed;
                        s.usage_park_attempts = None;
                    }
                } else if mode == DrainMode::Terminal {
                    // AI-41 (loop 1): on the TERMINAL drain there IS no next
                    // pass — the handle is being removed right after, and with
                    // it the cursor and the Run context. The park-and-retry
                    // claim must not silently fail exactly where loss is
                    // likeliest: say the batch is lost, out loud.
                    let lost = parsed.len() - committed_count;
                    let loss = format!(
                        "{}: {lost} usage event(s) in the final drained block could not be \
                         committed to the Usage Ledger and were NOT counted — this is the \
                         terminal drain (the process is dead or the handle is being removed), \
                         so the batch cannot be retried and is lost",
                        name.as_str(),
                    );
                    self.emit_diagnostic(&loss);
                } else {
                    // AI-41 (loop 2): BOUND the park. A permanently failing
                    // event (a row poisoned beyond what the dedup key covers)
                    // would otherwise wedge the cursor at this offset forever —
                    // silently stranding every later usage event for the Run.
                    // After USAGE_PARK_MAX_ATTEMPTS failed passes AT THE SAME
                    // offset, skip the block with a loud diagnostic: billing
                    // honesty cuts both ways — announce the loss, don't strand
                    // the ledger.
                    let attempts = match self.running.get(name).and_then(|s| s.usage_park_attempts)
                    {
                        Some((parked_cursor, n)) if parked_cursor == cursor => n + 1,
                        _ => 1,
                    };
                    if attempts >= USAGE_PARK_MAX_ATTEMPTS {
                        let lost = parsed.len() - committed_count;
                        if let Some(s) = self.running.get_mut(name) {
                            s.usage_cursor = cursor + consumed;
                            s.usage_park_attempts = None;
                        }
                        let skip = format!(
                            "{}: {lost} usage event(s) at byte offset {cursor} failed to \
                             commit on {attempts} consecutive drains and are SKIPPED (not \
                             counted) — the cursor moves past them so the rest of the Run's \
                             usage keeps counting; investigate the Usage Ledger store",
                            name.as_str(),
                        );
                        self.emit_diagnostic(&skip);
                    } else if let Some(s) = self.running.get_mut(name) {
                        s.usage_park_attempts = Some((cursor, attempts));
                    }
                }
            }
            // Nothing to consume this pass (an empty tail, or a MidRun tail with no
            // newline yet) — leave the cursor where it is, exactly as before.
            DrainPlan::Nothing => {}
            // Unreachable by construction: `plan_drain` returns `Shrunk` only when
            // its `cursor` argument exceeds the slice length, and the base here is
            // 0 (`0 > len` is impossible). The REAL shrink is handled above in
            // `read_usage_tail`, where the file length is known WITHOUT a whole-file
            // read. Leave the cursor untouched (nothing was ingested, so no
            // miscount) — a billing path must never panic.
            DrainPlan::Shrunk { .. } => {}
        }
    }

    /// Drain ENGINE-OBSERVED usage from EVERY currently-running observed instance
    /// (story 3-4 — the reaper cadence, parallel to [`Self::drain_usage_all`]).
    /// Best-effort per instance — one instance's drain never blocks another's or
    /// crash detection. A `self-reported` instance (no observed listener) is a
    /// no-op here (it rides the log-tail drain instead).
    fn drain_observed_all(&mut self, registry: &Registry) {
        let names: Vec<InstanceName> = self.running.keys().cloned().collect();
        for name in names {
            self.drain_observed_for(registry, &name, DrainMode::MidRun);
        }
    }

    /// Drain one instance's OBSERVED usage queue (the counts the loopback listener
    /// parsed out of the agent's model traffic) into the SAME [`Self::ingest_usage`]
    /// choke point (story 3-4), minting the per-Run `sequence` for each.
    ///
    /// The listener task PUSHES each parsed `(input, output)` pair; this reaper pass
    /// DRAINS the queue (event-driven, NOT the log-tail path — observed usage does
    /// NOT ride the agent-output log, AD-12 contrast), the [`ObservedUsageSource`]
    /// mints the engine-side `sequence` (the agent supplies none), and each becomes
    /// a `ParsedUsage` fed to `ingest_usage` under the instance's CURRENT Run id +
    /// `engine-observed` source. NO new ledger writer, NO new enforcement path — the
    /// SAME choke point stamps + records + enforces (so 3-2 budgets + 3-3 caps apply
    /// unchanged). A `self-reported` instance (no `observed_source`/`observed_listener`)
    /// is a no-op.
    ///
    /// **Story 12-4 — the AI-41 treatment for the observed channel (durability
    /// under store failure):** a commit failure PARKS the un-committed minted
    /// events (in [`Supervised::observed_park`]) and RETRIES the EXACT same
    /// `ParsedUsage` values on the next pass — never re-minted, so the dedup
    /// keys are stable and an already-committed neighbor re-drifts safely into
    /// the DB dedup (a `DuplicateReplay`, never a double-count). The retry is
    /// BOUNDED: after [`USAGE_PARK_MAX_ATTEMPTS`] consecutive failed passes at
    /// the SAME front event, that event is SKIPPED with a loud diagnostic and
    /// the events behind it keep counting (surfaced-not-silent: an announced
    /// loss, never a silent drop, never a wedged drain). In the TERMINAL mode
    /// (stop / crash-reap) there IS no next pass — the loss is announced
    /// explicitly and the parked buffer dies with the instance. The mode is the
    /// [`DrainMode`] analog of the self-reported drain's.
    fn drain_observed_for(&mut self, registry: &Registry, name: &InstanceName, mode: DrainMode) {
        // Read the Run context; park-retry first, then drain the queue, all under
        // the instance's held state (a short critical section). Ingest happens
        // OUTSIDE the borrow so `ingest_usage` can take `&mut self`.
        let (run_id, metering_source, events) = match self.running.get(name) {
            Some(s) => {
                // Only an observed instance has both a listener (its queue) + a source
                // (the sequence minter). A self-reported instance skips (no-op).
                let (Some(listener), Some(source)) =
                    (s.observed_listener.as_ref(), s.observed_source.as_ref())
                else {
                    return;
                };
                // 12-4: retry any PARKED events FIRST, preserving order — the exact
                // same `ParsedUsage` values as the failed pass (never re-minted).
                let mut events = s
                    .observed_park
                    .as_ref()
                    .map(|(pending, _)| pending.events.clone())
                    .unwrap_or_default();
                // Then drain the NEW queue entries and mint their per-Run sequences.
                // A poisoned queue lock is a best-effort skip of the DRAIN — any
                // parked events still retry below, and the queued pairs stay queued
                // for the next pass (never silently lost).
                let drained: Vec<(u64, u64)> = match listener.queue().lock() {
                    Ok(mut q) => q.drain(..).collect(),
                    Err(_) => Vec::new(),
                };
                for (input, output) in drained {
                    events.push(source.mint(input, output));
                }
                if events.is_empty() {
                    return;
                }
                (s.run_id.clone(), s.metering_source.clone(), events)
            }
            None => return,
        };
        // Ingest each observed event through the SAME single choke point (stamps the
        // Run id + `engine-observed` source + timestamp, records, and enforces).
        // Story 12-4: break on the FIRST store error — later events cannot leapfrog
        // the failed one (billing honesty: the ledger sequence stays order-faithful).
        let mut committed_count = 0usize;
        for usage in &events {
            if self
                .ingest_usage(registry, name, &run_id, &metering_source, usage, mode)
                .is_ok()
            {
                committed_count += 1;
            } else {
                break;
            }
        }
        if committed_count == events.len() {
            // Every event durable (or a recognized duplicate replay): the park
            // clears.
            if let Some(s) = self.running.get_mut(name) {
                s.observed_park = None;
            }
        } else if mode == DrainMode::Terminal {
            // 12-4, the terminal arm: there IS no next pass — the handle (and any
            // parked buffer) is dropped right after. Announce the loss explicitly,
            // never a silent drop and never a false retry claim, and drop the park
            // (no phantom retry state survives the instance). (The local is
            // deliberately NOT named `loss`: the embed-clean audit pins
            // `emit_diagnostic(&loss)` to the ONE AI-41 self-reported site, and a
            // duplicate marker would read as a route regression.)
            let lost = events.len() - committed_count;
            let observed_loss = format!(
                "{}: {lost} observed usage event(s) could not be committed to the Usage \
                 Ledger and are LOST — this is the terminal drain (the process is dead or \
                 the handle is being removed, and any parked buffer is dropped with it), \
                 so they cannot be retried",
                name.as_str(),
            );
            self.emit_diagnostic(&observed_loss);
            if let Some(s) = self.running.get_mut(name) {
                s.observed_park = None;
            }
        } else {
            // 12-4, the bounded MIDRUN park: park the un-committed tail (including
            // the failed front) and count the attempt against the SAME front event.
            // The 12-4 AMENDMENT cap runs FIRST — a store outage keeps minting
            // events every tick, so the tail itself must be bounded (overflow
            // drops the OLDEST with a loud diagnostic; see the helper).
            let remaining: Vec<ParsedUsage> =
                self.cap_observed_park_buffer(name, events[committed_count..].to_vec());
            if remaining.is_empty() {
                // The cap dropped EVERYTHING (a backlog larger than the whole
                // buffer, loudly announced by the helper): no park state
                // survives this pass — never a phantom one-front buffer.
                if let Some(s) = self.running.get_mut(name) {
                    s.observed_park = None;
                }
                return;
            }
            let front_sequence = remaining[0].sequence;
            let attempts = match self
                .running
                .get(name)
                .and_then(|s| s.observed_park.as_ref())
            {
                Some((pending, n)) if pending.front_sequence == front_sequence => n + 1,
                // A different front event resets the streak (the cursor-analog).
                _ => 1,
            };
            if attempts >= USAGE_PARK_MAX_ATTEMPTS {
                // The front event is poisoned beyond the dedup key's reach: SKIP it
                // LOUDLY and keep counting the rest (an announced loss — billing
                // honesty cuts both ways — never a wedged drain). The events BEHIND
                // the skipped front park again with a fresh streak (their front
                // changed), so they retry on the next pass.
                let rest: Vec<ParsedUsage> = remaining[1..].to_vec();
                let skip = format!(
                    "{}: an observed usage event (sequence {front_sequence}) failed to \
                     commit on {attempts} consecutive drains and is SKIPPED (not \
                     counted) — the rest of the Run's observed usage keeps counting; \
                     investigate the Usage Ledger store",
                    name.as_str(),
                );
                self.emit_diagnostic(&skip);
                if let Some(s) = self.running.get_mut(name) {
                    let next_front = rest.first().map(|next| next.sequence);
                    s.observed_park = next_front.map(|front_sequence| {
                        (
                            ObservedPending {
                                front_sequence,
                                events: rest.clone(),
                            },
                            1,
                        )
                    });
                }
            } else if let Some(s) = self.running.get_mut(name) {
                s.observed_park = Some((
                    ObservedPending {
                        front_sequence,
                        events: remaining,
                    },
                    attempts,
                ));
            }
        }
    }

    /// Story 12-4 AMENDMENT (review loop 1): bound the observed pending-park
    /// buffer to [`OBSERVED_PARK_MAX_EVENTS`]. Returns the events to park; when
    /// the tail exceeds the cap, the OLDEST events are dropped (the ledger's
    /// order-faithful sequence discipline cuts both ways: the newest events are
    /// the ones still reachable for retry) and the loss is announced LOUDLY —
    /// the count, the dropped sequence range, the why, and the remediation —
    /// never a silent truncation, never unbounded engine memory during a store
    /// outage.
    fn cap_observed_park_buffer(
        &mut self,
        name: &InstanceName,
        mut events: Vec<ParsedUsage>,
    ) -> Vec<ParsedUsage> {
        let overflow = events.len().saturating_sub(OBSERVED_PARK_MAX_EVENTS);
        if overflow > 0 {
            let dropped = events.drain(..overflow).collect::<Vec<_>>();
            let first = dropped.first().map(|e| e.sequence).unwrap_or(0);
            let last = dropped.last().map(|e| e.sequence).unwrap_or(0);
            let overflow_note = format!(
                "{}: {overflow} parked observed usage event(s) (sequences {first}..{last}) \
                 were DROPPED and are NOT counted — the pending buffer is capped at \
                 {OBSERVED_PARK_MAX_EVENTS} events while the Usage Ledger store keeps \
                 failing, so the oldest un-committed events had to go; investigate the \
                 store outage",
                name.as_str(),
            );
            self.emit_diagnostic(&overflow_note);
        }
        events
    }

    /// THE ledger-commit choke point (story 3-1, spine AD-7) — the SOLE writer of
    /// the `usage_events` table.
    ///
    /// Constructs the full [`UsageEvent`] from the agent-supplied [`ParsedUsage`]
    /// plus the engine-stamped fields (the current Run id, the instance name, the
    /// metering source, and the commit timestamp), then records it in its OWN
    /// transaction via `record_usage_event` (AD-6: one transaction per event). A
    /// re-delivered batch is classified [`RecordOutcome::DuplicateReplay`] by the
    /// DB `UNIQUE` index and is a no-op (AC-A no-double-count). On a fresh insert it
    /// builds the AD-14 [`UsageUpdateEvent`] (the wire shape frozen in 3-1;
    /// delivered on the event bus since story 7-2).
    ///
    /// **AI-41 — a store error is REPORTED, never silently dropped:** the method
    /// returns the failure so the self-reported drain can PARK its cursor
    /// and retry the event on the next pass (the observed drain reports + skips —
    /// it has no cursor). A diagnostic is emitted through the AD-12 sink either
    /// way, so dropped usage is always VISIBLE. Usage ingestion still never
    /// crashes the supervisor or a lifecycle op (the ledger is advisory to the
    /// RUN, not gating it) — honest reporting, not a panic.
    ///
    /// **12-4 AMENDMENT (review loop 1) — the diagnostic is MODE-aware:** the
    /// `mode` the CALLER is draining under decides what the failure text may
    /// honestly claim. A `MidRun` caller WILL re-attempt, so the text says the
    /// event "is parked for retry on the next drain". A `Terminal` caller
    /// (stop / crash-reap) has NO next pass — its text says the event is LOST
    /// and will NOT be retried — because a terminal diagnostic that promised a
    /// retry would contradict the adjacent LOST notice and lie to the operator
    /// (surfaced-not-silent means the surface must also be TRUE).
    ///
    /// **The AD-7 single-writer invariant lives here:** no other code path may call
    /// `record_usage_event`.
    ///
    /// **The AD-7 ENFORCEMENT stage lives here too (story 3-2):** IMMEDIATELY after
    /// a fresh `Inserted` commit — in the SAME synchronous path, before returning —
    /// this method reads the CURRENT resolved [`TokenBudget`] + [`BreachAction`]
    /// (a LIVE config read, so a budget changed while `running` applies on the very
    /// next event — AC-B), reads the just-committed per-run + cumulative token
    /// totals (3-1's `usage_totals`/`run_totals`), and calls the pure
    /// [`BudgetEvaluator`]. On a [`BreachDecision::Breached`] it RECORDS the breach
    /// event FIRST/independently ([`Self::record_breach`]) — so a best-effort/
    /// unsupported/failed pause never loses the breach record (FR-21 "always
    /// recorded regardless of action") — and THEN executes the action via Epic-1's
    /// lifecycle (`pause`/`stop`/`warn`). This is the SOLE enforcement site (the
    /// AD-7 companion to the single-writer invariant). A [`RecordOutcome::DuplicateReplay`]
    /// is NOT evaluated (nothing new was committed → no new breach can occur).
    /// Ingestion + enforcement stay best-effort to the RUN: a store/lifecycle error
    /// is a diagnostic, NEVER a supervisor crash (3-1's rule extended to enforcement).
    fn ingest_usage(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        metering_source: &str,
        parsed: &ParsedUsage,
        mode: DrainMode,
    ) -> Result<Option<UsageUpdateEvent>, super::error::RegistryError> {
        let event = assemble_usage_event(
            parsed,
            name.as_str(),
            run_id.clone(),
            metering_source,
            now_rfc3339(),
        );
        // Story 3-3 — NO-RETROACTIVE-REPRICING: resolve the EFFECTIVE Rate at COMMIT
        // (a live config read) and PERSIST it onto this row, so historical dollars
        // keep the Rate in force when consumed. A later Rate change re-prices FUTURE
        // events only (each row is priced at its own stored Rate on read). A degraded
        // config read / absent-or-half Rate → `None` (the row contributes $0; AC-B).
        let rate = registry
            .effective_config(name, ConfigLayer::empty())
            .ok()
            .and_then(|eff| config::resolve_cost(&eff).0);
        match registry.record_usage_event(&event, rate) {
            // A fresh row: build the AD-14 usage-update wire struct (frozen in 3-1;
            // published on the event bus since 7-2), THEN run the AD-7 enforcement
            // stage on the just-committed totals — synchronously, in this same
            // commit path.
            Ok(RecordOutcome::Inserted) => {
                let update = UsageUpdateEvent::new(event);
                // Story 7-2: the ledger row COMMITTED — publish onto the event
                // bus BEFORE the enforcement stage runs, preserving commit
                // order (the usage row precedes any breach/transition the
                // enforcement commits, so the bus shows exactly the durable
                // sequence: usage → token breach → pause → dollar breach).
                // Ordering obligation: this runs under the supervisor lock
                // (see the `domain::bus` caller-enforced invariant).
                self.publish(EngineEvent::UsageUpdate(update.clone()));
                self.enforce_budget(registry, name, run_id, metering_source);
                Ok(Some(update))
            }
            // A recognized replay — no double-count, no event emitted (nothing new
            // was committed). This is the AC-A guarantee in action; the evaluator is
            // NOT run (AC5 — no new total, no new breach).
            Ok(RecordOutcome::DuplicateReplay) => Ok(None),
            // AI-41: a store error must never SILENTLY drop the event. Report it
            // (diagnostic + Err) so the drains can park and retry the exact same
            // event on the next pass — the self-reported drain (its byte cursor)
            // since AI-41, and the OBSERVED drain (its pending buffer) since story
            // 12-4. Never a supervisor crash.
            Err(err) => {
                // Caller-factual (AI-41 loop 1; MODE-aware per the 12-4
                // AMENDMENT): a MIDRUN drain will really retry, so the text
                // says the event is parked for the next pass and not counted
                // until that commit succeeds (it does not claim the retry can
                // never fail — the bound skips loudly). A TERMINAL drain has
                // NO next pass, so its text must never claim "parked for
                // retry" (that would contradict the adjacent LOST notice) —
                // it names the loss and the why instead.
                let failure = if mode == DrainMode::Terminal {
                    format!(
                        "{}: a usage event could not be committed to the Usage Ledger: \
                         {err} — this is the terminal drain (the process is dead or the \
                         handle is being removed), so the event is LOST and will NOT be \
                         retried",
                        name.as_str(),
                    )
                } else {
                    format!(
                        "{}: a usage event could not be committed to the Usage Ledger: {err} — \
                         it is parked for retry on the next drain and is NOT counted until \
                         that commit succeeds",
                        name.as_str(),
                    )
                };
                self.emit_diagnostic(&failure);
                Err(err)
            }
        }
    }

    /// The AD-7 ENFORCEMENT stage (story 3-2 tokens + story 3-3 dollars), run INSIDE
    /// [`Self::ingest_usage`] right after a fresh commit — the SOLE place a budget or
    /// Cost Cap is evaluated + a Breach Action fired.
    ///
    /// Reads the CURRENT resolved budget/Rate/cap + action (live, AC-B), reads the
    /// committed per-run + cumulative totals, evaluates purely (TOKENS then DOLLARS,
    /// in the SAME choke point), and on a breach records the event FIRST then
    /// executes the action. Every step is best-effort to the RUN: a failed config
    /// read / totals read / lifecycle op is a diagnostic, never a crash (AD-12). A
    /// no-budget + no-cap instance evaluates to `WithinBudget` for both — so the
    /// common path is a cheap config read + two pure comparisons and nothing else.
    ///
    /// **STORY 3-3 — the DOLLAR evaluation folds in HERE (AD-7, no new path):** after
    /// the token evaluation, IF a [`Rate`](super::cost::Rate) is present AND the
    /// [`CostCap`](super::cost::CostCap) `is_set()`, derive the per-run + cumulative
    /// COST (each row priced at its own persisted Rate — no retro-repricing) and run
    /// the pure [`CostEvaluator`]. NO Rate ⇒ dollar enforcement is SKIPPED entirely
    /// (AC-B inert — a `CostCap` with no Rate cannot be enforced). Both dimensions
    /// reuse the SAME record-first-then-act path + the SAME per-Run latch, keyed by
    /// `(dimension, scope)` so a token breach and a dollar breach of the same scope
    /// each fire ONCE per Run (both can fire on the same event; the action is
    /// identical).
    ///
    /// **Idempotence — at most one breach per (dimension, scope) per Run:** this runs
    /// on EVERY committed usage event, so once a total crosses a ceiling every
    /// subsequent event would re-evaluate to the SAME breach. The per-Run breach
    /// LATCH ([`Supervised::breached_scopes`], keyed by `(dimension, scope)`)
    /// short-circuits BOTH the [`Self::record_breach`] and the action for an
    /// already-fired pair; the latch resets when a new Run starts.
    fn enforce_budget(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        metering_source: &str,
    ) {
        // (1) LIVE config read (AC-B "changes apply immediately"): resolve the
        // CURRENT effective config ONCE, for BOTH the token budget and the dollar
        // Rate/cap. A malformed on-disk layer degrades to "no budget / no Rate"
        // (best-effort — never a crash mid-ingestion).
        let Ok(effective) = registry.effective_config(name, ConfigLayer::empty()) else {
            return;
        };
        let (budget, token_action) = config::resolve_token_budget(&effective);
        let (rate, cost_cap, cost_action) = config::resolve_cost(&effective);

        // Whether each dimension is ARMED: a token budget is armed when a ceiling is
        // set; the dollar cap is armed ONLY when BOTH a Rate is present AND a cap
        // scope is set (AC-B: a cap with no Rate is inert). If NEITHER is armed, skip
        // the totals reads entirely (the common un-governed path).
        let token_armed = budget.is_set();
        let dollar_armed = rate.is_some() && cost_cap.is_set();
        if !token_armed && !dollar_armed {
            return;
        }

        // (2) TOKEN dimension (story 3-2): the just-committed token totals + the pure
        // evaluator. Reuses the record-first-then-act helper with the tokens dimension.
        if token_armed {
            let run_total = registry
                .run_usage_totals(name, run_id)
                .map(|t| t.total_tokens())
                .unwrap_or(0);
            let cumulative_total = registry
                .usage_totals(name)
                .map(|t| t.total_tokens())
                .unwrap_or(0);
            let decision =
                BudgetEvaluator::evaluate(run_total, cumulative_total, &budget, token_action);
            if let BreachDecision::Breached {
                scope,
                action,
                limit,
                observed,
            } = decision
            {
                let cause = TransitionCause::budget_exceeded(scope, limit, observed);
                self.apply_breach(
                    registry,
                    name,
                    run_id,
                    BreachDimension::Tokens,
                    scope,
                    action,
                    cause,
                    metering_source,
                    // The token breach event carries token counts (no dollar fields).
                    |registry, sup, run_id, scope, action, src| {
                        sup.record_token_breach(
                            registry, name, run_id, scope, limit, observed, action, src,
                        );
                    },
                );
            }
        }

        // (3) DOLLAR dimension (story 3-3): derive the per-run + cumulative COST from
        // the ledger (each row priced at its own persisted Rate — no retro-repricing),
        // then the pure CostEvaluator. Only when a Rate is present AND the cap is set
        // (AC-B inert otherwise). v1 the estimate label is always `estimated`.
        if dollar_armed {
            let run_cost = registry
                .run_cost_totals(name, run_id)
                .unwrap_or(Micros::ZERO);
            let cumulative_cost = registry.cost_totals(name).unwrap_or(Micros::ZERO);
            let decision =
                CostEvaluator::evaluate(run_cost, cumulative_cost, &cost_cap, cost_action);
            if let BreachDecision::Breached {
                scope,
                action,
                limit,
                observed,
            } = decision
            {
                let label = EstimateLabel::Estimated;
                let cause = TransitionCause::cost_cap_exceeded(
                    scope,
                    Micros(limit as i64),
                    Micros(observed as i64),
                    label,
                );
                self.apply_breach(
                    registry,
                    name,
                    run_id,
                    BreachDimension::Dollars,
                    scope,
                    action,
                    cause,
                    metering_source,
                    // The dollar breach event carries integer micros + the label.
                    |registry, sup, run_id, scope, action, src| {
                        sup.record_cost_breach(
                            registry,
                            name,
                            run_id,
                            scope,
                            Micros(limit as i64),
                            Micros(observed as i64),
                            label,
                            action,
                            src,
                        );
                    },
                );
            }
        }
    }

    /// Apply ONE breach decision for a given `dimension` (story 3-3 shared path):
    /// consult the per-Run `(dimension, scope)` latch, and if this pair has NOT yet
    /// fired this Run, RECORD the breach (via `record`, the dimension-specific event
    /// writer) FIRST/INDEPENDENTLY and THEN execute the action via Epic-1's lifecycle
    /// (AD-15 — a REASON, not a new edge). A pair already latched short-circuits both.
    /// A missing `Supervised` (not currently supervised — a race with stop) declines
    /// to enforce. All best-effort: a lifecycle error is a diagnostic, never a crash.
    #[allow(clippy::too_many_arguments)]
    fn apply_breach(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        dimension: BreachDimension,
        scope: BreachScope,
        action: BreachAction,
        cause: TransitionCause,
        metering_source: &str,
        record: impl FnOnce(&Registry, &Self, &RunId, BreachScope, BreachAction, &str),
    ) {
        // IDEMPOTENCE LATCH (story 3-2/3-3): fire at most once per (dimension, scope)
        // per Run. Insert the pair; if it was already present, short-circuit.
        match self.running.get_mut(name) {
            Some(supervised) => {
                if !supervised.breached_scopes.insert((dimension, scope)) {
                    return;
                }
            }
            None => return,
        }
        // RECORD THE BREACH FIRST (AC7/AC10 / FR-21 "always recorded regardless of
        // action"), BEFORE the lifecycle side-effect, so a best-effort/unsupported/
        // failing pause never loses the record.
        record(registry, self, run_id, scope, action, metering_source);
        // EXECUTE THE ACTION via Epic-1's EXISTING lifecycle. The breach is already
        // recorded; a lifecycle error here is a best-effort diagnostic, never a crash.
        match action {
            BreachAction::Warn => {
                // No lifecycle transition — the breach event is the whole guardrail.
            }
            BreachAction::Pause => {
                self.enforce_pause(registry, name, cause);
            }
            BreachAction::Stop => {
                self.enforce_stop(registry, name, cause);
            }
        }
    }

    /// Execute a `pause` Breach Action honestly (story 3-2 AC6, honoring story
    /// 1-5's Capability Declaration). Drives `running → paused` and STAMPS the
    /// resulting transition with the [`TransitionCause::BudgetExceeded`] cause (so
    /// the lifecycle log explains WHY), via [`Self::pause`]. A best-effort pause
    /// still transitions (1-5) and the breach is already recorded; an UNSUPPORTED
    /// pause fails fast in [`Self::pause`] — we do NOT fake a pause and do NOT
    /// silently escalate to stop (AC6), we surface the honest diagnostic on the
    /// engine log (the breach event already captured the fact). All best-effort:
    /// never a supervisor crash.
    fn enforce_pause(&mut self, registry: &Registry, name: &InstanceName, cause: TransitionCause) {
        match self.pause_with_cause(registry, name, cause) {
            Ok(_) => {}
            Err(e) => {
                // Honest surface (AD-12): pause could not be honored (unsupported /
                // not running / backend hiccup). The breach is ALREADY recorded; log
                // and move on — no fake pause, no escalation.
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!("budget breach pause could not be honored: {e}"),
                );
            }
        }
    }

    /// Execute a `stop` Breach Action (story 3-2). Drives `running → stopping →
    /// stopped` (story 1-4) and, before that, records the [`TransitionCause::BudgetExceeded`]
    /// as the WHY marker on the `running → stopping` edge (the stop path itself
    /// records the graceful/forced escalation on the terminal edge). Best-effort:
    /// a stop error is logged, never a crash (the breach is already recorded).
    fn enforce_stop(&mut self, registry: &Registry, name: &InstanceName, cause: TransitionCause) {
        match self.stop_with_cause(registry, name, cause) {
            Ok(_) => {}
            Err(e) => {
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!("budget breach stop could not be honored: {e}"),
                );
            }
        }
    }

    /// Record a TOKEN [`BudgetBreachEvent`] (story 3-2, AC7) — the token-dimension
    /// event writer passed to [`Self::apply_breach`]. Builds the token breach struct
    /// (token `limit`/`observed`, no dollar fields) and persists it via
    /// [`Self::persist_breach_event`].
    #[allow(clippy::too_many_arguments)]
    fn record_token_breach(
        &self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        scope: BreachScope,
        limit: u64,
        observed: u64,
        action: BreachAction,
        metering_source: &str,
    ) {
        let event = BudgetBreachEvent::new(
            name.as_str(),
            run_id.as_str(),
            scope,
            limit,
            observed,
            action,
            metering_source,
            now_rfc3339(),
        );
        self.persist_breach_event(registry, name, &event);
    }

    /// Record a DOLLAR [`BudgetBreachEvent`] (story 3-3, AC10) — the dollar-dimension
    /// event writer passed to [`Self::apply_breach`]. Builds the dollar breach struct
    /// (integer-micro `dollar_limit`/`dollar_observed` + the [`EstimateLabel`]) and
    /// persists it via [`Self::persist_breach_event`]. NO `$` string, NO `f64` — the
    /// wire carries integer micros + the label (AD-14).
    #[allow(clippy::too_many_arguments)]
    fn record_cost_breach(
        &self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        scope: BreachScope,
        limit_micros: Micros,
        observed_micros: Micros,
        label: EstimateLabel,
        action: BreachAction,
        metering_source: &str,
    ) {
        let event = BudgetBreachEvent::new_cost(
            name.as_str(),
            run_id.as_str(),
            scope,
            limit_micros,
            observed_micros,
            label,
            action,
            metering_source,
            now_rfc3339(),
        );
        self.persist_breach_event(registry, name, &event);
    }

    /// Persist a built [`BudgetBreachEvent`] to the durable per-instance breach log
    /// (story 3-2 shared path, AC7 / FR-21 "always recorded regardless of action").
    /// Recorded for EVERY action (including `warn`) and BEFORE the lifecycle
    /// side-effect, so the breach is never lost.
    ///
    /// Non-fatal but NOT swallowed: this is the PRIMARY durable record of the breach
    /// (FR-21), so a write failure (disk full / IO / perms) must not vanish silently
    /// while the action still fires — that would lose the mandated record with no
    /// diagnostic. We keep enforcement acting (the write failure is NOT made fatal),
    /// but SURFACE the error on the engine-log stderr breadcrumb (mirroring how
    /// `enforce_pause`/`enforce_stop` log their best-effort diagnostics), so a lost
    /// breach record is visible to an operator. Both the dir-create and the append
    /// failure are surfaced.
    fn persist_breach_event(
        &self,
        registry: &Registry,
        name: &InstanceName,
        event: &BudgetBreachEvent,
    ) {
        let path = registry.instance_breach_log_path(name);
        // Ensure the log dir exists (a never-transitioned instance may lack it) —
        // non-fatal, mirroring `ensure_log_dir`, but a failure is surfaced (below) if
        // it then makes the append fail.
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!(
                        "could not create the breach-log directory {}: {e} — the breach \
                         record may be lost",
                        parent.display()
                    ),
                );
            }
        }
        // Surface (do NOT swallow) an append failure: the breach record is the FR-21
        // mandated durable artifact; a lost record with no diagnostic is the bug. Log
        // and move on — enforcement still acts.
        match append_breach_event(&path, event) {
            Ok(()) => {
                // Story 7-2: the append COMMITTED — publish onto the event bus
                // (after the durable record exists, never before; a failed
                // append publishes nothing). Ordering obligation: this runs
                // under the supervisor lock (see the `domain::bus`
                // caller-enforced invariant).
                self.publish(EngineEvent::BudgetBreach(event.clone()));
            }
            Err(e) => {
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!(
                        "could not record the budget breach event to {}: {e} — the mandated \
                         breach record was NOT written",
                        path.display()
                    ),
                );
            }
        }
    }

    /// Surface one enforcement diagnostic through the engine's diagnostic
    /// emission (AD-12: enforcement diagnostics ride the engine log / stderr —
    /// or the host's story-10-2 sink when one is installed — NEVER `kt`
    /// stdout, NEVER a crash). Used when a breach action (pause/stop) could
    /// not be honored — the breach itself is already durably recorded in the
    /// breach log, so this is only an operator breadcrumb, not the record of
    /// the breach. `registry` is unused (the diagnostic is not persisted to a
    /// strict-parse log to avoid corrupting the transition-event reader) but
    /// kept for signature symmetry with the other enforcement helpers.
    fn log_enforcement_diagnostic(&self, _registry: &Registry, name: &InstanceName, detail: &str) {
        self.emit_diagnostic(&format!("{}: {detail}", name.as_str()));
    }

    /// Read back the recorded [`BudgetBreachEvent`]s for an instance from its
    /// breach log (observation helper for tests / embedders — the AD-14 seed, NOT
    /// the 7-2 bus). Empty vec if none recorded yet.
    pub fn read_breach_events(
        registry: &Registry,
        name: &str,
    ) -> Result<Vec<BudgetBreachEvent>, EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let path = registry.instance_breach_log_path(&name);
        read_breach_events_from(&path).map_err(|detail| EngineError::Log {
            name: name.as_str().to_string(),
            path: path.to_string_lossy().into_owned(),
            detail,
        })
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the INVOCATION-OVERRIDE config layer injecting the engine-computed
/// START-TIME values the adapter's EXISTING config-mapping (2-2) delivers into
/// the agent's native mechanism:
///
/// * `base_url` — the engine-observed loopback listener address
///   `http://127.0.0.1:<port>` at the reserved [`METERING_BASE_URL_KEY`]
///   (`metering.base_url`, story 3-4, AC6);
/// * `memory_dir` — the managed Memory Backing directory path at the reserved
///   [`MEMORY_DIR_KEY`] (`memory.dir`, story 5-1, AD-11 Delivery clause).
///
/// Both keys are documented in [`config`] with the same contract: ENGINE-computed,
/// engine-INJECTED as an invocation override (the strongest layer — AD-9 — so a
/// hand-set lower-layer value can never win), KNOWN so a mapping can target them,
/// OPERATOR-does-NOT-set, and explicitly NOT touching the Adapter Contract surface
/// (no `CONTRACT_VERSION` bump). Because the mapping reads them as ordinary string
/// leaves, NO new contract surface is introduced.
///
/// Returns `None` when neither value applies (the overwhelmingly common start) so
/// the caller keeps using the plain operator config. Pure — builds a TOML table.
///
/// [`METERING_BASE_URL_KEY`]: crate::domain::METERING_BASE_URL_KEY
/// [`MEMORY_DIR_KEY`]: crate::domain::MEMORY_DIR_KEY
fn invocation_overrides(base_url: Option<&str>, memory_dir: Option<&Path>) -> Option<ConfigLayer> {
    if base_url.is_none() && memory_dir.is_none() {
        return None;
    }
    let mut table = toml::value::Table::new();
    if let Some(url) = base_url {
        // A DOTTED key (`metering.base_url`) is a nested table in TOML; build the
        // nested shape so `resolve` flattens it to the dotted leaf the mapping targets.
        let mut metering = toml::value::Table::new();
        metering.insert("base_url".to_string(), toml::Value::String(url.to_string()));
        table.insert("metering".to_string(), toml::Value::Table(metering));
    }
    if let Some(dir) = memory_dir {
        // Same dotted-key construction for `memory.dir`.
        let mut memory = toml::value::Table::new();
        memory.insert(
            "dir".to_string(),
            toml::Value::String(dir.to_string_lossy().into_owned()),
        );
        table.insert("memory".to_string(), toml::Value::Table(memory));
    }
    Some(ConfigLayer::from_table(table))
}

/// The DC-10 delivery-honesty decision (story 5-1): given the attached
/// filesystem backing's managed dir (present only when one is attached) and the
/// resolved config mapping, return the ONE stderr notice to emit when the
/// adapter declares no target for the reserved key. `None` means nothing to say
/// — either no filesystem backing is attached, or the mapping DOES target the
/// key and delivery is genuinely declared. Pure + deterministic (unit-tested);
/// the caller routes it through `emit_diagnostic` (story 10-2: stderr by
/// default, the host's sink when installed).
fn memory_delivery_notice(
    memory_dir: Option<&Path>,
    mapping: &ConfigMapping,
    name: &InstanceName,
) -> Option<String> {
    let dir = memory_dir?;
    if mapping.target(super::config::MEMORY_DIR_KEY).is_some() {
        return None;
    }
    Some(format!(
        "{}: a 'filesystem' Memory Backing is attached (managed directory: {}), but this \
         adapter declares no config mapping for the reserved key 'memory.dir', so the \
         agent will NOT receive the path. Add [config.\"memory.dir\"] env = \"...\" to its \
         manifest to deliver it.",
        name.as_str(),
        dir.display(),
    ))
}

/// The AI-27 shadow diff (story 11-2): the env keys the mapping application
/// OVERWROTE — present in the pre-apply launch env (`before`, snapshotted from
/// `launch.env` ahead of the application, i.e. whatever the persisted
/// registration snapshot carried) whose value in the post-apply env (`after`)
/// has CHANGED. The application only inserts, never removes, so
/// presence-in-both alone proves nothing (every base var survives the apply);
/// a changed VALUE is exactly "the config mapping replaced the pre-apply
/// value". A mapping that re-writes the identical value is not reported — the
/// start is observably unchanged, and the quiet path must stay quiet. Pure +
/// deterministic (sorted by the BTreeMap key iteration — the diagnostic is
/// stable); unit-tested next to the other start-seam decision fns. The caller
/// formats the ONE diagnostic and routes it through `emit_diagnostic`.
/// Precedence is deliberately UNCHANGED (the config value wins — the
/// last-write-wins insert); this helper exists only so the overwrite is named
/// on stderr instead of passing silently.
fn shadowed_env_keys(
    before: &std::collections::BTreeMap<String, String>,
    after: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    before
        .iter()
        .filter_map(|(key, base_value)| {
            let new_value = after.get(key)?;
            (new_value != base_value).then(|| key.clone())
        })
        .collect()
}

/// A best-effort, HUMAN-READABLE one-line rendering of a [`TransitionEvent`]
/// for the `engine`-attributed capture line (story 4-2, Task 4) — the
/// RECOMMENDED default: mirror every `TransitionEvent` (start/stop/pause/
/// resume/crash/restart/breach-driven), the SAME set `instance.log` already
/// records, so this is a projection of IDENTICAL facts, not a second,
/// divergent notion of "notable". `instance.log` stays the structured,
/// machine-authoritative record; this text is NEVER parsed back — a wording
/// change here is not a wire-format change.
fn engine_transition_line_text(event: &TransitionEvent) -> String {
    format!(
        "engine: {} -> {}{}",
        event.prior_state,
        event.new_state,
        cause_suffix(&event.cause)
    )
}

/// The parenthetical detail suffix for [`engine_transition_line_text`], keyed
/// on the transition's [`TransitionCause`].
fn cause_suffix(cause: &TransitionCause) -> String {
    match cause {
        TransitionCause::Command { command } => format!(" ({command})"),
        TransitionCause::AdapterReady => String::new(),
        TransitionCause::LaunchError { detail } => format!(" (launch error: {detail})"),
        TransitionCause::StopGraceful => String::new(),
        TransitionCause::StopForced { detail } => format!(" (forced: {detail})"),
        TransitionCause::PauseBestEffort { detail } => format!(" (best-effort: {detail})"),
        TransitionCause::ResumeBestEffort { detail } => format!(" (best-effort: {detail})"),
        TransitionCause::Crashed { detail } => format!(" (crashed: {detail})"),
        TransitionCause::Restarted { count, waited_ms } => {
            format!(" (restart #{count}, waited {waited_ms}ms)")
        }
        TransitionCause::BudgetExceeded {
            scope, dimension, ..
        } => format!(" (breach: {scope} {dimension})"),
    }
}

/// Append one transition event as a single JSON line to the instance log.
///
/// One event per line (JSON Lines) so [`read_events_from`] can parse them back
/// and a human can `tail` the file. Append-only (AD-12 seed; rotation/attach are
/// Epic 4).
fn append_event(path: &Path, event: &TransitionEvent) -> Result<(), String> {
    use std::io::Write;
    let line = serde_json::to_string(event).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(file, "{line}").map_err(|e| e.to_string())
}

/// Read back the JSON-Lines transition events from an instance log.
///
/// Missing file → empty vec (no events recorded yet). A malformed line is an
/// error naming it (a corrupt log is worth surfacing).
fn read_events_from(path: &Path) -> Result<Vec<TransitionEvent>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut events = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: TransitionEvent = serde_json::from_str(line)
            .map_err(|e| format!("corrupt instance-log line {}: {e}", idx + 1))?;
        events.push(event);
    }
    Ok(events)
}

/// Parse JSON-Lines [`LogLine`] records from `text`, APPENDING them to `out`
/// in encounter order (story 4-2, AC-G — append order is the sole ordering
/// authority; callers must never re-sort the result). Blank lines are
/// skipped; a malformed line is an error naming it (a corrupt capture is
/// worth surfacing, mirroring [`read_events_from`]'s convention).
fn parse_log_lines(text: &str, out: &mut Vec<LogLine>) -> Result<(), String> {
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: LogLine = serde_json::from_str(line)
            .map_err(|e| format!("corrupt output-log line {}: {e}", idx + 1))?;
        out.push(parsed);
    }
    Ok(())
}

/// Read back one attributed-output-log FILE (one generation) and append its
/// parsed [`LogLine`]s to `out` — a missing generation (not every generation
/// exists yet) is a silent no-op, mirroring [`read_events_from`]'s "missing
/// file → empty" precedent, so [`Supervisor::read_agent_log`]'s
/// oldest-to-newest loop can unconditionally probe every generation.
fn read_log_lines_from(path: &Path, out: &mut Vec<LogLine>) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    parse_log_lines(&text, out)
}

/// Append one [`BudgetBreachEvent`] as a single JSON line to the per-instance
/// breach log (story 3-2, AD-14). JSON Lines, append-only — the same shape as
/// [`append_event`] so a human can `tail` it and [`read_breach_events_from`] can
/// parse it back. The ALWAYS-recorded breach record (FR-21).
fn append_breach_event(path: &Path, event: &BudgetBreachEvent) -> Result<(), String> {
    use std::io::Write;
    let line = serde_json::to_string(event).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(file, "{line}").map_err(|e| e.to_string())
}

/// Read back the JSON-Lines [`BudgetBreachEvent`]s from an instance's breach log.
/// Missing file → empty vec (no breaches recorded yet). A malformed line is an
/// error naming it (a corrupt log is worth surfacing).
fn read_breach_events_from(path: &Path) -> Result<Vec<BudgetBreachEvent>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut events = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: BudgetBreachEvent = serde_json::from_str(line)
            .map_err(|e| format!("corrupt breach-log line {}: {e}", idx + 1))?;
        events.push(event);
    }
    Ok(events)
}

/// Map a registry lookup/persist error into the lifecycle [`EngineError`].
///
/// Registration and lifecycle share the same NotFound/InvalidName shapes; keep
/// them as the lifecycle variants so `kt` maps them consistently. Exposed
/// `pub(crate)` so the engine facade's status read (story 1-6, AC9) maps registry
/// errors the same way the supervisor does.
pub(crate) fn registry_to_engine(err: super::error::RegistryError) -> EngineError {
    use super::error::RegistryError as R;
    match err {
        R::NotFound { name } => EngineError::NotFound { name },
        R::InvalidName { name, reason } => EngineError::InvalidName { name, reason },
        R::Io { name, path, source } => EngineError::Log {
            name,
            path,
            detail: source.to_string(),
        },
        R::Store(inner) => EngineError::Store(inner),
        // Any other registry error surfaces as an adapter-unresolved detail
        // (e.g. a missing/corrupt snapshot the supervisor needs to launch).
        other => EngineError::AdapterUnresolved {
            name: "<unknown>".to_string(),
            detail: other.to_string(),
        },
    }
}

/// Map a launch-spec resolution failure into the lifecycle [`EngineError`].
fn launch_to_engine(name: &InstanceName, err: LaunchResolveError) -> EngineError {
    EngineError::AdapterUnresolved {
        name: name.as_str().to_string(),
        detail: err.to_string(),
    }
}

/// Map a config-resolution failure (story 2-2) encountered while mapping the
/// resolved unified config into the launch into the lifecycle [`EngineError`]. A
/// malformed config layer / missing instance surfaces as an unresolved-adapter
/// launch failure (the config could not be mapped into the launch), naming the
/// instance + detail; the start rejects BEFORE any state change (mirrors a bad
/// manifest).
fn config_to_engine(name: &InstanceName, err: crate::domain::ConfigError) -> EngineError {
    EngineError::AdapterUnresolved {
        name: name.as_str().to_string(),
        detail: err.to_string(),
    }
}

/// Map a config-mapping APPLICATION failure (story 2-2) — a FILE target that
/// could not be rendered into the Agent Home — into the lifecycle [`EngineError`].
/// Surfaces as an unresolved-adapter launch failure naming the instance + detail;
/// the start rejects before any state change (the file write happens before the
/// `starting` transition), so a bad file target never leaves a spurious state.
fn config_apply_to_engine(name: &InstanceName, err: ConfigApplyError) -> EngineError {
    EngineError::AdapterUnresolved {
        name: name.as_str().to_string(),
        detail: err.to_string(),
    }
}

/// Map an effective-config SNAPSHOT-write failure (story 2-3) into the lifecycle
/// [`EngineError`]. The snapshot write lands BEFORE the `starting` transition, so
/// a failure here rejects the start with no state change. A
/// [`RegistryError::SnapshotWrite`] already carries the instance + snapshot path +
/// detail; map it to the dedicated [`EngineError::Snapshot`] naming the same, so
/// `kt` renders a precise "could not write the effective-config snapshot"
/// diagnostic with a permissions/disk remediation (NFR-1). Any other registry
/// error (not expected from this call) falls back to the shared registry mapper.
fn snapshot_to_engine(err: super::error::RegistryError) -> EngineError {
    match err {
        super::error::RegistryError::SnapshotWrite { name, path, detail } => {
            EngineError::Snapshot { name, path, detail }
        }
        other => registry_to_engine(other),
    }
}

/// Map a SECRET-resolution failure (story 2-4) into the lifecycle [`EngineError`].
/// The resolution runs BEFORE the config mapping + the `starting` transition, so a
/// failure here rejects the start with no state change (mirroring
/// [`snapshot_to_engine`]). The [`SecretError`] message names the `NAME` + the
/// resolvers tried (or the `chmod 600` remediation) but NEVER a resolved value, so
/// mapping it into [`EngineError::Secret`]'s `detail` cannot leak a secret (AC-B).
fn secret_to_engine(name: &InstanceName, err: crate::ports::SecretError) -> EngineError {
    EngineError::Secret {
        name: name.as_str().to_string(),
        detail: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterRef, StartLaunch};
    use crate::domain::RestartPolicy;
    use std::time::Instant;

    /// A fast backoff schedule so the crash/restart/crash-loop lib tests never
    /// sleep for real seconds (production stays 1s×2 cap 60s — Task 2 guards it).
    fn fast_backoff() -> BackoffSchedule {
        BackoffSchedule::with_base_and_cap(Duration::from_millis(5), Duration::from_millis(20))
    }

    /// Write a manifest whose `[lifecycle.start]` exec is `fake_agent` + `args`.
    fn write_fake_manifest(dir: &Path, kind: &str, args: &[&str]) {
        let bin = hemaka_conformance::fake_agent_bin();
        let args_toml = args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args_toml}]\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n",
            exec = bin.to_string_lossy(),
        );
        std::fs::write(dir.join("adapter.toml"), body).unwrap();
    }

    /// Register a `fake_agent`-backed instance under `name` with `args`, in a
    /// fresh state dir. Returns the (state dir, manifest dir, registry).
    fn setup_fake(name: &str, args: &[&str]) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
        let state = tempfile::tempdir().unwrap();
        let manifest = tempfile::tempdir().unwrap();
        write_fake_manifest(manifest.path(), name, args);
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        registry
            .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
            .unwrap();
        (state, manifest, registry)
    }

    /// Story 2-2: write a `fake_agent` manifest with `args` PLUS a `[config]`
    /// mapping section (`config_toml` is the section body, e.g.
    /// `"[config.model]\nflag = \"--model\"\n"`). Used by the manifest end-to-end
    /// mapping proofs.
    fn write_fake_manifest_with_config(dir: &Path, kind: &str, args: &[&str], config_toml: &str) {
        let bin = hemaka_conformance::fake_agent_bin();
        let args_toml = args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args_toml}]\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n\n\
             {config_toml}",
            exec = bin.to_string_lossy(),
        );
        std::fs::write(dir.join("adapter.toml"), body).unwrap();
    }

    /// Register a `fake_agent`-backed instance carrying a `[config]` mapping.
    /// Returns the (state dir, manifest dir, registry).
    fn setup_fake_with_config(
        name: &str,
        args: &[&str],
        config_toml: &str,
    ) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
        let state = tempfile::tempdir().unwrap();
        let manifest = tempfile::tempdir().unwrap();
        write_fake_manifest_with_config(manifest.path(), name, args, config_toml);
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        registry
            .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
            .unwrap();
        (state, manifest, registry)
    }

    /// Poll for the `fake_agent` readiness marker (`--marker <path>`, written
    /// at startup right after the ready line and BEFORE the `--dump` file) —
    /// the AI-35/38 readiness handshake (story 11-5): a `_live` test proceeds
    /// as soon as the spawned agent is PROVABLY up, on every OS, instead of
    /// being gated off macOS/Windows on an OS-fragile wall-clock assumption
    /// about spawn latency. The generous bound absorbs loaded CI runners (and
    /// the instrumented coverage run); the poll returns the moment the file
    /// appears, so the happy path pays nothing.
    fn wait_for_marker(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if path.exists() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "readiness marker never appeared at {path:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Poll for a `--dump` file to appear (the spawned `fake_agent` writes it at
    /// startup, right after its readiness `--marker`) and return its contents,
    /// bounded — avoids racing the spawn. Call [`Self::wait_for_marker`] first
    /// for the readiness handshake.
    fn wait_for_dump(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if !text.is_empty() {
                    return text;
                }
            }
            assert!(
                Instant::now() < deadline,
                "dump file never appeared at {path:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Poll until `poll_once` reports the crash (returns its plans), bounded.
    fn wait_for_crash(sup: &mut Supervisor, registry: &Registry) -> Vec<RestartPlan> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let plans = sup.poll_once(registry);
            // Once the instance has crashed it is no longer in `running`; the
            // crash transition has landed. `poll_once` returns the plan on the
            // pass that detects the exit.
            if !plans.is_empty() {
                return plans;
            }
            // Also stop once nothing is supervised AND state is failed (a `never`
            // policy returns no plan but still crashes).
            if sup.running.is_empty() {
                return plans;
            }
            assert!(Instant::now() < deadline, "crash was never detected");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn state_of(registry: &Registry, name: &str) -> LifecycleState {
        registry
            .lookup(&InstanceName::new(name).unwrap())
            .unwrap()
            .state
    }

    // ---- Story 2-2: unified→native config mapping proven at start (AC-A/AC-B) ----

    #[test]
    fn mock_native_start_maps_model_to_the_declared_env_target() {
        // AC-A + AC8 (the MOCK/native proof). The builtin `mock` is INERT (no live
        // process — NativeHasNoLaunch), so a `mock` start cannot spawn to observe.
        // Per the recorded inert-mock strategy (Decision 8), we assert on the
        // MAPPED launch the mapping application PRODUCES: register a mock, set the
        // documented `model` key (2-1), then resolve the mock's code-declared
        // mapping + the effective config and apply — the mock's declared native
        // target (env `MODEL`) must carry the value. This is exactly the transform
        // the start seam runs; a launchable native agent is a manifest adapter.
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        registry.register("mck", "mock").unwrap();
        let name = InstanceName::new("mck").unwrap();
        registry.set_config(&name, "model", "gpt-4").unwrap();

        // Resolve exactly as start_inner would for a native adapter.
        let (kind, manifest_path, launch) = registry.adapter_launch_facts(&name).unwrap();
        assert_eq!(kind, "mock");
        assert!(manifest_path.is_none(), "mock is native (no manifest)");
        assert!(launch.is_none(), "mock is native (no snapshotted launch)");
        let effective = registry
            .effective_config(&name, crate::domain::ConfigLayer::empty())
            .unwrap();
        let mapping = adapter::resolve_config_mapping(&kind, manifest_path.as_deref()).unwrap();
        // The mock declares `model` → env `MODEL`.
        assert_eq!(mapping.target("model").unwrap().env_var(), Some("MODEL"));

        // Apply onto a bare launch (the mock has no [lifecycle.start] template;
        // this is the launch shape the mapping would produce).
        let mut launch = StartLaunch {
            exec: "mock".to_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        };
        adapter::apply_config_mapping(
            &mut launch,
            &mapping,
            &effective,
            &std::collections::BTreeMap::new(),
            &registry.agent_home(&name),
        )
        .unwrap();
        assert_eq!(
            launch.env.get("MODEL").map(String::as_str),
            Some("gpt-4"),
            "the documented model key must land in the mock's declared env target"
        );
    }

    // ---- Story 11-2: AI-27 (env-shadow visibility) + AI-39 (secret→flag runtime) ----

    /// Story 11-2: write a manifest whose `[lifecycle.start]` declares a BASE
    /// env var (`BASEVAR`) and whose exec is a guaranteed-missing binary — so
    /// the spawn (which happens AFTER the diagnostic emissions) fails on EVERY
    /// OS, keeping the emission proofs OS-agnostic (no live process needed,
    /// mirroring the `_live` gates' rationale).
    fn write_manifest_with_base_env(dir: &Path, kind: &str, config_toml: &str) {
        let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = \"ktesio-definitely-missing-binary\"\nargs = []\nenv = {{ BASEVAR = \"base-value\" }}\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n\n\
             {config_toml}"
        );
        std::fs::write(dir.join("adapter.toml"), body).unwrap();
    }

    /// Register an instance from [`Self::write_manifest_with_base_env`].
    fn setup_base_env_instance(
        name: &str,
        config_toml: &str,
    ) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
        let state = tempfile::tempdir().unwrap();
        let manifest = tempfile::tempdir().unwrap();
        write_manifest_with_base_env(manifest.path(), name, config_toml);
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        registry
            .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
            .unwrap();
        (state, manifest, registry)
    }

    #[test]
    fn shadowed_env_keys_names_only_the_overwritten_base_vars() {
        // The pure AI-27 diff: an apply only INSERTS, so every base var survives
        // into the post-apply env — a base var counts as shadowed exactly when
        // its VALUE changed. A new-name target and an identical re-write yield
        // NOTHING (the quiet path must stay quiet).
        let base: std::collections::BTreeMap<String, String> = [
            ("SHADOWED".to_string(), "old".to_string()),
            ("KEPT".to_string(), "untouched".to_string()),
        ]
        .into_iter()
        .collect();
        let mut after = base.clone();
        after.insert("SHADOWED".to_string(), "new".to_string());
        after.insert("NEWVAR".to_string(), "fresh".to_string());

        assert_eq!(
            shadowed_env_keys(&base, &after),
            vec!["SHADOWED".to_string()],
            "exactly the value-changed base var is named, deterministically sorted"
        );
        // A mapping that re-writes the identical value is not a shadow.
        let mut same_value = base.clone();
        same_value.insert("SHADOWED".to_string(), "old".to_string());
        same_value.insert("NEWVAR".to_string(), "fresh".to_string());
        assert!(shadowed_env_keys(&base, &same_value).is_empty());
        // An empty base (no [lifecycle.start] env) can never shadow.
        let empty = std::collections::BTreeMap::new();
        assert!(shadowed_env_keys(&empty, &after).is_empty());
    }

    #[test]
    fn start_emits_one_diagnostic_when_a_mapped_env_target_shadows_a_base_var() {
        // AI-27 end-to-end: the manifest launches with BASEVAR=base-value; the
        // config maps `model` → env BASEVAR. The start's launch carries the
        // CONFIG value (precedence untouched — the insert won), and exactly ONE
        // diagnostic names the shadowed variable. The spawn itself fails (the
        // exec is deliberately missing) — AFTER the emission, proving the
        // diagnostic rides the start path regardless of launch outcome.
        let (_state, _manifest, registry) =
            setup_base_env_instance("shdw", "[config.model]\nenv = \"BASEVAR\"\n");
        let name = InstanceName::new("shdw").unwrap();
        registry.set_config(&name, "model", "config-value").unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        let buffer = install_capture_sink(&mut sup);
        let err = sup.start(&registry, "shdw").unwrap_err();
        assert!(
            matches!(err, EngineError::LaunchFailed { .. }),
            "the missing-binary spawn fails, but only AFTER the emissions; got {err:?}"
        );

        let captured = sink_text(&buffer);
        let lines: Vec<&str> = captured.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "exactly ONE diagnostic must be emitted; got {lines:?}"
        );
        assert!(
            lines[0].contains("shdw") && lines[0].contains("BASEVAR"),
            "the diagnostic names the instance + the shadowed var: {}",
            lines[0]
        );
        // Review-1 patch 6: the wording must NOT claim the shadowed vars are
        // "base-launch"/template vars (the snapshot may carry other launch env).
        assert!(
            lines[0].contains("launch environment variable(s)")
                && !lines[0].contains("base-launch"),
            "the diagnostic must say 'launch environment variable(s)' without the \
             base-launch claim: {}",
            lines[0]
        );
    }

    #[test]
    fn start_stays_quiet_when_mapped_env_targets_are_all_new_names() {
        // AI-27's clean path: the mapping targets a var the base launch does
        // NOT carry — no diagnostic at all (an empty capture). Same failing-exec
        // manifest so the only difference IS the shadow.
        let (_state, _manifest, registry) =
            setup_base_env_instance("quiet", "[config.model]\nenv = \"NEWVAR\"\n");
        let name = InstanceName::new("quiet").unwrap();
        registry.set_config(&name, "model", "config-value").unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        let buffer = install_capture_sink(&mut sup);
        let _ = sup.start(&registry, "quiet");

        assert!(
            sink_text(&buffer).is_empty(),
            "a non-shadowing start must emit NOTHING; got {}",
            sink_text(&buffer)
        );
    }

    /// Restore-on-drop guard for a process-global env var a test set (review-1
    /// patch 7): the match-prev/restore tail the sibling tests use is skipped
    /// when an assertion panics, leaking the sentinel into sibling tests on the
    /// shared process env — the guard restores even on failure.
    struct EnvGuard(&'static str, Option<std::ffi::OsString>);

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self(key, prev)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.1.take() {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }

    #[test]
    fn start_emits_one_diagnostic_when_a_secret_resolves_into_a_flag_target() {
        // AI-39 runtime end-to-end: a `model = secret:KEY` leaf mapped to a FLAG
        // target resolves and delivers cleartext into argv (the accepted
        // boundary), and the start emits exactly ONE warn-only diagnostic naming
        // the key. The SET-TIME half is pinned on the same shape: the registry
        // set succeeded AND returned the steering warning (warn-only, exit 0).
        const SENTINEL: &str = "flag-steering-sentinel";
        let env_key = "KTESIO_SUP_FLAG_STEERING_KEY";
        let _env = EnvGuard::set(env_key, SENTINEL);

        let (_state, _manifest, registry) =
            setup_base_env_instance("flgrt", "[config.model]\nflag = \"--model\"\n");
        let name = InstanceName::new("flgrt").unwrap();
        let warnings = registry
            .set_config(&name, "model", &format!("secret:{env_key}"))
            .unwrap();
        assert_eq!(warnings.len(), 1, "the set-time warning fires (AI-33)");
        assert!(warnings[0].contains("model"), "{}", warnings[0]);

        let mut sup = Supervisor::with_backoff(fast_backoff());
        let buffer = install_capture_sink(&mut sup);
        let _ = sup.start(&registry, "flgrt");

        let captured = sink_text(&buffer);
        let lines: Vec<&str> = captured.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "exactly ONE runtime diagnostic must be emitted; got {lines:?}"
        );
        assert!(
            lines[0].contains("flgrt") && lines[0].contains("model"),
            "the diagnostic names the instance + the key: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("FLAG"),
            "the diagnostic names the flag-target argv fact: {}",
            lines[0]
        );
        // NEVER the resolved value — the diagnostic names the fact, not the key.
        assert!(
            !lines[0].contains(SENTINEL),
            "the diagnostic must not leak the cleartext: {}",
            lines[0]
        );
        // The EnvGuard restores the env var even on a failed assertion.
    }

    // ---- The launch-snapshot fix (hosted-runner arg-loss): start uses the
    //      REGISTRATION snapshot, never a start-time manifest re-read ----

    #[test]
    fn start_uses_the_registration_launch_snapshot_not_a_manifest_reread() {
        // The FIX, at the EXACT seam the supervisor uses (`adapter_launch_facts`),
        // OS-agnostically (no spawn — runs on every platform, including the
        // macOS/Windows CI that dropped the args): register a fake_agent manifest
        // with distinctive args, DELETE the manifest file, then read the launch
        // facts. The persisted exec + args + env still come back intact — and the
        // fallback re-read now FAILS (the manifest is gone), proving the snapshot,
        // not a re-read, is what carries the launch into `start`.
        let (_state, manifest, registry) =
            setup_fake("snap", &["--emit-usage", "5", "--linger-ms", "600000"]);
        let name = InstanceName::new("snap").unwrap();

        // Remove the manifest entirely — any start-time re-read of it now fails.
        std::fs::remove_file(manifest.path().join("adapter.toml")).unwrap();

        let (kind, manifest_path, launch) = registry.adapter_launch_facts(&name).unwrap();
        assert_eq!(kind, "snap");
        assert!(
            manifest_path.is_some(),
            "a manifest adapter records its path"
        );
        let launch = launch.expect("the launch is snapshotted at registration");
        let bin = hemaka_conformance::fake_agent_bin();
        assert_eq!(launch.exec, bin.to_string_lossy().into_owned());
        // The manifest's [lifecycle.start] args survived — INCLUDING the args the
        // hosted runners dropped on re-read.
        assert_eq!(
            launch.args,
            vec!["--emit-usage", "5", "--linger-ms", "600000"]
        );

        // The fallback re-read WOULD fail now (the manifest is gone): proof that
        // the snapshot — not a re-read — is what makes the start work.
        assert!(
            adapter::resolve_start_launch(&kind, manifest_path.as_deref()).is_err(),
            "the manifest re-read is gone/broken; the snapshot carried the launch"
        );
    }

    #[test]
    fn manifest_start_uses_the_snapshot_launch_even_when_the_manifest_changes_live() {
        // The FIX end-to-end: after registration the launch is FIXED by the
        // snapshot, so mutating the manifest's [lifecycle.start] args no longer
        // affects the started process. Register a fake_agent, REWRITE its manifest
        // with a decoy arg only a re-read would surface, then START — the spawned
        // argv carries the ORIGINAL args and NOT the decoy. Runs on ALL three
        // OSes (AI-35/38, story 11-5): the spawn+observe is a readiness
        // HANDSHAKE — the manifest passes `--marker`, the test waits for the
        // agent's marker file, then for its argv `--dump` — so no OS-fragile
        // wall-clock assumption about spawn latency remains (see
        // `wait_for_marker`).
        let dump = tempfile::tempdir().unwrap();
        let dump_path = dump.path().join("argv.txt");
        let marker_path = dump.path().join("ready.marker");
        let marker = marker_path.to_str().unwrap();
        let (_state, manifest, registry) = setup_fake(
            "del",
            &[
                "--linger-ms",
                "600000",
                "--dump",
                dump_path.to_str().unwrap(),
                "--marker",
                marker,
            ],
        );

        // Rewrite the manifest AFTER registration, appending a decoy arg. The
        // manifest stays valid (no [config]), so the unchanged config-mapping
        // re-read still succeeds; only a LAUNCH re-read would surface the decoy.
        write_fake_manifest(
            manifest.path(),
            "del",
            &[
                "--linger-ms",
                "600000",
                "--dump",
                dump_path.to_str().unwrap(),
                "--marker",
                marker,
                "--decoy-from-reread",
            ],
        );

        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "del").unwrap();
        assert_eq!(state_of(&registry, "del"), LifecycleState::Running);

        // The spawned fake_agent dumped its argv: the ORIGINAL start args are there,
        // and the post-registration decoy is NOT — the launch came from the
        // registration snapshot, not a re-read of the mutated manifest.
        wait_for_marker(&marker_path);
        let dumped = wait_for_dump(&dump_path);
        assert!(
            dumped.lines().any(|l| l == "arg=--linger-ms"),
            "the snapshotted start args must reach argv; dump=\n{dumped}"
        );
        assert!(
            !dumped.lines().any(|l| l == "arg=--decoy-from-reread"),
            "the re-read decoy must NOT appear — the launch came from the snapshot; dump=\n{dumped}"
        );
    }

    #[test]
    fn manifest_start_maps_model_to_the_declared_flag_target_live() {
        // Cross-OS (AI-35/38, story 11-5): the delivery logic proven here —
        // unified config → native env/flag/file mapping — is OS-agnostic engine
        // code, identical on every OS, and the `_live` spawn+observe is now a
        // readiness HANDSHAKE (the manifest passes `--marker`; the test waits
        // for the marker file, then the argv `--dump` — see `wait_for_marker`),
        // so the old "fragile spawn on macOS/Windows CI" Linux-only gate is
        // dropped and the proof runs on all three legs.
        // AC-A + AC8 (the MANIFEST proof, live). A `fake_agent` manifest declares
        // `[config.model]` → flag `--model`; set model, start the REAL process
        // with `--dump`, and assert the mapped flag landed in the spawned
        // process's argv (observed via the dump file — no stdout race).
        let dump = tempfile::tempdir().unwrap();
        let dump_path = dump.path().join("argv.txt");
        let marker_path = dump.path().join("ready.marker");
        let (_state, _manifest, registry) = setup_fake_with_config(
            "flg",
            &[
                "--linger-ms",
                "600000",
                "--dump",
                dump_path.to_str().unwrap(),
                "--marker",
                marker_path.to_str().unwrap(),
            ],
            "[config.model]\nflag = \"--model\"\n",
        );
        let name = InstanceName::new("flg").unwrap();
        registry.set_config(&name, "model", "gpt-4o").unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "flg").unwrap();
        assert_eq!(state_of(&registry, "flg"), LifecycleState::Running);

        // The spawned fake_agent dumped its argv; the mapped flag + value are there.
        wait_for_marker(&marker_path);
        let dumped = wait_for_dump(&dump_path);
        assert!(
            dumped.lines().any(|l| l == "arg=--model"),
            "the mapped --model flag must reach the process argv; dump=\n{dumped}"
        );
        assert!(
            dumped.lines().any(|l| l == "arg=gpt-4o"),
            "the mapped model VALUE must reach the process argv; dump=\n{dumped}"
        );
        // Teardown.
        let _ = sup.stop(&registry, "flg", Some(Duration::from_millis(200)));
    }

    #[test]
    fn secret_leaf_delivers_cleartext_to_the_adapter_but_masks_snapshot_and_events() {
        // Cross-OS (AI-35/38, story 11-5): see
        // manifest_start_maps_model_to_the_declared_flag_target_live — the
        // `_live` observe is a readiness handshake (`--marker` → `--dump`), so
        // the Linux-only gate is dropped.
        // Story 2-4 (AC-A/AC9 delivery + AC-B no-leak, engine level). A
        // `model = secret:NAME` leaf resolves (env resolver) to a sentinel; the
        // spawned agent's argv carries the CLEARTEXT (usable), while the persisted
        // snapshot AND every transition event carry the MASK, never the sentinel.
        // Uses a UNIQUE env-var name to avoid racing sibling in-process tests.
        const SENTINEL: &str = "s3cr3t-engine-sentinel-abc";
        let env_key = "KTESIO_SUP_SECRET_TEST_KEY";
        let prev = std::env::var_os(env_key);
        std::env::set_var(env_key, SENTINEL);

        let dump = tempfile::tempdir().unwrap();
        let dump_path = dump.path().join("argv.txt");
        let marker_path = dump.path().join("ready.marker");
        let (_state, _manifest, registry) = setup_fake_with_config(
            "sekeng",
            &[
                "--linger-ms",
                "600000",
                "--dump",
                dump_path.to_str().unwrap(),
                "--marker",
                marker_path.to_str().unwrap(),
            ],
            "[config.model]\nflag = \"--model\"\n",
        );
        let name = InstanceName::new("sekeng").unwrap();
        registry
            .set_config(&name, "model", &format!("secret:{env_key}"))
            .unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "sekeng").unwrap();
        assert_eq!(state_of(&registry, "sekeng"), LifecycleState::Running);

        // (POSITIVE) the spawned process argv carries the resolved CLEARTEXT.
        wait_for_marker(&marker_path);
        let dumped = wait_for_dump(&dump_path);
        assert!(
            dumped.lines().any(|l| l == format!("arg={SENTINEL}")),
            "the resolved secret cleartext must reach the process argv; dump=\n{dumped}"
        );

        // (NO-LEAK) the persisted snapshot masks the secret.
        let snapshot = std::fs::read_to_string(registry.paths().effective_config_snapshot(&name))
            .expect("snapshot written");
        assert!(
            !snapshot.contains(SENTINEL),
            "the snapshot leaked the secret:\n{snapshot}"
        );
        assert!(
            snapshot.contains("secret:****"),
            "snapshot must mask; {snapshot}"
        );

        // (NO-LEAK) no transition event payload carries the sentinel (AD-14).
        let events = Supervisor::read_events(&registry, "sekeng").unwrap();
        let events_json = serde_json::to_string(&events).unwrap();
        assert!(
            !events_json.contains(SENTINEL),
            "a transition event leaked the secret:\n{events_json}"
        );

        // Teardown + restore env.
        let _ = sup.stop(&registry, "sekeng", Some(Duration::from_millis(200)));
        match prev {
            Some(v) => std::env::set_var(env_key, v),
            None => std::env::remove_var(env_key),
        }
    }

    #[test]
    fn unresolved_secret_rejects_start_before_any_state_change() {
        // Story 2-4 (AC5/AC9, engine level): a `secret:NAME` unresolved by env AND
        // the (absent) secrets file rejects the start with a typed EngineError::Secret
        // that NEVER echoes a value, leaving the instance in its PRIOR state and NO
        // snapshot written. The env var is deliberately unset.
        let env_key = "KTESIO_SUP_DEFINITELY_UNSET_SECRET_KEY_XYZ";
        std::env::remove_var(env_key);
        let (_state, _manifest, registry) = setup_fake("noresolve_eng", &["--linger-ms", "600000"]);
        let name = InstanceName::new("noresolve_eng").unwrap();
        registry
            .set_config(&name, "model", &format!("secret:{env_key}"))
            .unwrap();
        let prior = state_of(&registry, "noresolve_eng");

        let mut sup = Supervisor::with_backoff(fast_backoff());
        let err = sup.start(&registry, "noresolve_eng").unwrap_err();
        match &err {
            EngineError::Secret { name: n, detail } => {
                assert_eq!(n, "noresolve_eng");
                // Names the NAME + resolvers, NEVER a value.
                assert!(detail.contains(env_key), "detail must name NAME; {detail}");
            }
            other => panic!("expected EngineError::Secret, got {other:?}"),
        }
        // Prior state preserved; no snapshot written (rejected before both).
        assert_eq!(state_of(&registry, "noresolve_eng"), prior);
        assert!(
            !registry.paths().effective_config_snapshot(&name).exists(),
            "an unresolved secret must reject before the snapshot write"
        );
    }

    #[test]
    fn manifest_start_maps_model_to_the_declared_file_target_live() {
        // AC-A + AC4 (the MANIFEST FILE proof, live). A `fake_agent` manifest
        // declares `[config.model]` → a file target; set model, start, and assert
        // the engine RENDERED the native config file into the Agent Home at the
        // declared native key (the engine is the sole writer — path authority).
        let (_state, _manifest, registry) = setup_fake_with_config(
            "fil",
            &["--linger-ms", "600000"],
            "[config.model]\nfile = { path = \"config/agent.toml\", key = \"llm.model\" }\n",
        );
        let name = InstanceName::new("fil").unwrap();
        registry.set_config(&name, "model", "claude-opus").unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "fil").unwrap();
        assert_eq!(state_of(&registry, "fil"), LifecycleState::Running);

        // The engine rendered the native config file into the Agent Home.
        let rendered = registry.agent_home(&name).join("config/agent.toml");
        assert!(
            rendered.is_file(),
            "the file target must render into the home"
        );
        let parsed: toml::Table = std::fs::read_to_string(&rendered).unwrap().parse().unwrap();
        assert_eq!(
            parsed["llm"]["model"].as_str(),
            Some("claude-opus"),
            "the documented model key must land at the declared native key path"
        );
        // Teardown.
        let _ = sup.stop(&registry, "fil", Some(Duration::from_millis(200)));
    }

    #[test]
    fn manifest_start_delivers_agent_pass_through_verbatim_live() {
        // Cross-OS (AI-35/38, story 11-5): see
        // manifest_start_maps_model_to_the_declared_flag_target_live — the
        // `_live` observe is a readiness handshake (`--marker` → `--dump`), so
        // the Linux-only gate is dropped.
        // AC-B (the `agent.*` verbatim proof, live). Set an `agent.*` pass-through
        // key, start the REAL fake_agent with `--dump`, and assert the value was
        // delivered VERBATIM into the native mechanism (an env var named by the
        // verbatim key-tail) — no rewriting, no known-key mapping.
        let dump = tempfile::tempdir().unwrap();
        let dump_path = dump.path().join("env.txt");
        let marker_path = dump.path().join("ready.marker");
        // No [config] mapping at all — pass-through does not need one (AC6).
        let (_state, _manifest, registry) = setup_fake_with_config(
            "pth",
            &[
                "--linger-ms",
                "600000",
                "--dump",
                dump_path.to_str().unwrap(),
                "--marker",
                marker_path.to_str().unwrap(),
            ],
            "",
        );
        let name = InstanceName::new("pth").unwrap();
        registry
            .set_config(&name, "agent.CUSTOM_TOKEN", "verbatim-xyz")
            .unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "pth").unwrap();
        assert_eq!(state_of(&registry, "pth"), LifecycleState::Running);

        wait_for_marker(&marker_path);
        let dumped = wait_for_dump(&dump_path);
        assert!(
            dumped.lines().any(|l| l == "env=CUSTOM_TOKEN=verbatim-xyz"),
            "the agent.* value must be delivered verbatim into the native env; dump=\n{dumped}"
        );
        // Teardown.
        let _ = sup.stop(&registry, "pth", Some(Duration::from_millis(200)));
    }

    // ---- Story 2-3: the persisted effective-config snapshot at start (AC5/AC6/AC7) ----

    /// Parse the persisted effective-config snapshot for `name` and return the
    /// entry map (key → (rendered value, source label)). Panics if the file is
    /// missing/unparseable (the test wants it present).
    fn read_snapshot_entries(
        registry: &Registry,
        name: &str,
    ) -> std::collections::BTreeMap<String, (String, String)> {
        let path = registry
            .paths()
            .effective_config_snapshot(&InstanceName::new(name).unwrap());
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["key"].as_str().unwrap().to_string(),
                    (
                        e["value"].as_str().unwrap().to_string(),
                        e["source"].as_str().unwrap().to_string(),
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn start_writes_the_effective_config_snapshot_tagged_with_source() {
        // AC5 (AD-9/AD-6): starting an instance writes the effective-config
        // snapshot FILE into the Agent Home at EnginePaths::effective_config_snapshot,
        // and it parses + carries model=<v> tagged `instance`. Register a live
        // fake_agent, set model, start it, assert the snapshot.
        let (_state, _manifest, registry) = setup_fake("snp", &["--linger-ms", "600000"]);
        let name = InstanceName::new("snp").unwrap();
        registry.set_config(&name, "model", "gpt-4o").unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "snp").unwrap();
        assert_eq!(state_of(&registry, "snp"), LifecycleState::Running);

        let path = registry.paths().effective_config_snapshot(&name);
        assert!(path.is_file(), "the snapshot must exist at {path:?}");
        let entries = read_snapshot_entries(&registry, "snp");
        assert_eq!(
            entries.get("model"),
            Some(&("gpt-4o".to_string(), "instance".to_string())),
            "model must be present tagged `instance`; entries={entries:?}"
        );
        // Teardown.
        let _ = sup.stop(&registry, "snp", Some(Duration::from_millis(200)));
    }

    #[test]
    fn restart_via_start_inner_overwrites_the_snapshot_with_the_new_value() {
        // AC7: the snapshot is OVERWRITTEN on every start — a re-start (which flows
        // through the SAME start_inner seam, story 1-6) refreshes it with the newly
        // resolved value, never a stale earlier resolution. Start, stop, change the
        // value, start again; the snapshot reflects the LATEST value.
        let (_state, _manifest, registry) = setup_fake("rsn", &["--linger-ms", "600000"]);
        let name = InstanceName::new("rsn").unwrap();
        registry.set_config(&name, "model", "first").unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "rsn").unwrap();
        assert_eq!(
            read_snapshot_entries(&registry, "rsn").get("model"),
            Some(&("first".to_string(), "instance".to_string()))
        );
        sup.stop(&registry, "rsn", Some(Duration::from_millis(200)))
            .unwrap();

        // Change the value and start again (stopped → starting → running via
        // start_inner). The snapshot must be overwritten with the new value.
        registry.set_config(&name, "model", "second").unwrap();
        sup.start(&registry, "rsn").unwrap();
        assert_eq!(state_of(&registry, "rsn"), LifecycleState::Running);
        assert_eq!(
            read_snapshot_entries(&registry, "rsn").get("model"),
            Some(&("second".to_string(), "instance".to_string())),
            "the snapshot must reflect the latest resolved value after re-start (AC7)"
        );
        // Teardown.
        let _ = sup.stop(&registry, "rsn", Some(Duration::from_millis(200)));
    }

    #[test]
    fn snapshot_write_failure_rejects_the_start_before_the_starting_transition() {
        // AC6: a snapshot-write failure rejects the start with NO state change (the
        // write lands before the `starting` transition). Force the write to fail by
        // making the snapshot path a DIRECTORY, then assert start errors and the
        // instance stays in its prior state (`registered`), with NO agent spawned.
        let (_state, _manifest, registry) = setup_fake("bad", &["--linger-ms", "600000"]);
        let name = InstanceName::new("bad").unwrap();
        registry.set_config(&name, "model", "gpt-4").unwrap();
        // A directory where the snapshot file must be → std::fs::write fails.
        let snap_path = registry.paths().effective_config_snapshot(&name);
        std::fs::create_dir(&snap_path).unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        let err = sup.start(&registry, "bad").unwrap_err();
        assert!(
            matches!(&err, EngineError::Snapshot { name, .. } if name == "bad"),
            "expected a typed Snapshot error, got {err:?}"
        );
        // The instance stayed in its prior state — the start was rejected cleanly
        // BEFORE the `starting` transition (no spurious state change, AC6).
        assert_eq!(state_of(&registry, "bad"), LifecycleState::Registered);
    }

    #[test]
    fn snapshot_to_engine_maps_snapshot_write_and_falls_back_for_others() {
        // Unit-cover the snapshot error mapper: a SnapshotWrite maps to the
        // dedicated EngineError::Snapshot naming the instance + path; any other
        // registry error falls back to the shared registry mapper (NotFound here).
        let mapped = snapshot_to_engine(crate::domain::RegistryError::SnapshotWrite {
            name: "demo".into(),
            path: "/x/agents/demo/effective-config.json".into(),
            detail: "disk full".into(),
        });
        match mapped {
            EngineError::Snapshot { name, path, detail } => {
                assert_eq!(name, "demo");
                assert!(path.ends_with("effective-config.json"), "path={path}");
                assert_eq!(detail, "disk full");
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
        // Fallback: a non-snapshot registry error goes through registry_to_engine.
        let fallback = snapshot_to_engine(crate::domain::RegistryError::NotFound {
            name: "demo".into(),
        });
        assert!(matches!(fallback, EngineError::NotFound { name } if name == "demo"));
    }

    #[test]
    fn crash_of_a_never_policy_instance_lands_failed_no_restart() {
        // AC-A / AC5: a `never`-policy instance that crashes lands `failed` with a
        // `crashed` cause + NO restart. Set policy=never, start a crash-after
        // agent, poll until the crash is detected, assert failed + no plan.
        let (_state, _manifest, registry) = setup_fake("nevr", &["--crash-after-ms", "450"]);
        registry
            .set_restart_policy(&InstanceName::new("nevr").unwrap(), RestartPolicy::Never)
            .unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "nevr").unwrap();
        assert_eq!(state_of(&registry, "nevr"), LifecycleState::Running);

        let plans = wait_for_crash(&mut sup, &registry);
        assert!(plans.is_empty(), "never policy must NOT schedule a restart");
        assert_eq!(state_of(&registry, "nevr"), LifecycleState::Failed);

        // The crash was recorded with a `crashed` cause.
        let events = Supervisor::read_events(&registry, "nevr").unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.new_state, LifecycleState::Failed);
        let cause = serde_json::to_string(&last.cause).unwrap();
        assert!(cause.contains("crashed"), "cause={cause}");
    }

    #[test]
    fn on_failure_crash_restarts_increments_count_then_a_clean_run_resets() {
        // AC-A / AC4: an `on-failure` instance that crashes is restarted, the
        // restart count increments, the `failed→starting`… restart event records
        // the backoff, and a subsequent CLEAN start resets the count to 0. Uses
        // the injected fast backoff so no real seconds elapse.
        let (_state, _manifest, registry) = setup_fake("recov", &["--crash-after-ms", "450"]);
        // Default policy is on-failure; be explicit.
        registry
            .set_restart_policy(
                &InstanceName::new("recov").unwrap(),
                RestartPolicy::OnFailure,
            )
            .unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "recov").unwrap();

        // Detect the crash → a restart plan for attempt 1.
        let plans = wait_for_crash(&mut sup, &registry);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].attempt, 1);
        assert_eq!(state_of(&registry, "recov"), LifecycleState::Failed);

        // Perform the restart after its (fast) backoff. The instance is running
        // again and the record shows restart_count == 1.
        std::thread::sleep(plans[0].delay);
        sup.restart(&registry, "recov", plans[0].attempt, plans[0].delay)
            .unwrap();
        assert_eq!(state_of(&registry, "recov"), LifecycleState::Running);
        let rec = registry
            .spawn_record(&InstanceName::new("recov").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(rec.restart_count, 1);

        // The restart event recorded the count + waited backoff.
        let events = Supervisor::read_events(&registry, "recov").unwrap();
        let restart_evt = events
            .iter()
            .find(|e| matches!(e.cause, TransitionCause::Restarted { .. }))
            .expect("a restart event must be recorded");
        match &restart_evt.cause {
            TransitionCause::Restarted { count, .. } => assert_eq!(*count, 1),
            _ => unreachable!(),
        }

        // A CLEAN stop then start resets the consecutive count to 0 (AC4).
        sup.stop(&registry, "recov", Some(Duration::from_millis(200)))
            .unwrap();
        sup.start(&registry, "recov").unwrap();
        let rec = registry
            .spawn_record(&InstanceName::new("recov").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(rec.restart_count, 0, "a fresh start resets the count");
        // Teardown.
        let _ = sup.stop(&registry, "recov", Some(Duration::from_millis(200)));
    }

    #[test]
    fn crash_loop_stops_after_exactly_five_consecutive_failures() {
        // AC4: an instance that crashes immediately every restart stops after
        // EXACTLY 5 consecutive failures, left `failed` with the crash-loop
        // reason. Drive the crash→restart cycle manually with the fast backoff;
        // the 5th restart's crash yields NO further plan (crash-loop), and the
        // recorded cause states the crash loop.
        let (_state, _manifest, registry) = setup_fake("loopy", &["--crash-after-ms", "400"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "loopy").unwrap();

        let mut last_attempt = 0;
        // Up to 5 restarts, each preceded by a detected crash.
        for _ in 0..6 {
            let plans = wait_for_crash(&mut sup, &registry);
            assert_eq!(state_of(&registry, "loopy"), LifecycleState::Failed);
            if plans.is_empty() {
                // Crash-loop reached: no more restarts scheduled.
                break;
            }
            assert_eq!(plans.len(), 1);
            last_attempt = plans[0].attempt;
            std::thread::sleep(plans[0].delay);
            // The restart re-launches; it will crash again on the next poll.
            sup.restart(&registry, "loopy", plans[0].attempt, plans[0].delay)
                .unwrap();
        }

        // The last scheduled attempt was the 4th → the 5th crash trips the loop
        // (is_crash_loop(5) == true), so no 5th restart plan is issued.
        assert_eq!(
            last_attempt, 4,
            "the last restart plan should be attempt 4 (the 5th crash trips the loop)"
        );
        assert_eq!(state_of(&registry, "loopy"), LifecycleState::Failed);

        // F-Low-2: the crash-loop is a TERMINAL path, so the record's LIVE
        // fingerprint is dropped (settled to a pid-0 seed) — a later open will
        // NOT adopt-attempt the dead PID (the reconcile skips pid-0). The policy
        // is re-seeded so `show` can still report it (AC9).
        let rec = registry
            .spawn_record(&InstanceName::new("loopy").unwrap())
            .unwrap()
            .expect("a policy seed is retained after the terminal crash-loop");
        assert_eq!(
            rec.fingerprint.pid, 0,
            "the terminal path must drop the live fingerprint (pid-0 seed, not adopt-attempted)"
        );
        assert_eq!(
            rec.restart_policy,
            RestartPolicy::OnFailure,
            "the policy is retained for AC9's show"
        );
        // The crash-loop REASON now rides in the last `crashed` event's cause (so
        // `instance_status` surfaces it via its event-log fallback).
        let events = Supervisor::read_events(&registry, "loopy").unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.new_state, LifecycleState::Failed);
        let cause = serde_json::to_string(&last.cause).unwrap();
        assert!(
            cause.contains("crash-loop") && cause.contains("5 consecutive failures"),
            "the crash-loop reason must be in the event cause; cause={cause}"
        );
    }

    #[test]
    fn poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash() {
        // The reaper's "not a crash" branch: if the store shows the instance
        // `stopping` (an operator stop in flight) when its process exits,
        // poll_once must NOT apply a `failed` crash transition — it just drops the
        // dead handle. Start an instance, mark it `stopping` in the store, let it
        // crash, and assert poll_once returns no plans and does NOT move it to
        // `failed` (it stays `stopping`, the requested end state).
        let (_state, _manifest, registry) = setup_fake("stopping", &["--crash-after-ms", "450"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "stopping").unwrap();
        // Simulate an operator stop in flight (row → stopping) while the handle
        // is still held.
        registry
            .set_state(
                &InstanceName::new("stopping").unwrap(),
                LifecycleState::Stopping,
            )
            .unwrap();
        // Wait for the process to actually exit, then poll.
        std::thread::sleep(Duration::from_millis(700));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let plans = sup.poll_once(&registry);
            assert!(
                plans.is_empty(),
                "an exit during a requested stop is not a crash"
            );
            if sup.running.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "handle should be dropped after exit"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // The state is NOT `failed` (no crash transition was applied); it stays
        // `stopping` (the requested end state the operator stop will finalize).
        assert_eq!(state_of(&registry, "stopping"), LifecycleState::Stopping);
    }

    // ---- Fix pass (review of #80 follow-up — the CRITICAL finding): the
    // bound-post-SIGKILL-wait fix's retry/no-compounding/self-healing logic.
    //
    // These are WHITE-BOX tests: rather than needing a genuinely OS-unkillable
    // process (which requires disk-exhaustion-induced uninterruptible I/O
    // wait — reproduced separately via a dedicated, SAFE ramdisk experiment;
    // see the story file's Dev Agent Record for the full empirical proof),
    // they directly construct the EXACT bookkeeping state a real
    // `BackendError::StopUnconfirmed` would have left behind
    // (`Supervised::stop_unconfirmed = true`, store state `stopping`, handle
    // retained) and prove `stop_inner`'s/`poll_once`'s reconciliation logic
    // against it. This is deterministic and fast (no real 5s wait), mirroring
    // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`
    // immediately above's own technique of forcing `stopping` via
    // `registry.set_state` directly.

    #[test]
    fn stop_on_a_stopping_instance_without_the_unconfirmed_flag_takes_the_ordinary_path() {
        // The negative-space complement of the retry tests below: an
        // instance that is `stopping` with a held handle but is NOT marked
        // `stop_unconfirmed` (the flag ONLY a real `BackendError::
        // StopUnconfirmed` sets) must NOT take the new cheap-poll retry
        // branch — it falls through to the ORIGINAL, unchanged
        // `next_state` gate, which rejects with the uniform
        // `InvalidTransition` exactly as it did before this fix pass. This
        // proves the new branch is gated precisely on `stop_unconfirmed`,
        // not merely "state is stopping" (which
        // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`
        // above ALSO forces, for a different, pre-existing reason).
        let (_state, _manifest, registry) = setup_fake("notflagged", &["--linger-ms", "600000"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "notflagged").unwrap();
        let name = InstanceName::new("notflagged").unwrap();

        registry.set_state(&name, LifecycleState::Stopping).unwrap();
        assert!(
            !sup.running.get(&name).unwrap().stop_unconfirmed,
            "a freshly-started handle must default to NOT stop_unconfirmed"
        );

        let err = sup.stop(&registry, "notflagged", None).unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidTransition(_)),
            "without the flag, this must be the ORIGINAL uniform InvalidTransition, not the new \
             StopUnconfirmed retry path: {err:?}"
        );

        // Teardown.
        let supervised = sup.running.get_mut(&name).unwrap();
        let _ = sup
            .backend
            .stop(&mut supervised.handle, Duration::from_secs(2));
    }

    #[test]
    fn stop_retry_on_a_stuck_unconfirmed_instance_polls_cheaply_no_compounding() {
        // A retry `stop()` against an instance whose handle is marked
        // `stop_unconfirmed` (a prior real stop attempt hit
        // KILL_CONFIRM_TIMEOUT) must NOT re-run the whole
        // SIGTERM/graceful-window/SIGKILL/confirm sequence — it polls ONCE,
        // cheaply, and fails fast with the SAME honest error while the
        // process is still genuinely alive (no new signal, no new wait).
        let (_state, _manifest, registry) = setup_fake("stuck", &["--linger-ms", "600000"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "stuck").unwrap();
        let name = InstanceName::new("stuck").unwrap();

        // Simulate the aftermath of a real StopUnconfirmed (stop_inner's
        // own bookkeeping on that path — see its docs): the row is
        // `stopping`, the handle is retained, and it is marked unconfirmed.
        registry.set_state(&name, LifecycleState::Stopping).unwrap();
        sup.running.get_mut(&name).unwrap().stop_unconfirmed = true;

        let start = Instant::now();
        let err = sup.stop(&registry, "stuck", None).unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(&err, EngineError::StopUnconfirmed { name, .. } if name == "stuck"),
            "expected StopUnconfirmed, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "a retry against a still-alive stuck instance must poll cheaply (ProcessBackend::poll, \
             never ProcessBackend::stop), not re-block for a whole new \
             graceful-window/SIGKILL/confirm cycle: {elapsed:?}"
        );
        // No compounding: the row is untouched (still stopping), the handle
        // is still retained (not dropped) for a further retry to reconcile.
        assert_eq!(state_of(&registry, "stuck"), LifecycleState::Stopping);
        assert!(
            sup.running.contains_key(&name),
            "the handle must be retained across a failed retry, never silently dropped"
        );

        // Teardown: really kill the still-running process so it does not
        // leak past this test.
        let supervised = sup.running.get_mut(&name).unwrap();
        let _ = sup
            .backend
            .stop(&mut supervised.handle, Duration::from_secs(2));
    }

    #[test]
    fn stop_retry_self_heals_once_the_stuck_process_actually_exits() {
        // The self-healing counterpart: once the process behind a
        // stop_unconfirmed handle has ACTUALLY died (the OS condition that
        // made confirmation time out has cleared), a retry `stop()` must
        // reconcile the instance to `stopped` — never leaving it permanently
        // stuck just because one earlier attempt could not confirm death in
        // time.
        let (_state, _manifest, registry) = setup_fake("heals", &["--linger-ms", "600000"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "heals").unwrap();
        let name = InstanceName::new("heals").unwrap();

        registry.set_state(&name, LifecycleState::Stopping).unwrap();
        sup.running.get_mut(&name).unwrap().stop_unconfirmed = true;

        // Simulate the OS condition clearing: the process ACTUALLY exits now.
        // A real, portable kill via the SAME ProcessBackend::stop the
        // production code already uses (cfg-free at this call site — the
        // OS-specific mechanics live entirely in `backends/`), not a raw
        // OS-specific signal call, so this stays a legitimate domain-layer
        // test. The process is NOT genuinely stuck in this test (only its
        // BOOKKEEPING pretends it was), so this succeeds quickly.
        {
            let supervised = sup.running.get_mut(&name).unwrap();
            sup.backend
                .stop(&mut supervised.handle, Duration::from_secs(2))
                .expect("the process is not genuinely stuck in this test and must die promptly");
        }

        let instance = sup
            .stop(&registry, "heals", None)
            .expect("a retry must self-heal once the process is confirmed dead");
        assert_eq!(
            instance.state,
            LifecycleState::Stopped,
            "the stuck stopping row must reconcile to stopped, not stay stuck forever"
        );
        assert!(
            !sup.running.contains_key(&name),
            "the handle must be released once reconciled"
        );
    }

    #[test]
    fn poll_once_reconciles_a_stuck_unconfirmed_stop_to_stopped_self_healing() {
        // The crash reaper's OWN reconciliation path (`poll_once`) — the
        // OTHER self-healing route besides a manual retry `stop()` (whichever
        // observes the exit first): when a `stop_unconfirmed`-marked handle's
        // process is found `Exited` during a routine reaper poll, poll_once
        // must finalize `stopping -> stopped` itself, rather than silently
        // dropping the handle (which would leave the row PERMANENTLY stuck,
        // since a later retry `stop()` would find no handle to poll).
        // Contrast directly with
        // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`
        // above: that test's contrived `stopping` row is NOT
        // `stop_unconfirmed` (it never went through a real stop attempt), so
        // it correctly keeps the ORIGINAL silent-drop behavior — proving this
        // fix pass changes behavior ONLY for the scenario it targets.
        // --crash-after-ms must comfortably EXCEED READINESS_WINDOW (300ms) or
        // the process looks like an immediate-exit launch failure (AC2)
        // instead of a clean start that later crashes — the same pitfall
        // documented above for `--linger-ms` in this file; 500ms is that
        // established, proven-safe margin.
        let (_state, _manifest, registry) = setup_fake("reaperheals", &["--crash-after-ms", "500"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "reaperheals").unwrap();
        let name = InstanceName::new("reaperheals").unwrap();

        registry.set_state(&name, LifecycleState::Stopping).unwrap();
        sup.running.get_mut(&name).unwrap().stop_unconfirmed = true;

        // Wait for the process to actually exit on its own, then let the
        // reaper observe it.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let plans = sup.poll_once(&registry);
            assert!(
                plans.is_empty(),
                "this is a reconciliation, never a crash/restart"
            );
            if !sup.running.contains_key(&name) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "handle should be reconciled after exit"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            state_of(&registry, "reaperheals"),
            LifecycleState::Stopped,
            "the reaper must finalize a stuck-unconfirmed stopping row to stopped, not leave it \
             permanently stuck"
        );
    }

    #[test]
    fn poll_once_with_no_handles_is_a_noop() {
        // The empty-reaper path: with nothing supervised, poll_once returns no
        // plans and touches nothing.
        let (_state, _manifest, registry) = setup_fake("idle", &["--linger-ms", "600000"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        assert!(sup.poll_once(&registry).is_empty());
    }

    #[test]
    fn adopt_orphans_with_no_records_adopts_nothing() {
        // The empty-reconcile path: with no persisted spawn records, adoption
        // adopts nothing and returns 0.
        let (_state, _manifest, registry) = setup_fake("idle", &["--linger-ms", "600000"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        assert_eq!(sup.adopt_orphans(&registry), 0);
        assert!(sup.running.is_empty());
    }

    #[test]
    fn adopt_orphans_skips_a_policy_only_seed_record() {
        // A pid-0 record is a policy-only config seed (set before any start), NOT
        // a supervised process — adoption skips it (adopts nothing) and does NOT
        // reconcile the registered instance to failed or clear its policy.
        let (_state, _manifest, registry) = setup_fake("seedonly", &["--linger-ms", "600000"]);
        registry
            .set_restart_policy(
                &InstanceName::new("seedonly").unwrap(),
                RestartPolicy::Never,
            )
            .unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        assert_eq!(sup.adopt_orphans(&registry), 0);
        // Still registered; the policy seed survives (was not cleared).
        assert_eq!(state_of(&registry, "seedonly"), LifecycleState::Registered);
        let rec = registry
            .spawn_record(&InstanceName::new("seedonly").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(rec.restart_policy, RestartPolicy::Never);
    }

    #[test]
    fn default_stop_window_is_30s() {
        assert_eq!(DEFAULT_STOP_WINDOW, Duration::from_secs(30));
    }

    #[test]
    fn read_events_from_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.log");
        assert!(read_events_from(&path).unwrap().is_empty());
    }

    #[test]
    fn append_then_read_round_trips_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.log");
        let e1 = TransitionEvent::new(
            "demo",
            LifecycleState::Registered,
            LifecycleState::Starting,
            TransitionCause::command("start"),
            "2026-07-04T00:00:00Z",
        );
        let e2 = TransitionEvent::new(
            "demo",
            LifecycleState::Starting,
            LifecycleState::Running,
            TransitionCause::AdapterReady,
            "2026-07-04T00:00:01Z",
        );
        append_event(&path, &e1).unwrap();
        append_event(&path, &e2).unwrap();
        let back = read_events_from(&path).unwrap();
        assert_eq!(back, vec![e1, e2]);
    }

    #[test]
    fn read_events_rejects_a_corrupt_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.log");
        std::fs::write(&path, "{ not valid json\n").unwrap();
        let err = read_events_from(&path).unwrap_err();
        assert!(err.contains("corrupt instance-log line 1"), "{err}");
    }

    #[test]
    fn supervisor_constructs_empty() {
        let sup = Supervisor::new();
        assert!(sup.running.is_empty());
    }

    #[test]
    fn registry_to_engine_maps_each_variant() {
        use super::super::error::RegistryError as R;
        use super::super::name::NameError;

        // NotFound → NotFound.
        assert!(matches!(
            registry_to_engine(R::NotFound { name: "x".into() }),
            EngineError::NotFound { .. }
        ));
        // InvalidName → InvalidName.
        assert!(matches!(
            registry_to_engine(R::InvalidName {
                name: "X".into(),
                reason: NameError::BadChar,
            }),
            EngineError::InvalidName { .. }
        ));
        // Io → Log (naming the path).
        assert!(matches!(
            registry_to_engine(R::Io {
                name: "x".into(),
                path: "/p".into(),
                source: std::io::Error::other("boom"),
            }),
            EngineError::Log { .. }
        ));
        // A snapshot-shaped registry error → AdapterUnresolved.
        assert!(matches!(
            registry_to_engine(R::ManifestNotFound { path: "/m".into() }),
            EngineError::AdapterUnresolved { .. }
        ));
    }

    #[test]
    fn launch_to_engine_wraps_as_adapter_unresolved() {
        let name = InstanceName::new("svc").unwrap();
        let err = launch_to_engine(
            &name,
            LaunchResolveError::NativeHasNoLaunch {
                kind: "mock".into(),
            },
        );
        match err {
            EngineError::AdapterUnresolved { name, detail } => {
                assert_eq!(name, "svc");
                assert!(detail.contains("no launch command"));
            }
            other => panic!("expected AdapterUnresolved, got {other}"),
        }
    }

    #[test]
    fn config_to_engine_and_config_apply_to_engine_wrap_as_adapter_unresolved() {
        // Story 2-2: a config-resolution failure and a config-apply (file-render)
        // failure both surface as an unresolved-adapter launch failure naming the
        // instance + preserving the detail, so `start` rejects cleanly.
        let name = InstanceName::new("svc").unwrap();
        let cfg_err = config_to_engine(
            &name,
            crate::domain::ConfigError::NotFound { name: "svc".into() },
        );
        match cfg_err {
            EngineError::AdapterUnresolved { name, detail } => {
                assert_eq!(name, "svc");
                assert!(detail.contains("svc"), "detail preserved: {detail}");
            }
            other => panic!("expected AdapterUnresolved, got {other}"),
        }
        let apply_err = config_apply_to_engine(
            &name,
            ConfigApplyError::FileRender {
                key: "config/agent.toml".into(),
                path: "config/agent.toml".into(),
                detail: "disk full".into(),
            },
        );
        match apply_err {
            EngineError::AdapterUnresolved { name, detail } => {
                assert_eq!(name, "svc");
                assert!(detail.contains("config/agent.toml"), "{detail}");
                assert!(detail.contains("disk full"), "{detail}");
            }
            other => panic!("expected AdapterUnresolved, got {other}"),
        }
    }

    #[test]
    fn start_with_an_unwritable_file_target_rejects_before_any_state_change() {
        // Story 2-2 end-to-end error path (accurate atomicity, Fix #5): a manifest
        // `[config.model]` FILE target whose parent path is blocked (a regular file
        // sits where the config directory must be in the Agent Home) fails the
        // config-mapping application at start. Because the mapping is applied BEFORE
        // the `starting` transition, the start REJECTS (AdapterUnresolved) and the
        // instance stays in its PRIOR state (`registered`) — it does NOT land
        // `failed`, and never reaches `running`. Exercises the start_inner
        // config-apply error branch + config_apply_to_engine.
        let (_state, _manifest, registry) = setup_fake_with_config(
            "badfile",
            &["--linger-ms", "600000"],
            "[config.model]\nfile = { path = \"blocked/agent.toml\", key = \"k\" }\n",
        );
        let name = InstanceName::new("badfile").unwrap();
        registry.set_config(&name, "model", "gpt-4").unwrap();
        // Block the file target's parent: put a regular FILE at <home>/blocked so
        // create_dir_all(<home>/blocked) fails when rendering blocked/agent.toml.
        let home = registry.agent_home(&name);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("blocked"), b"not a dir").unwrap();

        let mut sup = Supervisor::with_backoff(fast_backoff());
        let err = sup.start(&registry, "badfile").unwrap_err();
        assert!(
            matches!(err, EngineError::AdapterUnresolved { .. }),
            "a bad file target must fail the start; got {err}"
        );
        // Never reached running (the failure was before the starting transition, so
        // the instance stays registered).
        assert_eq!(state_of(&registry, "badfile"), LifecycleState::Registered);
    }

    #[test]
    fn read_events_skips_blank_lines() {
        // Blank lines in the log are ignored (only JSON event lines are parsed).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.log");
        let e = TransitionEvent::new(
            "demo",
            LifecycleState::Registered,
            LifecycleState::Starting,
            TransitionCause::command("start"),
            "2026-07-04T00:00:00Z",
        );
        let line = serde_json::to_string(&e).unwrap();
        std::fs::write(&path, format!("\n{line}\n\n")).unwrap();
        let back = read_events_from(&path).unwrap();
        assert_eq!(back, vec![e]);
    }

    // ---- Story 3-2: the breach record write error is surfaced, not swallowed ----

    #[test]
    fn record_breach_surfaces_a_write_failure_instead_of_swallowing_it() {
        // FR-21 ("the breach is always recorded"): if the durable breach-log write
        // fails (disk full / IO / perms), the error must be SURFACED (an honest
        // stderr diagnostic), NOT silently discarded while the action still fires —
        // otherwise the mandated record is lost with no trace. We force BOTH the
        // dir-create AND the append to fail by placing a regular FILE where the
        // per-instance log DIRECTORY (`<home>/logs`, the breach log's parent) must
        // be: `create_dir_all(parent)` fails (a file sits at that path) and the
        // subsequent append cannot open `logs/breaches.log` either. `record_breach`
        // must NOT panic (it stays non-fatal) and must not lose data silently — this
        // proves both surfaced-error branches are reachable and the enforcement path
        // survives. Pure unit test: no process, no OS gate.
        let (_state, _manifest, registry) = setup_fake("breachio", &["--linger-ms", "600000"]);
        let name = InstanceName::new("breachio").unwrap();
        // Block the log-dir path with a regular file so create_dir_all(<home>/logs)
        // fails (its target is a file, not a directory) and the append fails too.
        let log_dir = registry.instance_log_dir(&name);
        std::fs::create_dir_all(log_dir.parent().unwrap()).unwrap();
        std::fs::write(&log_dir, b"not a directory").unwrap();
        assert!(
            log_dir.is_file(),
            "the log-dir path must be a FILE to force both the dir-create and append failures"
        );

        let sup = Supervisor::with_backoff(fast_backoff());
        // Must not panic — both the dir-create and the append failures are logged
        // (surfaced) and enforcement continues rather than crashing.
        sup.record_token_breach(
            &registry,
            &name,
            &RunId::mint(),
            BreachScope::Cumulative,
            30,
            60,
            BreachAction::Warn,
            "self-reported",
        );
        // The blocking file is untouched — no breach file was sneaked in, confirming
        // the write genuinely failed and we exercised the surfaced-error branches.
        assert!(log_dir.is_file());
    }

    // ---- Story 3-1 drain planning: H1 terminal-tail + M2 shrink guard ----

    #[test]
    fn plan_drain_midrun_stops_at_the_last_newline() {
        // MID-RUN: a live process may still finish a partial final line, so only bytes
        // up to the last newline are consumed; the trailing partial waits.
        let bytes = b"a\nb\nhalf-written";
        let plan = plan_drain(bytes, 0, DrainMode::MidRun);
        // Consumes "a\nb\n" (4 bytes), leaving "half-written" for the next pass.
        assert_eq!(
            plan,
            DrainPlan::Consume {
                range: 0..4,
                new_cursor: 4
            }
        );
    }

    #[test]
    fn plan_drain_midrun_with_no_newline_yet_consumes_nothing() {
        // A tail with no complete line yet: nothing to consume this pass (MidRun).
        assert_eq!(
            plan_drain(b"no newline yet", 0, DrainMode::MidRun),
            DrainPlan::Nothing
        );
    }

    #[test]
    fn plan_drain_terminal_consumes_a_newline_less_final_line() {
        // H1: on a TERMINAL drain the process is dead, so a final usage line flushed
        // WITHOUT a trailing newline must be consumed to end-of-log (or it is stranded
        // and the next Run's cursor skips past it → a permanent under-count).
        let bytes = b"a\nKTESIO_USAGE {\"sequence\":0,\"input_tokens\":10,\"output_tokens\":20}";
        let plan = plan_drain(bytes, 0, DrainMode::Terminal);
        // The WHOLE tail is consumed (no trailing newline required).
        assert_eq!(
            plan,
            DrainPlan::Consume {
                range: 0..bytes.len(),
                new_cursor: bytes.len() as u64
            }
        );
    }

    #[test]
    fn plan_drain_terminal_from_a_cursor_consumes_only_the_new_tail() {
        // The terminal tail is measured FROM the cursor (already-read bytes are not
        // re-consumed) and still needs no trailing newline.
        let bytes = b"old\nnew-tail-no-nl";
        let plan = plan_drain(bytes, 4, DrainMode::Terminal); // cursor past "old\n"
        assert_eq!(
            plan,
            DrainPlan::Consume {
                range: 4..bytes.len(),
                new_cursor: bytes.len() as u64
            }
        );
    }

    #[test]
    fn plan_drain_shrink_snaps_the_cursor_and_ingests_nothing() {
        // M2: the log is shorter than the cursor (a truncate/rotation). We must NOT
        // re-read from 0 (double-count → inflated bill); instead snap the cursor to
        // the new length and ingest nothing. Holds for BOTH modes.
        let bytes = b"short"; // len 5
        assert_eq!(
            plan_drain(bytes, 100, DrainMode::MidRun),
            DrainPlan::Shrunk { new_cursor: 5 }
        );
        assert_eq!(
            plan_drain(bytes, 100, DrainMode::Terminal),
            DrainPlan::Shrunk { new_cursor: 5 },
            "the shrink guard applies on the terminal path too"
        );
    }

    #[test]
    fn plan_drain_at_end_of_log_consumes_nothing() {
        // Cursor exactly at len (all bytes already read): an empty tail → Nothing, in
        // both modes (no phantom terminal consume of zero bytes).
        let bytes = b"a\nb\n";
        assert_eq!(plan_drain(bytes, 4, DrainMode::MidRun), DrainPlan::Nothing);
        assert_eq!(
            plan_drain(bytes, 4, DrainMode::Terminal),
            DrainPlan::Nothing
        );
    }

    // ---- AI-63: incremental usage-tail read (drain_usage_for no longer reads the
    //      whole never-rotated agent.log on every reaper tick under the global lock).
    //      These prove (1) the read is bounded by NEW bytes not total size, and
    //      (2)/(3)/(4) the three billing semantics — MidRun tail, Terminal newline-
    //      less tail, and the M2 shrink guard — are byte-identical via the new path.

    /// THE fix proof: a large already-consumed prefix is NOT re-read. The per-pass
    /// read is bounded by the NEW bytes (`len - cursor`), never the total file size.
    #[test]
    fn read_usage_tail_reads_only_the_new_bytes_not_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        // A big already-drained prefix (what the OLD code re-read every 250ms) plus
        // one small fresh usage line — the only bytes a drain should now touch.
        let prefix = vec![b'x'; 5 * 1024 * 1024]; // 5 MiB already consumed
        let fresh = b"KTESIO_USAGE {\"sequence\":0,\"input_tokens\":1,\"output_tokens\":2}\n";
        let mut content = prefix.clone();
        content.extend_from_slice(fresh);
        std::fs::write(&path, &content).unwrap();
        let cursor = prefix.len() as u64;

        let UsageTail::Tail { bytes } = read_usage_tail(&path, cursor) else {
            panic!("expected a Tail read");
        };
        // Read EXACTLY the new tail, not the 5 MiB prefix.
        assert_eq!(
            bytes.len(),
            fresh.len(),
            "read must be bounded by NEW bytes"
        );
        assert_eq!(bytes.as_slice(), fresh.as_slice());
        // ...and it is byte-identical to the slice the OLD whole-file read produced.
        let whole = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes.as_slice(),
            &whole[cursor as usize..],
            "the tail must equal the old code's bytes[cursor..] slice"
        );
    }

    /// (2) MidRun across MULTIPLE sequential drains: each pass reads only the tail
    /// appended since the last cursor, and the cursor advances identically to the
    /// old whole-file path — proving incremental draining loses nothing.
    #[test]
    fn read_usage_tail_incremental_across_multiple_drains_matches_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");

        // Drain 1: two complete lines.
        std::fs::write(&path, b"a\nb\n").unwrap();
        assert_eq!(
            select_new(&path, 0, DrainMode::MidRun),
            (Some(b"a\nb\n".to_vec()), 4)
        );
        assert_eq!(
            select_new(&path, 0, DrainMode::MidRun),
            select_old(&path, 0, DrainMode::MidRun)
        );
        // The read at cursor 4 sees only the NEW bytes (none yet) — an empty tail.
        assert_eq!(
            read_usage_tail(&path, 4),
            UsageTail::Tail { bytes: Vec::new() }
        );

        // Drain 2: append two more lines; draining from cursor 4 consumes ONLY them.
        std::fs::write(&path, b"a\nb\nc\nd\n").unwrap();
        let got = read_usage_tail(&path, 4);
        assert_eq!(
            got,
            UsageTail::Tail {
                bytes: b"c\nd\n".to_vec()
            },
            "only the new tail"
        );
        assert_eq!(
            select_new(&path, 4, DrainMode::MidRun),
            (Some(b"c\nd\n".to_vec()), 8)
        );
        assert_eq!(
            select_new(&path, 4, DrainMode::MidRun),
            select_old(&path, 4, DrainMode::MidRun)
        );

        // A partial trailing line (no newline yet) waits — MidRun consumes nothing.
        std::fs::write(&path, b"a\nb\nc\nd\nhalf").unwrap();
        assert_eq!(select_new(&path, 8, DrainMode::MidRun), (None, 8));
        assert_eq!(
            select_new(&path, 8, DrainMode::MidRun),
            select_old(&path, 8, DrainMode::MidRun)
        );
    }

    /// (3) Terminal newline-less tail (the H1 fix): the process is dead, so a final
    /// usage line flushed WITHOUT a trailing newline is consumed to end-of-log via
    /// the incremental read too — never stranded.
    #[test]
    fn new_read_path_terminal_consumes_a_newline_less_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        let head = b"old\n"; // already consumed
        let newline_less_tail = b"KTESIO_USAGE {\"sequence\":0}"; // no trailing \n
        let mut content = head.to_vec();
        content.extend_from_slice(newline_less_tail);
        std::fs::write(&path, &content).unwrap();
        let cursor = head.len() as u64;
        let end = content.len() as u64;
        // From a cursor past "old\n", Terminal consumes the whole newline-less tail
        // to end-of-log (H1) — never stranding the final usage line.
        assert_eq!(
            select_new(&path, cursor, DrainMode::Terminal),
            (Some(newline_less_tail.to_vec()), end)
        );
        assert_eq!(
            select_new(&path, cursor, DrainMode::Terminal),
            select_old(&path, cursor, DrainMode::Terminal),
            "Terminal newline-less tail must be byte-identical via the incremental read"
        );
        // Contrast: MidRun would strand that partial line (no newline) — unchanged.
        assert_eq!(select_new(&path, cursor, DrainMode::MidRun), (None, cursor));
    }

    /// (4) M2 shrink guard: a file shorter than the cursor (truncate/rotation) snaps
    /// the cursor to the new length and ingests NOTHING — never re-reads from 0
    /// (which would double-count → an inflated bill).
    #[test]
    fn read_usage_tail_shrink_snaps_the_cursor_and_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        std::fs::write(&path, b"short").unwrap(); // len 5, cursor claims 100
        assert_eq!(
            read_usage_tail(&path, 100),
            UsageTail::Shrunk { new_cursor: 5 }
        );
        // Same decision as the old whole-file path, in both modes, ingesting nothing.
        assert_eq!(select_new(&path, 100, DrainMode::MidRun), (None, 5));
        assert_eq!(
            select_new(&path, 100, DrainMode::MidRun),
            select_old(&path, 100, DrainMode::MidRun)
        );
        assert_eq!(select_new(&path, 100, DrainMode::Terminal), (None, 5));
        assert_eq!(
            select_new(&path, 100, DrainMode::Terminal),
            select_old(&path, 100, DrainMode::Terminal)
        );
    }

    /// A missing/unreadable log is a best-effort skip (cursor untouched), exactly
    /// like the old `std::fs::read` `Err(_)` arm.
    #[test]
    fn read_usage_tail_missing_file_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.log");
        assert_eq!(read_usage_tail(&path, 0), UsageTail::Unavailable);
        assert_eq!(
            select_new(&path, 7, DrainMode::MidRun),
            (None, 7),
            "cursor untouched"
        );
        assert_eq!(
            select_new(&path, 7, DrainMode::MidRun),
            select_old(&path, 7, DrainMode::MidRun)
        );
    }

    /// The exhaustive equivalence harness: for a battery of (content, cursor, mode)
    /// states, the block consumed AND the resulting cursor are byte-identical
    /// between the OLD whole-file path and the NEW incremental path. This is the
    /// reviewer's "counts are unchanged" oracle — falsifying it means the fix
    /// changed billing, and this test would fail.
    #[test]
    fn new_read_path_matches_whole_file_path_over_a_battery_of_states() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        let contents: &[&[u8]] = &[
            b"",
            b"\n",
            b"a\nb\n",
            b"a\nb\nhalf-written",
            b"KTESIO_USAGE {\"sequence\":0,\"input_tokens\":10,\"output_tokens\":20}\n",
            b"KTESIO_USAGE {\"sequence\":0,\"input_tokens\":10,\"output_tokens\":20}", // no nl
            b"line-with-no-newline-at-all",
        ];
        for content in contents {
            std::fs::write(&path, content).unwrap();
            let len = content.len() as u64;
            // Cursors at, around, and beyond the length (the last exercises shrink).
            for cursor in [0u64, 1, len.saturating_sub(1), len, len + 1, len + 1000] {
                for mode in [DrainMode::MidRun, DrainMode::Terminal] {
                    assert_eq!(
                        select_new(&path, cursor, mode),
                        select_old(&path, cursor, mode),
                        "divergence at content={content:?} cursor={cursor} mode={mode:?}"
                    );
                }
            }
        }
    }

    /// The NEW selection logic, mirroring `drain_usage_for`'s incremental path
    /// (minus the ingest side effect): returns the block that would be handed to
    /// `usage_source.drain` and the resulting ABSOLUTE `usage_cursor`.
    fn select_new(path: &Path, cursor: u64, mode: DrainMode) -> (Option<Vec<u8>>, u64) {
        match read_usage_tail(path, cursor) {
            UsageTail::Unavailable => (None, cursor),
            UsageTail::Shrunk { new_cursor } => (None, new_cursor),
            UsageTail::Tail { bytes } => match plan_drain(&bytes, 0, mode) {
                DrainPlan::Consume {
                    range,
                    new_cursor: consumed,
                } => (Some(bytes[range].to_vec()), cursor + consumed),
                DrainPlan::Nothing | DrainPlan::Shrunk { .. } => (None, cursor),
            },
        }
    }

    /// The OLD selection logic, mirroring the PRE-AI-63 `drain_usage_for` (whole-file
    /// `std::fs::read` + `plan_drain(&bytes, cursor, mode)`) — the reference the new
    /// path must match byte-for-byte.
    fn select_old(path: &Path, cursor: u64, mode: DrainMode) -> (Option<Vec<u8>>, u64) {
        let Ok(bytes) = std::fs::read(path) else {
            return (None, cursor);
        };
        match plan_drain(&bytes, cursor, mode) {
            DrainPlan::Shrunk { new_cursor } => (None, new_cursor),
            DrainPlan::Nothing => (None, cursor),
            DrainPlan::Consume { range, new_cursor } => (Some(bytes[range].to_vec()), new_cursor),
        }
    }

    // ---- Story 4-2: read_agent_log_since's follow-cursor planning (AC-D/AC-H) ----

    #[test]
    fn plan_follow_consumes_only_complete_lines_leaving_a_partial_tail() {
        let bytes = b"a\nb\nhalf-written";
        assert_eq!(
            plan_follow(bytes, 0),
            FollowPlan::Consume {
                range: 0..4,
                new_cursor: 4
            }
        );
    }

    #[test]
    fn plan_follow_with_no_newline_yet_consumes_nothing() {
        assert_eq!(
            plan_follow(b"no newline yet", 0),
            FollowPlan::Consume {
                range: 0..0,
                new_cursor: 0
            }
        );
    }

    #[test]
    fn plan_follow_from_a_cursor_consumes_only_the_new_tail() {
        let bytes = b"old\nnew-tail\n";
        assert_eq!(
            plan_follow(bytes, 4),
            FollowPlan::Consume {
                range: 4..bytes.len(),
                new_cursor: bytes.len() as u64
            }
        );
    }

    #[test]
    fn plan_follow_shrink_snaps_the_cursor_and_delivers_nothing() {
        // AC-D/AC-H rotation-notice path: the file is shorter than the
        // cursor (a rotation happened since the last poll). Snap, deliver
        // nothing this pass — the caller detects the snap-back itself.
        let bytes = b"short"; // len 5
        assert_eq!(
            plan_follow(bytes, 100),
            FollowPlan::Shrunk { new_cursor: 5 }
        );
    }

    #[test]
    fn plan_follow_at_end_of_log_consumes_nothing() {
        let bytes = b"a\nb\n";
        assert_eq!(
            plan_follow(bytes, 4),
            FollowPlan::Consume {
                range: 4..4,
                new_cursor: 4
            }
        );
    }

    // ---- Story 4-2: engine-attributed line rendering (Task 4) ----

    #[test]
    fn cause_suffix_and_engine_transition_line_text_cover_every_transition_cause() {
        // A direct, pure-function proof of the RENDERER's own completeness,
        // independent of which causes the current supervisor wiring happens
        // to route a live log_capture through (e.g. a launch-failure never
        // gets a log_capture today — see the Dev Agent Record) — the match
        // itself must stay exhaustive and correct for every variant.
        let cases: Vec<(TransitionCause, &str)> = vec![
            (TransitionCause::command("start"), " (start)"),
            (TransitionCause::AdapterReady, ""),
            (
                TransitionCause::launch_error("boom"),
                " (launch error: boom)",
            ),
            (TransitionCause::StopGraceful, ""),
            (
                TransitionCause::stop_forced("escalated"),
                " (forced: escalated)",
            ),
            (
                TransitionCause::pause_best_effort("windows"),
                " (best-effort: windows)",
            ),
            (
                TransitionCause::resume_best_effort("windows"),
                " (best-effort: windows)",
            ),
            (
                TransitionCause::crashed("exit code 1"),
                " (crashed: exit code 1)",
            ),
            (
                TransitionCause::restarted(2, 500),
                " (restart #2, waited 500ms)",
            ),
            (
                TransitionCause::budget_exceeded(BreachScope::PerRun, 1000, 1200),
                " (breach: per-run tokens)",
            ),
            (
                TransitionCause::cost_cap_exceeded(
                    BreachScope::Cumulative,
                    Micros(5_000_000),
                    Micros(5_250_000),
                    EstimateLabel::Estimated,
                ),
                " (breach: cumulative dollars)",
            ),
        ];
        for (cause, want_suffix) in cases {
            assert_eq!(cause_suffix(&cause), want_suffix, "{cause:?}");
        }

        // engine_transition_line_text wraps the suffix into the full "engine:
        // A -> B(...)" sentence.
        let event = TransitionEvent::new(
            "svc",
            LifecycleState::Running,
            LifecycleState::Paused,
            TransitionCause::pause_best_effort("windows"),
            "2026-07-15T00:00:00Z",
        );
        assert_eq!(
            engine_transition_line_text(&event),
            "engine: running -> paused (best-effort: windows)"
        );
    }

    // ---- Story 4-2: Supervisor::read_agent_log / read_agent_log_since ----

    fn log_line(instance: &str, stream: LogStream, text: &str, at: &str) -> LogLine {
        LogLine::new(instance, stream, text, at)
    }

    #[test]
    fn read_agent_log_on_an_unregistered_name_is_not_found() {
        // The deliberate improvement over read_events/read_breach_events'
        // precedent: an unregistered name is NotFound, not a silent empty.
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        let err = Supervisor::read_agent_log(&registry, "ghost").unwrap_err();
        assert!(matches!(err, EngineError::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn read_agent_log_on_a_registered_but_never_started_instance_is_empty_not_an_error() {
        let (_state, _manifest, registry) = setup_fake("neverstarted", &["--linger-ms", "600000"]);
        let (lines, cursor) = Supervisor::read_agent_log(&registry, "neverstarted").unwrap();
        assert!(lines.is_empty());
        assert_eq!(cursor, 0, "no file yet → the cursor starts at 0");
    }

    #[test]
    fn read_agent_log_reports_a_damaged_generation_as_a_typed_error_naming_the_file() {
        // `kt agent logs` is the only window a user has into what an agent
        // actually said, so a damaged log must FAIL LOUDLY and name the exact
        // file. The dangerous alternative is not a panic — it is a silent skip:
        // dropping an unparseable line (or an unreadable generation) would return
        // a shorter, plausible-looking log and hide agent output the user is
        // reading precisely because something went wrong. All three damage sites
        // are asserted because they are three separate arms on the read path, and
        // each one names a DIFFERENT path (a rotated generation vs the current
        // one), which is the part that makes the diagnostic actionable.
        let corrupt_line = "{not valid json}\n";

        // (1) The CURRENT generation contains an unparseable line.
        {
            let (_state, _manifest, registry) =
                setup_fake("badcurrent", &["--linger-ms", "600000"]);
            let name = InstanceName::new("badcurrent").unwrap();
            std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
            let current = registry.attributed_output_log_path(&name);
            std::fs::write(&current, corrupt_line).unwrap();

            let err = Supervisor::read_agent_log(&registry, "badcurrent").unwrap_err();
            match err {
                EngineError::Log { name, path, detail } => {
                    assert_eq!(name, "badcurrent");
                    assert_eq!(path, current.to_string_lossy());
                    // The line NUMBER is what makes this fixable by hand.
                    assert!(detail.contains("corrupt output-log line 1"), "{detail}");
                }
                other => panic!("expected EngineError::Log, got {other:?}"),
            }
        }

        // (2) A ROTATED generation contains an unparseable line — the error must
        // name THAT generation's path, not the current one, or the user deletes
        // the wrong file.
        {
            let (_state, _manifest, registry) = setup_fake("badgen", &["--linger-ms", "600000"]);
            let name = InstanceName::new("badgen").unwrap();
            std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
            let rotated = registry.attributed_output_log_generation_path(&name, 1);
            std::fs::write(&rotated, corrupt_line).unwrap();
            // A perfectly good current generation must NOT rescue the read.
            std::fs::write(
                registry.attributed_output_log_path(&name),
                format!(
                    "{}\n",
                    serde_json::to_string(&log_line(
                        "badgen",
                        LogStream::Engine,
                        "fine",
                        "2026-07-15T00:00:00Z"
                    ))
                    .unwrap()
                ),
            )
            .unwrap();

            let err = Supervisor::read_agent_log(&registry, "badgen").unwrap_err();
            match err {
                EngineError::Log { path, .. } => {
                    assert_eq!(path, rotated.to_string_lossy());
                }
                other => panic!("expected EngineError::Log, got {other:?}"),
            }
        }

        // (3) The CURRENT log path is unreadable for a reason OTHER than "missing"
        // — a directory sits where the file belongs. A missing file is a legitimate
        // empty log (asserted elsewhere); this must NOT be quietly folded into that
        // case, because "no output yet" and "your log is broken" are different
        // answers to the user's question.
        {
            let (_state, _manifest, registry) = setup_fake("blocked", &["--linger-ms", "600000"]);
            let name = InstanceName::new("blocked").unwrap();
            std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
            let current = registry.attributed_output_log_path(&name);
            std::fs::create_dir(&current).unwrap();

            let err = Supervisor::read_agent_log(&registry, "blocked").unwrap_err();
            match err {
                EngineError::Log { name, path, detail } => {
                    assert_eq!(name, "blocked");
                    assert_eq!(path, current.to_string_lossy());
                    assert!(!detail.is_empty(), "the OS detail must be preserved");
                }
                other => panic!("expected EngineError::Log, got {other:?}"),
            }
        }
    }

    #[test]
    fn read_agent_log_concatenates_generations_oldest_to_newest() {
        // AC-A/AC-G: hand-craft the 3 generations directly (deterministic,
        // no real rotation/process needed) and assert the read order is
        // oldest-generation-first, current-generation-last — append order,
        // never a timestamp re-sort.
        let (_state, _manifest, registry) = setup_fake("gens", &["--linger-ms", "600000"]);
        let name = InstanceName::new("gens").unwrap();
        std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();

        let write_line = |path: &Path, l: &LogLine| {
            std::fs::write(path, format!("{}\n", serde_json::to_string(l).unwrap())).unwrap();
        };
        write_line(
            &registry.attributed_output_log_generation_path(&name, 2),
            &log_line(
                "gens",
                LogStream::AgentOut,
                "oldest",
                "2026-07-15T00:00:00Z",
            ),
        );
        write_line(
            &registry.attributed_output_log_generation_path(&name, 1),
            &log_line(
                "gens",
                LogStream::AgentErr,
                "middle",
                "2026-07-15T00:00:01Z",
            ),
        );
        let current_path = registry.attributed_output_log_path(&name);
        write_line(
            &current_path,
            &log_line("gens", LogStream::Engine, "newest", "2026-07-15T00:00:02Z"),
        );

        let (lines, cursor) = Supervisor::read_agent_log(&registry, "gens").unwrap();
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["oldest", "middle", "newest"]);
        let streams: Vec<LogStream> = lines.iter().map(|l| l.stream).collect();
        assert_eq!(
            streams,
            vec![LogStream::AgentOut, LogStream::AgentErr, LogStream::Engine]
        );
        // The returned cursor (M1, review of #80) is the CURRENT generation's
        // exact byte length — matching read_agent_log_since's cursor shape —
        // never the concatenated multi-generation total.
        assert_eq!(
            cursor,
            std::fs::metadata(&current_path).unwrap().len(),
            "cursor must be the CURRENT generation's byte length only"
        );
    }

    #[test]
    fn read_agent_log_since_on_an_unregistered_name_is_not_found() {
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        let err = Supervisor::read_agent_log_since(&registry, "ghost", 0).unwrap_err();
        assert!(matches!(err, EngineError::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn read_agent_log_since_happy_path_reads_only_the_new_tail() {
        let (_state, _manifest, registry) = setup_fake("since", &["--linger-ms", "600000"]);
        let name = InstanceName::new("since").unwrap();
        std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
        let path = registry.attributed_output_log_path(&name);

        let l1 = log_line("since", LogStream::AgentOut, "one", "2026-07-15T00:00:00Z");
        std::fs::write(&path, format!("{}\n", serde_json::to_string(&l1).unwrap())).unwrap();
        let (first, cursor1) = Supervisor::read_agent_log_since(&registry, "since", 0).unwrap();
        assert_eq!(first, vec![l1]);
        assert!(cursor1 > 0);

        // No new bytes yet: an empty read at the same cursor.
        let (none, cursor_same) =
            Supervisor::read_agent_log_since(&registry, "since", cursor1).unwrap();
        assert!(none.is_empty());
        assert_eq!(cursor_same, cursor1);

        // Append a second line; read_agent_log_since(cursor1) returns ONLY it.
        let l2 = log_line("since", LogStream::AgentErr, "two", "2026-07-15T00:00:01Z");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        writeln!(f, "{}", serde_json::to_string(&l2).unwrap()).unwrap();
        drop(f);
        let (second, cursor2) =
            Supervisor::read_agent_log_since(&registry, "since", cursor1).unwrap();
        assert_eq!(second, vec![l2]);
        assert!(cursor2 > cursor1);
    }

    #[test]
    fn read_agent_log_since_detects_a_rotation_shrink_and_snaps_the_cursor() {
        // AC-D/AC-H: simulate a rotation having happened between two polls by
        // shrinking the current-generation file below the previously
        // returned cursor. The caller (Task 6's CLI) detects this by
        // comparing `next_cursor < cursor` — assert that property holds.
        let (_state, _manifest, registry) = setup_fake("rot", &["--linger-ms", "600000"]);
        let name = InstanceName::new("rot").unwrap();
        std::fs::create_dir_all(registry.instance_log_dir(&name)).unwrap();
        let path = registry.attributed_output_log_path(&name);

        let l1 = log_line(
            "rot",
            LogStream::AgentOut,
            "before-rotation",
            "2026-07-15T00:00:00Z",
        );
        std::fs::write(&path, format!("{}\n", serde_json::to_string(&l1).unwrap())).unwrap();
        let (_lines, cursor) = Supervisor::read_agent_log_since(&registry, "rot", 0).unwrap();
        assert!(cursor > 0);

        // Simulate rotation: the current generation is now a FRESH, SHORTER
        // file (as if it had just been rotated and a new line appended).
        let l2 = log_line(
            "rot",
            LogStream::AgentOut,
            "after-rotation",
            "2026-07-15T00:00:05Z",
        );
        std::fs::write(&path, format!("{}\n", serde_json::to_string(&l2).unwrap())).unwrap();
        let new_len = std::fs::metadata(&path).unwrap().len();
        assert!(new_len < cursor, "the fixture must genuinely shrink");

        let (lines, next_cursor) =
            Supervisor::read_agent_log_since(&registry, "rot", cursor).unwrap();
        assert!(lines.is_empty(), "nothing delivered on the shrink pass");
        assert!(
            next_cursor < cursor,
            "the returned cursor must snap BELOW the one just passed in — the \
             caller's rotation-notice signal"
        );
        assert_eq!(next_cursor, new_len);
    }

    #[test]
    fn read_agent_log_on_an_invalid_name_is_invalid_name() {
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        let err = Supervisor::read_agent_log(&registry, "Not Valid!").unwrap_err();
        assert!(matches!(err, EngineError::InvalidName { .. }), "{err:?}");
    }

    #[test]
    fn read_agent_log_since_on_an_invalid_name_is_invalid_name() {
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        let err = Supervisor::read_agent_log_since(&registry, "Not Valid!", 0).unwrap_err();
        assert!(matches!(err, EngineError::InvalidName { .. }), "{err:?}");
    }

    #[test]
    fn read_agent_log_since_on_a_registered_but_never_started_instance_is_empty() {
        // The current-generation file does not exist yet at all (the
        // instance was registered but never started) — an honest empty
        // read (Vec::new fallback for a NotFound file), never an error.
        let (_state, _manifest, registry) = setup_fake("neverstarted2", &["--linger-ms", "600000"]);
        let (lines, cursor) =
            Supervisor::read_agent_log_since(&registry, "neverstarted2", 0).unwrap();
        assert!(lines.is_empty());
        assert_eq!(cursor, 0);
    }

    // ---- Story 3-4: engine-observed base_url injection + source selection ----

    #[test]
    fn invocation_overrides_build_the_reserved_metering_leaf() {
        // AC6 (story 3-4): the engine injects the loopback URL at the reserved
        // `metering.base_url` key as an INVOCATION override, so the adapter's
        // config-mapping delivers it.
        let layer = invocation_overrides(Some("http://127.0.0.1:54321"), None).unwrap();
        let resolved = crate::domain::resolve([
            crate::domain::ConfigLayer::empty(),
            crate::domain::ConfigLayer::empty(),
            crate::domain::ConfigLayer::empty(),
            layer,
        ]);
        assert_eq!(
            resolved
                .value_display(crate::domain::METERING_BASE_URL_KEY)
                .as_deref(),
            Some("http://127.0.0.1:54321"),
            "the loopback URL lands at the reserved metering.base_url leaf"
        );
    }

    #[test]
    fn invocation_overrides_build_the_reserved_memory_dir_leaf() {
        // Story 5-1: the managed memory dir lands at the reserved `memory.dir`
        // leaf as an INVOCATION override.
        let layer =
            invocation_overrides(None, Some(Path::new("/state/agents/demo/memory"))).unwrap();
        let resolved = crate::domain::resolve([
            crate::domain::ConfigLayer::empty(),
            crate::domain::ConfigLayer::empty(),
            crate::domain::ConfigLayer::empty(),
            layer,
        ]);
        assert_eq!(
            resolved
                .value_display(crate::domain::MEMORY_DIR_KEY)
                .as_deref(),
            Some("/state/agents/demo/memory"),
            "the managed dir lands at the reserved memory.dir leaf"
        );
        // And a hand-set lower-layer value CANNOT win (AD-9 invocation precedence).
        let lower = crate::domain::ConfigLayer::parse(
            crate::domain::SourceLayer::Instance,
            "<test>",
            "[memory]\ndir = \"/operator/set\"\n",
        )
        .unwrap();
        let resolved = crate::domain::resolve([
            crate::domain::ConfigLayer::empty(),
            crate::domain::ConfigLayer::empty(),
            lower,
            invocation_overrides(None, Some(Path::new("/engine/computed/memory"))).unwrap(),
        ]);
        assert_eq!(
            resolved
                .value_display(crate::domain::MEMORY_DIR_KEY)
                .as_deref(),
            Some("/engine/computed/memory"),
        );
    }

    #[test]
    fn invocation_overrides_compose_both_and_are_none_for_neither() {
        // Both engine-injected values compose into ONE layer; neither → None so
        // the caller keeps the plain operator config.
        let combined =
            invocation_overrides(Some("http://127.0.0.1:1"), Some(Path::new("/m"))).unwrap();
        let resolved = crate::domain::resolve([
            crate::domain::ConfigLayer::empty(),
            crate::domain::ConfigLayer::empty(),
            crate::domain::ConfigLayer::empty(),
            combined,
        ]);
        assert_eq!(
            resolved
                .value_display(crate::domain::METERING_BASE_URL_KEY)
                .as_deref(),
            Some("http://127.0.0.1:1")
        );
        assert_eq!(
            resolved
                .value_display(crate::domain::MEMORY_DIR_KEY)
                .as_deref(),
            Some("/m")
        );
        assert!(invocation_overrides(None, None).is_none());
    }

    #[test]
    fn memory_delivery_notice_fires_only_when_attached_but_unmapped() {
        // DC-10 decision table: attached + unmapped ⇒ the notice names the
        // instance + path + the reserved key; mapped or unattached ⇒ silence.
        let name = InstanceName::new("svc").unwrap();
        let dir = Path::new("/state/agents/svc/memory");
        let unmapped = ConfigMapping::new(); // declares nothing
        let notice = memory_delivery_notice(Some(dir), &unmapped, &name).unwrap();
        assert!(notice.contains("svc"), "{notice}");
        assert!(notice.contains(dir.to_string_lossy().as_ref()), "{notice}");
        assert!(notice.contains("memory.dir"), "{notice}");

        let mapped = ConfigMapping::new().with(
            crate::domain::MEMORY_DIR_KEY,
            hemaka_adapter_api::ConfigTarget::env("SVC_MEMORY_DIR"),
        );
        assert!(memory_delivery_notice(Some(dir), &mapped, &name).is_none());
        assert!(memory_delivery_notice(None, &unmapped, &name).is_none());
    }

    #[test]
    fn self_reported_start_observed_listener_is_a_no_op() {
        // Source selection: a `self-reported` instance's start path is UNCHANGED —
        // start_observed_listener returns Ok(None), NO listener (even with an upstream
        // configured, which a self-reported instance ignores).
        let (_state, _manifest, registry) = setup_fake("selfrep_obs", &["--linger-ms", "1000"]);
        let name = InstanceName::new("selfrep_obs").unwrap();
        registry
            .set_config(&name, "metering.upstream_base_url", "http://127.0.0.1:9")
            .unwrap();
        let effective = registry
            .effective_config(&name, crate::domain::ConfigLayer::empty())
            .unwrap();
        let sup = Supervisor::with_backoff(fast_backoff());
        // self-reported (the fake manifest declares self-reported) → Ok(None).
        let result = sup
            .start_observed_listener(&name, "self-reported", &effective)
            .expect("self-reported is a no-op, not an error");
        assert!(
            result.is_none(),
            "a self-reported instance runs no listener"
        );
    }

    #[test]
    fn engine_observed_without_upstream_rejects_with_a_clear_error() {
        // AC-A: an `engine-observed` instance with NO configured upstream URL rejects
        // start_observed_listener with a traffic-free ObservedMetering error naming the
        // key (nothing to forward to). Uses the handle-less test supervisor, but the
        // upstream check fails FIRST (before the runtime-handle check), so the error
        // names the missing config key.
        let (_state, _manifest, registry) = setup_fake("obs_noup", &["--linger-ms", "1000"]);
        let name = InstanceName::new("obs_noup").unwrap();
        let effective = registry
            .effective_config(&name, crate::domain::ConfigLayer::empty())
            .unwrap();
        let sup = Supervisor::with_backoff(fast_backoff());
        let err = match sup.start_observed_listener(&name, "engine-observed", &effective) {
            Err(e) => e,
            Ok(_) => panic!("an engine-observed instance with no upstream must reject"),
        };
        match err {
            EngineError::ObservedMetering { name: n, detail } => {
                assert_eq!(n, "obs_noup");
                assert!(
                    detail.contains("metering.upstream_base_url"),
                    "detail names the missing key: {detail}"
                );
            }
            other => panic!("expected ObservedMetering, got {other:?}"),
        }
    }

    #[test]
    fn engine_observed_without_runtime_handle_rejects_cleanly() {
        // With an upstream configured but NO engine runtime handle (the handle-less
        // test supervisor), an engine-observed start rejects with a clear, traffic-free
        // error rather than panicking — the async engine is required to observe.
        let (_state, _manifest, registry) = setup_fake("obs_nort", &["--linger-ms", "1000"]);
        let name = InstanceName::new("obs_nort").unwrap();
        registry
            .set_config(&name, "metering.upstream_base_url", "http://127.0.0.1:9")
            .unwrap();
        let effective = registry
            .effective_config(&name, crate::domain::ConfigLayer::empty())
            .unwrap();
        let sup = Supervisor::with_backoff(fast_backoff()); // no runtime handle
        let err = match sup.start_observed_listener(&name, "engine-observed", &effective) {
            Err(e) => e,
            Ok(_) => panic!("no runtime handle must reject an engine-observed start"),
        };
        assert!(
            matches!(err, EngineError::ObservedMetering { .. }),
            "expected ObservedMetering, got {err:?}"
        );
        assert!(
            err.to_string().contains("runtime"),
            "names the cause: {err}"
        );
    }

    // ---- Story 12-1: detached start — the refusal + the in-process shape ----

    /// Write an `engine-observed` manifest (the `adoption.rs` shape) whose start
    /// exec is `fake_agent` + `args`.
    fn write_observed_manifest(dir: &Path, kind: &str, args: &[&str]) {
        let bin = hemaka_conformance::fake_agent_bin();
        let args_toml = args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args_toml}]\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"engine-observed\"\n\n\
             [config.\"metering.base_url\"]\nenv = \"OPENAI_BASE_URL\"\n",
            exec = bin.to_string_lossy(),
        );
        std::fs::write(dir.join("adapter.toml"), body).unwrap();
    }

    #[test]
    fn detached_start_of_an_engine_observed_instance_is_refused_before_any_side_effect() {
        // Story 12-1 (the AC): `start_detached` on an `engine-observed`
        // manifest refuses BEFORE ANY SIDE EFFECT — no `starting` transition,
        // no loopback listener, no spawn, no write-ahead record (a refusal
        // must not leave a phantom `running` row or a live half-start). The
        // error names WHY (the listener dies with the command) + the
        // remediation. The handle-less supervisor suffices: the refusal fires
        // before the listener would ever need a runtime.
        let state = tempfile::tempdir().unwrap();
        let manifest = tempfile::tempdir().unwrap();
        write_observed_manifest(manifest.path(), "obsdetach", &["--linger-ms", "600000"]);
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        registry
            .register_with_adapter(
                "obsdetach",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        let err = sup.start_detached(&registry, "obsdetach").unwrap_err();
        match &err {
            EngineError::DetachRefused { name, detail } => {
                assert_eq!(name, "obsdetach");
                assert!(
                    detail.contains("listener") && detail.contains("dies with the command"),
                    "the refusal must name the listener-lifetime why: {detail}"
                );
                assert!(
                    detail.contains("without --detach"),
                    "the refusal must carry the remediation: {detail}"
                );
            }
            other => panic!("expected DetachRefused, got {other:?}"),
        }
        // NO side effects: the row is still `registered`, and no write-ahead
        // spawn record exists (nothing was spawned or half-started).
        let instance = registry
            .lookup(&InstanceName::new("obsdetach").unwrap())
            .unwrap();
        assert_eq!(
            instance.state,
            LifecycleState::Registered,
            "the refusal must leave the prior state untouched"
        );
        assert!(
            registry
                .spawn_record(&InstanceName::new("obsdetach").unwrap())
                .unwrap()
                .is_none(),
            "the refusal must not leave a write-ahead spawn record"
        );
        // And a NON-detached start of the same instance is NOT refused by this
        // arm (it proceeds to the listener path — here the handle-less
        // supervisor's ObservedMetering failure, which proves the refusal is
        // detach-specific, not a general observed-start blocker).
        let err2 = sup.start(&registry, "obsdetach").unwrap_err();
        assert!(
            matches!(err2, EngineError::ObservedMetering { .. }),
            "a plain start of the observed instance must not hit DetachRefused, got {err2:?}"
        );
    }

    #[test]
    fn detached_start_of_a_self_reported_instance_runs_and_stops_in_process() {
        // Story 12-1: a self-reported instance detaches fine (no listener to
        // strand): it starts to `running`, the write-ahead record is committed
        // (the re-adoption input — detach must NOT clear or skip it), and
        // stop/pause keep working IN-PROCESS for as long as this supervisor
        // holds the (disarmed) handle. The survival-across-exit half is proven
        // end-to-end in tests/adoption.rs; this pins the in-process contract.
        let (_state, _manifest, registry) = setup_fake("detachproc", &["--linger-ms", "600000"]);
        let name = InstanceName::new("detachproc").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        let instance = sup.start_detached(&registry, "detachproc").unwrap();
        assert_eq!(instance.state, LifecycleState::Running);
        assert!(
            registry.spawn_record(&name).unwrap().is_some(),
            "a detached start must still commit the write-ahead record (adoption input)"
        );
        let stopped = sup
            .stop(&registry, "detachproc", Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(stopped.state, LifecycleState::Stopped);
    }

    #[test]
    fn a_detached_start_force_clears_the_stdin_pipe_despite_guaranteed_interaction() {
        // Review round 2 (verification gap): the detached stdin force-clear
        // (`pipe_stdin && !detach` in `start_inner`) had NO test — the backend
        // unit tests pre-clear the flag on their specs, so the supervisor's
        // clear was never observed end to end. The premise: a manifest that
        // DECLARES guaranteed interaction (an ATTACHED start would get a live
        // pipe). With `--detach` the pipe is force-cleared before the spawn,
        // so the observable is a HARD send failure — the no-pipe
        // `InteractionUnavailable`, never a silent write into a dead pipe and
        // never an EPIPE strand (the story 12-1 I/O matrix row).
        let (state, _manifest, registry) =
            setup_pause_guaranteed("detstdin", &["--linger-ms", "600000"]);
        let _ = state;
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start_detached(&registry, "detstdin").unwrap();
        let err = sup
            .send_input(&registry, "detstdin", "hello\n")
            .unwrap_err();
        assert!(
            matches!(&err, EngineError::InteractionUnavailable { detail, .. }
                if detail.contains("no live stdin pipe")),
            "a detached start must hold NO stdin pipe even on a guaranteed-interaction \
             manifest; got {err:?}"
        );
        sup.stop(&registry, "detstdin", Some(Duration::from_secs(5)))
            .unwrap();
    }

    #[test]
    fn a_crashed_detached_agent_restarts_detached_and_the_record_keeps_the_flag() {
        // Review round 2 (behavioral): `restart` used to hardcode the spawn's
        // detach flag to `false` — a crashed DETACHED agent restarted
        // ATTACHED, and the rewritten record silently downgraded
        // detach=true→false, so the next engine exit would kill the agent the
        // operator asked to survive. The restart must read the flag off the
        // surviving record and pass it through: the restarted agent stays
        // detached and the record still carries detach=true. (A real crash is
        // simulated with `--crash-after-ms` — NOT a stop, which would CLEAR
        // the record and make the fallback-to-attached read vacuous.)
        let (_state, _manifest, registry) =
            // 450ms: AFTER the 300ms readiness window (an earlier crash is a
            // launch failure, not a post-start crash — see
            // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`).
            setup_fake("detachrestart", &["--crash-after-ms", "450"]);
        let name = InstanceName::new("detachrestart").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start_detached(&registry, "detachrestart").unwrap();
        assert!(
            registry.spawn_record(&name).unwrap().unwrap().detach,
            "premise: the detached start recorded detach=true"
        );

        // The agent crashes on its own; the reaper lands the honest
        // `running → failed` transition and the record SURVIVES (a crash is
        // not a clean stop — the record is the restart's input).
        std::thread::sleep(Duration::from_millis(700));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            sup.poll_once(&registry);
            if sup.running.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "the crash was never reaped");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            registry.spawn_record(&name).unwrap().unwrap().detach,
            "the crash must leave the record (and its detach flag) intact"
        );

        // The Restart Policy restart: the flag comes off the record, the
        // restarted spawn stays detached, and the rewritten record KEEPS it.
        let instance = sup
            .restart(&registry, "detachrestart", 1, Duration::from_millis(10))
            .unwrap();
        assert_eq!(instance.state, LifecycleState::Running);
        let record = registry.spawn_record(&name).unwrap().unwrap();
        assert!(
            record.detach,
            "the RESTARTED record must still carry detach=true (the flag survives the \
             crash-restart, never silently downgraded)"
        );
        // And the restarted instance is stoppable in-process like any
        // detached handle (control ops are not disarmed — only the drop is).
        sup.stop(&registry, "detachrestart", Some(Duration::from_secs(5)))
            .unwrap();
    }

    // ---- Story 4-1: `send_input` — narrow branches best exercised as
    // Supervisor-level unit tests (no reaper/Engine involved), complementing
    // the AC-level proofs in `crates/hemaka-engine/tests/interaction.rs`. ----

    #[test]
    fn send_input_on_invalid_name_is_rejected() {
        // The name-resolve step: an invalid name is rejected with
        // EngineError::InvalidName, BEFORE any registry lookup.
        let (_state, _manifest, registry) = setup_fake("x", &["--linger-ms", "1000"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        let err = sup.send_input(&registry, "Bad Name", "hi").unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidName { .. }),
            "expected InvalidName, got {err:?}"
        );
    }

    #[test]
    fn send_input_after_the_process_exits_on_its_own_is_a_backend_error() {
        // A genuine BackendError write failure (Testing Notes: "a genuine
        // BackendError write failure if practically triggerable"). The
        // process exits ON ITS OWN (a short --linger-ms) but nothing has yet
        // reaped/transitioned the persisted row (deliberately no
        // `poll_once` call here — that would reap the handle and transition
        // to `failed`, which is exactly the race this test avoids so the
        // write is genuinely attempted). `send_input`'s write to the now
        // read-end-closed pipe fails at the OS level (EPIPE/BrokenPipe on
        // Unix), mapped to `EngineError::Backend` — the SAME generic mapping
        // every other backend op uses — never silently swallowed, never
        // misreported as `InteractionUnavailable`.
        //
        // Unix-only (EPIPE-on-closed-pipe semantics + a portable "is this pid
        // still alive" probe both need a real Unix liveness check); runtime
        // skip on Windows, NO `#[cfg]` (this file is outside the `backends`
        // allowlist) — mirrors the rest of the codebase's data-driven OS skip
        // convention.
        if OsId::current() == OsId::Windows {
            return;
        }
        // --linger-ms must comfortably EXCEED READINESS_WINDOW (300ms) or the
        // process looks like an immediate-exit launch failure (AC2) instead
        // of a clean start reaching `running`.
        let (_state, _manifest, registry) =
            setup_fake("exiter", &["--echo-stdin", "--linger-ms", "500"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "exiter").unwrap();

        // Wait past the KNOWN, self-configured linger deadline (a FIXED timer
        // this test itself set, not a guess at some other operation's
        // duration — the AI-35/38 "never guess" lesson is about polling
        // unknown async completion, which this is not). A liveness-probing
        // poll loop (`kill -0`) cannot substitute here: since nothing in this
        // test reaps the child (deliberately, so the persisted row stays
        // `running`), the process becomes a defunct zombie on exit, and a
        // zombie still answers `kill -0` as "alive" — the probe would never
        // observe the exit and the loop would spin until its own timeout.
        std::thread::sleep(Duration::from_millis(800));

        // The persisted row is STILL `running` (nothing has reaped it yet) —
        // send_input reaches the write, which fails at the OS level.
        let err = sup.send_input(&registry, "exiter", "hello").unwrap_err();
        assert!(
            matches!(err, EngineError::Backend { .. }),
            "expected Backend, got {err:?}"
        );
    }

    #[test]
    fn send_input_writes_into_a_live_stdin_pipe_that_the_agent_never_reads_and_reports_success() {
        // `send_input`'s SUCCESS arm (`Ok(()) => Ok(())`) at Supervisor level,
        // reachable WITHOUT any stdin round trip: `fake_agent` is spawned with
        // NEITHER `--echo-stdin` NOR `--sniff-stdin-at-startup`, so it provably
        // never reads a byte of its piped stdin — the write simply lands in the
        // OS pipe buffer (64KiB on Linux, 16KiB on macOS, 4KiB on Windows; a
        // handful of bytes never fills any of them) and `write_all` + `flush`
        // return immediately. Nothing here waits on the child for anything.
        //
        // That "no round trip" property is also what makes this the CHEAPEST
        // exerciser of the arm: the four AC-level proofs in
        // `crates/hemaka-engine/tests/interaction.rs` own it end to end, but
        // they each pay a real child echo, and a pure unit test that needs
        // nothing back from the child cannot be destabilised by anything
        // happening inside it.
        //
        // Bonus (branch, not line): the two sends straddle AC-F's trailing-
        // newline branch — "hello" takes the `push(b'\n')` side, "world\n" the
        // already-terminated side.
        let (_state, _manifest, registry) = setup_fake("liveio", &["--linger-ms", "600000"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "liveio").unwrap();

        sup.send_input(&registry, "liveio", "hello")
            .expect("a small write into a live, unfilled stdin pipe must succeed");
        sup.send_input(&registry, "liveio", "world\n")
            .expect("an already-newline-terminated write must succeed the same way");

        // Teardown.
        let _ = sup.stop(&registry, "liveio", Some(Duration::from_millis(200)));
    }

    #[test]
    fn send_input_past_the_stdin_pipe_buffer_times_out_and_poisons_the_handle_until_restart() {
        // Two `send_input` arms in one flow, sharing the ONE expensive wait
        // (`STDIN_WRITE_TIMEOUT`, 5s — the product's own bound, paid once):
        //
        //   1. `Err(BackendError::StdinTimedOut { .. })` =>
        //      `EngineError::InteractionTimedOut` — the mapping arm.
        //   2. The `self.backend.stdin_timed_out(..)` PRE-FLIGHT early return —
        //      a handle whose prior write timed out is permanently poisoned for
        //      the rest of this engine session (`StdinState::TimedOut` is never
        //      recoverable; only a stop+start builds a fresh pipe), so a SECOND
        //      send must fail fast with no new write attempted.
        //
        // The two arms are told apart WITHOUT any timing argument: the ordering
        // inside `send_input` puts `stdin_timed_out` BEFORE `has_stdin`, and a
        // `TimedOut` state is not `Live`, so if arm 2 were absent the second
        // send would land on the `has_stdin` check and return the materially
        // DIFFERENT `InteractionUnavailable`. Getting `InteractionTimedOut`
        // back is therefore positive proof that the pre-flight branch ran.
        //
        // DETERMINISM (Epic-2-retro AI-35/38 — never sleep to await state).
        // There is no sleep and no polling here. `fake_agent` without
        // `--echo-stdin`/`--sniff-stdin-at-startup` provably never drains its
        // stdin, and 8MB comfortably outruns every OS pipe buffer, so the write
        // blocks with certainty rather than by luck; the 5s is the engine's own
        // bounded `recv_timeout` elapsing, not a test guessing at a duration.
        // Every assertion is on a RETURNED value. This is exactly the
        // "deterministic stuck-agent harness ... a `fake_agent` flag that
        // provably never drains stdin plus a deliberately-filled pipe, no
        // sleeps" that the Epic 4 retrospective left open as AI-69.
        //
        // INDEPENDENT VALUE BEYOND COVERAGE (AI-69). The retro recorded that
        // exit code 6 / `InteractionTimedOut` has NO end-to-end assertion on any
        // OS — it was pinned only by the `kt` crate's mapper/classifier unit
        // test, composed with the separate end-to-end proofs of codes 0-4. This
        // closes the ENGINE half of that gap: the real `Supervisor`, the real
        // backend, a real non-draining child, and a real bounded write actually
        // producing `InteractionTimedOut` — deterministically and on all three
        // OSes. It does NOT close the CLI half (the `kt` process exiting 6),
        // which still rests on the mapper pin.
        //
        // Robustness: unlike the interaction.rs proofs, this one needs nothing
        // FROM the child — only that it keeps NOT reading — so no property of
        // the child's own output can make it fail.
        let (_state, _manifest, registry) = setup_fake("stuck", &["--linger-ms", "600000"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "stuck").unwrap();

        // Far past any realistic OS pipe buffer, so the write blocks once the
        // buffer fills — the adversarial audit's original reproduction vehicle.
        let huge_payload = "x".repeat(8 * 1024 * 1024);
        let err = sup
            .send_input(&registry, "stuck", &huge_payload)
            .expect_err("a write past the buffer of a never-draining pipe must time out");
        match err {
            EngineError::InteractionTimedOut { name, timeout_secs } => {
                assert_eq!(name, "stuck");
                // Pinned to the product constant the arm forwards, not a
                // literal restated here.
                assert_eq!(timeout_secs, crate::ports::STDIN_WRITE_TIMEOUT.as_secs());
            }
            other => panic!("expected InteractionTimedOut, got {other:?}"),
        }
        // No upper wall-clock bound is asserted on that call: "bounded, not
        // indefinite" is interaction.rs's property to prove (it also needs the
        // engine's shared lock and a second instance), and a tight-margin
        // timing assertion here would buy nothing but flake surface. What this
        // test owns is the ARM, and the arm is proven by the returned value.

        let start = Instant::now();
        let err = sup
            .send_input(&registry, "stuck", "second attempt")
            .expect_err("a poisoned handle must reject every later send");
        let elapsed = start.elapsed();
        match err {
            EngineError::InteractionTimedOut { name, timeout_secs } => {
                assert_eq!(name, "stuck");
                assert_eq!(timeout_secs, crate::ports::STDIN_WRITE_TIMEOUT.as_secs());
            }
            other => panic!("expected a fast-path InteractionTimedOut, got {other:?}"),
        }
        // The fast path does no I/O at all, so it returns in microseconds. The
        // bound is deliberately the FULL production timeout rather than a tight
        // one: with ~6 orders of magnitude of headroom it cannot flake, even
        // under coverage instrumentation, yet it still fails loudly on the one
        // regression it is here to catch — a second doomed bounded write.
        assert!(
            elapsed < crate::ports::STDIN_WRITE_TIMEOUT,
            "the poisoned-handle fast path must not wait out a second bounded write: {elapsed:?}"
        );

        // Teardown: killing the group closes the pipe's read end, so the
        // abandoned 8MB write thread (never joinable by design — see
        // `write_stdin_bounded`'s docs) unblocks with EPIPE and exits on its
        // own; this test does not wait for it.
        let _ = sup.stop(&registry, "stuck", Some(Duration::from_millis(200)));
    }

    // ---- Epic 11 / story 11-1 tests (AI-9, AI-12, AI-41) ----

    /// Write a `fake_agent` manifest declaring GUARANTEED pause for the CURRENT
    /// OS (the lib-test sibling of `tests/pause.rs`'s `write_pause_manifest`).
    fn write_pause_guaranteed_manifest(dir: &Path, kind: &str, args: &[&str]) {
        let bin = hemaka_conformance::fake_agent_bin();
        let args_toml = args
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let os = match OsId::current() {
            OsId::Linux => "linux",
            OsId::Macos => "macos",
            OsId::Windows => "windows",
            OsId::Other => "other",
        };
        let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{kind}\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [{args_toml}]\n\n\
             [capabilities.pause]\n{os} = \"guaranteed\"\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n",
            exec = bin.to_string_lossy(),
        );
        std::fs::write(dir.join("adapter.toml"), body).unwrap();
    }

    /// Count `heartbeat <n>` lines in an agent-output log (0 when absent).
    fn heartbeat_lines(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|c| c.lines().filter(|l| l.starts_with("heartbeat ")).count())
            .unwrap_or(0)
    }

    #[test]
    fn poll_verdict_tolerates_transient_errors_and_trips_at_the_threshold() {
        // AI-12 pure decision fn: a clean `Alive` resets the streak; an error
        // below MAX_CONSECUTIVE_POLL_ERRORS stays transient (the streak
        // increments — the historical tolerate-and-retry for a short error
        // burst); the error AT the threshold turns into crash input; a real
        // observed exit passes its code through (streak reset — the handle
        // leaves the map anyway).
        let poll_err = || BackendError::Control {
            op: "poll",
            detail: "boom".to_string(),
        };
        // Below the threshold: transient, streak grows one per error.
        for streak in 0..MAX_CONSECUTIVE_POLL_ERRORS - 1 {
            assert_eq!(
                poll_verdict(streak, Err(poll_err())),
                (PollVerdict::TransientError, streak + 1),
                "error {}/{MAX_CONSECUTIVE_POLL_ERRORS} must stay transient",
                streak + 1
            );
        }
        // AT the threshold: the streak stops being tolerated — crash input.
        assert_eq!(
            poll_verdict(MAX_CONSECUTIVE_POLL_ERRORS - 1, Err(poll_err())),
            (PollVerdict::PersistentError, MAX_CONSECUTIVE_POLL_ERRORS)
        );
        // Beyond (defensive; saturating): still crash input.
        assert_eq!(
            poll_verdict(MAX_CONSECUTIVE_POLL_ERRORS, Err(poll_err())),
            (
                PollVerdict::PersistentError,
                MAX_CONSECUTIVE_POLL_ERRORS + 1
            )
        );
        // Clean reads reset the streak to 0.
        assert_eq!(
            poll_verdict(MAX_CONSECUTIVE_POLL_ERRORS - 1, Ok(ProcessStatus::Alive)),
            (PollVerdict::Alive, 0)
        );
        assert_eq!(
            poll_verdict(9, Ok(ProcessStatus::Exited { code: Some(3) })),
            (PollVerdict::Exited(Some(3)), 0)
        );
        assert_eq!(
            poll_verdict(0, Ok(ProcessStatus::Exited { code: None })),
            (PollVerdict::Exited(None), 0)
        );
    }

    #[test]
    fn poll_error_streaks_clear_when_a_handle_is_removed() {
        // AI-12 streak hygiene: a clean poll leaves no streak entry (Ok(Alive)
        // clears), and once a REAL crash is reaped the removed handle's entry is
        // gone — no stale streak can pre-load the instance's next Run.
        let (_state, _manifest, registry) = setup_fake("streak", &["--crash-after-ms", "450"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "streak").unwrap();
        // Clean passes: no entry may survive.
        sup.poll_once(&registry);
        assert!(
            sup.poll_error_streaks.is_empty(),
            "clean polls leave no streak entry: {:?}",
            sup.poll_error_streaks
        );
        // The crash is reaped; the removed handle leaves no entry behind.
        let _ = wait_for_crash(&mut sup, &registry);
        assert!(
            sup.poll_error_streaks.is_empty(),
            "a removed handle must leave no streak entry: {:?}",
            sup.poll_error_streaks
        );
    }

    /// An in-memory `Write` sink capturing engine diagnostics (the story-10-2
    /// `DiagnosticSink`), so tests can assert a diagnostic reaches the sink.
    #[derive(Clone)]
    struct SharedCapture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Install a capturing diagnostic sink on `sup`; returns the shared buffer.
    fn install_capture_sink(sup: &mut Supervisor) -> Arc<Mutex<Vec<u8>>> {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        sup.install_diagnostics(Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(
            &buffer,
        ))))));
        buffer
    }

    /// The sink's captured text so far.
    fn sink_text(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    /// Register TWO `fake_agent`-backed instances in ONE state dir (the
    /// cross-handle corroboration tests need multiple live handles in one
    /// supervisor). Returns the (state dir, manifest dir, registry).
    fn setup_two_fakes(
        names: [&str; 2],
        args: &[&str],
    ) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
        let state = tempfile::tempdir().unwrap();
        let manifest = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        for name in names {
            let dir = manifest.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            write_fake_manifest(&dir, name, args);
            registry
                .register_with_adapter(name, &AdapterRef::Manifest(dir))
                .unwrap();
        }
        (state, manifest, registry)
    }

    #[test]
    fn truncate_for_cause_preserves_short_text_and_bounds_long_text() {
        // AI-12 (loop 1) helper: short text passes through untouched; long text
        // is cut to the bound plus the ellipsis.
        assert_eq!(
            truncate_for_cause("boom", POLL_ERROR_CAUSE_MAX_CHARS),
            "boom"
        );
        let long = "x".repeat(500);
        let cut = truncate_for_cause(&long, 200);
        assert_eq!(cut.chars().count(), 201, "200 chars plus the ellipsis");
        assert!(cut.ends_with('…'), "the cut is marked: {cut}");
    }

    #[test]
    fn poll_once_handle_specific_poll_failure_wiring_lands_failed_and_removes_the_handle() {
        // AI-12 wiring (loop 1, amendment c) — the HANDLE-SPECIFIC direction,
        // through the cfg(test) backend fault seam: ONE handle's persistent
        // poll failure drives the REAL `poll_once` crash path end to end — the
        // instance lands `failed` with the persistent-poll-failure cause
        // carrying the LAST error's text, the handle is removed (its Drop
        // kills the un-pollable group), and no stale streak/error bookkeeping
        // survives. A regression to the old silent `Err(_) => None` leaves the
        // instance `running` forever and fails this test.
        let (_state, _manifest, registry) = setup_fake("wired", &["--linger-ms", "600000"]);
        registry
            .set_restart_policy(&InstanceName::new("wired").unwrap(), RestartPolicy::Never)
            .unwrap();
        let name = InstanceName::new("wired").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "wired").unwrap();

        // Arm the seam: every poll of THIS handle now errors.
        let pid = sup.backend.pid(&sup.running.get(&name).unwrap().handle);
        sup.arm_poll_fault(pid);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let plans = sup.poll_once(&registry);
            if state_of(&registry, "wired") == LifecycleState::Failed {
                assert!(
                    plans.is_empty(),
                    "a `never` policy must not schedule a restart on the poll-failure crash"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the persistent poll failure never landed the instance failed"
            );
        }
        assert!(
            sup.running.is_empty(),
            "the un-pollable handle must be removed from the supervisor"
        );
        assert!(
            sup.poll_error_streaks.is_empty() && sup.poll_last_errors.is_empty(),
            "the removed handle leaves no streak or last-error entry behind"
        );
        // The crash cause carries the full story: the persistent failure AND
        // the last error's text.
        let events = Supervisor::read_events(&registry, "wired").unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.new_state, LifecycleState::Failed);
        let cause = serde_json::to_string(&last.cause).unwrap();
        assert!(cause.contains("persistent poll failure"), "cause={cause}");
        assert!(cause.contains("last error:"), "cause={cause}");
        assert!(
            cause.contains("injected cfg(test) poll fault"),
            "the last poll error's text must reach the cause (AI-12b): {cause}"
        );
    }

    #[test]
    fn poll_once_multi_handle_same_tick_failure_is_environmental_and_keeps_handles_alive() {
        // AI-12 wiring (loop 1, amendment a) — the ENVIRONMENTAL direction: two
        // handles erroring in the SAME tick corroborate as a backend/
        // environment-wide condition (the procfs/sysctl-outage shape). Past the
        // crash threshold many times over: NO streak may grow, NO handle may be
        // crashed (kill-on-drop would kill RUNNING agents), and the
        // environmental diagnostic must reach the diagnostic sink.
        let (_state, _manifest, registry) =
            setup_two_fakes(["enva", "envb"], &["--linger-ms", "600000"]);
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "enva").unwrap();
        sup.start(&registry, "envb").unwrap();
        let sink = install_capture_sink(&mut sup);

        let pid_a = sup.backend.pid(
            &sup.running
                .get(&InstanceName::new("enva").unwrap())
                .unwrap()
                .handle,
        );
        let pid_b = sup.backend.pid(
            &sup.running
                .get(&InstanceName::new("envb").unwrap())
                .unwrap()
                .handle,
        );
        sup.arm_poll_fault(pid_a);
        sup.arm_poll_fault(pid_b);

        for _ in 0..(MAX_CONSECUTIVE_POLL_ERRORS + 2) {
            sup.poll_once(&registry);
        }

        assert_eq!(
            sup.running.len(),
            2,
            "an environmental failure must keep every handle alive"
        );
        assert_eq!(state_of(&registry, "enva"), LifecycleState::Running);
        assert_eq!(state_of(&registry, "envb"), LifecycleState::Running);
        assert!(
            sup.poll_error_streaks.is_empty(),
            "no streak may grow on an environmental tick: {:?}",
            sup.poll_error_streaks
        );
        let text = sink_text(&sink);
        assert!(
            text.contains("environmental poll failure"),
            "the environmental diagnostic must reach the sink: {text}"
        );
        assert!(
            text.contains("handles stay alive"),
            "the diagnostic must say what the guard did: {text}"
        );
    }

    #[test]
    fn pause_and_resume_transition_failures_fail_before_the_signal_persist_first_ai9() {
        // AI-9 (order mirrors `stop_inner`), the loop-1 extended failure-
        // injection proof (mirrors
        // `snapshot_write_failure_rejects_the_start_before_the_starting_transition`),
        // three legs + the guaranteed RESUME leg, all on one live instance:
        //
        // * Leg A (NOTHING commits): make the LOGS DIRECTORY un-creatable (a
        //   file stands where `logs/` must be) so the pause fails at
        //   `ensure_log_dir` — BEFORE any persist. The ledger must still read
        //   `running` (the transition truly did not commit) and no signal may
        //   fire.
        // * Leg B (persist commits, append fails): replace instance.log with a
        //   DIRECTORY so the transition's event append fails. The durable state
        //   LEADS (persist-first): the ledger reads `paused`, and the process
        //   keeps running — no signal was sent.
        // * Resume leg (persist-first for SIGCONT): with the process genuinely
        //   SUSPENDED (a real pause in between) and the append sabotaged again,
        //   resume fails at the append AFTER committing `paused → running` —
        //   and the heartbeat must stay FROZEN: no SIGCONT was delivered. The
        //   pre-AI-9 signal-first order would have woken the agent behind an
        //   errored resume.
        //
        // Windows runtime skips (data-driven `OsId`, NO #[cfg] — the
        // tests/pause.rs idiom): Leg A's directory-rename injection is a
        // sharing violation while the child holds agent.log open, and the two
        // frozen-heartbeat probes observe a REAL suspension that Windows
        // honestly never performs (AD-4 cooperative pause). What Windows still
        // proves cross-platform: Leg B + the resume leg — the persist-first
        // LEDGER semantics (the committed transition visible despite the
        // errored append, the typed Log error) run on every OS.
        let (_state, _manifest, registry) =
            setup_pause_guaranteed("pz", &["--heartbeat-ms", "50", "--linger-ms", "600000"]);
        let name = InstanceName::new("pz").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "pz").unwrap();
        assert_eq!(state_of(&registry, "pz"), LifecycleState::Running);
        let agent_log = registry.agent_output_log_path(&name);
        let log_path = registry.instance_log_path(&name);
        let wait_heartbeat = |at_least: usize| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if heartbeat_lines(&agent_log) >= at_least {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "heartbeat never reached {at_least}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait_heartbeat(2);

        // ---- Leg A: the pause fails BEFORE any persist. ----
        // Runtime-skipped on Windows (data-driven, NO #[cfg] — the
        // tests/pause.rs guaranteed-suspend idiom): the injection RENAMES the
        // live log directory, which Windows refuses while the child holds
        // agent.log / agent-stderr.log open inside it (a sharing violation —
        // os error 5, observed on the first windows-latest CI run). What
        // Windows misses is only this pre-persist INJECTION, not the
        // persist-first contract: legs B + resume below run on every OS.
        if OsId::current() != OsId::Windows {
            let log_dir = registry.instance_log_dir(&name);
            let held_dir = log_dir.with_extension("ai9-held");
            std::fs::rename(&log_dir, &held_dir).unwrap();
            std::fs::write(&log_dir, b"not a directory").unwrap();
            let a_err = sup.pause(&registry, "pz").unwrap_err();
            assert!(
                matches!(&a_err, EngineError::Log { .. }),
                "the ensure_log_dir failure must surface as a typed Log error, got {a_err:?}"
            );
            assert_eq!(
                state_of(&registry, "pz"),
                LifecycleState::Running,
                "with the whole transition rejected, the ledger must still read running"
            );
            std::fs::remove_file(&log_dir).unwrap();
            std::fs::rename(&held_dir, &log_dir).unwrap();
            let a_before = heartbeat_lines(&agent_log);
            std::thread::sleep(Duration::from_millis(400));
            let a_after = heartbeat_lines(&agent_log);
            assert!(
                a_after > a_before,
                "a pause rejected before any persist must NOT have signalled SIGSTOP: heartbeat {a_before} -> {a_after}"
            );
        }

        // ---- Leg B: the persist commits, the event append fails. ----
        let b_before = heartbeat_lines(&agent_log);
        std::fs::remove_file(&log_path).unwrap();
        std::fs::create_dir(&log_path).unwrap();
        let b_err = sup.pause(&registry, "pz").unwrap_err();
        assert!(
            matches!(&b_err, EngineError::Log { .. }),
            "the append failure must surface as a typed Log error, got {b_err:?}"
        );
        // Persist-first: the durable state LEADS — the ledger reads `paused`
        // even though the append (the event record) failed.
        assert_eq!(
            state_of(&registry, "pz"),
            LifecycleState::Paused,
            "persist-first: the committed transition must be visible in the ledger"
        );
        std::thread::sleep(Duration::from_millis(400));
        let b_after = heartbeat_lines(&agent_log);
        assert!(
            b_after > b_before,
            "a pause whose persist committed but errored must NOT have signalled SIGSTOP: heartbeat {b_before} -> {b_after}"
        );
        std::fs::remove_dir(&log_path).unwrap();

        // Realign: Leg B's persist committed `paused`, so a real resume (the
        // remediation the AI-9 diagnostic names) brings the ledger back to
        // `running` — the SIGCONT is a harmless no-op on the still-running
        // process.
        sup.resume(&registry, "pz").unwrap();
        assert_eq!(state_of(&registry, "pz"), LifecycleState::Running);

        // ---- The genuine suspension (so the resume leg proves SIGCONT). ----
        sup.pause(&registry, "pz").unwrap();
        assert_eq!(state_of(&registry, "pz"), LifecycleState::Paused);
        std::thread::sleep(Duration::from_millis(200));
        let frozen_before = heartbeat_lines(&agent_log);
        std::thread::sleep(Duration::from_millis(300));
        // Windows: pause there is an honest cooperative NO-OP (AD-4 — no
        // guaranteed whole-process suspend exists from std), so there is no
        // real suspension for a frozen-heartbeat probe to observe; the probe
        // is the Unix guarantee (same data-driven skip as tests/pause.rs).
        if OsId::current() != OsId::Windows {
            assert_eq!(
                heartbeat_lines(&agent_log),
                frozen_before,
                "the probe: a successful guaranteed pause really suspends (heartbeat frozen)"
            );
        }

        // ---- Resume leg: persist commits, append fails, NO SIGCONT. ----
        std::fs::remove_file(&log_path).unwrap();
        std::fs::create_dir(&log_path).unwrap();
        let r_err = sup.resume(&registry, "pz").unwrap_err();
        assert!(
            matches!(&r_err, EngineError::Log { .. }),
            "the resume append failure must surface as a typed Log error, got {r_err:?}"
        );
        assert_eq!(
            state_of(&registry, "pz"),
            LifecycleState::Running,
            "persist-first resume: the committed paused-to-running transition must be visible"
        );
        std::thread::sleep(Duration::from_millis(300));
        // The frozen-heartbeat half is Unix-only (Windows has no real
        // suspension to stay in — see the AD-4 note above); the LEDGER half
        // (persist committed `paused → running`, the Log error surfaced)
        // above is the cross-platform claim.
        if OsId::current() != OsId::Windows {
            assert_eq!(
                heartbeat_lines(&agent_log),
                frozen_before,
                "a resume whose persist committed but errored must NOT have signalled SIGCONT: the process stays suspended"
            );
        }
        std::fs::remove_dir(&log_path).unwrap();

        // Teardown: the process is SIGSTOPped (SIGTERM would only pend), so the
        // stop's forced escalation is the honest way down.
        sup.stop(&registry, "pz", Some(Duration::from_millis(600)))
            .unwrap();
    }

    /// Register a `fake_agent`-backed instance whose manifest declares GUARANTEED
    /// pause for the current OS. Returns the (state dir, manifest dir, registry).
    fn setup_pause_guaranteed(
        name: &str,
        args: &[&str],
    ) -> (tempfile::TempDir, tempfile::TempDir, Registry) {
        let state = tempfile::tempdir().unwrap();
        let manifest = tempfile::tempdir().unwrap();
        write_pause_guaranteed_manifest(manifest.path(), name, args);
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        registry
            .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
            .unwrap();
        (state, manifest, registry)
    }

    /// Append usage sentinel lines (`(sequence, input, output)` triples) to the
    /// instance's agent-output log — the drain's input.
    fn append_usage_lines(path: &Path, lines: &[(u64, u64, u64)]) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        for (seq, input, output) in lines {
            f.write_all(
                format!(
                    "KTESIO_USAGE {{\"sequence\":{seq},\"input_tokens\":{input},\"output_tokens\":{output}}}\n"
                )
                .as_bytes(),
            )
            .unwrap();
        }
    }

    /// The (row count, summed input tokens) of `name`'s committed ledger rows,
    /// via a direct connection to the state DB.
    fn ledger_totals(state: &Path, name: &str) -> (i64, i64) {
        let conn = rusqlite::Connection::open(state.join("state.db")).unwrap();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0) FROM usage_events e \
             JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn a_failed_ledger_insert_parks_the_cursor_and_retries_exactly_once() {
        // AI-41 (billing honesty), loop-1 repaired + extended: a usage event
        // whose INSERT fails must NOT have the drain cursor advanced past it
        // (the pre-fix code advanced FIRST, silently dropping the event). Four
        // proofs in one flow:
        //   (1) a whole-table fault parks the cursor and the failure diagnostic
        //       reaches the story-10-2 sink;
        //   (2) the repair restores the FULL schema — the table AND every index
        //       including the UNIQUE(instance_id, run_id, sequence) dedup index
        //       (the loop-0 restore lost the dedup invariant);
        //   (3) the retry commits each event EXACTLY once;
        //   (4) a PARTIAL failure (a trigger fails only the SECOND insert of a
        //       block) parks the cursor, and the re-drift proves the dedup key:
        //       the already-committed first event comes back as `DuplicateReplay`
        //       (no double-count) while the failed second commits.
        let (state, _manifest, registry) = setup_fake("ledger", &["--linger-ms", "600000"]);
        let name = InstanceName::new("ledger").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "ledger").unwrap();
        let sink = install_capture_sink(&mut sup);
        let log = registry.agent_output_log_path(&name);
        let db = state.path().join("state.db");

        // (1) Whole-table fault: the drain parks, the diagnostic is audible.
        append_usage_lines(&log, &[(0, 10, 20), (1, 11, 22)]);
        let cursor_before = sup.running.get(&name).unwrap().usage_cursor;
        let conn = rusqlite::Connection::open(&db).unwrap();
        // The FULL schema of `usage_events` — the table AND every index (the
        // dedup index is its own sqlite_master row), tables first.
        let mut schema: Vec<(String, String)> = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT type, sql FROM sqlite_master \
                 WHERE tbl_name = 'usage_events' AND sql IS NOT NULL \
                 ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for row in rows {
            schema.push(row.unwrap());
        }
        drop(stmt);
        assert!(
            schema
                .iter()
                .any(|(t, sql)| t == "index" && sql.contains("CREATE UNIQUE INDEX")),
            "the fixture premise: a UNIQUE dedup index exists on usage_events"
        );
        conn.execute("DROP TABLE usage_events", []).unwrap();
        drop(conn);

        sup.drain_usage_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(
            sup.running.get(&name).unwrap().usage_cursor,
            cursor_before,
            "a failed INSERT must park the drain cursor (AI-41: no silent drop)"
        );
        assert!(
            sink_text(&sink).contains("could not be committed to the Usage Ledger"),
            "the ingest-failure diagnostic must reach the diagnostic sink"
        );

        // (2) Restore the FULL schema — table first, then every index.
        let conn = rusqlite::Connection::open(&db).unwrap();
        for (_kind, sql) in &schema {
            conn.execute(sql, []).unwrap();
        }
        let unique_indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' \
                 AND tbl_name = 'usage_events' AND sql LIKE 'CREATE UNIQUE INDEX%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert!(
            unique_indexes >= 1,
            "the repair must restore the UNIQUE dedup index, not only the table"
        );

        // (3) The retry commits both events, each exactly once.
        sup.drain_usage_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(
            ledger_totals(state.path(), "ledger"),
            (2, 21),
            "the retried events must commit exactly once (10 + 11 input tokens)"
        );
        let log_len = std::fs::metadata(&log).unwrap().len();
        assert_eq!(
            sup.running.get(&name).unwrap().usage_cursor,
            log_len,
            "the cursor advances past the block only once every event committed"
        );

        // (4) PARTIAL failure: the FIRST insert of a block commits, the SECOND
        // fails (a trigger RAISEs once the table already holds 3 rows). The
        // cursor parks; after the fault is repaired, the re-drift must classify
        // the committed event as a DuplicateReplay (the UNIQUE dedup key — no
        // double-count) while the failed one finally commits.
        append_usage_lines(&log, &[(2, 1, 2), (3, 3, 4)]);
        let parked_at = sup.running.get(&name).unwrap().usage_cursor;
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TRIGGER ai41_fail_second_insert BEFORE INSERT ON usage_events \
             WHEN (SELECT COUNT(*) FROM usage_events) >= 3 \
             BEGIN SELECT RAISE(ABORT, 'injected second-insert fault'); END;",
            [],
        )
        .unwrap();
        drop(conn);
        sup.drain_usage_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(
            ledger_totals(state.path(), "ledger"),
            (3, 22),
            "the first event of the block commits (10+11+1), the second fails"
        );
        assert_eq!(
            sup.running.get(&name).unwrap().usage_cursor,
            parked_at,
            "the partial failure must park the cursor before the block (re-drift both)"
        );
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("DROP TRIGGER ai41_fail_second_insert", [])
            .unwrap();
        drop(conn);
        sup.drain_usage_for(&registry, &name, DrainMode::MidRun);
        // If the dedup index were missing, the re-drifted sequence-2 line would
        // insert AGAIN (4 rows / 23 input) — (4, 25) proves DuplicateReplay.
        assert_eq!(
            ledger_totals(state.path(), "ledger"),
            (4, 25),
            "the re-drifted committed event must be a DuplicateReplay (no double-count); \
             the failed event commits on retry"
        );
        assert_eq!(
            sup.running.get(&name).unwrap().usage_cursor,
            std::fs::metadata(&log).unwrap().len(),
            "the cursor advances only once every event is durable"
        );
        // Teardown.
        sup.stop(&registry, "ledger", Some(Duration::from_millis(200)))
            .unwrap();
    }

    // ---- Story 12-4: the OBSERVED drain's park + bounded retry + loud skip ----

    /// Give the supervisor's (already-started, self-reported) instance a REAL
    /// observed listener + source — the 12-4 drain tests push `(input, output)`
    /// pairs into the listener's queue directly (no model traffic needed) and
    /// drive `drain_observed_for` synchronously. The listener points at a dead
    /// upstream (nothing ever forwards); its accept-loop task lives on the
    /// returned runtime (kept alive by the caller for the test's duration).
    fn attach_observed_channel(
        sup: &mut Supervisor,
        registry: &Registry,
        name: &str,
    ) -> (
        tokio::runtime::Runtime,
        crate::metering::listener::ObservedQueue,
    ) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime for the observed listener");
        let dead_upstream = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let listener = ObservedListener::start(rt.handle(), dead_upstream).expect("listener start");
        let queue = listener.queue();
        let inst = InstanceName::new(name).unwrap();
        let s = sup.running.get_mut(&inst).expect("supervised entry");
        s.observed_listener = Some(listener);
        s.observed_source = Some(crate::ports::ObservedUsageSource::new());
        s.metering_source = "engine-observed".to_string();
        // Silence the unused-registry lint; the registry read keeps the signature
        // symmetric with the drain call sites.
        let _ = registry;
        (rt, queue)
    }

    /// The FULL `usage_events` schema (table + every index, tables first) read
    /// from the live DB — the repair fixture the fault-injection drops and
    /// restores (the AI-41 park test's exact technique, shared).
    fn usage_events_schema(db: &Path) -> Vec<(String, String)> {
        let conn = rusqlite::Connection::open(db).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT type, sql FROM sqlite_master \
                 WHERE tbl_name = 'usage_events' AND sql IS NOT NULL \
                 ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        let schema: Vec<(String, String)> = rows.map(|r| r.unwrap()).collect();
        drop(stmt);
        schema
    }

    /// The minted `sequence` ordinals of `name`'s committed ledger rows —
    /// proves the retried events kept their ORIGINAL sequences (never re-minted).
    fn ledger_sequences(state: &Path, name: &str) -> Vec<i64> {
        let conn = rusqlite::Connection::open(state.join("state.db")).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT e.sequence FROM usage_events e \
                 JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1 \
                 ORDER BY e.sequence",
            )
            .unwrap();
        let rows = stmt.query_map([name], |r| r.get::<_, i64>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn an_observed_drain_parks_and_retries_the_same_minted_events() {
        // Story 12-4 (the observed AI-41): a store failure MIDRUN parks the
        // minted events; the retry commits the EXACT same events — the same
        // minted `sequence` ordinals (never re-minted, the dedup-key stability
        // the design pins) — and the park clears once everything is durable.
        let (state, _manifest, registry) = setup_fake("obspark", &["--linger-ms", "600000"]);
        let name = InstanceName::new("obspark").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "obspark").unwrap();
        let sink = install_capture_sink(&mut sup);
        let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obspark");
        let db = state.path().join("state.db");

        // Two observed completions; then the store dies.
        queue.lock().unwrap().push_back((10, 20));
        queue.lock().unwrap().push_back((11, 22));
        let schema = usage_events_schema(&db);
        assert!(
            schema
                .iter()
                .any(|(t, sql)| t == "index" && sql.contains("CREATE UNIQUE INDEX")),
            "fixture premise: a UNIQUE dedup index exists on usage_events"
        );
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("DROP TABLE usage_events", []).unwrap();
        drop(conn);

        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        {
            let s = sup.running.get(&name).unwrap();
            let (pending, attempts) = s.observed_park.as_ref().expect("the drain must park");
            assert_eq!(*attempts, 1, "first failure: attempt streak 1");
            assert_eq!(
                pending.front_sequence, 0,
                "the front event's minted sequence"
            );
            assert_eq!(
                pending.events.len(),
                2,
                "both minted events park (the queue has no byte cursor)"
            );
        }
        assert!(
            sink_text(&sink).contains("parked for retry"),
            "the failure diagnostic must reach the diagnostic sink"
        );

        // Repair (FULL schema incl. the dedup index) and retry.
        let conn = rusqlite::Connection::open(&db).unwrap();
        for (_kind, sql) in &schema {
            conn.execute(sql, []).unwrap();
        }
        drop(conn);
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(
            ledger_totals(state.path(), "obspark"),
            (2, 21),
            "the retried events commit exactly once"
        );
        assert_eq!(
            ledger_sequences(state.path(), "obspark"),
            vec![0, 1],
            "the retry kept the ORIGINAL minted sequences (never re-minted)"
        );
        let s = sup.running.get(&name).unwrap();
        assert!(
            s.observed_park.is_none(),
            "the park clears once every event is durable"
        );
        sup.stop(&registry, "obspark", Some(Duration::from_millis(200)))
            .unwrap();
    }

    #[test]
    fn an_observed_partial_failure_parks_only_the_uncommitted_tail() {
        // Break-on-first-error: the FIRST event of a pass commits, the SECOND
        // fails (a trigger RAISEs from the second row on) — only the FAILED
        // event parks (discrete events need no block re-drift), and after the
        // repair the retry adds exactly the missing one (no double-count).
        let (state, _manifest, registry) = setup_fake("obspart", &["--linger-ms", "600000"]);
        let name = InstanceName::new("obspart").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "obspart").unwrap();
        let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obspart");
        let db = state.path().join("state.db");

        queue.lock().unwrap().push_back((10, 20));
        queue.lock().unwrap().push_back((11, 22));
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TRIGGER obs_fail_second BEFORE INSERT ON usage_events \
             WHEN (SELECT COUNT(*) FROM usage_events) >= 1 \
             BEGIN SELECT RAISE(ABORT, 'injected second-insert fault'); END;",
            [],
        )
        .unwrap();
        drop(conn);

        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(
            ledger_totals(state.path(), "obspart"),
            (1, 10),
            "the first event commits, the second fails"
        );
        {
            let s = sup.running.get(&name).unwrap();
            let (pending, attempts) = s.observed_park.as_ref().expect("the failure parks");
            assert_eq!(
                pending.events.len(),
                1,
                "only the UNCOMMITTED event parks — the committed one is never retried"
            );
            assert_eq!(pending.front_sequence, 1, "the front is the second event");
            assert_eq!(*attempts, 1);
        }

        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("DROP TRIGGER obs_fail_second", []).unwrap();
        drop(conn);
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(
            ledger_totals(state.path(), "obspart"),
            (2, 21),
            "the retry adds exactly the missing event (no double-count)"
        );
        let s = sup.running.get(&name).unwrap();
        assert!(s.observed_park.is_none(), "the park clears");
        sup.stop(&registry, "obspart", Some(Duration::from_millis(200)))
            .unwrap();
    }

    #[test]
    fn an_observed_poisoned_event_is_skipped_loudly_after_the_bound() {
        // The bounded park: an event the store PERMANENTLY rejects (a trigger
        // keyed on its token count) parks 1→2→3 times, then is SKIPPED with a
        // loud diagnostic while the events behind it keep counting — an
        // announced loss, never a wedged drain, never a silent drop.
        let (state, _manifest, registry) = setup_fake("obsskip", &["--linger-ms", "600000"]);
        let name = InstanceName::new("obsskip").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "obsskip").unwrap();
        let sink = install_capture_sink(&mut sup);
        let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obsskip");
        let db = state.path().join("state.db");

        queue.lock().unwrap().push_back((42, 1)); // the poisoned event
        queue.lock().unwrap().push_back((7, 2));
        queue.lock().unwrap().push_back((8, 3));
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TRIGGER obs_poison BEFORE INSERT ON usage_events \
             WHEN NEW.input_tokens = 42 \
             BEGIN SELECT RAISE(ABORT, 'injected permanent fault'); END;",
            [],
        )
        .unwrap();
        drop(conn);

        // Passes 1 and 2: the drain parks at the poisoned front.
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        {
            let s = sup.running.get(&name).unwrap();
            let (pending, attempts) = s.observed_park.as_ref().unwrap();
            assert_eq!(
                *attempts, 2,
                "two consecutive failed passes at the same front"
            );
            assert_eq!(pending.front_sequence, 0);
            assert_eq!(
                pending.events.len(),
                3,
                "nothing committed, nothing skipped yet"
            );
        }
        // Pass 3 hits the bound: the poisoned front is SKIPPED loudly; the two
        // healthy events park behind it with a FRESH streak (a new front).
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        assert!(
            sink_text(&sink).contains("SKIPPED (not counted)"),
            "the skip must be announced loudly"
        );
        assert!(
            sink_text(&sink).contains("sequence 0"),
            "the skip names the poisoned event's minted sequence"
        );
        {
            let s = sup.running.get(&name).unwrap();
            let (pending, attempts) = s.observed_park.as_ref().unwrap();
            assert_eq!(
                pending.front_sequence, 1,
                "the new front is the second event"
            );
            assert_eq!(pending.events.len(), 2, "the healthy events keep counting");
            assert_eq!(*attempts, 1, "a different front resets the streak");
        }
        assert_eq!(
            ledger_totals(state.path(), "obsskip"),
            (0, 0),
            "nothing committed yet (the healthy events are parked, not dropped)"
        );

        // Remove the fault: the healthy events commit on the next pass; the
        // skipped one is NOT resurrected (an announced loss stays lost).
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("DROP TRIGGER obs_poison", []).unwrap();
        drop(conn);
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(
            ledger_totals(state.path(), "obsskip"),
            (2, 15),
            "the healthy events landed; the skipped event's 42 tokens are gone (announced)"
        );
        let s = sup.running.get(&name).unwrap();
        assert!(s.observed_park.is_none());
        sup.stop(&registry, "obsskip", Some(Duration::from_millis(200)))
            .unwrap();
    }

    #[test]
    fn an_oversized_observed_park_drops_the_oldest_events_loudly() {
        // The 12-4 AMENDMENT cap: a store outage keeps minting events every
        // tick, so the pending park buffer must be BOUNDED
        // (OBSERVED_PARK_MAX_EVENTS) — never unbounded engine memory. When the
        // cap bites, the OLDEST parked events are dropped and the loss is
        // announced LOUDLY (count + sequence range + remediation) — a capped
        // loss, never a silent truncation; the newest events keep their park.
        let (state, _manifest, registry) = setup_fake("obscap", &["--linger-ms", "600000"]);
        let name = InstanceName::new("obscap").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "obscap").unwrap();
        let sink = install_capture_sink(&mut sup);
        let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obscap");
        let db = state.path().join("state.db");

        // MORE events than the cap, all minted in one drain while the store
        // is dead: the buffer can hold only OBSERVED_PARK_MAX_EVENTS of them.
        let total = OBSERVED_PARK_MAX_EVENTS + 2;
        for i in 0..total {
            queue.lock().unwrap().push_back((1, i as u64));
        }
        let schema = usage_events_schema(&db);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("DROP TABLE usage_events", []).unwrap();
        drop(conn);
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);

        // The park holds EXACTLY the cap — the two OLDEST events (sequences
        // 0 and 1) were dropped, loudly.
        {
            let s = sup.running.get(&name).unwrap();
            let (pending, attempts) = s.observed_park.as_ref().expect("the drain still parks");
            assert_eq!(
                pending.events.len(),
                OBSERVED_PARK_MAX_EVENTS,
                "the park buffer is capped, never unbounded"
            );
            assert_eq!(
                pending.front_sequence, 2,
                "the two oldest minted events (0, 1) were the dropped ones"
            );
            assert_eq!(*attempts, 1, "a first failed pass at the (new) front");
        }
        let text = sink_text(&sink);
        assert!(
            text.contains("2 parked observed usage event(s)"),
            "the overflow must be announced with the dropped COUNT: {text}"
        );
        assert!(
            text.contains("sequences 0..1"),
            "the overflow diagnostic names the dropped sequence range: {text}"
        );
        assert!(
            text.contains("NOT counted"),
            "the dropped events are honestly not-counted, never silently held: {text}"
        );

        // Repair: the capped survivors commit with their ORIGINAL minted
        // sequences (the cap never re-mints), the dropped two stay lost.
        let conn = rusqlite::Connection::open(&db).unwrap();
        for (_kind, sql) in &schema {
            conn.execute(sql, []).unwrap();
        }
        drop(conn);
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(
            ledger_sequences(state.path(), "obscap").len(),
            OBSERVED_PARK_MAX_EVENTS,
            "exactly the capped survivors landed"
        );
        assert_eq!(
            ledger_sequences(state.path(), "obscap")[0],
            2,
            "the survivors kept their original sequences (the dropped two are gone)"
        );
        let s = sup.running.get(&name).unwrap();
        assert!(s.observed_park.is_none());
        sup.stop(&registry, "obscap", Some(Duration::from_millis(200)))
            .unwrap();
    }

    #[test]
    fn a_terminal_observed_drain_failure_announces_the_loss_without_a_retry_claim() {
        // The TERMINAL arm: a commit failure on the stop/reap drain is LOST —
        // announced explicitly (with the why and no retry claim), never a silent
        // drop and never a false "will retry". Any buffer a PRIOR MidRun pass
        // parked is dropped with the instance (the terminal pass makes no new
        // park and rescues nothing once the drain itself failed).
        let (state, _manifest, registry) = setup_fake("obslost", &["--linger-ms", "600000"]);
        let name = InstanceName::new("obslost").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "obslost").unwrap();
        let sink = install_capture_sink(&mut sup);
        let (_rt, queue) = attach_observed_channel(&mut sup, &registry, "obslost");
        let db = state.path().join("state.db");

        // (1) Store UP: event A commits immediately.
        queue.lock().unwrap().push_back((30, 40));
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(ledger_totals(state.path(), "obslost"), (1, 30));

        // (2) Store DIES: event B parks on the MidRun pass.
        queue.lock().unwrap().push_back((3, 4));
        let schema = usage_events_schema(&db);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("DROP TABLE usage_events", []).unwrap();
        drop(conn);
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        assert!(sup.running.get(&name).unwrap().observed_park.is_some());

        // (3) The TERMINAL drain (store still dead): B's failure is announced
        // as LOST — not retried, not a bounded skip — and B does NOT survive
        // into a new park. The 12-4 AMENDMENT: NO diagnostic on this path may
        // claim "parked for retry" — the shared ingest-failure text is
        // mode-aware, and the absence is asserted, not assumed (a terminal
        // diagnostic promising a retry would contradict the LOST notice). The
        // assertion scopes to the TERMINAL pass's own lines: the earlier
        // MidRun pass's "parked for retry" line is honest history (that drain
        // really did park), so only the delta may not contain the claim.
        let before_terminal = sink_text(&sink).len();
        sup.drain_observed_for(&registry, &name, DrainMode::Terminal);
        let text = sink_text(&sink)[before_terminal..].to_string();
        assert!(
            text.contains("are LOST"),
            "the terminal loss must be announced: {text}"
        );
        assert!(
            text.contains("cannot be retried"),
            "the loss notice must not claim a retry: {text}"
        );
        assert!(
            !text.contains("parked for retry"),
            "NO terminal diagnostic may claim the parked event will be retried \
             (the 12-4 AMENDMENT mode-aware text): {text}"
        );
        assert!(
            !text.contains("SKIPPED"),
            "the terminal loss is not a bounded skip: {text}"
        );
        assert!(
            sup.running.get(&name).unwrap().observed_park.is_none(),
            "the parked buffer is dropped with the instance (no phantom retry state)"
        );

        // (4) Repair: a later pass finds nothing to re-add — the lost event
        // stays lost (announced). (The DROP TABLE fixture also wiped A's row,
        // so the repaired ledger starts empty; the point is that B is never
        // resurrected.)
        let conn = rusqlite::Connection::open(&db).unwrap();
        for (_kind, sql) in &schema {
            conn.execute(sql, []).unwrap();
        }
        drop(conn);
        sup.drain_observed_for(&registry, &name, DrainMode::MidRun);
        assert_eq!(
            ledger_totals(state.path(), "obslost"),
            (0, 0),
            "B was announced lost and is never resurrected by a later pass"
        );
        sup.stop(&registry, "obslost", Some(Duration::from_millis(200)))
            .unwrap();
    }

    #[test]
    fn a_post_commit_signal_failure_emits_the_divergence_breadcrumb_and_returns_err() {
        // AI-9 (loop 2) — the post-commit signal-failure branch, executed END TO
        // END through the cfg(test) signal-fault seam: persist-first commits the
        // `running → paused` transition, THEN the seam makes `signal_backend`
        // fail with an injected error, so the ledger (now `paused`) and the live
        // process DIVERGE. The branch must (a) still surface the error, (b) emit
        // the divergence breadcrumb — instance + committed state + signal error
        // + the real recovery — to the story-10-2 sink, and (c) leave the
        // committed row `paused` (the durable state leads). A regression to a
        // silent swallow, or a breadcrumb that loses any quarter of the story,
        // fails here. The budget-driven leg proves the loop-2 remediation split:
        // a breach-driven pause must recommend `stop` ONLY (the latch is spent —
        // `resume` would leave an over-budget run unenforced), never `resume`.
        let (state, _manifest, registry) =
            setup_pause_guaranteed("sigfault", &["--linger-ms", "600000"]);
        let name = InstanceName::new("sigfault").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "sigfault").unwrap();
        let sink = install_capture_sink(&mut sup);
        sup.arm_signal_fault(name.clone());

        // (a) The command fails with the backend error ...
        let err = sup.pause(&registry, "sigfault").unwrap_err();
        assert!(
            matches!(&err, EngineError::Backend { .. }),
            "the post-commit signal failure must surface as the Backend error, got {err:?}"
        );
        // (b) ... and the divergence breadcrumb reaches the sink: the committed
        // state, the divergence claim, the recovery, and the injected why.
        let text = sink_text(&sink);
        assert!(
            text.contains("says 'paused'"),
            "the breadcrumb must name the COMMITTED state: {text}"
        );
        assert!(
            text.contains("may diverge"),
            "the breadcrumb must state the divergence: {text}"
        );
        assert!(
            text.contains("kt agent resume sigfault"),
            "a plain-command pause remediation must offer `resume` to realign: {text}"
        );
        assert!(
            text.contains("injected cfg(test) signal fault"),
            "the breadcrumb must carry the signal error's text: {text}"
        );
        // (c) The transition COMMITTED before the signal failed: the row reads
        // `paused` even though the command errored.
        assert_eq!(
            state_of(&registry, "sigfault"),
            LifecycleState::Paused,
            "persist-first: the committed transition must survive the signal failure"
        );

        // Budget-driven leg (AI-9 loop 2): with the cause_override
        // BudgetExceeded the per-Run breach latch is ALREADY spent — the
        // remediation must recommend `stop` only, never `resume`.
        {
            let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
            let n = conn
                .execute(
                    "UPDATE agent_instances SET state = 'running' WHERE name = 'sigfault'",
                    [],
                )
                .unwrap();
            assert_eq!(n, 1);
        }
        let breach = TransitionCause::budget_exceeded(BreachScope::Cumulative, 15, 30);
        let text_before = sink_text(&sink).len();
        sup.pause_with_cause(&registry, &name, breach).unwrap_err();
        let tail = &sink_text(&sink)[text_before..];
        assert!(
            tail.contains("the pause was budget-driven"),
            "the breach-driven remediation must say WHY stop is the only advice: {tail}"
        );
        assert!(
            tail.contains("kt agent stop sigfault"),
            "the breach-driven remediation must lead with `stop`: {tail}"
        );
        assert!(
            !tail.contains("kt agent resume"),
            "a breach-driven pause must NEVER advise `resume` (the latch is spent): {tail}"
        );

        // Teardown: the process was never suspended (both signals failed), so a
        // normal stop lands (stop does not consult `signal_backend`).
        sup.stop(&registry, "sigfault", Some(Duration::from_millis(600)))
            .unwrap();
    }

    #[test]
    fn a_budget_driven_pause_with_no_in_memory_handle_wraps_the_override_in_the_qualifier() {
        // AI-8 (loop 2) — the override honesty wrap: a BUDGET-DRIVEN pause that
        // reaches `suspend_or_resume` with NO in-memory handle held (the row
        // says `running`; e.g. enforcement re-evaluating a run this engine
        // session does not hold) must record `pause-best-effort` whose detail
        // WRAPS the override ("the requested override was: ...") — never the
        // bare `budget-exceeded` cause (which would read as a performed
        // suspension of a process nothing signalled) and never a plain command.
        let (_state, _manifest, registry) =
            setup_pause_guaranteed("ovrwrap", &["--linger-ms", "600000"]);
        let name = InstanceName::new("ovrwrap").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "ovrwrap").unwrap();
        // Drop the ONLY held handle (its kill-on-drop ends the process; the row
        // still says `running`) — the AI-8 premise: a pause with nothing held
        // to signal.
        sup.running.remove(&name);
        let cause = TransitionCause::budget_exceeded(BreachScope::Cumulative, 15, 30);
        sup.pause_with_cause(&registry, &name, cause).unwrap();
        assert_eq!(state_of(&registry, "ovrwrap"), LifecycleState::Paused);
        let events = Supervisor::read_events(&registry, "ovrwrap").unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.new_state, LifecycleState::Paused);
        let cause_json = serde_json::to_string(&last.cause).unwrap();
        assert!(
            cause_json.contains("\"kind\":\"pause-best-effort\""),
            "a budget pause with no in-memory handle must record the best-effort \
             qualifier, not the bare override: {cause_json}"
        );
        assert!(
            !cause_json.contains("\"kind\":\"budget-exceeded\""),
            "the override must be WRAPPED, never recorded as a performed budget \
             suspension: {cause_json}"
        );
        assert!(
            cause_json.contains("no live process handle"),
            "the qualifier must name the missing handle (the honest why): {cause_json}"
        );
        assert!(
            cause_json.contains("the requested override was:")
                && cause_json.contains("breach: cumulative"),
            "the qualifier's detail must wrap the requested BudgetExceeded override: \
             {cause_json}"
        );
        // No teardown `stop`: no handle is held (removed above; its Drop already
        // ended the process) — a stop here would be a handle-less no-op
        // transition, not a real teardown.
    }

    #[test]
    fn a_failed_terminal_drain_announces_the_lost_batch_to_the_sink() {
        // AI-41 (loop 2) — the Terminal-drain loss notice, EXECUTED: on the
        // TERMINAL drain there IS no next pass (the handle is being removed
        // right after), so a store failure there must announce the batch LOST —
        // with the count — to the story-10-2 sink, never reuse the MidRun
        // park-and-retry story (a lie exactly where loss is likeliest). Mirrors
        // the MidRun park test's full-schema sabotage below.
        let (state, _manifest, registry) = setup_fake("tdrain", &["--linger-ms", "600000"]);
        let name = InstanceName::new("tdrain").unwrap();
        let mut sup = Supervisor::with_backoff(fast_backoff());
        sup.start(&registry, "tdrain").unwrap();
        let sink = install_capture_sink(&mut sup);
        let log = registry.agent_output_log_path(&name);
        append_usage_lines(&log, &[(0, 10, 20), (1, 11, 22)]);
        let cursor_before = sup.running.get(&name).unwrap().usage_cursor;
        let db = state.path().join("state.db");

        // Sabotage the ledger (drop the table; the full schema is restored
        // below so the stop's own terminal drain sees a real ledger).
        let conn = rusqlite::Connection::open(&db).unwrap();
        let mut schema: Vec<String> = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT sql FROM sqlite_master \
                     WHERE tbl_name = 'usage_events' AND sql IS NOT NULL \
                     ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            for row in rows {
                schema.push(row.unwrap());
            }
        }
        conn.execute("DROP TABLE usage_events", []).unwrap();
        drop(conn);

        sup.drain_usage_for(&registry, &name, DrainMode::Terminal);
        let text = sink_text(&sink);
        assert!(
            text.contains("terminal drain (the process is dead"),
            "the terminal-drain loss notice must reach the sink: {text}"
        );
        assert!(
            text.contains("cannot be retried and is lost"),
            "the notice must say the batch is lost, not parked for retry: {text}"
        );
        assert!(
            text.contains("2 usage event(s)"),
            "the notice must name the lost COUNT: {text}"
        );
        assert_eq!(
            sup.running.get(&name).unwrap().usage_cursor,
            cursor_before,
            "a terminal drain that commits nothing must not advance the cursor"
        );

        // Repair the store, then tear down (the stop's own terminal drain now
        // sees a real ledger).
        let conn = rusqlite::Connection::open(&db).unwrap();
        for sql in &schema {
            conn.execute(sql, []).unwrap();
        }
        drop(conn);
        sup.stop(&registry, "tdrain", Some(Duration::from_millis(200)))
            .unwrap();
    }

    // ---- Story 11-5 (AI-71): the read/observation helpers' error surfaces ----
    //
    // These pin the documented contracts the read helpers promise but that no
    // test exercised: a log that cannot be READ (not merely absent — absent is
    // an honest empty) is a TYPED error naming the instance + path, never a
    // silent empty vec; an invalid name is the typed InvalidName; and the
    // blank-line/missing-generation tolerances are the only silent paths. A
    // directory where the log file must be makes `read_to_string` fail on
    // every OS (no OS-cfg — portable setup).

    #[test]
    fn read_events_rejects_an_invalid_name_with_the_typed_error() {
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        let err = Supervisor::read_events(&registry, "Bad Name").unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidName { ref name, .. } if name == "Bad Name"),
            "expected InvalidName, got {err:?}"
        );
    }

    #[test]
    fn read_events_surfaces_an_unreadable_instance_log_as_a_typed_log_error() {
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        registry.register("evt", "mock").unwrap();
        let name = InstanceName::new("evt").unwrap();
        let path = registry.instance_log_path(&name);
        std::fs::create_dir_all(&path).unwrap();
        let err = Supervisor::read_events(&registry, "evt").unwrap_err();
        match &err {
            EngineError::Log {
                name: n,
                path: p,
                detail,
            } => {
                assert_eq!(n, "evt");
                assert_eq!(p, &path.to_string_lossy().into_owned());
                assert!(!detail.is_empty(), "the error must name the read failure");
            }
            other => panic!("expected EngineError::Log, got {other:?}"),
        }
    }

    #[test]
    fn read_breach_events_rejects_an_invalid_name_with_the_typed_error() {
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        let err = Supervisor::read_breach_events(&registry, "Bad Name").unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidName { ref name, .. } if name == "Bad Name"),
            "expected InvalidName, got {err:?}"
        );
    }

    #[test]
    fn read_breach_events_surfaces_an_unreadable_breach_log_as_a_typed_log_error() {
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        registry.register("brk", "mock").unwrap();
        let name = InstanceName::new("brk").unwrap();
        let path = registry.instance_breach_log_path(&name);
        std::fs::create_dir_all(&path).unwrap();
        let err = Supervisor::read_breach_events(&registry, "brk").unwrap_err();
        match &err {
            EngineError::Log {
                name: n,
                path: p,
                detail,
            } => {
                assert_eq!(n, "brk");
                assert_eq!(p, &path.to_string_lossy().into_owned());
                assert!(!detail.is_empty(), "the error must name the read failure");
            }
            other => panic!("expected EngineError::Log, got {other:?}"),
        }
    }

    #[test]
    fn read_agent_log_since_surfaces_an_unreadable_attributed_log_as_a_typed_error() {
        // The NOT-FOUND arm is the documented empty tail; every OTHER read
        // failure is a typed EngineError::Log (the live-tail reader must
        // distinguish "nothing yet" from "cannot read").
        let state = tempfile::tempdir().unwrap();
        let registry = Registry::open(Some(state.path().to_path_buf())).unwrap();
        registry.register("alr", "mock").unwrap();
        let name = InstanceName::new("alr").unwrap();
        let path = registry.attributed_output_log_path(&name);
        std::fs::create_dir_all(&path).unwrap();
        let expected_path = path.to_string_lossy().into_owned();
        let err = Supervisor::read_agent_log_since(&registry, "alr", 0).unwrap_err();
        assert!(
            matches!(err, EngineError::Log { ref name, ref path, .. } if name == "alr"
                && path == &expected_path),
            "expected a Log error naming the instance + path, got {err:?}"
        );
    }

    #[test]
    fn log_line_helpers_skip_blank_lines_and_surface_unreadable_generations() {
        // parse_log_lines: blank lines are skipped (the documented AC-G
        // convention) — an all-blank input is Ok with nothing parsed.
        let mut out = Vec::new();
        parse_log_lines("\n   \n", &mut out).expect("blank lines are skipped, not errors");
        assert!(out.is_empty());
        // read_log_lines_from: a missing generation is a silent no-op (the
        // documented oldest-to-newest probe convention); an UNREADABLE one is
        // an error, mirroring read_events_from.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gen-missing.log");
        let mut out = Vec::new();
        read_log_lines_from(&missing, &mut out).expect("missing generation is a no-op");
        assert!(out.is_empty());
        let obstructed = dir.path().join("gen-0.log");
        std::fs::create_dir(&obstructed).unwrap();
        let mut out = Vec::new();
        let err = read_log_lines_from(&obstructed, &mut out).unwrap_err();
        assert!(!err.is_empty(), "an unreadable generation must be an error");
    }

    #[test]
    fn default_constructs_the_standard_supervisor() {
        // Default delegates to new() (the standard schedule); trivially
        // non-panicking, pinned so the impl block stays honest.
        let _ = Supervisor::default();
    }
}
