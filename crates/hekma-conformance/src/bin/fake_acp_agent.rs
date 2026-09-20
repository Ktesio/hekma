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
//! ## Story 14-3 modes (the metering tiers)
//!
//! * `usage-update` — for each `session/prompt`, first emit ONE
//!   `session/update` carrying a `usage_update` (CONTEXT-grain:
//!   `used`/`size` + the optional `cost` block) before the chunks. The
//!   engine must surface it as CONTEXT usage (diagnostic + the
//!   `acp_context_usage` show/fleet field) and mint NOTHING into the
//!   billing ledger.
//! * `sentinel-stderr` — for each `session/prompt`, write ONE
//!   `KTESIO_USAGE {json}` line to STDERR (sequence 0,1,… per turn) before
//!   the chunks. An acp instance's stdout is the protocol stream, so its
//!   self-reported sentinel channel is stderr — the engine's stderr
//!   sentinel drain must land each line in the billing ledger
//!   (input/output tokens).
//! * `observed-call` — for each `session/prompt`, make ONE real
//!   OpenAI-compatible POST to `$OPENAI_BASE_URL/chat/completions` (the
//!   env the engine injects when the instance opts into the observed
//!   channel) and consume the response, then the chunks. The loopback
//!   forward listener must parse the fixed `usage` into the billing
//!   ledger, tagged `engine-observed`.
//!
//! Coverage note: like `fake_agent`, this binary only ever runs as a
//! SPAWNED SUBPROCESS (a tarpaulin-instrumented parent can never record its
//! lines), so it is excluded from coverage — its behavior is proven by the
//! spawning tests.

//! ## Story 14-2 modes (sessions across lifetimes, D4)
//!
//! * `--advertise-load` — the `initialize` response advertises
//!   `agentCapabilities.loadSession: true` (the gate the engine must honor
//!   before ever sending `session/load`).
//! * `session/load` handler (all modes) — answers the engine's
//!   `session/load` with the resumed session id (`fake-session-1`), logging
//!   receipt to STDERR (`fake_acp_agent: session/load received: <id>` — the
//!   observation channel the tests poll via `agent-stderr.log`). With
//!   `--fail-load`, the handler instead answers with a JSON-RPC ERROR (code
//!   -32000, "session not found"), pinning the engine's fallback-to-
//!   `session/new` + surfaced-note path.
//! * `--linger-on-eof` — on stdin EOF the agent does NOT exit (the default:
//!   an ACP agent whose client is gone exits, which is exactly why a REAL
//!   acp child rarely survives its engine); it parks, so the process
//!   SURVIVES the spawning engine's death for the adoption test to re-hold.

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

/// The `usage_update` notification for one turn (story 14-3, T1): the
/// CONTEXT-grain figure — a fixed `used`/`size` plus the optional
/// agent-reported cost block, so the engine's surfacing (diagnostic + the
/// `acp_context_usage` field) is observable with and without the cost.
#[cfg(not(tarpaulin_include))]
fn usage_update() -> serde_json::Value {
    notification(
        "session/update",
        serde_json::json!({
            "sessionId": SESSION_ID,
            "update": {
                "sessionUpdate": "usage_update",
                "used": 1200,
                "size": 200000,
                "cost": { "amount": 0.0034, "currency": "USD" },
            },
        }),
    )
}

/// One `KTESIO_USAGE {json}` line to STDERR (story 14-3, T3): the acp kind's
/// self-reported sentinel channel. Pure `std`, `sequence` stamped by the
/// caller (per-turn monotonic), token counts FIXED so the ledger total is an
/// exact-match assertion (K turns × 40 in / 20 out).
#[cfg(not(tarpaulin_include))]
fn emit_stderr_sentinel(sequence: u64) {
    eprintln!("KTESIO_USAGE {{\"sequence\":{sequence},\"input_tokens\":40,\"output_tokens\":20}}");
}

/// ONE OpenAI-compatible POST to the injected base URL (story 14-3, T2): the
/// observed-call mode's model traffic for the loopback forward listener to
/// intercept. Pure `std` (a blocking `TcpStream`), bounded reads, no
/// dependency; a failed call is noted on stderr and the turn continues (the
/// engine's ledger simply stays empty — the honest gap).
#[cfg(not(tarpaulin_include))]
fn post_one_observed_call(base_url: &str) {
    // Parse `http://127.0.0.1:<port>` into host:port (loopback only in tests).
    let Some(authority) = base_url.strip_prefix("http://") else {
        eprintln!("fake_acp_agent: unsupported base URL shape: {base_url}");
        return;
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (authority, "80"),
    };
    let body = r#"{"model":"fake-acp","messages":[{"role":"user","content":"hi"}]}"#;
    let request = format!(
        "POST /chat/completions HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: \
         application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    let Ok(port) = port.parse::<u16>() else {
        eprintln!("fake_acp_agent: unsupported base URL port: {base_url}");
        return;
    };
    let outcome = std::net::TcpStream::connect((host, port)).and_then(|mut stream| {
        use std::io::Read;
        stream.write_all(request.as_bytes())?;
        let mut response = String::new();
        // Bounded read: the stub answers a fixed body and closes.
        let _ = stream.take(64 * 1024).read_to_string(&mut response);
        Ok(response)
    });
    match outcome {
        Ok(response) if response.contains("chat.completion") || response.contains("chatcmpl") => {}
        Ok(response) => {
            eprintln!(
                "fake_acp_agent: observed call answered unexpectedly ({} bytes)",
                response.len()
            );
        }
        Err(err) => {
            eprintln!("fake_acp_agent: observed call failed: {err}");
        }
    }
}

#[cfg(not(tarpaulin_include))]
fn main() {
    use std::sync::mpsc;

    // Parse the script: `--mode <mode>`, `--delay-ms <ms>`, `--advertise-load`,
    // `--fail-load`, `--linger-on-eof`.
    let mut mode = "chunky".to_string();
    let mut delay = Duration::from_secs(5);
    let mut advertise_load = false;
    let mut fail_load = false;
    let mut linger_on_eof = false;
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
            "--fail-load" => fail_load = true,
            "--linger-on-eof" => linger_on_eof = true,
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
        Request {
            id: u64,
            method: String,
            /// The `params.sessionId`, when the request carried one (the
            /// `session/load` receipt log names the REQUESTED id, story 14-2).
            session_id: Option<String>,
        },
        Response {
            id: u64,
        },
        Cancelled,
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
                    // Story 14-2: `--linger-on-eof` models the cooperative
                    // survivor — the process outlives its client's death
                    // (parking instead of exiting) so the adoption test can
                    // re-hold it and read the persisted session state. The
                    // DEFAULT (exit on EOF) is the real-agent behavior this
                    // deviation is deliberately against.
                    if linger_on_eof {
                        eprintln!("fake_acp_agent: stdin EOF; lingering (survivor mode)");
                        loop {
                            std::thread::sleep(Duration::from_secs(3600));
                        }
                    }
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
                        // The reader thread wakes the instant the line
                        // arrives and hands the cancellation to the main
                        // loop, which ends the turn (`stopReason: cancelled`)
                        // and exits cooperatively — the graceful stage of the
                        // client's stop ladder then reaps an already-exited
                        // child on every OS. (The engine's unix stop maps a
                        // vanished group to success — ESRCH is pinned — and
                        // its reap-first ordering makes the old fear of a
                        // SIGTERM/zombie race unfounded; an earlier draft of
                        // this branch only LOGGED the cancel and never
                        // exited, which left the WINDOWS stop ladder polling
                        // its whole graceful window for an exit that never
                        // came: the stop test's 30s timeout.)
                        eprintln!("fake_acp_agent: session/cancel received");
                        let _ = tx.send(Inbound::Cancelled);
                        continue;
                    }
                    let id = value.get("id").and_then(|v| v.as_u64());
                    let method = value
                        .get("method")
                        .and_then(|m| m.as_str())
                        .map(str::to_string);
                    match (method, id) {
                        (Some(method), Some(id)) => {
                            let session_id = value
                                .get("params")
                                .and_then(|p| p.get("sessionId"))
                                .and_then(|s| s.as_str())
                                .map(str::to_string);
                            let _ = tx.send(Inbound::Request {
                                id,
                                method,
                                session_id,
                            });
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
    // The per-turn counter the sentinel-stderr mode stamps (a per-Run
    // monotonic sequence, the sentinel convention's dedup ordinal).
    let mut next_turn_sequence: u64 = 0;

    while let Ok(message) = rx.recv() {
        let Inbound::Request {
            id,
            method,
            session_id,
        } = message
        else {
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
            ("session/load", _) => {
                // Story 14-2: the resume request. Receipt is logged to STDERR
                // (stdout stays pure ACP) naming the REQUESTED session id, so
                // the tests can pin that the engine resumed THE SAME id it
                // persisted. The answer is the resumed session id — or, under
                // `--fail-load`, a JSON-RPC error (the engine must fall back
                // to `session/new` with a surfaced note, never fatal).
                eprintln!(
                    "fake_acp_agent: session/load received: {}",
                    session_id.as_deref().unwrap_or("<none>")
                );
                if fail_load {
                    let mut stdout = std::io::stdout();
                    let _ = writeln!(
                        stdout,
                        r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32000,"message":"session not found"}}}}"#
                    );
                    let _ = stdout.flush();
                } else {
                    emit(&response(
                        id,
                        serde_json::json!({ "sessionId": SESSION_ID }),
                    ));
                }
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
                    // attempt a second prompt or a stop against it. The wait
                    // is CANCELLABLE: a `session/cancel` arriving mid-delay
                    // (relayed by the reader thread) ends the turn the way a
                    // real ACP agent ends it — the prompt response carries
                    // `stopReason: "cancelled"` — and the process exits
                    // cooperatively, so the client's stop ladder reaps an
                    // already-exited child promptly on every OS.
                    let deadline = std::time::Instant::now() + delay;
                    let mut cancelled = false;
                    while std::time::Instant::now() < deadline {
                        match rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(Inbound::Cancelled) => {
                                cancelled = true;
                                break;
                            }
                            Ok(Inbound::Eof) => return,
                            Ok(_) => continue,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    if cancelled {
                        emit(&response(
                            id,
                            serde_json::json!({ "stopReason": "cancelled" }),
                        ));
                        let _ = std::io::stdout().flush();
                        return;
                    }
                }
                // Story 14-3 tier modes, in turn order before the chunks.
                if mode == "usage-update" {
                    emit(&usage_update());
                }
                if mode == "observed-call" {
                    // The model traffic for the loopback listener to observe;
                    // the base URL arrives via the env the engine injected.
                    if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
                        post_one_observed_call(&base_url);
                    } else {
                        eprintln!(
                            "fake_acp_agent: observed-call mode but OPENAI_BASE_URL is unset"
                        );
                    }
                }
                if mode == "sentinel-stderr" {
                    emit_stderr_sentinel(next_turn_sequence);
                    next_turn_sequence += 1;
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
