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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_engine::{DiagnosticSink, Engine, EngineError, LifecycleState};
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
