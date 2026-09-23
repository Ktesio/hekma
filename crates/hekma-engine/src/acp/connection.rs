//! The ACP connection (spine AD-19): the engine's client-side transport
//! state for ONE running `acp` instance — the reader thread over the child's
//! stdout, the bounded write path to the child's stdin, request-response
//! correlation, the one-in-flight-turn flag, permission auto-denial, and the
//! surfaced-notice queue.
//!
//! ## Threading + lock discipline (AD-17/AD-18 — the load-bearing shape)
//!
//! The reader is a DEDICATED OS THREAD (the direct sibling of the AD-12
//! output-log tailer's proven thread shape — the engine's std child pipes
//! are blocking fds, which tokio tasks cannot consume without either a
//! tokio-process rewrite of the ProcessBackend port or a new crate, both out
//! of scope for the lean-transport decision; recorded as the transport's
//! documented deviation from "tokio tasks", same isolation guarantees). It
//! NEVER takes an engine mutex (the Registry or Supervisor locks): it owns
//! the stdout line stream, appends each raw line to the instance's
//! `agent.log` (the honest raw record — see below), routes responses through
//! a mutex-guarded pending map that holds ONLY correlation channels, and
//! queues surfaced facts as notices. The notice queue is drained by the
//! supervisor's EXISTING cadence (the crash-reaper tick) which already holds
//! the supervisor lock — so `emit_diagnostic`'s choke-point contract holds
//! without the reader ever touching that lock.
//!
//! **stdout-capture reconciliation (AD-12, the deliberate choice):** a
//! normal instance's stdout is redirected DIRECTLY to `agent.log` (the
//! crash-immune file) and tailer-attributed from there. An `acp` instance's
//! stdout is instead a PIPE — the protocol stream the reader must consume —
//! so the reader itself appends every raw line to `agent.log` as it parses,
//! byte-for-byte preserving the honest raw record; the existing tailer then
//! attributes those lines into `output.log` exactly as for every other
//! kind. The log stays the record; the transport stays pure ACP. The
//! accepted trade (documented, NFR-1): an engine crash mid-Run closes the
//! pipe (the child's next stdout write gets EPIPE) where a file redirect
//! would have kept it writable — inherent to speaking a stdio protocol; the
//! write-ahead spawn record + stop ladder still reconcile the instance.
//!
//! All writes (prompts, cancellation, permission denials) serialize through
//! ONE mutex over the child's stdin and use the SHARED bounded-write
//! primitive ([`write_stdin_bounded`], the AI-59 5s bound) — never an
//! unbounded write, never two interleaved writers.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::ChildStdout;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::client;
use super::codec::{self, Inbound};
use super::updates::{parse_session_update, SessionUpdate};
use crate::domain::{LogLine, LogStream};
use crate::ports::{
    write_stdin_bounded, BackendError, LogCapture, StdinState, STDIN_WRITE_TIMEOUT,
};
use crate::time::now_rfc3339;

/// The bound on queued surfaced notices (malformed lines, permission
/// denials, usage updates, unhandled messages) between reader and drain.
/// Generous for a chat-rate stream; a pathological agent that outproduces
/// the ~250ms reaper cadence loses its OLDEST notices with a loud dropped
/// count on the next drain — never silent truncation (AI-18), never
/// unbounded memory.
const MAX_QUEUED_NOTICES: usize = 64;

/// One surfaced fact the reader queued for the supervisor to emit through
/// the `emit_diagnostic` choke point (the reader cannot take the supervisor
/// lock; the reaper tick drains these under it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AcpNotice {
    /// A stdout line was not decodable ACP — surfaced, skipped, counted
    /// (never fatal to the stream).
    MalformedLine {
        /// The 1-based position of the offending line in the stream.
        line_number: u64,
        /// The running count of malformed lines seen so far.
        total: u64,
        /// The typed codec reason (traffic-free).
        detail: String,
    },
    /// A `session/update` with an unknown/missing discriminator — counted.
    UnhandledUpdate {
        /// The raw discriminator (or `<missing>` / `<notification:m>`).
        discriminator: String,
        /// The running count of unhandled updates so far.
        total: u64,
    },
    /// An agent→client request the engine does not answer natively —
    /// answered with a JSON-RPC method-not-found error and surfaced.
    UnhandledAgentRequest {
        /// The method the agent asked for.
        method: String,
    },
    /// An incoming `session/request_permission` — answered DENIED (the
    /// agent's offered denial option, else `cancelled`), one diagnostic per
    /// request (spine AD-19 / D3).
    PermissionDenied {
        /// How the denial was answered: `denied via '<option>'` or
        /// `cancelled (no denial option offered)`.
        outcome: String,
    },
    /// A `usage_update` — CONTEXT-grain usage. Surfaced here and NOWHERE
    /// near the billing ledger (the two grains must never mix, spine AD-19);
    /// story 14-3 also records the latest figure on the connection so the
    /// show/fleet surfaces can display it as the context metric.
    UsageUpdate {
        /// Context-window tokens used (agent-reported).
        used: Option<u64>,
        /// The context-window size (agent-reported).
        size: Option<u64>,
        /// The optional agent-reported cost block, as its raw display pair.
        cost: Option<(String, String)>,
    },
    /// A response arrived for an id nobody sent — a protocol desync.
    UnexpectedResponse {
        /// The unclaimed id.
        id: u64,
    },
    /// The reader exited while the connection was still held (EOF or a read
    /// error). The instance's honest record keeps the raw tail; the
    /// supervisor surfaces this once.
    Ended {
        /// Why the reader stopped (traffic-free).
        detail: String,
    },
}

impl AcpNotice {
    /// The diagnostic text for this notice (the instance-name prefix is
    /// added by the draining caller, matching the emission shape every
    /// other engine diagnostic uses).
    pub(crate) fn message(&self) -> String {
        match self {
            AcpNotice::MalformedLine {
                line_number,
                total,
                detail,
            } => format!(
                "acp: malformed line #{line_number} on the agent's stdout was skipped \
                 ({detail}); {total} malformed line(s) so far this Run — the stream continues"
            ),
            AcpNotice::UnhandledUpdate {
                discriminator,
                total,
            } => format!(
                "acp: unhandled session update '{discriminator}' was counted and skipped; \
                 {total} unhandled update(s) so far this Run — the stream continues"
            ),
            AcpNotice::UnhandledAgentRequest { method } => format!(
                "acp: the agent issued an unsupported request '{method}'; it was answered with \
                 a method-not-found error (the engine advertises no client capabilities)"
            ),
            AcpNotice::PermissionDenied { outcome } => format!(
                "acp: the agent requested permission and was refused per the default \
                 no-capability posture ({outcome})"
            ),
            AcpNotice::UsageUpdate { used, size, cost } => {
                // CONTEXT-grain only: the wording names the grain so no one
                // mistakes this for a billing figure.
                let cost_note = match cost {
                    Some((amount, currency)) => format!(
                        ", agent-reported cost {amount} {currency} (context-grain, NOT billed)"
                    ),
                    None => String::new(),
                };
                format!(
                    "acp: usage_update (CONTEXT-grain) — {} of {} context tokens reported{}",
                    used.map(|u| u.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    size.map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    cost_note,
                )
            }
            AcpNotice::UnexpectedResponse { id } => format!(
                "acp: the agent sent a response for request id {id}, which this engine did \
                 not issue (protocol desync) — counted and skipped"
            ),
            AcpNotice::Ended { detail } => {
                format!("acp: the agent's stdout stream ended ({detail})")
            }
        }
    }
}

/// The LATEST `usage_update` context figure for one connection (story 14-3,
/// spine AD-19 — T1). Recorded per instance so `kt agent show` / `list` /
/// `show --json` / fleet can DISPLAY it as the context metric it is. This is
/// CONTEXT-grain, never billing: it names the agent's session context window
/// (`used` of `size` tokens, optional agent-reported `cost`) and is stored
/// HERE ONLY — it never enters `usage_events` (the two usage grains must
/// never mix; the billing tiers are the observed/sentinel channels).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AcpContextUsage {
    /// Context-window tokens used (agent-reported).
    pub used: Option<u64>,
    /// The context-window size in tokens (agent-reported).
    pub size: Option<u64>,
    /// The optional agent-reported cost block, `(amount string, currency)` —
    /// the agent's OWN claim, surfaced labeled as such; never derived into a
    /// billed figure by this engine (AD-8: never fabricate, never mix grains).
    pub cost: Option<(String, String)>,
}

/// Why a prompt could not be dispatched (surfaced as the typed refusal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PromptError {
    /// No ACP session is established (the handshake has not completed for
    /// this connection).
    NoSession,
    /// A turn is already in flight — the surfaced typed refusal (spine
    /// AD-19: ACP serializes turns per session).
    InFlight,
    /// The bounded write did not return within [`STDIN_WRITE_TIMEOUT`] —
    /// the AI-59 bounded-failure shape (mapped to the typed
    /// `InteractionTimedOut` surface).
    TimedOut,
    /// The bounded write failed outright (broken pipe — the agent exited).
    Write(String),
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::NoSession => {
                write!(f, "no ACP session is established for this instance")
            }
            PromptError::InFlight => write!(f, "an ACP turn is already in flight"),
            PromptError::TimedOut => write!(
                f,
                "the bounded stdin write did not return within {}s (the agent is not \
                 consuming its stdin)",
                STDIN_WRITE_TIMEOUT.as_secs()
            ),
            PromptError::Write(detail) => {
                write!(f, "the bounded write to the agent failed: {detail}")
            }
        }
    }
}

/// Why a bounded stdin write failed (the typed mapping the supervisor's
/// send path uses).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WriteError {
    /// The write exceeded [`STDIN_WRITE_TIMEOUT`].
    TimedOut,
    /// The write failed outright (a broken pipe / an OS error).
    Failed(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::TimedOut => write!(
                f,
                "the bounded stdin write did not return within {}s (the agent is not \
                 consuming its stdin)",
                STDIN_WRITE_TIMEOUT.as_secs()
            ),
            WriteError::Failed(detail) => write!(f, "{detail}"),
        }
    }
}

/// The shared connection state (one `Arc` behind every clone). Every lock
/// here guards a SHORT, BOUNDED swap (map insert/remove, an id swap, a
/// queue push) — the AD-17/AD-18 bounded-work rule holds by construction;
/// nothing waits on external state under these mutexes.
struct Inner {
    /// The child's stdin write half — the SOLE writer path (prompts from the
    /// supervisor thread, denials from the reader thread, cancellation from
    /// stop), serialized by this mutex and bounded per write.
    stdin: Mutex<StdinState>,
    /// Pending request→waiter correlation: request id → the channel the
    /// (synchronous, bounded) waiter receives the reply on. Registered
    /// BEFORE the request is written, so a fast agent cannot outrun its
    /// waiter.
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    /// The next outbound request id (monotonic, starts at 1).
    next_id: AtomicU64,
    /// The ACP session id from `session/new` (set once by the handshake).
    session_id: Mutex<Option<String>>,
    /// Whether the agent advertised `loadSession` at initialize (14-2 reads
    /// this; in-memory for this story).
    load_session: AtomicBool,
    /// The ONE in-flight turn: `Some(request id)` while a `session/prompt`
    /// is outstanding, `None` otherwise. A second prompt is refused (the
    /// surfaced typed in-flight refusal, spine AD-19).
    prompt_id: Mutex<Option<u64>>,
    /// Set by the connection's teardown to ask the reader to stop (it still
    /// exits primarily on EOF — the flag covers abandoning a live child).
    shutdown: AtomicBool,
    /// The surfaced-notice queue (bounded; drained by the reaper cadence
    /// under the supervisor lock).
    notices: Mutex<Notices>,
    /// The LATEST `usage_update` context figure (story 14-3, T1) — recorded
    /// by the reader thread (a short bounded swap, the AD-17/AD-18 shape),
    /// read by the supervisor's Fleet/status read helpers. CONTEXT-grain
    /// only; never the billing ledger.
    context_usage: Mutex<Option<AcpContextUsage>>,
}

/// The bounded notice queue + its silent-loss counter (AI-18: a drop is
/// announced on the next drain, never silent).
#[derive(Default)]
struct Notices {
    queued: Vec<AcpNotice>,
    dropped: u64,
}

impl Inner {
    /// Queue one surfaced notice (bounded; the oldest is dropped with a
    /// counted announcement when the queue overflows). Called from the
    /// reader thread only — never while an engine mutex is held.
    fn push_notice(&self, notice: AcpNotice) {
        let mut notices = self
            .notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if notices.queued.len() >= MAX_QUEUED_NOTICES {
            notices.queued.remove(0);
            notices.dropped += 1;
        }
        notices.queued.push(notice);
    }
}

/// The engine's ACP client connection for one running `acp` instance.
/// Clones share the same state (the supervisor holds one; the reader thread
/// holds one). Dropping the LAST clone tears the transport down: the
/// shutdown flag rises and the child's stdin is closed, so the reader sees
/// EOF and exits — the connection teardown on stop/drop/terminal-settle
/// (spine AD-19).
#[derive(Clone)]
pub(crate) struct AcpConnection {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AcpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpConnection")
            .field("session_id", &self.session_id())
            .field("in_flight", &self.turn_in_flight())
            .finish_non_exhaustive()
    }
}

impl AcpConnection {
    /// Start the connection: take ownership of the child's stdin/stdout
    /// halves and spawn the reader thread. `raw_log` is the instance's
    /// `agent.log` path (the reader appends every raw line there — the
    /// honest-record reconciliation documented on this module). `capture`,
    /// when present, is the instance's attributed-output handle — the reader
    /// routes notable protocol facts (the turn's stopReason, the stream's
    /// end) into it as `engine`-attributed lines via the SAME machinery the
    /// supervisor uses (AD-12, reused, not duplicated).
    ///
    /// Errors are launch-fatal and traffic-free (missing pipe halves mean
    /// the spawn did not give the transport what it needs).
    pub(crate) fn start(
        instance: &str,
        stdin: Option<StdinState>,
        stdout: Option<ChildStdout>,
        raw_log: std::path::PathBuf,
        capture: Option<LogCapture>,
    ) -> Result<Self, String> {
        let stdin = stdin.ok_or_else(|| {
            "cannot establish the ACP connection: no stdin pipe is held for the child \
             (the ACP transport writes its requests to the child's stdin)"
                .to_string()
        })?;
        let stdout = stdout.ok_or_else(|| {
            "cannot establish the ACP connection: no stdout pipe is held for the child \
             (the ACP transport reads the protocol from the child's stdout)"
                .to_string()
        })?;
        let inner = Arc::new(Inner {
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
            load_session: AtomicBool::new(false),
            prompt_id: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            notices: Mutex::new(Notices::default()),
            context_usage: Mutex::new(None),
        });
        let connection = Self {
            inner: Arc::clone(&inner),
        };
        let reader_name = format!("acp-reader-{instance}");
        let instance_name = instance.to_string();
        let reader = std::thread::Builder::new()
            .name(reader_name)
            .spawn(move || run_reader(instance_name, stdout, raw_log, capture, inner))
            .map_err(|e| format!("could not spawn the ACP reader thread: {e}"))?;
        // Detached by design: the thread exits on EOF (the stop ladder kills
        // the child, closing the pipe) or on the teardown flag. Joining here
        // would block the start path on external process behavior — the
        // exact unbounded wait AD-17 forbids.
        drop(reader);
        Ok(connection)
    }

    // ---- outbound (supervisor thread; every write bounded) ----

    /// Mint the next outbound request id.
    pub(crate) fn next_request_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a waiter for `id` BEFORE the request is written, so a fast
    /// agent's reply can never outrun its waiter. Returns the receiving
    /// half; [`Self::await_response`] consumes it, bounded.
    pub(crate) fn register_waiter(&self, id: u64) -> mpsc::Receiver<Result<Value, String>> {
        let (tx, rx) = mpsc::channel();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        rx
    }

    /// Await a registered response, bounded by `timeout`. `None` on timeout
    /// (the waiter stays registered — the reader fails every waiter on
    /// teardown; a timed-out handshake refuses the start anyway).
    pub(crate) fn await_response(
        &self,
        rx: mpsc::Receiver<Result<Value, String>>,
        timeout: Duration,
    ) -> Option<Result<Value, String>> {
        rx.recv_timeout(timeout).ok()
    }

    /// Write ONE pre-encoded ndJSON line to the child's stdin — the SOLE
    /// write path, bounded to [`STDIN_WRITE_TIMEOUT`] (the AI-59 shape: a
    /// blocked child must never freeze supervision; the write runs either
    /// on the supervisor's blocking thread or the reader thread, both
    /// off-lock or bounded).
    pub(crate) fn write_line(&self, line: &str) -> Result<(), WriteError> {
        let mut guard = self
            .inner
            .stdin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut payload = line.to_string();
        payload.push_str(codec::LINE_TERMINATOR);
        write_stdin_bounded(&mut guard, payload.as_bytes(), STDIN_WRITE_TIMEOUT).map_err(|err| {
            match err {
                BackendError::StdinTimedOut { .. } => WriteError::TimedOut,
                other => WriteError::Failed(other.to_string()),
            }
        })
    }

    /// Dispatch ONE `session/prompt` (spine AD-19: `send` = one prompt, ONE
    /// text ContentBlock). Bounded write; on success the turn is marked
    /// in-flight and the method returns IMMEDIATELY — the turn's chunks and
    /// stopReason arrive asynchronously through the reader.
    pub(crate) fn send_prompt(&self, text: &str) -> Result<(), PromptError> {
        let session_id = self.session_id().ok_or(PromptError::NoSession)?;
        let mut prompt_id = self
            .inner
            .prompt_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if prompt_id.is_some() {
            return Err(PromptError::InFlight);
        }
        let id = self.next_request_id();
        let line = codec::encode_request(
            id,
            "session/prompt",
            codec::session_prompt_params(&session_id, text),
        );
        self.write_line(&line).map_err(|err| match err {
            WriteError::TimedOut => PromptError::TimedOut,
            WriteError::Failed(detail) => PromptError::Write(detail),
        })?;
        *prompt_id = Some(id);
        Ok(())
    }

    /// Write the `session/cancel` notification for an in-flight turn (the
    /// stop path calls this BEFORE the termination ladder, best-effort).
    pub(crate) fn cancel_turn(&self) -> Result<(), String> {
        let session_id = self.session_id().ok_or_else(|| {
            "no ACP session is established, so there is no turn to cancel".to_string()
        })?;
        let line =
            codec::encode_notification("session/cancel", codec::session_cancel_params(&session_id));
        self.write_line(&line).map_err(|err| match err {
            WriteError::TimedOut => format!(
                "the bounded stdin write did not return within {}s",
                STDIN_WRITE_TIMEOUT.as_secs()
            ),
            WriteError::Failed(detail) => detail,
        })
    }

    // ---- state reads (short, bounded — the AD-17/AD-18 swap shape) ----

    /// The ACP session id, once the handshake has completed.
    pub(crate) fn session_id(&self) -> Option<String> {
        self.inner
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Record the handshake's session id (called by the handshake driver).
    pub(crate) fn set_session_id(&self, session_id: String) {
        *self
            .inner
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session_id);
    }

    /// Record the agent's `loadSession` capability (called by the handshake).
    pub(crate) fn set_load_session(&self, load_session: bool) {
        self.inner
            .load_session
            .store(load_session, Ordering::Relaxed);
    }

    /// Record the LATEST `usage_update` context figure (story 14-3, T1) —
    /// called by the reader thread on every `usage_update`, REPLACING any
    /// earlier figure (the metric is the agent's current context state, not a
    /// cumulative count — replacing is the honest semantics; the diagnostic
    /// log keeps every report's history). A short bounded swap, never an
    /// engine mutex.
    pub(crate) fn record_context_usage(
        &self,
        used: Option<u64>,
        size: Option<u64>,
        cost: Option<(String, String)>,
    ) {
        *self
            .inner
            .context_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(AcpContextUsage { used, size, cost });
    }

    /// The latest recorded context-usage figure, or `None` while the agent
    /// has not reported one this Run (`show`/fleet render the honest absence).
    pub(crate) fn context_usage(&self) -> Option<AcpContextUsage> {
        self.inner
            .context_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether a turn is currently in flight (the in-flight refusal's check).
    pub(crate) fn turn_in_flight(&self) -> bool {
        self.inner
            .prompt_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Drain the queued notices (the supervisor's cadence calls this under
    /// its lock and routes each through `emit_diagnostic`). A dropped-count
    /// announcement is PREPENDED when the bound ever bit — surfaced-not-
    /// silent (AI-18).
    pub(crate) fn drain_notices(&self) -> Vec<String> {
        let mut notices = self
            .inner
            .notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dropped = notices.dropped;
        let queued = std::mem::take(&mut notices.queued);
        notices.dropped = 0;
        let mut out = Vec::with_capacity(queued.len() + 1);
        if dropped > 0 {
            out.push(format!(
                "acp: {dropped} older acp diagnostic(s) were dropped because the notice \
                 queue overflowed before any drain — the bound is deliberate, the loss is \
                 announced (surfaced-not-silent)"
            ));
        }
        out.extend(queued.iter().map(AcpNotice::message));
        out
    }
}

impl Drop for AcpConnection {
    /// Teardown (spine AD-19: on stop/drop/terminal-settle): raise the
    /// shutdown flag and CLOSE the child's stdin. The reader then exits on
    /// EOF (or the flag); the stop ladder's termination of the process group
    /// guarantees the pipe closes even for an uncooperative child. Closing
    /// stdin is what delivers EOF to a well-behaved ACP agent.
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        // Take the stdin state out and drop it — closing the pipe's write
        // half. A state already `TimedOut` holds no pipe (the bounded-write
        // thread owns it) — nothing to close, honestly none.
        let mut guard = self
            .inner
            .stdin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _closed = std::mem::replace(&mut *guard, StdinState::NoPipe);
        drop(guard);
    }
}

/// The reader loop (a dedicated thread — see the module docs for the
/// threading + lock discipline). Consumes the child's stdout as pure ACP:
/// appends every raw line to the instance's agent log (the honest record),
/// then decodes + routes.
fn run_reader(
    instance: String,
    stdout: ChildStdout,
    raw_log: std::path::PathBuf,
    capture: Option<LogCapture>,
    inner: Arc<Inner>,
) {
    // A connection handle over the SAME shared state, for the writer +
    // notice paths (the reader never touches engine mutexes).
    let connection = AcpConnection {
        inner: Arc::clone(&inner),
    };
    // One append handle for the whole Run (agent.log is append-only and
    // never rotated — Epic 4 shipped rotation only for output.log). The
    // reader is the ONLY writer to this file for an acp instance (stdout is
    // piped, not redirected), so there is no interleaving concern.
    let mut raw: Option<std::fs::File> = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&raw_log)
    {
        Ok(file) => Some(file),
        Err(_) => {
            // The raw record cannot be kept — say so once and keep the
            // PROTOCOL stream alive anyway (the transport must not die for a
            // logging failure).
            inner.push_notice(AcpNotice::Ended {
                detail: format!(
                    "the raw agent log at {} could not be opened, so raw lines are NOT being \
                     recorded there (the transport continues)",
                    raw_log.display()
                ),
            });
            None
        }
    };
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut line_number: u64 = 0;
    let mut malformed_total: u64 = 0;
    let mut unhandled_total: u64 = 0;
    loop {
        if inner.shutdown.load(Ordering::Relaxed) {
            break;
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF: the child closed its stdout (a stop kills the group;
                // the agent exited on its own; or stdin closure ended it).
                inner.push_notice(AcpNotice::Ended {
                    detail: "end of stream (the agent closed stdout or exited)".to_string(),
                });
                break;
            }
            Ok(_) => {
                line_number += 1;
                // Honest-record append FIRST (the raw line, verbatim, with
                // its terminator), best-effort — a logging failure never
                // kills the protocol stream.
                if let Some(file) = raw.as_mut() {
                    let _ = file.write_all(line.as_bytes());
                    let _ = file.flush();
                }
                let trimmed = line.trim_end_matches(['\n', '\r']);
                match codec::parse_line(trimmed) {
                    Ok(None) => continue, // blank framing line
                    Ok(Some(inbound)) => route_inbound(
                        &instance,
                        &connection,
                        capture.as_ref(),
                        inbound,
                        &mut unhandled_total,
                    ),
                    Err(err) => {
                        // Surfaced, skipped, COUNTED (never fatal — the
                        // stream continues on the next line).
                        malformed_total += 1;
                        inner.push_notice(AcpNotice::MalformedLine {
                            line_number,
                            total: malformed_total,
                            detail: err.to_string(),
                        });
                    }
                }
            }
            Err(err) => {
                inner.push_notice(AcpNotice::Ended {
                    detail: format!("stdout read failed: {err}"),
                });
                break;
            }
        }
    }
    // Teardown courtesy: fail any still-registered waiter so a bounded
    // handshake never waits out its full timeout after the stream died.
    let pending = {
        let mut guard = inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *guard)
    };
    for (_, waiter) in pending {
        let _ = waiter.send(Err(
            "the agent's stdout stream ended before the response arrived".to_string(),
        ));
    }
    // A turn can never complete now — clear the in-flight flag so the
    // surface does not report a phantom in-flight turn on a dead transport.
    *inner
        .prompt_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Route ONE decoded inbound message (reader thread; NO engine locks).
/// `unhandled_total` is the reader's running unhandled count — carried
/// through so every surfaced notice names the honest total so far.
fn route_inbound(
    instance: &str,
    connection: &AcpConnection,
    capture: Option<&LogCapture>,
    inbound: Inbound,
    unhandled_total: &mut u64,
) {
    match inbound {
        Inbound::Response { id, result } => {
            route_response(instance, connection, capture, id, Ok(result))
        }
        Inbound::ResponseError { id, code, .. } => {
            // Traffic-free: the CODE is surfaced, never the agent-authored
            // message (which may quote prompt/file content). The raw line is
            // already in the honest agent log.
            route_response(
                instance,
                connection,
                capture,
                id,
                Err(format!("error response (code {code})")),
            )
        }
        Inbound::AgentRequest { id, method, params } => {
            // The agent is CALLING us. The engine advertises no client
            // capabilities (D3): permission requests are DENIED (the offered
            // denial option, else cancelled) with exactly one surfaced
            // diagnostic per request; anything else is answered
            // method-not-found and surfaced.
            if method == "session/request_permission" {
                let denial = client::permission_denial_result(&params);
                let outcome = match denial["outcome"]["optionId"].as_str() {
                    Some(option) => format!("denied via option '{option}'"),
                    None => "cancelled (no denial option offered)".to_string(),
                };
                let line = codec::encode_response(id, denial);
                let _ = connection.write_line(&line);
                connection
                    .inner
                    .push_notice(AcpNotice::PermissionDenied { outcome });
                return;
            }
            let _ = connection.write_line(&agent_request_unsupported_line(id));
            connection
                .inner
                .push_notice(AcpNotice::UnhandledAgentRequest { method });
        }
        Inbound::Notification { method, params } => {
            if method == "session/update" {
                match parse_session_update(&params) {
                    SessionUpdate::Unhandled { discriminator } => {
                        *unhandled_total += 1;
                        connection.inner.push_notice(AcpNotice::UnhandledUpdate {
                            discriminator,
                            total: *unhandled_total,
                        });
                    }
                    SessionUpdate::UsageUpdate { used, size, cost } => {
                        // CONTEXT-grain surfacing ONLY — never the ledger
                        // (spine AD-19; the billing tiers are the observed /
                        // sentinel channels). Story 14-3: the figure is ALSO
                        // recorded on the connection (a short bounded swap)
                        // so `show`/fleet can display the context metric —
                        // labeled context-grain by its field name, never
                        // inside the billing token/cost cells.
                        connection.record_context_usage(used, size, cost.clone());
                        connection
                            .inner
                            .push_notice(AcpNotice::UsageUpdate { used, size, cost });
                    }
                    // The turn-content updates (message/thought chunks, tool
                    // calls, plan, mode/command updates) land ONLY in the
                    // honest record: the raw line was already appended to
                    // agent.log above, and the existing AD-12 tailer
                    // attributes it into output.log. No second engine line —
                    // the record stays the agent's own voice.
                    SessionUpdate::AgentMessageChunk
                    | SessionUpdate::AgentThoughtChunk
                    | SessionUpdate::ToolCall
                    | SessionUpdate::ToolCallUpdate
                    | SessionUpdate::Plan
                    | SessionUpdate::AvailableCommandsUpdate
                    | SessionUpdate::CurrentModeUpdate => {}
                }
                return;
            }
            // Any other (unknown) notification is surfaced + counted —
            // tolerant, the stream continues.
            *unhandled_total += 1;
            connection.inner.push_notice(AcpNotice::UnhandledUpdate {
                discriminator: format!("<notification:{method}>"),
                total: *unhandled_total,
            });
        }
    }
}

/// Route a response (success or error) to its waiter — or, for the
/// in-flight prompt, complete the turn (clear the flag + land the
/// stopReason as an `engine`-attributed line via the EXISTING AD-12
/// capture machinery).
fn route_response(
    instance: &str,
    connection: &AcpConnection,
    capture: Option<&LogCapture>,
    id: u64,
    outcome: Result<Value, String>,
) {
    // (1) A registered waiter (the handshake's correlation) gets it first.
    let waiter = connection
        .inner
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
    if let Some(waiter) = waiter {
        let _ = waiter.send(outcome);
        return;
    }
    // (2) The in-flight prompt's completion: clear the turn state and land
    // the stopReason in the attributed record.
    let prompt_id = connection
        .inner
        .prompt_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if prompt_id == Some(id) {
        let outcome_value = outcome.ok();
        let stop_reason = outcome_value
            .as_ref()
            .and_then(|result| result.get("stopReason"))
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        if let Some(capture) = capture {
            capture.send_engine_line(LogLine::new(
                instance,
                LogStream::Engine,
                format!("acp: turn complete (stopReason: {stop_reason})"),
                now_rfc3339(),
            ));
        }
        return;
    }
    // (3) Neither: a protocol desync — surfaced + skipped.
    connection
        .inner
        .push_notice(AcpNotice::UnexpectedResponse { id });
}

/// The method-not-found error LINE for an unsupported agent request (built
/// here because [`codec::encode_response`] renders a RESULT envelope and the
/// error shape is distinct).
fn agent_request_unsupported_line(id: u64) -> String {
    serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32601,
            "message": "method not supported by this client",
        },
    }))
    .unwrap_or_else(|_| format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"unsupported\"}}}}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `AcpNotice` arm formats with its contract's key phrases (the
    /// surfaced-not-silent wording is part of the product surface — a Display
    /// arm that panics or drops a fact is a bug). 2026-09-23 coverage batch.
    #[test]
    fn every_acp_notice_arm_formats() {
        let notices = [
            AcpNotice::MalformedLine {
                line_number: 3,
                total: 2,
                detail: "not json".to_string(),
            },
            AcpNotice::UnhandledUpdate {
                discriminator: "weird".to_string(),
                total: 1,
            },
            AcpNotice::UnhandledUpdate {
                discriminator: "<missing>".to_string(),
                total: 2,
            },
            AcpNotice::UnhandledAgentRequest {
                method: "fs/read".to_string(),
            },
            AcpNotice::PermissionDenied {
                outcome: "denied via 'deny'".to_string(),
            },
            AcpNotice::UsageUpdate {
                used: Some(1200),
                size: Some(200_000),
                cost: Some(("0.0034".to_string(), "USD".to_string())),
            },
            AcpNotice::UsageUpdate {
                used: None,
                size: None,
                cost: None,
            },
            AcpNotice::UnexpectedResponse { id: 42 },
            AcpNotice::Ended {
                detail: "stdout read failed".to_string(),
            },
        ];
        let texts: Vec<String> = notices.iter().map(AcpNotice::message).collect();
        assert!(texts[0].contains("malformed line #3"), "{}", texts[0]);
        assert!(texts[0].contains("stream continues"), "{}", texts[0]);
        assert!(texts[1].contains("'weird'"), "{}", texts[1]);
        assert!(texts[2].contains("<missing>"), "{}", texts[2]);
        assert!(texts[3].contains("method-not-found"), "{}", texts[3]);
        assert!(texts[4].contains("refused"), "{}", texts[4]);
        assert!(texts[5].contains("1200 of 200000"), "{}", texts[5]);
        assert!(texts[5].contains("NOT billed"), "{}", texts[5]);
        assert!(texts[6].contains("unknown"), "{}", texts[6]);
        assert!(!texts[6].contains("NOT billed"), "{}", texts[6]);
        assert!(texts[7].contains("protocol desync"), "{}", texts[7]);
        assert!(texts[8].contains("stdout read failed"), "{}", texts[8]);
    }

    /// The `PromptError`/`WriteError` Display arms (the send path's typed
    /// refusals) format with their operator-facing facts.
    #[test]
    fn prompt_and_write_error_arms_format() {
        assert!(PromptError::NoSession
            .to_string()
            .contains("no ACP session is established"));
        assert!(PromptError::InFlight
            .to_string()
            .contains("already in flight"));
        assert!(PromptError::TimedOut
            .to_string()
            .contains("not consuming its stdin"));
        assert!(PromptError::Write("broken".to_string())
            .to_string()
            .contains("bounded write to the agent failed: broken"));
        assert!(WriteError::TimedOut
            .to_string()
            .contains("not consuming its stdin"));
        assert_eq!(WriteError::Failed("boom".to_string()).to_string(), "boom");
    }

    /// `AcpConnection::start` refuses, traffic-free and launch-fatal, when
    /// either pipe half is missing (both error arms, each naming the fact).
    #[test]
    fn start_requires_both_pipe_halves() {
        let err = AcpConnection::start(
            "halves",
            None,
            None,
            std::env::temp_dir().join("acp-halves-test-raw.log"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("no stdin pipe"), "{err}");
        let err = AcpConnection::start(
            "halves",
            Some(StdinState::NoPipe),
            None,
            std::env::temp_dir().join("acp-halves-test-raw.log"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("no stdout pipe"), "{err}");
    }

    /// A live connection WITHOUT a completed handshake has no session:
    /// `send_prompt` refuses with `NoSession` and `cancel_turn` refuses with
    /// its no-session fact — the pre-handshake send/cancel arms.
    #[test]
    fn send_and_cancel_without_a_session_are_refused() {
        let child = std::process::Command::new(
            std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()),
        )
        .args(["build", "-p", "hekma-conformance", "--bin", "fake_agent"])
        .status()
        .expect("run cargo for the fake agent build");
        assert!(child.success(), "fake_agent build failed");
        let bin = hekma_conformance::fake_agent_bin();
        let mut sleeper = std::process::Command::new(&bin)
            .arg("--linger-ms")
            .arg("15000")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the fake agent for the pipe halves");
        let stdin = sleeper.stdin.take().expect("stdin pipe");
        let stdout = sleeper.stdout.take().expect("stdout pipe");
        let conn = AcpConnection::start(
            "nosession",
            Some(StdinState::Live(stdin)),
            Some(stdout),
            std::env::temp_dir().join("acp-nosession-test-raw.log"),
            None,
        )
        .expect("start with both halves");
        let err = conn.send_prompt("hello").unwrap_err();
        assert_eq!(err, PromptError::NoSession);
        let err = conn.cancel_turn().unwrap_err();
        assert!(err.contains("no ACP session is established"), "{err}");
        // The Debug impl (the operations surface) names the session state.
        let debug = format!("{conn:?}");
        assert!(debug.contains("AcpConnection"), "{debug}");
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    /// The notice queue is BOUNDED: overflowing it drops the OLDEST notices
    /// and the drain announces the loss (surfaced-not-silent — the loss is
    /// never silent).
    #[test]
    fn queue_overflow_drops_the_oldest_and_announces_it() {
        let conn = AcpConnection {
            inner: Arc::new(Inner {
                stdin: std::sync::Mutex::new(StdinState::NoPipe),
                pending: std::sync::Mutex::new(std::collections::HashMap::new()),
                next_id: std::sync::atomic::AtomicU64::new(1),
                session_id: std::sync::Mutex::new(None),
                load_session: std::sync::atomic::AtomicBool::new(false),
                prompt_id: std::sync::Mutex::new(None),
                shutdown: std::sync::atomic::AtomicBool::new(false),
                notices: std::sync::Mutex::new(Notices::default()),
                context_usage: std::sync::Mutex::new(None),
            }),
        };
        for n in 0..(MAX_QUEUED_NOTICES + 4) {
            conn.inner.push_notice(AcpNotice::UnhandledUpdate {
                discriminator: format!("d{n}"),
                total: n as u64,
            });
        }
        let drained = conn.drain_notices();
        // The announcement leads, then the MAX surviving notices.
        assert_eq!(drained.len(), MAX_QUEUED_NOTICES + 1);
        assert!(
            drained[0].contains("were dropped"),
            "the overflow announcement must lead the drain: {}",
            drained[0]
        );
        assert!(
            drained[1].contains("'d4'"),
            "the oldest SURVIVING notice follows the announcement: {}",
            drained[1]
        );
    }
}
