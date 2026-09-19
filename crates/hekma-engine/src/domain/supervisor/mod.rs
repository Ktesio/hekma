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
}

impl Supervisor {
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

// Family modules — story 13-3 (ratified Option B): pure impl-block moves.
mod interaction;
mod lifecycle;
mod reaper;
mod spawn;
mod usage;
