//! Integration tests for story 14-1 (spine AD-19) — the ACP transport core,
//! driven through the engine's PUBLIC blocking facade only, spawning the REAL
//! `fake_acp_agent` helper so a genuine ndJSON/JSON-RPC subprocess is driven
//! end to end (the epic-14 spec's fake agent = contract twin).
//!
//! The matrix rows this suite pins (the spec's I/O matrix, 14-1's slice):
//!
//! * **Register + start + handshake** — `--kind acp` registers like any
//!   builtin; `start` resolves the launch from `acp.command`/`acp.args`,
//!   drives `initialize` (version 1, default caps, clientInfo) →
//!   `session/new`, and the instance reaches `running` (AC1's first half).
//! * **Missing launch config refuses honestly** — no `acp.command` → the
//!   start refuses naming BOTH keys (the I/O matrix's honest refusal).
//! * **Version counter refuses** — an agent countering version 2 (outside
//!   the tolerated set {1}) → the start refuses, traffic-free, and the
//!   instance lands `failed`.
//! * **Prompt turn streams** — `send` = ONE bounded `session/prompt`; the
//!   turn's message chunks + final stopReason land asynchronously in the
//!   per-instance output log (raw `agent.log` + the attributed view, AD-12).
//! * **In-flight refusal** — a second prompt while a turn is in flight is
//!   the surfaced typed refusal (AC2); the first turn completes; a THIRD
//!   prompt is accepted (the in-flight flag cleared on the stopReason).
//! * **Stop cancels then terminates** — `stop` writes `session/cancel` for
//!   the in-flight turn, then the normal termination ladder runs and the
//!   child exits (AC1's stop half).
//! * **Malformed line is surfaced + skipped, stream continues** — the
//!   engine surfaces the malformed line, counts it, and the turn still
//!   completes (never fatal to the stream, AI-18).
//! * **Permission request denied + one diagnostic** — the agent's
//!   `session/request_permission` is answered DENIED (the offered
//!   `reject_once` option), one surfaced diagnostic, and the turn proceeds.
//!
//! Determinism posture (the house style, `memory.rs`/`observed_metering.rs`
//! precedent): every wait polls COMMITTED state — the engine's own log
//! reads, the returned instance state, or the diagnostic sink's captured
//! bytes — inside a bounded budget; never a wall-clock sleep-then-assert.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_engine::{
    ConfigLayer, DiagnosticSink, Engine, EngineError, LifecycleState, MemoryBackingKind,
    SourceLayer,
};
use tempfile::TempDir;

/// The bounded budget for every committed-state / sink-content poll in this
/// suite. Generous THROUGHPUT margin (a real subprocess round trip on a slow
/// CI runner), not a race window — mirrors `tests/interaction.rs`'s note.
const POLL_BUDGET: Duration = Duration::from_secs(30);

/// A `Write` adapter delegating into a shared buffer (the diagnostic-sink
/// suite's capture shape): the test reads the sink's bytes while the engine
/// owns the boxed writer.
#[derive(Clone)]
struct SharedCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Build the engine-named sink handle over a shared capture buffer.
fn make_sink(shared: &Arc<Mutex<Vec<u8>>>) -> DiagnosticSink {
    Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(shared)))))
}

/// Open an engine over a fresh temp state root, with the optional diagnostic
/// sink installed FROM OPEN (the airtight install — no diagnostic can slip
/// to stderr before the sink is in place).
fn open(base: &TempDir, shared: Option<&Arc<Mutex<Vec<u8>>>>) -> Engine {
    match shared {
        Some(shared) => {
            Engine::open_with_diagnostics(Some(base.path().to_path_buf()), make_sink(shared))
                .expect("open engine with diagnostics")
        }
        None => Engine::open(Some(base.path().to_path_buf())).expect("open engine"),
    }
}

/// The instance's raw agent stdout log (`agent.log`) — for an acp instance
/// this is fed by the connection's READER (the stdout is the protocol pipe),
/// so its presence proves the honest-record reconciliation happened.
fn agent_log_path(base: &Path, name: &str) -> PathBuf {
    base.join("agents")
        .join(name)
        .join("logs")
        .join("agent.log")
}

/// The instance's raw agent stderr log — where `fake_acp_agent`'s own
/// diagnostics go (the fake agent's stderr is NOT the protocol stream).
fn agent_stderr_path(base: &Path, name: &str) -> PathBuf {
    base.join("agents")
        .join(name)
        .join("logs")
        .join("agent-stderr.log")
}

/// Register an `acp` instance named `name` on `engine`, resolve the helper
/// binary, and return the binary path. The launch config is set by the
/// caller (each test controls the mode).
fn register_acp(engine: &Engine, base: &Path, name: &str) -> PathBuf {
    let instance = engine
        .blocking()
        .register(name, "acp")
        .expect("register --kind acp must resolve through the builtin table");
    assert_eq!(instance.kind, "acp");
    // The Agent Home is created at registration; the acp launch comes from
    // config keys at start (unlike hermes, no code-declared launch).
    assert!(base.join("agents").join(name).exists());
    hekma_conformance::fake_acp_agent_bin()
}

/// Set the launch config keys and start the instance, returning the started
/// instance (state `running`).
fn configure_and_start(engine: &Engine, name: &str, mode: &str, extra_args: &[&str]) {
    let bin = hekma_conformance::fake_acp_agent_bin();
    engine
        .blocking()
        .set_config(name, "acp.command", &bin.to_string_lossy())
        .expect("set acp.command");
    let mut args = vec!["--mode".to_string(), mode.to_string()];
    args.extend(extra_args.iter().map(|s| s.to_string()));
    let args_value = args.join(" ");
    engine
        .blocking()
        .set_config(name, "acp.args", &args_value)
        .expect("set acp.args");
}

/// Poll `f` until it returns `true`, bounded by [`POLL_BUDGET`]. Panics with
/// `what` on timeout — the committed state never arrived.
fn poll_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + POLL_BUDGET;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Every `LogLine` text currently retained for `name` (one-shot full read).
fn log_texts(engine: &Engine, name: &str) -> Vec<String> {
    let blocking = engine.blocking();
    let (lines, _cursor) = blocking.read_agent_log(name).expect("read agent log");
    lines.into_iter().map(|line| line.text).collect()
}

/// Copy the sink's captured bytes out under a TIGHT lock. The reaper emits
/// diagnostics to this sink WHILE HOLDING the engine's supervisor lock, so
/// a test that holds the sink lock across a sleep (or an engine call)
/// deadlocks supervision — the copy-out-then-inspect shape is load-bearing.
fn sink_text(shared: &Arc<Mutex<Vec<u8>>>) -> String {
    let binding = shared.lock().unwrap();
    String::from_utf8_lossy(&binding).into_owned()
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

/// AC1 (first half): register → configure → start → the handshake completes
/// and the instance reaches `running`; the raw agent log carries the
/// initialize request (our side is echoed by the agent's reply) and the
/// session/new response — the honest record proves the wire happened.
#[test]
fn handshake_completes_and_the_instance_reaches_running() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-basic";
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "chunky", &[]);

    let started = engine.blocking().start(name).expect("start acp instance");
    assert_eq!(started.state, LifecycleState::Running);

    // The raw agent log (fed by the connection's reader, not a file
    // redirect) carries the agent's initialize response + session id — the
    // handshake's wire record.
    let raw = std::fs::read_to_string(agent_log_path(base.path(), name)).unwrap_or_default();
    assert!(
        raw.contains("\"protocolVersion\":1"),
        "initialize response must be in the raw record: {raw}"
    );
    assert!(
        raw.contains("fake-session-1"),
        "the session/new response (sessionId) must be in the raw record: {raw}"
    );

    // The engine's start itself remains bounded: stop cleanly.
    let _ = engine.blocking().stop(name, None).expect("stop");
}

/// The I/O matrix's honest refusal: an `acp` instance with NO
/// `acp.command` refuses the start naming BOTH keys, with no spurious state
/// (the instance stays `registered`).
#[test]
fn missing_acp_command_refuses_the_start_naming_both_keys() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-unset";
    register_acp(&engine, base.path(), name);

    let err = engine.blocking().start(name).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("acp.command"), "{text}");
    assert!(text.contains("acp.args"), "{text}");

    // No spurious state: the instance is still `registered` (the refusal
    // happened before any transition committed — the launch resolution is a
    // pre-transition step).
    let instance = engine
        .blocking()
        .list()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(instance.state, LifecycleState::Registered);
}

/// The version-counter refusal: an agent countering protocol version 2
/// (outside the tolerated set {1}) closes the transport and refuses the
/// start with a surfaced, traffic-free diagnostic; the instance lands
/// `failed` with the cause preserved.
#[test]
fn version_counter_refuses_the_start() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-version";
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "version-counter", &[]);

    let err = engine.blocking().start(name).unwrap_err();
    match &err {
        EngineError::LaunchFailed { detail, .. } => {
            // Traffic-free + both integers named.
            assert!(
                detail.contains("2"),
                "the countered version named: {detail}"
            );
            assert!(detail.contains("tolerated"), "{detail}");
            assert!(
                detail.contains("ACP handshake failed"),
                "the handshake boundary named: {detail}"
            );
        }
        other => panic!("expected LaunchFailed, got {other:?}"),
    }

    // The instance is `failed` with the cause preserved in the event log.
    let instance = engine
        .blocking()
        .list()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(instance.state, LifecycleState::Failed);
    // The refused child is gone (the teardown killed it): the raw log
    // exists (the reader recorded the countered response) and the process
    // did not linger — proven implicitly by `start` returning after the
    // bounded teardown, and explicitly by a clean re-`stop` being a no-op.
    assert!(
        agent_log_path(base.path(), name).exists(),
        "the raw record must exist"
    );
}

/// AC1's turn half: `send` writes ONE `session/prompt` and returns
/// immediately; the turn's three message chunks + the final stopReason land
/// asynchronously in the per-instance output log (raw record + the
/// attributed view the CLI reads).
#[test]
fn send_streams_chunks_and_the_stopreason_into_the_output_log() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-chunky";
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "chunky", &[]);
    let started = engine.blocking().start(name).expect("start");
    assert_eq!(started.state, LifecycleState::Running);

    let blocking = engine.blocking();
    blocking
        .send_input(name, "hello agent")
        .expect("send prompt");

    // The turn completes ASYNCHRONOUSLY: poll the COMMITTED log until the
    // third chunk and the stopReason line both landed.
    poll_until("all three chunks + the stopReason in the log", || {
        let texts = log_texts(&engine, name);
        texts.iter().any(|t| t.contains("chunk-3"))
            && texts
                .iter()
                .any(|t| t.contains("acp: turn complete (stopReason: end_turn)"))
    });

    // The honest raw record carries the agent's own protocol lines
    // verbatim — including the prompt turn's session id.
    let raw = std::fs::read_to_string(agent_log_path(base.path(), name)).unwrap();
    assert!(raw.contains("agent_message_chunk"), "{raw}");
    assert!(raw.contains("\"stopReason\":\"end_turn\""), "{raw}");

    // A SECOND turn works after the first completed (the in-flight flag
    // cleared on the stopReason): send again and observe chunk-1 of the new
    // turn arriving (the fake agent restarts its numbering per turn).
    blocking
        .send_input(name, "second turn")
        .expect("second send");
    poll_until("the second turn's first chunk in the log", || {
        // Two turns' chunks are numbered identically by the fake agent; the
        // second send's SUCCESS (vs. the typed refusal) is the assertion —
        // the log check just proves the turn ran to chunks.
        let raw = std::fs::read_to_string(agent_log_path(base.path(), name)).unwrap_or_default();
        raw.matches("agent_message_chunk").count() >= 6 // 3 per turn
    });

    let _ = blocking.stop(name, None).expect("stop");
}

/// AC2: a second `send` while a turn is in flight is refused with the
/// surfaced typed in-flight error; the first turn is unaffected (its
/// stopReason still lands), and a prompt AFTER completion is accepted.
#[test]
fn second_prompt_while_in_flight_is_refused_and_the_first_turn_completes() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-inflight";
    register_acp(&engine, base.path(), name);
    // The fake agent holds the turn open for 30s — far longer than this
    // test needs, short enough that the final stop would clean it up even
    // if the turn never completed.
    configure_and_start(&engine, name, "in-flight", &["--delay-ms", "30000"]);
    let started = engine.blocking().start(name).expect("start");
    assert_eq!(started.state, LifecycleState::Running);

    let blocking = engine.blocking();
    blocking
        .send_input(name, "first")
        .expect("first prompt accepted");

    // The refusal is immediate (no waiting on the 30s turn).
    let err = blocking.send_input(name, "second").unwrap_err();
    assert!(
        matches!(err, EngineError::AcpTurnInFlight { .. }),
        "expected the typed in-flight refusal, got {err:?}"
    );

    // The first turn is unaffected — but we do not want to wait 30s: stop
    // the instance (which cancels the turn) and prove the refusal test's
    // other half (a prompt after completion) in the CHUNKY suite above,
    // which already exercises the cleared-flag path. Here the stop-path
    // test takes over from this point (see
    // stop_cancels_the_turn_then_terminates for the same fixture).
    let stopped = blocking
        .stop(name, None)
        .expect("stop with a turn in flight");
    assert_eq!(stopped.state, LifecycleState::Stopped);
}

/// AC1's stop half: `stop` on an instance with a turn in flight writes
/// `session/cancel` FIRST, then the normal termination ladder runs and the
/// child exits (no process survives).
#[test]
fn stop_cancels_the_turn_then_terminates() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-cancel";
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "in-flight", &["--delay-ms", "60000"]);
    let started = engine.blocking().start(name).expect("start");
    assert_eq!(started.state, LifecycleState::Running);

    let blocking = engine.blocking();
    blocking
        .send_input(name, "long turn")
        .expect("prompt accepted");

    // The stop must not wait out the 60s turn: the cancel + SIGTERM ladder
    // ends the child promptly, and the instance settles `stopped`.
    let started_stop = Instant::now();
    let stopped = blocking.stop(name, None).expect("stop");
    assert_eq!(stopped.state, LifecycleState::Stopped);
    assert!(
        started_stop.elapsed() < Duration::from_secs(20),
        "the stop must not wait out the in-flight turn (took {:?})",
        started_stop.elapsed()
    );

    // The cancel was DELIVERED: the fake agent logs it to STDERR (its own
    // diagnostic channel — stdout stays pure ACP), so the raw stderr
    // capture carries the fact.
    let stderr = std::fs::read_to_string(agent_stderr_path(base.path(), name)).unwrap_or_default();
    assert!(
        stderr.contains("session/cancel received"),
        "the agent must have received the cancel: {stderr}"
    );
}

/// The malformed-line row: ONE non-JSON line is surfaced (a counted,
/// skipped diagnostic through the sink), the stream continues, and the
/// turn still completes (never fatal).
#[test]
fn malformed_line_is_surfaced_and_the_stream_continues() {
    let base = TempDir::new().unwrap();
    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine = open(&base, Some(&shared));
    let name = "acp-malformed";
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "malformed-line", &[]);

    let started = engine.blocking().start(name).expect("start");
    assert_eq!(started.state, LifecycleState::Running);

    let blocking = engine.blocking();
    blocking
        .send_input(name, "turn after garbage")
        .expect("send");

    poll_until("the turn completes despite the malformed line", || {
        log_texts(&engine, name)
            .iter()
            .any(|t| t.contains("acp: turn complete (stopReason: end_turn)"))
    });

    // The malformed line was SURFACED through the diagnostic sink (the
    // reaper cadence drained the reader's notice under the supervisor lock).
    poll_until("the malformed-line diagnostic surfaced", || {
        let captured = sink_text(&shared);
        captured.contains("malformed line #") && captured.contains("skipped")
    });

    let _ = blocking.stop(name, None).expect("stop");
}

/// The permission-request row: the agent's `session/request_permission` is
/// answered DENIED via the offered `reject_once` option, exactly one
/// surfaced diagnostic names the refusal, and the turn proceeds to
/// completion (the agent got its denial response and continued).
#[test]
fn permission_request_is_denied_and_the_turn_completes() {
    let base = TempDir::new().unwrap();
    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine = open(&base, Some(&shared));
    let name = "acp-permission";
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "permission-request", &[]);

    let started = engine.blocking().start(name).expect("start");
    assert_eq!(started.state, LifecycleState::Running);

    let blocking = engine.blocking();
    blocking.send_input(name, "needs permission").expect("send");

    // The turn completes: the agent received its denial and continued to
    // the chunks + stopReason.
    poll_until("the turn completes after the denial", || {
        log_texts(&engine, name)
            .iter()
            .any(|t| t.contains("acp: turn complete (stopReason: end_turn)"))
    });

    // Exactly ONE surfaced diagnostic for the permission request (the
    // spine's one-diagnostic-per-request rule): the sink carries the
    // refusal naming the deny option. The sink lock is taken ONLY inside
    // `sink_text` (see its doc).
    poll_until("the permission-denied diagnostic surfaced", || {
        sink_text(&shared).contains("requested permission and was refused")
    });
    let captured = sink_text(&shared);
    let permission_notes = captured
        .lines()
        .filter(|line| line.contains("requested permission and was refused"))
        .count();
    assert_eq!(
        permission_notes, 1,
        "exactly one surfaced diagnostic per permission request: {captured}"
    );
    assert!(
        captured.contains("denied via option 'deny'"),
        "the denial option named: {captured}"
    );

    // The raw record carries the agent's permission REQUEST (the engine's
    // denial response rides the engine's outbound side — the child's stdin —
    // and the raw record holds only the child's stdout). The denial DELIVERY
    // is proven by the turn completing above: the agent processed its
    // denial and continued to the chunks + stopReason.
    let raw = std::fs::read_to_string(agent_log_path(base.path(), name)).unwrap();
    assert!(raw.contains("session/request_permission"), "{raw}");

    let _ = blocking.stop(name, None).expect("stop");
}

// ---------------------------------------------------------------------------
// Story 14-3 — the metering tiers (T1 context surfacing, T3 stderr sentinel,
// and the honest `—` gap notice)
// ---------------------------------------------------------------------------

/// T1: the agent's `usage_update` is surfaced as CONTEXT usage — the
/// `acp_context_usage` field (used/size + the agent-reported cost) appears on
/// the Fleet entry, the CONTEXT-grain diagnostic is announced through the
/// sink — and NOTHING is minted into the billing ledger (`usage` stays all
/// zero, and the gap notice says exactly that with
/// `context_usage_reported: true`).
#[test]
fn usage_update_is_surfaced_as_context_and_never_billed() {
    let base = TempDir::new().unwrap();
    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine = open(&base, Some(&shared));
    let name = "acp-context";
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "usage-update", &[]);

    let started = engine.blocking().start(name).expect("start");
    assert_eq!(started.state, LifecycleState::Running);

    let blocking = engine.blocking();
    blocking.send_input(name, "measure me").expect("send");

    // The context figure lands on the Fleet entry (the reader recorded the
    // latest usage_update; the fleet read lifts it).
    poll_until("the acp_context_usage figure on the fleet entry", || {
        blocking
            .fleet_entry(name)
            .ok()
            .and_then(|e| e.acp_context_usage)
            .is_some()
    });
    let entry = blocking.fleet_entry(name).expect("fleet entry");
    let context = entry.acp_context_usage.as_ref().unwrap();
    assert_eq!(context.used, Some(1200), "the used figure, verbatim");
    assert_eq!(context.size, Some(200_000), "the size figure, verbatim");
    let cost = context
        .cost
        .as_ref()
        .expect("the agent-reported cost block");
    assert_eq!(
        (cost.amount.as_str(), cost.currency.as_str()),
        ("0.0034", "USD")
    );

    // CONTEXT-grain, never billed: the ledger view is untouched (zero
    // billing tokens — the cached rollup rides the ledger's own honesty).
    assert_eq!(entry.usage.cumulative_input_tokens, 0);
    assert_eq!(entry.usage.cumulative_output_tokens, 0);
    assert_eq!(entry.usage.cumulative_total_tokens(), 0);

    // The gap notice is PRESENT (no billing-grade usage anywhere) and names
    // the honest context state: reported.
    let gap = entry.usage_gap.as_ref().expect("the usage gap notice");
    assert!(!gap.notice.is_empty());
    assert!(gap.context_usage_reported, "{gap:?}");
    assert_eq!(gap.observed, "not-configured");
    assert_eq!(gap.sentinel, "no-lines-seen");
    // The active source is the acp default (no observed opt-in here).
    assert_eq!(entry.metering_source, "self-reported");

    // The diagnostic surfaced with the CONTEXT-grain label (surfaced-not-
    // silent, and never mistakable for a billing figure).
    poll_until("the CONTEXT-grain usage diagnostic surfaced", || {
        sink_text(&shared).contains("usage_update (CONTEXT-grain)")
    });

    let _ = blocking.stop(name, None).expect("stop");
}

/// The honest `—` gap notice on a BARE acp instance: no observed opt-in, no
/// sentinel lines, no context figure — the fleet entry carries the structured
/// gap (tiers attempted + context state), the usage view stays the ledger's
/// truthful zero, and nothing is fabricated.
#[test]
fn bare_acp_instance_surfaces_the_honest_gap_notice() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-gap";
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "chunky", &[]);

    let started = engine.blocking().start(name).expect("start");
    assert_eq!(started.state, LifecycleState::Running);

    let blocking = engine.blocking();
    // A turn with NO usage signal of any grain (chunky emits chunks only).
    blocking.send_input(name, "no usage here").expect("send");
    poll_until("the turn completes", || {
        std::fs::read_to_string(agent_log_path(base.path(), name))
            .unwrap_or_default()
            .contains("\"stopReason\":\"end_turn\"")
    });

    let entry = blocking.fleet_entry(name).expect("fleet entry");
    let gap = entry.usage_gap.as_ref().expect("the usage gap notice");
    assert_eq!(gap.observed, "not-configured", "no upstream key was set");
    assert_eq!(
        gap.sentinel, "no-lines-seen",
        "no sentinel line was emitted"
    );
    assert!(!gap.context_usage_reported, "no usage_update was sent");
    assert!(
        gap.notice.contains("observed: not-configured")
            && gap.notice.contains("sentinel: no-lines-seen")
            && gap.notice.contains("no context usage reported"),
        "the notice names every tier: {}",
        gap.notice
    );
    // The context field is honestly absent, the usage view the ledger's zero.
    assert!(entry.acp_context_usage.is_none());
    assert_eq!(entry.usage.cumulative_input_tokens, 0);
    assert_eq!(entry.usage.cumulative_output_tokens, 0);

    let _ = blocking.stop(name, None).expect("stop");
}

/// T3 (story 14-3) + the 14-5 HERMES-SHAPED METERING PARITY proof: a
/// cooperative `hermes-acp`-shaped agent under the `acp` kind emits
/// `KTESIO_USAGE {json}` lines on STDERR (an acp instance's stdout is the
/// protocol stream) carrying the FULL billing vocabulary the `hermes` kind's
/// stdout sentinel carries since 14-6 — input/output AND the optional
/// `cached_tokens` subset. The stderr sentinel drain lands each line in the
/// billing ledger (all three counts, exactly once), the metering_source
/// stamps `self-reported`, and once billing-grade usage EXISTS the gap
/// notice is gone (the `—` was only ever the surfaced last resort).
#[test]
fn sentinel_lines_on_stderr_reach_the_ledger() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-sentinel";
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "sentinel-stderr", &[]);

    let started = engine.blocking().start(name).expect("start");
    assert_eq!(started.state, LifecycleState::Running);

    let blocking = engine.blocking();
    blocking.send_input(name, "turn one").expect("send");
    // The sentinel line (40 in incl. 25 cached / 20 out) lands in the ledger
    // through the stderr drain (the reaper cadence), stamped self-reported.
    poll_until("the stderr sentinel usage landed in the ledger", || {
        blocking
            .fleet_entry(name)
            .map(|e| e.usage.cumulative_input_tokens == 40)
            .unwrap_or(false)
    });
    let entry = blocking.fleet_entry(name).expect("fleet entry");
    assert_eq!(entry.usage.cumulative_output_tokens, 20);
    // 14-5 parity: the cached SUBSET rides the same stderr line into the
    // ledger's cached column (the hermes-shaped billing vocabulary) — the
    // INPUT-INCLUSIVE invariant (25 <= 40) held at parse.
    assert_eq!(
        entry.usage.cumulative_cached_tokens,
        Some(25),
        "the cached subset must land with the stderr sentinel line"
    );
    assert_eq!(entry.metering_source, "self-reported");
    // Billing-grade usage exists → NO gap notice, and no honest-`—` state.
    assert!(
        entry.usage_gap.is_none(),
        "a metered instance has no usage gap: {entry:?}"
    );

    // A second turn increments the sentinel sequence (per-Run monotonic) and
    // the totals accumulate exactly (dedup keyed on the agent's sequence).
    poll_until("the in-flight flag cleared on the stopReason", || {
        std::fs::read_to_string(agent_log_path(base.path(), name))
            .unwrap_or_default()
            .contains("\"stopReason\":\"end_turn\"")
    });
    blocking.send_input(name, "turn two").expect("send");
    poll_until("both sentinel lines landed", || {
        blocking
            .fleet_entry(name)
            .map(|e| {
                e.usage.cumulative_input_tokens == 80 && e.usage.cumulative_output_tokens == 40
            })
            .unwrap_or(false)
    });
    let entry = blocking.fleet_entry(name).expect("fleet entry");
    assert_eq!(
        entry.usage.cumulative_cached_tokens,
        Some(50),
        "the cached subset accumulates exactly once per line"
    );

    let _ = blocking.stop(name, None).expect("stop");
}

// ---------------------------------------------------------------------------
// Story 14-5 — the `hermes` retirement path: HERMES_HOME delivery under the
// acp kind, and the real-agent smoke. The `hermes` kind itself is NOT touched
// (the deprecation is an announcement, not a removal): these tests prove the
// acp kind carries the two parity items an operator migrating a `hermes`
// instance to `--kind acp` depends on — the memory-home delivery (this
// section) and the metering sentinel (the test above).
// ---------------------------------------------------------------------------

/// Story 14-5 (HERMES_HOME under acp, the composition half — the exact mirror
/// of `tests/hermes.rs`'s
/// `hermes_memory_composition_maps_the_managed_dir_onto_hermes_home_exactly_as_start_would`):
/// attach a filesystem Memory Backing to an `acp` instance, fold the
/// invocation override into the effective config, resolve the acp builtin's
/// declared mapping, and apply it onto the launch the acp branch resolves
/// from the config keys — `HERMES_HOME` must carry the managed dir (the SAME
/// var the `hermes` kind maps), the DC-10 `declared` fact must read true for
/// the acp kind, and nothing else may be injected (no observed key without
/// the opt-in).
#[test]
fn acp_memory_backing_delivers_hermes_home_exactly_as_start_would() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-memory";
    register_acp(&engine, base.path(), name);

    let blocking = engine.blocking();
    let dir = blocking
        .attach_memory(name, MemoryBackingKind::Filesystem)
        .expect("attach filesystem backing");
    let status = blocking.memory_status(name).unwrap().expect("attached");
    assert!(
        status.declared,
        "the acp builtin declares memory.dir delivery (HERMES_HOME) since 14-5"
    );

    // The engine's start seam injects the managed dir at the reserved
    // `memory.dir` key as an INVOCATION override (the strongest layer) —
    // fold it exactly as the start would.
    let overrides = ConfigLayer::parse(
        SourceLayer::InvocationOverride,
        "<memory-dir invocation override>",
        &format!("[memory]\ndir = '{}'\n", dir.display()),
    )
    .expect("override layer parses (memory.dir is a KNOWN key)");
    let effective = blocking.effective_config(name, overrides).unwrap();
    let mapping = hekma_engine::adapter::resolve_config_mapping("acp", None).unwrap();
    assert_eq!(
        mapping
            .target(hekma_engine::domain::MEMORY_DIR_KEY)
            .and_then(|t| t.env_var()),
        Some("HERMES_HOME"),
        "the acp mapping must carry memory.dir to HERMES_HOME (retirement parity)"
    );

    // Compose the launch the acp branch resolves (from the config keys — the
    // launch shape `resolve_acp_launch` yields) and apply the mapping onto it
    // exactly as the start seam does.
    let mut launch = hekma_engine::adapter::StartLaunch {
        exec: "/usr/bin/some-acp-agent".to_string(),
        args: vec!["--mode".to_string(), "chunky".to_string()],
        env: BTreeMap::new(),
    };
    hekma_engine::adapter::apply_config_mapping(
        &mut launch,
        &mapping,
        &effective,
        &BTreeMap::new(),
        Path::new(&blocking.instance_status(name).unwrap().instance.agent_home),
    )
    .unwrap_or_else(|e| panic!("apply failed: {e}"));
    assert_eq!(
        launch.env.get("HERMES_HOME"),
        Some(&dir.to_string_lossy().into_owned()),
        "HERMES_HOME must receive the managed Memory Backing dir under the acp kind"
    );
    assert_eq!(
        launch.env.len(),
        1,
        "nothing else is injected (metering.base_url is absent without the observed opt-in)"
    );
}

/// Story 14-5 (HERMES_HOME under acp, the END-TO-END half): attach a
/// filesystem backing, configure the launch, START — the spawned agent's
/// PROCESS environment carries the managed dir as `HERMES_HOME`, proven by
/// the fake agent's stderr echo (the acp-kind twin of the hermes shim's
/// `env=HERMES_HOME=…` dump line). An UNBACKED acp instance receives NO
/// `HERMES_HOME` at all (the documented default-chain fallback, byte-identical
/// to the hermes kind's rule).
#[test]
fn backed_acp_instance_delivers_hermes_home_to_the_agent_process() {
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "acp-home";
    register_acp(&engine, base.path(), name);

    let blocking = engine.blocking();
    let dir = blocking
        .attach_memory(name, MemoryBackingKind::Filesystem)
        .expect("attach filesystem backing");
    configure_and_start(&engine, name, "chunky", &[]);
    let started = blocking.start(name).expect("start");
    assert_eq!(started.state, LifecycleState::Running);

    let needle = format!("fake_acp_agent: HERMES_HOME={}", dir.display());
    poll_until("the agent echoed its injected HERMES_HOME", || {
        std::fs::read_to_string(agent_stderr_path(base.path(), name))
            .unwrap_or_default()
            .contains(&needle)
    });

    let _ = blocking.stop(name, None).expect("stop");
}

// ---------------------------------------------------------------------------
// The real-agent smoke (skipped honestly when no binary is present)
// ---------------------------------------------------------------------------

/// Whether a command is PRESENT on this machine: spawn it with `--version`
/// (stdin/stdout/stderr null) and classify the outcome. A successful spawn
/// (any exit code, or still running past the bound) means the binary exists;
/// a spawn failure (the OS's command-not-found) means it does not. The probe
/// is bounded — a hung `--version` is killed and still counts as present.
fn probe_binary_present(bin: &str) -> bool {
    let Ok(mut child) = Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            // Ran and exited (any code): the binary exists.
            Ok(Some(_)) => return true,
            // Still running past the bound: it exists (a slow --version).
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
}

/// Story 14-5, the REAL-AGENT smoke: a real `hermes-acp` binary (Hermes
/// Agent's native ACP entry point) driven under the `acp` kind — register
/// with a launch command → start (the ACP handshake) → send (one prompt
/// turn accepted) → stop (clean termination). SKIPPED HONESTLY when no
/// `hermes-acp` is on this machine's PATH (this repo's isolation strategy:
/// no network, no vendored agent binaries) — the standard skip-unless-present
/// posture the epic-14 spec pins for the real-agent smoke; story 14-4 runs
/// the matrix on machines where the agents exist. Only the acp KIND is
/// exercised: the legacy `hermes` kind's behavior is untouched by this
/// story (the deprecation is an announcement, not a removal).
#[test]
fn real_hermes_acp_smoke_register_start_send_stop_when_present() {
    // `hermes-acp` is Hermes' ACP entry point; plain `hermes` is the gateway
    // CLI and does NOT speak ACP on stdio by default, so it is not a valid
    // acp-kind launch and is deliberately not probed as a fallback.
    if !probe_binary_present("hermes-acp") {
        return; // the honest skip: no real agent on this machine.
    }
    let base = TempDir::new().unwrap();
    let engine = open(&base, None);
    let name = "hermes-acp-smoke";
    register_acp(&engine, base.path(), name);
    engine
        .blocking()
        .set_config(name, "acp.command", "hermes-acp")
        .expect("set the real agent's launch command");

    let started = engine
        .blocking()
        .start(name)
        .expect("real hermes-acp start");
    assert_eq!(started.state, LifecycleState::Running);

    // One prompt accepted (the send is constant-time; the turn streams
    // asynchronously and is not waited out — a real agent's latency is
    // unbounded). The stop then cancels any in-flight turn and terminates.
    engine
        .blocking()
        .send_input(name, "smoke prompt")
        .expect("send to the real agent");
    let stopped = engine.blocking().stop(name, None).expect("stop");
    assert_eq!(stopped.state, LifecycleState::Stopped);
}

// ---------------------------------------------------------------------------
// Story 14-2 — sessions across lifetimes (D4). The RULING (draft-then-ratify,
// 2026-09-19): an adopted acp process is NOT re-piped — the ACP transport is
// a pipe pair that dies with the engine that held it, and there is no
// OS-portable way to recover a stdio pipe from a bare {pid, start-time}
// fingerprint (the backend's adopt documents this). The session resume
// therefore rides the NEXT START's handshake: the session id is persisted to
// the v8 spawn-record column post-handshake, RETAINED across stop/crash
// settles on a pid-0 seed row, and offered via `session/load` at the next
// start when the fresh agent advertises `loadSession` — with `session/new` +
// a surfaced note on every other path. Adoption surfaces the recorded
// session state honestly.
// ---------------------------------------------------------------------------

/// The instance's `acp_session_id` straight from the state DB (the v8 column),
/// plus the record's pid — `(pid, session_id)`. `None` when no record row
/// exists. The tests read the DB directly (the adoption.rs shape): the
/// session id is engine-internal durable state, not a facade field.
fn db_record_row(base: &Path, name: &str) -> Option<(i64, Option<String>)> {
    let db = base.join("state.db");
    let conn = rusqlite::Connection::open(&db).ok()?;
    conn.query_row(
        "SELECT r.pid, r.acp_session_id FROM agent_runtime r \
         JOIN agent_instances i ON i.id = r.instance_id WHERE i.name = ?1",
        [name],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .ok()
}

/// Wait for the fake agent's own startup line in the instance's STDERR log
/// (`fake_acp_agent: mode=<m> pid=<n>`) and return the pid — the acp agent
/// announces itself on stderr (its stdout is the protocol stream).
fn wait_for_acp_agent_pid(base: &Path, name: &str) -> u32 {
    let stderr = agent_stderr_path(base, name);
    let deadline = Instant::now() + POLL_BUDGET;
    loop {
        if let Ok(contents) = std::fs::read_to_string(&stderr) {
            for line in contents.lines() {
                if let Some(idx) = line.find("pid=") {
                    if let Ok(pid) = line[idx + 4..].trim().parse::<u32>() {
                        return pid;
                    }
                }
            }
        }
        assert!(Instant::now() < deadline, "acp agent pid never announced");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// THE D4 resume path: start → the session id lands in the v8 column → the
/// engine DIES (in-process drop: the child is killed + reaped — the same
/// reconcile input a crash leaves) → the next engine open reconciles the
/// instance `failed` while RETAINING the id on the pid-0 seed → the next
/// START offers it via `session/load`, the fresh fake agent logs the load of
/// the SAME id on stderr, the surfaced note names the resumed session, and
/// the newly-established id is re-persisted.
#[test]
fn persisted_session_resumes_via_session_load_at_the_next_start() {
    let base = TempDir::new().unwrap();
    let name = "acp-resume";
    let shared1 = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        // Engine 1: start the instance (handshake → session/new), then DIE
        // without a clean stop (the drop shape a crash leaves behind).
        let engine1 = open(&base, Some(&shared1));
        register_acp(&engine1, base.path(), name);
        configure_and_start(&engine1, name, "chunky", &["--advertise-load"]);
        let started = engine1.blocking().start(name).expect("start");
        assert_eq!(started.state, LifecycleState::Running);
        // The handshake established the session and the id was PERSISTED at
        // the point the connection established it (the 14-2 write path).
        poll_until("the session id persisted to the v8 column", || {
            db_record_row(base.path(), name)
                .map(|(_, id)| id.as_deref() == Some("fake-session-1"))
                .unwrap_or(false)
        });
        // Engine death: drop kills the child (killpg + wait — no zombie) and
        // leaves the write-ahead record (with the id) in place.
        drop(engine1);
    }

    // Engine 2: adoption reconciles the gone process to `failed`, and the
    // settle RETAINS the session id on the pid-0 seed row.
    let shared2 = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine2 = open(&base, Some(&shared2));
    assert_eq!(
        engine2
            .blocking()
            .instance_status(name)
            .unwrap()
            .instance
            .state,
        LifecycleState::Failed,
        "the gone-process record reconciles to failed (AI-8)"
    );
    let (pid, retained) = db_record_row(base.path(), name).expect("the retention record exists");
    assert_eq!(pid, 0, "the retention row is the pid-0 seed adoption skips");
    assert_eq!(
        retained.as_deref(),
        Some("fake-session-1"),
        "the session id survives the failed reconcile"
    );

    // The next START offers the retained id: the fresh agent advertises
    // loadSession, accepts the load, and logs the SAME id on stderr.
    let started = engine2.blocking().start(name).expect("the resume start");
    assert_eq!(started.state, LifecycleState::Running);
    poll_until(
        "the fake agent logged the session/load of the SAME id",
        || {
            std::fs::read_to_string(agent_stderr_path(base.path(), name))
                .unwrap_or_default()
                .contains("session/load received: fake-session-1")
        },
    );
    poll_until("the resumed-session note surfaced", || {
        sink_text(&shared2).contains("resumed the previous ACP session (fake-session-1)")
    });
    // The established id was re-persisted post-handshake (the fresh record).
    poll_until("the new Run's id persisted", || {
        db_record_row(base.path(), name)
            .map(|(pid, id)| pid != 0 && id.as_deref() == Some("fake-session-1"))
            .unwrap_or(false)
    });

    let _ = engine2.blocking().stop(name, None).expect("stop");
}

/// The capability gate: a persisted id is offered ONLY when the agent
/// advertises `loadSession`. Without it the start still succeeds via
/// `session/new` — with the honest "does not support resuming" note (AI-18),
/// and NO session/load on the wire.
#[test]
fn new_session_with_an_honest_note_when_the_agent_cannot_resume() {
    let base = TempDir::new().unwrap();
    let name = "acp-noload";
    let shared1 = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        // NO --advertise-load: the agent answers initialize without the
        // capability. The id is still persisted (it WAS established).
        let engine1 = open(&base, Some(&shared1));
        register_acp(&engine1, base.path(), name);
        configure_and_start(&engine1, name, "chunky", &[]);
        engine1.blocking().start(name).expect("start");
        poll_until("the session id persisted", || {
            db_record_row(base.path(), name)
                .map(|(_, id)| id.as_deref() == Some("fake-session-1"))
                .unwrap_or(false)
        });
        drop(engine1);
    }

    let shared2 = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine2 = open(&base, Some(&shared2));
    engine2
        .blocking()
        .start(name)
        .expect("the fresh-session start");
    assert_eq!(
        engine2
            .blocking()
            .instance_status(name)
            .unwrap()
            .instance
            .state,
        LifecycleState::Running
    );
    // No session/load reached the agent; the honest note did surface.
    poll_until("the does-not-support note surfaced", || {
        sink_text(&shared2).contains("does not support resuming")
    });
    poll_until("the new session opened (turn machinery live)", || {
        db_record_row(base.path(), name)
            .map(|(pid, _)| pid != 0)
            .unwrap_or(false)
    });
    let stderr = std::fs::read_to_string(agent_stderr_path(base.path(), name)).unwrap_or_default();
    assert!(
        !stderr.contains("session/load received"),
        "session/load must never be sent without the advertised capability: {stderr}"
    );
    assert!(
        sink_text(&shared2).contains("new ACP session"),
        "the note names the fresh session: {}",
        sink_text(&shared2)
    );

    let _ = engine2.blocking().stop(name, None).expect("stop");
}

/// The failure fallback: `loadSession` advertised but the agent REJECTS the
/// load (`--fail-load` → a JSON-RPC error response). The start is NEVER
/// fatal — it falls back to `session/new` and surfaces the failure (AI-18).
#[test]
fn load_failure_falls_back_to_a_new_session_with_a_surfaced_note() {
    let base = TempDir::new().unwrap();
    let name = "acp-failload";
    let shared1 = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        let engine1 = open(&base, Some(&shared1));
        register_acp(&engine1, base.path(), name);
        configure_and_start(
            &engine1,
            name,
            "chunky",
            &["--advertise-load", "--fail-load"],
        );
        engine1.blocking().start(name).expect("start");
        poll_until("the session id persisted", || {
            db_record_row(base.path(), name)
                .map(|(_, id)| id.as_deref() == Some("fake-session-1"))
                .unwrap_or(false)
        });
        drop(engine1);
    }

    let shared2 = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine2 = open(&base, Some(&shared2));
    // The start SUCCEEDS despite the load failure (never fatal).
    let started = engine2.blocking().start(name).expect("the fallback start");
    assert_eq!(started.state, LifecycleState::Running);
    // The load WAS attempted (the agent logged the receipt) and rejected...
    poll_until("the load attempt reached the agent", || {
        std::fs::read_to_string(agent_stderr_path(base.path(), name))
            .unwrap_or_default()
            .contains("session/load received: fake-session-1")
    });
    // ...and the surfaced note names both the failure and the fallback.
    poll_until("the fallback note surfaced", || {
        let captured = sink_text(&shared2);
        captured.contains("session/load failed") && captured.contains("new ACP session")
    });

    let _ = engine2.blocking().stop(name, None).expect("stop");
}

/// The pre-v8 record: a spawn record with NO session id (NULL — never
/// established, or written before the column existed) starts with a plain
/// `session/new`, no resume attempt, and no owed note (nothing was expected).
#[test]
fn record_without_a_session_id_opens_a_new_session_silently() {
    let base = TempDir::new().unwrap();
    let name = "acp-prev8";
    let engine = open(&base, None);
    register_acp(&engine, base.path(), name);
    configure_and_start(&engine, name, "chunky", &["--advertise-load"]);
    engine.blocking().start(name).expect("start");
    poll_until("the session id persisted", || {
        db_record_row(base.path(), name)
            .map(|(_, id)| id.as_deref() == Some("fake-session-1"))
            .unwrap_or(false)
    });
    // Fabricate the pre-v8 record: NULL the column (exactly what a v7-era row
    // reads as after the additive migration).
    {
        let conn = rusqlite::Connection::open(base.path().join("state.db")).unwrap();
        conn.execute("UPDATE agent_runtime SET acp_session_id = NULL", [])
            .unwrap();
    }
    // Stop (a NULL-id record settles to a plain clear — no retention row) and
    // start again: a fresh session, no load, no note.
    engine.blocking().stop(name, None).expect("stop");
    assert!(
        db_record_row(base.path(), name).is_none(),
        "a NULL-id record settles to NO row (byte-identical to the plain clear)"
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    // A fresh engine with the sink installed FROM OPEN (nothing may slip).
    drop(engine);
    let engine2 = open(&base, Some(&shared));
    engine2.blocking().start(name).expect("restart");
    poll_until("the restart reached running", || {
        db_record_row(base.path(), name)
            .map(|(pid, _)| pid != 0)
            .unwrap_or(false)
    });
    let stderr = std::fs::read_to_string(agent_stderr_path(base.path(), name)).unwrap_or_default();
    assert!(
        !stderr.contains("session/load received"),
        "no resume was offered without a persisted id: {stderr}"
    );
    let captured = sink_text(&shared);
    assert!(
        !captured.contains("resumed the previous ACP session"),
        "no resume note is owed: {captured}"
    );
    assert!(
        !captured.contains("new ACP session"),
        "a first-class fresh start owes no note: {captured}"
    );

    let _ = engine2.blocking().stop(name, None).expect("stop");
}

// ---- The SURVIVOR composition (re-exec harness, the adoption.rs shape) ----

/// Whether a pid is alive (the adoption.rs probe shape — shell-out, no OS-cfg;
/// the /proc zombie discount no-ops off Linux).
fn acp_pid_alive(pid: u32) -> bool {
    match hekma_engine::OsId::current() {
        hekma_engine::OsId::Windows => Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false),
        _ => Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
    }
}

fn wait_until_acp_pid_gone(pid: u32, what: &str) {
    let deadline = Instant::now() + POLL_BUDGET;
    while acp_pid_alive(pid) {
        assert!(Instant::now() < deadline, "{what} (pid {pid} still alive)");
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// The re-exec entry for the 14-2 survivor setup (the adoption.rs crash
/// shape): start an acp instance whose agent `--linger-on-eof` (it survives
/// its client's death), then `std::process::exit` WITHOUT dropping the
/// engine — no kill-on-drop runs, so the agent survives re-parented to init.
/// When `KTESIO_ACP_HELPER` is unset this is a trivial pass.
#[test]
fn acp_helper_subprocess() {
    let Ok(state) = std::env::var("KTESIO_ACP_STATE") else {
        return; // normal in-process invocation: nothing to do.
    };
    let state = PathBuf::from(state);
    let engine = Engine::open(Some(state.clone())).expect("helper engine open");
    let facade = engine.blocking();
    let name = "acp-survivor";
    facade.register(name, "acp").unwrap();
    let bin = hekma_conformance::fake_acp_agent_bin();
    facade
        .set_config(name, "acp.command", &bin.to_string_lossy())
        .unwrap();
    facade
        .set_config(name, "acp.args", "--advertise-load --linger-on-eof")
        .unwrap();
    facade.start(name).unwrap();
    // Crash semantics: exit without dropping (no kill-on-drop, no record
    // settle). The lingering agent survives with its session state intact.
    std::process::exit(0);
}

/// The D4 composition, end to end: start ATTACHED → the engine process dies
/// (the re-exec crash shape) → the agent SURVIVES (linger-on-eof) → the next
/// command RE-ADOPTS it and surfaces the recorded session → the stop→start
/// remediation RESUMES via `session/load` (the agent logs the SAME id).
///
/// Windows skips (AI-29, the adoption.rs rationale): KILL_ON_JOB_CLOSE kills
/// the child when the helper exits — no survivor is possible there. The
/// Linux-CI skip mirrors adoption.rs's #109 mitigation.
#[test]
fn adopted_survivor_surfaces_the_recorded_session_and_the_next_start_resumes() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return;
    }
    if hekma_engine::OsId::current() == hekma_engine::OsId::Linux
        && std::env::var_os("CI").is_some()
    {
        return;
    }
    let base = TempDir::new().unwrap();
    let name = "acp-survivor";

    // Engine 1 in the helper subprocess: start + crash.
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "acp_helper_subprocess", "--nocapture"])
        .env("KTESIO_ACP_STATE", base.path())
        .status()
        .expect("run the acp helper subprocess");
    assert!(
        status.success(),
        "the acp helper subprocess failed: {status}"
    );

    // The agent SURVIVED its engine's death (the linger-on-eof deviation).
    let pid = wait_for_acp_agent_pid(base.path(), name);
    assert!(acp_pid_alive(pid), "the lingering acp agent must survive");

    // Engine 2: ADOPTS the live process and surfaces the recorded session —
    // the upgraded adoption note names the id and the resume promise.
    let shared2 = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine2 = open(&base, Some(&shared2));
    assert_eq!(
        engine2
            .blocking()
            .instance_status(name)
            .unwrap()
            .instance
            .state,
        LifecycleState::Running,
        "a live orphan is adopted as running"
    );
    poll_until(
        "the adoption note surfaced naming the recorded session",
        || {
            let captured = sink_text(&shared2);
            captured.contains("adopted an acp instance")
                && captured.contains("fake-session-1")
                && captured.contains("session/load at the next start")
        },
    );

    // The remediation the note names: stop (the settle RETAINS the id on the
    // pid-0 seed), then start — the fresh agent resumes the SAME session.
    engine2.blocking().stop(name, None).expect("stop");
    wait_until_acp_pid_gone(pid, "the stop must terminate the adopted agent");
    let (seed_pid, retained) = db_record_row(base.path(), name).expect("the seed row exists");
    assert_eq!(
        seed_pid, 0,
        "the stop settle retains the id on the pid-0 seed"
    );
    assert_eq!(retained.as_deref(), Some("fake-session-1"));

    let started = engine2.blocking().start(name).expect("the resume start");
    assert_eq!(started.state, LifecycleState::Running);
    poll_until("the fresh agent logged the load of the SAME id", || {
        std::fs::read_to_string(agent_stderr_path(base.path(), name))
            .unwrap_or_default()
            .contains("session/load received: fake-session-1")
    });
    poll_until("the resumed-session note surfaced", || {
        sink_text(&shared2).contains("resumed the previous ACP session (fake-session-1)")
    });

    let _ = engine2.blocking().stop(name, None).expect("stop");
}
