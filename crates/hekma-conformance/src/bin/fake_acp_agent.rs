//! `fake_acp_agent` — a tiny, scriptable ACP v1 agent for the transport
//! tests (story 14-1, spine AD-19; story 14-4 extends it).
//!
//! A DEV/TEST artifact: the engine's `acp_lifecycle` integration tests point
//! an instance's `acp.command` at this binary, so the supervisor drives a
//! REAL subprocess through the JSON-RPC-over-stdio handshake, prompt turns,
//! permission denials, and the stop ladder. It is pure `std` with NO
//! OS-conditional code (the OS-cfg CI gate applies to `hekma-conformance`
//! too), and its STDOUT IS PURE ACP (ndJSON, one message per line) — every
//! diagnostic the binary itself needs goes to stderr, never stdout.
//!
//! ## Protocol behavior (all modes)
//!
//! On startup the agent waits for the client's `initialize` request and
//! echoes `protocolVersion` 1 (the tolerated set) with default agent
//! capabilities (`loadSession` advertised only when `--advertise-load` is
//! passed — the flag story 14-2 will exercise). It answers `session/new`
//! with a fixed `sessionId` (`fake-session-1`). It then loops reading
//! ndJSON requests from stdin until EOF, responding to `session/prompt`.
//!
//! ## Prompt behavior (selected by `--mode <mode>`)
//!
//! * `chunky` (DEFAULT) — for each `session/prompt`, emit 3
//!   `session/update` notifications (`agent_message_chunk` with a numbered
//!   text chunk), then the prompt response `{"stopReason": "end_turn"}`.
//!   This is the happy-path transport proof.
//! * `version-counter` — the `initialize` response counters with
//!   `protocolVersion` 2 (outside the tolerated set {1}), then exits: the
//!   engine must refuse the start with a surfaced, traffic-free refusal.
//! * `malformed-line` — after the initialize response, emit ONE non-JSON
//!   line on stdout, then behave exactly like `chunky`: the engine must
//!   surface + skip the malformed line and the stream must continue (the
//!   turn still completes).
//! * `in-flight` — like `chunky`, but DELAYS the prompt response by
//!   `--delay-ms` (default 5000): the turn stays in flight long enough for
//!   a test to attempt a second prompt (the typed in-flight refusal) or a
//!   stop (the `session/cancel` + termination ladder).
//! * `permission-request` — for each `session/prompt`, first send the
//!   client a `session/request_permission` REQUEST (options include a
//!   `reject_once` denial) and read the response; then emit the chunks +
//!   `end_turn`. The engine must answer DENIED and surface one diagnostic;
//!   the turn proceeds to completion so the test can observe both facts.
//!
//! Coverage note: like `fake_agent`, this binary only ever runs as a
//! SPAWNED SUBPROCESS (a tarpaulin-instrumented parent can never record its
//! lines), so it is excluded from coverage — its behavior is proven by the
//! spawning tests.

use std::io::{BufRead, Write};
use std::time::Duration;

/// The fixed session id every `session/new` response carries.
const SESSION_ID: &str = "fake-session-1";

/// The ACP protocol version the agent speaks (the tolerated set {1});
/// `version-counter` counters with this instead.
const PROTOCOL_VERSION: i64 = 1;
const COUNTERED_VERSION: i64 = 2;

/// One ndJSON line to stdout, flushed (the agent's half of the framing
/// purity: one message, one line, always flushed so the engine's reader
/// sees it immediately).
#[cfg(not(tarpaulin_include))]
fn emit(value: &serde_json::Value) {
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
}

/// One ndJSON REQUEST/RESPONSE/NOTIFICATION builder — thin `json!` wrappers
/// keep every call site one line.
#[cfg(not(tarpaulin_include))]
fn response(id: u64, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

#[cfg(not(tarpaulin_include))]
fn notification(method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

#[cfg(not(tarpaulin_include))]
fn request(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// The `session/update` notification for one numbered message chunk.
#[cfg(not(tarpaulin_include))]
fn message_chunk(n: usize) -> serde_json::Value {
    notification(
        "session/update",
        serde_json::json!({
            "sessionId": SESSION_ID,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": format!("chunk-{n}") },
            },
        }),
    )
}

/// Emit the chunky turn body: three message chunks then `end_turn`.
#[cfg(not(tarpaulin_include))]
fn emit_chunks() {
    for n in 1..=3 {
        emit(&message_chunk(n));
    }
}

#[cfg(not(tarpaulin_include))]
fn main() {
    use std::sync::mpsc;

    // Parse the script: `--mode <mode>` and `--delay-ms <ms>`.
    let mut mode = "chunky".to_string();
    let mut delay = Duration::from_secs(5);
    let mut advertise_load = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => {
                if let Some(m) = args.next() {
                    mode = m;
                }
            }
            "--delay-ms" => {
                if let Some(ms) = args.next().and_then(|s| s.parse::<u64>().ok()) {
                    delay = Duration::from_millis(ms);
                }
            }
            "--advertise-load" => advertise_load = true,
            other => {
                // Unknown args are ignored (a manifest may pass extra tokens).
                let _ = other;
            }
        }
    }
    eprintln!("fake_acp_agent: mode={mode} pid={}", std::process::id());

    // A READER THREAD feeds parsed inbound messages to the main loop, so a
    // `session/cancel` is observed WHILE a turn's delay is running (a
    // single-threaded read loop would never see the cancel until the turn
    // finished — the exact hang the stop test would exercise).
    enum Inbound {
        Request { id: u64, method: String },
        Response { id: u64 },
        Eof,
    }
    let (tx, rx) = mpsc::channel::<Inbound>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = std::io::BufReader::new(stdin.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(Inbound::Eof);
                    break;
                }
                Ok(_) => {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim_end())
                    else {
                        continue;
                    };
                    if value.get("method").and_then(|m| m.as_str()) == Some("session/cancel") {
                        // An ACP agent aborts its in-flight turn on cancel.
                        // Handled HERE (the reader thread wakes the instant
                        // the line arrives) so the receipt is observable
                        // before the client's termination ladder escalates;
                        // stderr (NOT stdout — the protocol stream stays
                        // pure) notes it for the stop test. The process does
                        // NOT self-exit: the client's stop ladder owns the
                        // termination (a self-exit here would race its
                        // SIGTERM with a not-yet-reaped zombie — EPERM on
                        // macOS).
                        eprintln!("fake_acp_agent: session/cancel received");
                        continue;
                    }
                    let id = value.get("id").and_then(|v| v.as_u64());
                    let method = value
                        .get("method")
                        .and_then(|m| m.as_str())
                        .map(str::to_string);
                    match (method, id) {
                        (Some(method), Some(id)) => {
                            let _ = tx.send(Inbound::Request { id, method });
                        }
                        (None, Some(id)) => {
                            // A response to one of OUR requests (the
                            // permission denial reply).
                            let _ = tx.send(Inbound::Response { id });
                        }
                        _ => {}
                    }
                }
            }
        }
    });

    let mut next_agent_request_id: u64 = 1000;

    while let Ok(message) = rx.recv() {
        let Inbound::Request { id, method } = message else {
            // Eof (or anything unaddressed): the client is gone — exit.
            break;
        };
        match (method.as_str(), id) {
            ("initialize", _) => {
                // Echo (or counter) the protocol version + the agent caps.
                let version = if mode == "version-counter" {
                    COUNTERED_VERSION
                } else {
                    PROTOCOL_VERSION
                };
                let mut agent_capabilities = serde_json::Map::new();
                if advertise_load {
                    agent_capabilities.insert("loadSession".to_string(), true.into());
                }
                emit(&response(
                    id,
                    serde_json::json!({
                        "protocolVersion": version,
                        "agentCapabilities": agent_capabilities,
                    }),
                ));
                if mode == "version-counter" {
                    // The engine will close the connection; exit promptly so
                    // the refusal test does not wait on a lingering child.
                    break;
                }
                if mode == "malformed-line" {
                    // ONE non-JSON line on stdout, then the normal protocol
                    // continues — the surfaced-and-skipped malformed line.
                    let mut stdout = std::io::stdout();
                    let _ = writeln!(stdout, "this is not json at all <EOF?>");
                    let _ = stdout.flush();
                }
            }
            ("session/new", _) => {
                emit(&response(
                    id,
                    serde_json::json!({ "sessionId": SESSION_ID, "mcpServers": [] }),
                ));
            }
            ("session/prompt", _) => {
                if mode == "permission-request" {
                    // Ask the CLIENT for permission (an agent→client request,
                    // method + id), then WAIT for the response before
                    // proceeding. The engine must answer denied (the
                    // reject_once option).
                    next_agent_request_id += 1;
                    let request_id = next_agent_request_id;
                    emit(&request(
                        request_id,
                        "session/request_permission",
                        serde_json::json!({
                            "sessionId": SESSION_ID,
                            "options": [
                                { "kind": "allow_once", "name": "Allow", "id": "allow" },
                                { "kind": "reject_once", "name": "Deny", "id": "deny" },
                            ],
                        }),
                    ));
                    // Bounded wait for the matching client response.
                    let mut answered = false;
                    let deadline = std::time::Instant::now() + Duration::from_secs(30);
                    while !answered {
                        match rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(Inbound::Response { id: reply_id }) if reply_id == request_id => {
                                answered = true;
                            }
                            Ok(Inbound::Eof) => return,
                            Ok(_) => continue,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if std::time::Instant::now() >= deadline {
                                    eprintln!(
                                        "fake_acp_agent: timed out waiting for the \
                                         permission response"
                                    );
                                    break;
                                }
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    if !answered {
                        eprintln!("fake_acp_agent: did not receive a matching permission response");
                    }
                }
                if mode == "in-flight" {
                    // Keep the turn in flight for the delay so a test can
                    // attempt a second prompt or a stop against it. A
                    // `session/cancel` that arrives mid-delay is handled by
                    // the reader thread above (abort + exit) — exactly the
                    // cooperative-abort shape a real ACP agent has.
                    std::thread::sleep(delay);
                }
                emit_chunks();
                emit(&response(
                    id,
                    serde_json::json!({ "stopReason": "end_turn" }),
                ));
            }
            _ => {
                // Unknown request from the client: nothing to answer here —
                // the fake agent only implements the pinned surface.
            }
        }
    }
}
