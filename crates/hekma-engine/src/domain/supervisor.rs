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

use hekma_adapter_api::{Capability, ConfigMapping, OsId, SupportLevel};

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
/// on stderr today, `[hekma] ` prefixed, `\n` terminated — so a sink
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
    /// (same `[hekma] `-prefixed wording, one line, AD-12's "diagnostics ride
    /// the engine log / stderr, NEVER `kt` stdout").
    ///
    /// The `[hekma] ` marker prefix and the terminating `\n` are added HERE,
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
        line.push_str("[hekma] ");
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
mod tests;
