//! The Control API's conformance suite (story 17-3): the WHOLE v0 surface
//! driven over REAL HTTP against the conformance fake agents — every route's
//! auth matrix (401 without a valid bearer, handler reached with one), the
//! happy + error mappings, an end-to-end lifecycle through the AD-15 states
//! honoring `window_secs`, the SSE stream + the cursor pull's lossless
//! idempotent continuation, and the no-secret/no-raw-output leak matrix.
//!
//! Hermeticity: each test owns a TempDir state root and resolves the bearer
//! through the SECRETS FILE leg (`<state>/secrets.toml`) — never the
//! process env (parallel tests share one env; a file per state root does not
//! contend). The server runs `serve_with_signal` on a thread with the
//! channel-triggered signal factory (the drain suite's pattern); the bound
//! address (OS-ephemeral `127.0.0.1:0`) is parsed from the host's own
//! "control api listening on" notice — the same line an operator watches.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hekma_conformance::test_support::ManifestFixture;
use hekma_engine::{AdapterRef, Engine};
use ureq::http::StatusCode;

/// The bearer secret NAME every test resolves (the secrets-file leg; the env
/// leg is deliberately left unset).
const BEARER_NAME: &str = "control_api_test_bearer";
/// The bearer token VALUE the tests present.
const BEARER_VALUE: &str = "test-bearer-token-17-3";

/// A sink that records every notice into a shared buffer the test can assert
/// on (the drain suite's shape).
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl SharedSink {
    fn new() -> (Self, hekma_engine::DiagnosticSink) {
        let shared = Self(Arc::new(Mutex::new(Vec::new())));
        let sink: hekma_engine::DiagnosticSink = Arc::new(Mutex::new(Box::new(shared.clone())));
        (shared, sink)
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl std::io::Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Write the state root's secrets file so `env_then_file` resolves
/// [`BEARER_NAME`] WITHOUT touching the process env (hermetic under
/// parallel tests). The Unix chmod is REQUIRED — the resolver refuses a
/// group/other-accessible secrets file (AC6's 0600 rule, correctly firing
/// on a naive 0644 write). The `#[cfg(unix)]` gate is the integration-test
/// allowlist's convention (the AI-35 disclosure; the engine's
/// `atomic_config_writes.rs` shape): on Windows the file-leg ACL check
/// accepts a user-owned fresh file.
fn write_secrets_file(base: &Path) {
    let path = base.join(hekma_engine::paths::SECRETS_FILE);
    std::fs::write(&path, format!("{BEARER_NAME} = \"{BEARER_VALUE}\"\n")).unwrap();
    #[cfg(unix)]
    chmod_600(&path);
}

#[cfg(unix)]
fn chmod_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A running host + its parsed Control-API address. Dropping this does NOT
/// stop the serve thread — call [`ServeHandle::signal`] and [`ServeHandle::join`]
/// for a clean shutdown (the tests' own discipline: leave no thread behind).
struct ServeHandle {
    addr: String,
    signal: Option<tokio::sync::oneshot::Sender<&'static str>>,
    thread: std::thread::JoinHandle<Result<(), hekma_host::ServeError>>,
}

impl ServeHandle {
    /// Fire the shutdown signal and join the serve thread.
    fn stop(mut self) -> Result<(), hekma_host::ServeError> {
        self.signal
            .take()
            .expect("signal sent twice")
            .send("SIGTEST")
            .ok();
        self.thread.join().expect("serve thread panicked")
    }
}

/// Spawn `serve_with_signal` on a thread with the Control API bound to an
/// OS-ephemeral loopback port, wait for the listening notice, and return the
/// parsed address.
fn spawn_serve_with_api(base: PathBuf, sink: SharedSink) -> ServeHandle {
    write_secrets_file(&base);
    let options = hekma_host::ServeOptions {
        state_base: Some(base.clone()),
        control_listen: Some("127.0.0.1:0".parse().unwrap()),
        bearer_secret: Some(BEARER_NAME.to_string()),
        metering_listen_port: 0,
        stop_window: Some(2),
        log_capture: hekma_engine::LogCaptureMode::Full,
        diagnostics: Some(diagnostic_sink_of(&sink)),
        otlp: Default::default(),
        ..Default::default()
    };
    let mut server = spawn_serve(options);
    // Wait for the listening notice, then parse the OS-chosen address —
    // the notice is the address of record an operator (and this suite)
    // reads.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let text = sink.text();
        if let Some(addr) = text
            .lines()
            .find_map(|line| line.split("control api listening on ").nth(1))
            .map(|rest| rest.split(" (").next().unwrap().to_string())
        {
            server.addr = addr;
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the control api never reported listening:\n{}",
            sink.text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    server
}

fn diagnostic_sink_of(sink: &SharedSink) -> hekma_engine::DiagnosticSink {
    // A clone shares the same inner buffer: serve's notices land where the
    // test reads them.
    Arc::new(Mutex::new(Box::new(sink.clone())))
}

/// The common spawn: options + the signal channel. Does NOT wait for any
/// notice — the caller decides what to wait for (`spawn_serve_with_api`
/// waits for the listening notice; an API-off serve has none by design).
fn spawn_serve(options: hekma_host::ServeOptions) -> ServeHandle {
    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<&'static str>();
    let thread = std::thread::spawn(move || {
        hekma_host::serve_with_signal(options, move |_sink| async move {
            match signal_rx.await {
                Ok(name) => Ok(name),
                Err(_) => Err("the test signal source was dropped".to_string()),
            }
        })
    });
    ServeHandle {
        addr: String::new(),
        signal: Some(signal_tx),
        thread,
    }
}

/// An HTTP client for the suite: status-as-error OFF (the matrix asserts
/// 4xx/5xx statuses as VALUES), a per-read body timeout so a malformed SSE
/// expectation fails the test instead of hanging it.
fn client() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_recv_response(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(10)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// Percent-encode a query-parameter value (the JSON cursor contains `{`, `"`,
/// `:` — every byte outside the RFC 3986 unreserved set is escaped).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A GET with the valid bearer.
fn get_ok(client: &ureq::Agent, addr: &str, path: &str) -> ureq::http::Response<ureq::Body> {
    client
        .get(format!("http://{addr}{path}"))
        .header("Authorization", format!("Bearer {BEARER_VALUE}"))
        .call()
        .expect("authenticated request must transport")
}

/// The response body as a JSON value.
fn json_body(response: &mut ureq::http::Response<ureq::Body>) -> serde_json::Value {
    let text = response
        .body_mut()
        .read_to_string()
        .expect("body must read");
    serde_json::from_str(&text).expect("the body must be JSON")
}

// ---------------------------------------------------------------------------
// The auth matrix (the frozen acceptance criterion: EVERY route answers a
// valid bearer and returns 401 without one; a handler never runs for an
// unauthenticated request).
// ---------------------------------------------------------------------------

/// Every v0 route, as (method, path) pairs a raw check can drive. The
/// identity-bearing paths use a name that may not exist — a 404 AFTER auth
/// (not a 401) is the point.
fn every_route(addr: &str) -> Vec<(&'static str, String)> {
    vec![
        ("GET", format!("http://{addr}/v0/version")),
        ("GET", format!("http://{addr}/v0/fleet")),
        ("GET", format!("http://{addr}/v0/events")),
        ("POST", format!("http://{addr}/v0/instances")),
        ("GET", format!("http://{addr}/v0/instances/ghost")),
        ("DELETE", format!("http://{addr}/v0/instances/ghost")),
        ("GET", format!("http://{addr}/v0/instances/ghost/status")),
        (
            "GET",
            format!("http://{addr}/v0/instances/ghost/capabilities"),
        ),
        ("GET", format!("http://{addr}/v0/instances/ghost/config")),
        ("PUT", format!("http://{addr}/v0/instances/ghost/config")),
        ("GET", format!("http://{addr}/v0/instances/ghost/events")),
        ("POST", format!("http://{addr}/v0/instances/ghost/start")),
        ("POST", format!("http://{addr}/v0/instances/ghost/stop")),
        ("POST", format!("http://{addr}/v0/instances/ghost/pause")),
        ("POST", format!("http://{addr}/v0/instances/ghost/resume")),
        // The ephemeral-host export surface (story 17-4) is behind the SAME
        // auth layer — no anonymous route.
        ("GET", format!("http://{addr}/v0/export")),
        ("POST", format!("http://{addr}/v0/export/ack")),
        // Story 21-2: the Run routes. Story 21-5 adds the listing (`GET
        // /v0/runs`) beside the open.
        ("POST", format!("http://{addr}/v0/runs")),
        ("GET", format!("http://{addr}/v0/runs")),
        ("GET", format!("http://{addr}/v0/runs/run_ghost")),
        ("POST", format!("http://{addr}/v0/runs/run_ghost/close")),
        // Story 21-9: the bound-label routes.
        ("GET", format!("http://{addr}/v0/runs/run_ghost/labels")),
        ("PATCH", format!("http://{addr}/v0/runs/run_ghost/labels")),
        // Story 21-6: the Run-grain verbs, Run limits and Selector sets.
        ("POST", format!("http://{addr}/v0/runs/run_ghost/stop")),
        ("POST", format!("http://{addr}/v0/runs/run_ghost/pause")),
        ("POST", format!("http://{addr}/v0/runs/run_ghost/resume")),
        ("POST", format!("http://{addr}/v0/runs/run_ghost/drain")),
        ("GET", format!("http://{addr}/v0/runs/run_ghost/limits")),
        ("PATCH", format!("http://{addr}/v0/runs/run_ghost/limits")),
        ("POST", format!("http://{addr}/v0/runs/select")),
        ("POST", format!("http://{addr}/v0/runs/stop")),
        ("POST", format!("http://{addr}/v0/runs/pause")),
        ("POST", format!("http://{addr}/v0/runs/resume")),
        ("POST", format!("http://{addr}/v0/runs/drain")),
        ("POST", format!("http://{addr}/v0/runs/close")),
        ("PATCH", format!("http://{addr}/v0/runs/labels")),
        ("POST", format!("http://{addr}/v0/runs/limits")),
        // The per-call read surface (the Operator family's newest rows).
        ("GET", format!("http://{addr}/v0/calls")),
        (
            "GET",
            format!("http://{addr}/v0/calls/call_01JZ6Q9G3S8V2X7K4M1N5P0R2T"),
        ),
    ]
}

#[test]
fn every_route_401s_without_a_bearer_and_a_wrong_bearer_and_reaches_its_handler_with_the_right_one()
{
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle.clone());
    let client = client();

    for (method, url) in every_route(&server.addr) {
        // No Authorization header at all.
        let response = match method {
            "GET" => client.get(&url).call().unwrap(),
            "POST" => client.post(&url).send_json("").unwrap(),
            "PUT" => client.put(&url).send_json("").unwrap(),
            "PATCH" => client.patch(&url).send_json("").unwrap(),
            "DELETE" => client.delete(&url).call().unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {url} must refuse an anonymous request"
        );
        // The challenge header is present; the body is the static refusal.
        assert_eq!(
            response
                .headers()
                .get("WWW-Authenticate")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer"),
            "{method} {url} must advertise the scheme"
        );
        // A WRONG bearer is equally refused — and never echoed.
        let wrong = format!("Bearer {BEARER_VALUE}-nope");
        let mut response = match method {
            "GET" => client
                .get(&url)
                .header("Authorization", &wrong)
                .call()
                .unwrap(),
            "POST" => client
                .post(&url)
                .header("Authorization", &wrong)
                .send_json("")
                .unwrap(),
            "PUT" => client
                .put(&url)
                .header("Authorization", &wrong)
                .send_json("")
                .unwrap(),
            "PATCH" => client
                .patch(&url)
                .header("Authorization", &wrong)
                .send_json("")
                .unwrap(),
            "DELETE" => client
                .delete(&url)
                .header("Authorization", &wrong)
                .call()
                .unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {url} must refuse a wrong token"
        );
        let text = response.body_mut().read_to_string().unwrap();
        assert!(
            !text.contains(&wrong),
            "the 401 must never echo the presented token: {text}"
        );
    }

    // With the RIGHT bearer every route reaches its HANDLER: the identity
    // routes answer 200, the ghost routes answer 404 (a HANDLER answer —
    // auth passed, the resource does not exist), and the register POST runs
    // its exactly-one-of + engine validation (an unknown kind → 400).
    let right = format!("Bearer {BEARER_VALUE}");
    for (method, url) in every_route(&server.addr) {
        let mut response = match method {
            "GET" => client
                .get(&url)
                .header("Authorization", &right)
                .call()
                .unwrap(),
            "POST" => client
                .post(&url)
                .header("Authorization", &right)
                .send_json(serde_json::json!({"name": "ghost", "kind": "no-such-kind"}))
                .unwrap(),
            "PUT" => client
                .put(&url)
                .header("Authorization", &right)
                // A Value, NOT a raw &str — a bare str serializes as a JSON
                // STRING (422 at the extractor; the handler would never run
                // and the ghost 404 below would be unreachable).
                .send_json(serde_json::json!({"key": "model", "value": "m"}))
                .unwrap(),
            // A valid label change, so the ghost Run's 404 is the answer.
            "PATCH" => client
                .patch(&url)
                .header("Authorization", &right)
                .send_json(serde_json::json!({"set": {"team": "core"}}))
                .unwrap(),
            "DELETE" => client
                .delete(&url)
                .header("Authorization", &right)
                .call()
                .unwrap(),
            _ => unreachable!(),
        };
        let identity = url.ends_with("/v0/version")
            || url.ends_with("/v0/fleet")
            || url.ends_with("/v0/events")
            // The outbox pull (story 17-4) with a valid bearer answers 200 —
            // an empty/never-written journal is a valid EMPTY batch, not a
            // missing resource.
            // The Run listing (story 21-5) likewise: an empty state
            // directory is a valid EMPTY listing.
            || url.ends_with("/v0/export")
        // The Run listing (story 21-5) is identity-shaped only on its GET —
        // an empty state directory is a valid EMPTY listing. The POST
        // (open) keeps its exactly-one-parent 400 below.
        || (method == "GET" && url.ends_with("/v0/runs"))
        // The calls listing (story 19-4): no calls recorded is a valid EMPTY
        // listing too (the by-id read keeps its 404).
        || (method == "GET" && url.ends_with("/v0/calls"));
        let register = method == "POST" && url.ends_with("/v0/instances");
        // The ack POST reached its handler but this loop's register-shaped
        // body carries no `seq` — the Json extractor's 422 is the
        // handler-reached answer (auth passed, the request shape was wrong).
        let ack_wrong_body = method == "POST" && url.ends_with("/v0/export/ack");
        // Story 21-6: the set routes and the limits PATCH deny unknown
        // fields, so this loop's bodies (no `set_token`; a label change on
        // a limits route) are the extractor's 422 — the handler-reached
        // answer.
        let set_wrong_body = [
            "select", "stop", "pause", "resume", "drain", "close", "labels", "limits",
        ]
        .iter()
        .any(|route| url.ends_with(&format!("/v0/runs/{route}")))
            || (method == "PATCH" && url.ends_with("/run_ghost/limits"));
        let ack_wrong_body = ack_wrong_body || set_wrong_body;
        // The Run open POST reached its handler: the register-shaped body
        // names no parent, so the exactly-one-parent rule answers 400.
        let run_open = method == "POST" && url.ends_with("/v0/runs");
        let expected = match (register || run_open, identity) {
            // The SSE stream answers 200 and STAYS OPEN — never drained
            // here (read_to_string would block until shutdown); dropping
            // the response closes the socket.
            (_, true) => StatusCode::OK,
            // The register POST runs its exactly-one-of + engine validation
            // (an unknown kind → 400).
            (true, false) => StatusCode::BAD_REQUEST,
            (false, false) if ack_wrong_body => StatusCode::UNPROCESSABLE_ENTITY,
            // Every other ghost route: the HANDLER's engine lookup answers
            // 404 (auth passed, the resource does not exist).
            (false, false) => StatusCode::NOT_FOUND,
        };
        assert_eq!(
            response.status(),
            expected,
            "{method} {url} with the valid bearer must reach its handler"
        );
        if !identity {
            // Drain the non-SSE bodies so the connection returns cleanly.
            let _ = response.body_mut().read_to_string();
        }
    }

    server.stop().expect("serve must shut down cleanly");
    let text = sink_handle.text();
    assert!(text.contains("control api listening on"), "{text}");
    assert!(text.contains("control api stopped"), "{text}");
}

#[test]
fn the_version_document_self_describes_the_surface() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();

    let mut response = get_ok(&client, &server.addr, "/v0/version");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let json = json_body(&mut response);
    assert_eq!(json["api"], "hekma-control-api");
    assert_eq!(json["version"], "v0");
    assert_eq!(json["stability"], "unstable");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["engine"]["fleet"], 2);
    assert_eq!(json["engine"]["event"], 1);
    // Review finding 21: the capture posture is part of the
    // self-description — a Host learns it without a failed round-trip.
    assert_eq!(
        json["log_capture"], "full",
        "the default serve posture is named on the version document"
    );

    server.stop().unwrap();
}

#[test]
fn fleet_show_and_the_error_mappings_round_trip() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();

    // An EMPTY fleet is an honest empty listing (never an error).
    let mut response = get_ok(&client, &server.addr, "/v0/fleet");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["schema_version"], 2);
    assert_eq!(json["instances"], serde_json::json!([]));
    // The Fleet-wide totals are real zeros (zero-not-absent — AC4), with no
    // dollar estimate until an instance carries a Rate.
    assert_eq!(json["totals"]["total_input_tokens"], 0);
    assert_eq!(json["totals"]["total_dollars"], serde_json::Value::Null);

    // Register through the API itself (the native mock kind — register-only
    // here; the lifecycle test drives the manifest fake agent).
    let mut response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", format!("Bearer {BEARER_VALUE}"))
        .send_json(serde_json::json!({"name": "probe", "kind": "mock"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json = json_body(&mut response);
    assert_eq!(json["schema_version"], 2);
    assert_eq!(json["instance"]["name"], "probe");
    assert_eq!(json["instance"]["state"], "registered");

    // The show route mirrors the Fleet row; unknown names 404 with the
    // frozen kind; a bad name in the path is a 400 with its own kind.
    let mut response = get_ok(&client, &server.addr, "/v0/instances/probe");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(&mut response)["instance"]["name"], "probe");

    let mut response = get_ok(&client, &server.addr, "/v0/instances/ghost");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "not_found");

    let mut response = get_ok(&client, &server.addr, "/v0/instances/Bad%20Name");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&mut response)["error"]["kind"], "invalid_name");

    // The fleet listing now carries the registered row.
    let mut response = get_ok(&client, &server.addr, "/v0/fleet");
    let json = json_body(&mut response);
    assert_eq!(json["instances"].as_array().unwrap().len(), 1);
    assert_eq!(json["instances"][0]["name"], "probe");

    // Status + capabilities project the instance (registered: never run).
    let mut response = get_ok(&client, &server.addr, "/v0/instances/probe/status");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["status"]["instance"]["state"], "registered");
    assert_eq!(json["status"]["restart_count"], 0);

    let mut response = get_ok(&client, &server.addr, "/v0/instances/probe/capabilities");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["schema_version"], 1);
    // The mock kind's declaration projects SOME current-OS view — the
    // engine's own shape, verbatim.
    assert!(json["capabilities"]["os"].is_string());
    assert!(json["capabilities"]["entries"].is_array());

    server.stop().unwrap();
}

#[test]
fn the_register_body_enforces_exactly_one_adapter_source() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    // BOTH sources: ambiguous, 400.
    let mut response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({
            "name": "both", "kind": "mock", "manifest": "/nowhere"
        }))
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_body");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("exactly one"));

    // NEITHER: no source, 400.
    let response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"name": "neither"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // An INVALID NAME is rejected by the engine's naming rule → 400
    // invalid_name (NOT invalid_body — the mapper's fine-grained class).
    let mut response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"name": "Bad Name", "kind": "mock"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&mut response)["error"]["kind"], "invalid_name");

    // An UNKNOWN native kind is the caller's bad input → 400 invalid_body
    // with the engine's surfaced reason.
    let mut response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"name": "kindless", "kind": "no-such-kind"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_body");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no-such-kind"));

    // A DUPLICATE name is a 409 conflict.
    client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"name": "dupe", "kind": "mock"}))
        .unwrap();
    let mut response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"name": "dupe", "kind": "mock"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(&mut response)["error"]["kind"], "conflict");

    server.stop().unwrap();
}

// ---------------------------------------------------------------------------
// The end-to-end lifecycle against a REAL fake agent (the AC: the verbs
// drive it through the AD-15 states; stop honors window_secs; DELETE's
// running-refusal and success).
// ---------------------------------------------------------------------------

/// A fresh state root with a manifest fixture registered under `name` (the
/// engine that registered it is DROPPED — serve opens the same root fresh).
fn registered_fake_agent(base: &Path, name: &str) -> tempfile::TempDir {
    let manifest_dir = tempfile::tempdir().unwrap();
    ManifestFixture::new(name, &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("pause")
        .guaranteed_on_all_oses("interaction")
        .write(manifest_dir.path());
    let engine = Engine::open(Some(base.to_path_buf())).expect("engine open");
    engine
        .blocking()
        .register_with_adapter(
            name,
            &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
        )
        .expect("register");
    drop(engine);
    manifest_dir
}

/// Register the OS-AGNOSTIC native mock under `name` (config-shape tests:
/// registration only — the mock is never started; the checkpoint's warning
/// is about RUNNING the inert builtin, not registering it).
fn registered_native(base: &Path, name: &str) {
    let engine = Engine::open(Some(base.to_path_buf())).expect("engine open");
    engine.blocking().register(name, "mock").expect("register");
    drop(engine);
}

#[test]
fn the_lifecycle_verbs_drive_a_real_agent_and_stop_honors_window_secs() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        // The manifest fake agent is the POSIX `sh` shape; the Windows
        // matrix covers the same engine ladder on Windows (the drain
        // suite's pattern — NO `#[cfg]` outside the allowlist).
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _manifest = registered_fake_agent(&base, "worker");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let instance = format!("http://{}/v0/instances/worker", server.addr);

    // start → running (the read-back is the read-back of TRUTH).
    let mut response = client
        .post(format!("{instance}/start"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["instance"]["state"], "running");
    assert_eq!(json["schema_version"], 2);

    // The fleet reflects it (the usage/budget projection rides the row).
    let mut response = get_ok(&client, &server.addr, "/v0/fleet");
    assert_eq!(json_body(&mut response)["instances"][0]["state"], "running");

    // pause → paused → resume → running (the fixture guarantees pause).
    let mut response = client
        .post(format!("{instance}/pause"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(&mut response)["instance"]["state"], "paused");

    let mut response = client
        .post(format!("{instance}/resume"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(&mut response)["instance"]["state"], "running");

    // Invalid transitions are 409s: start on running, resume on running.
    for verb in ["start", "resume"] {
        let mut response = client
            .post(format!("{instance}/{verb}"))
            .header("Authorization", &auth)
            .send("")
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT, "{verb}");
        assert_eq!(json_body(&mut response)["error"]["kind"], "conflict");
    }

    // DELETE on a RUNNING instance is refused with the stop-first
    // remediation (409; v0 carries no force flag).
    let mut response = client
        .delete(&instance)
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "conflict");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("stop it first"));

    // stop honors the explicit window (the fake agent honors SIGTERM, so
    // the ladder completes well inside it).
    let mut response = client
        .post(format!("{instance}/stop"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"window_secs": 1}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(&mut response)["instance"]["state"], "stopped");

    // stop on a STOPPED instance: 409 (a plain conflict, not a 500).
    let response = client
        .post(format!("{instance}/stop"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // An out-of-bounds window is a rejected REQUEST (400), never a
    // zero-second kill.
    let mut response = client
        .post(format!("{instance}/stop"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"window_secs": 0}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_body");
    assert!(json["error"]["message"].as_str().unwrap().contains("1"));

    // DELETE the stopped instance → 204 empty; then it is GONE (404).
    let mut response = client
        .delete(&instance)
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.body_mut().read_to_string().unwrap(), "");

    let response = get_ok(&client, &server.addr, "/v0/instances/worker");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    server.stop().unwrap();
}

// ---------------------------------------------------------------------------
// The drain route (story 18-4): the DRAIN composite driven over REAL HTTP.
// BOTH outcomes are a `200` — the composite's last step (the stop) is
// unconditional — with the outcome carried honestly in the body; an
// out-of-bounds bound is a rejected REQUEST (400), never a zero-millisecond
// drain. The manifest is HAND-WRITTEN (`ManifestFixture` carries no
// `[lifecycle.pause]`/`[drain]` sections) pointed at the COMPILED
// `fake_agent` binary — the engine drain suite's shape — so unlike the
// `sh`-shaped lifecycle fixture above this test runs on ALL THREE OSes
// (no skip, no `#[cfg]`).
// ---------------------------------------------------------------------------

/// Write the story-18-4 drain manifest into a fresh dir and register it under
/// `name` (the registering engine is DROPPED — serve opens the same root
/// fresh, exactly like [`registered_fake_agent`]). `agent_args` are the start
/// template's extra args; `drain_body` is the raw `[drain]` table body. The
/// `[capabilities.pause]` table is deliberately ABSENT: the cooperative
/// command path is OS-independent and needs no capability row (the capability
/// table governs ONLY the signal path).
fn registered_drain_fake_agent(
    base: &Path,
    name: &str,
    agent_args: &[&str],
    drain_body: &str,
) -> tempfile::TempDir {
    let manifest_dir = tempfile::tempdir().unwrap();
    let bin = hekma_conformance::fake_agent_bin();
    let args_toml = |args: &[&str]| {
        args.iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let body = format!(
        "\ncontract_version = \"1.1.0\"\n\n[adapter]\nkind = \"{name}\"\n\n\
         [lifecycle.start]\nexec = {bin:?}\nargs = [{agent_toml}]\n\n\
         [lifecycle.pause]\nexec = {bin:?}\nargs = [\"--touch\", \"pause.sentinel\"]\n\n\
         [lifecycle.resume]\nexec = {bin:?}\nargs = [\"--touch\", \"resume.sentinel\"]\n\n\
         [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
         [drain]\n{drain_body}\n\n[metering]\nsource = \"self-reported\"\n",
        bin = bin.to_string_lossy(),
        agent_toml = args_toml(agent_args),
    );
    std::fs::write(manifest_dir.path().join("adapter.toml"), body).unwrap();
    let engine = Engine::open(Some(base.to_path_buf())).expect("engine open");
    engine
        .blocking()
        .register_with_adapter(
            name,
            &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
        )
        .expect("register");
    drop(engine);
    manifest_dir
}

#[test]
fn the_drain_route_serves_both_outcomes_and_rejects_an_out_of_range_bound() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // Completed shape: the idle signal lands 800ms after the PAUSE SENTINEL —
    // the file this drain's own pause command writes — so it provably
    // arrives during the drain's wait (review round 1, B13's re-anchored
    // fixture; the old process-start clock could let the file predate the
    // drain entry and complete for the wrong reason). The agent also EXITS
    // 1500ms after the same anchor: on Windows the stop ladder's graceful
    // step is a pure exit-watch (no signal to deliver), so a never-ending
    // agent would burn the ladder's whole default 30s window — far past
    // this suite's 10s response client — before the forced kill; the
    // anchored self-exit keeps the ladder GRACEFUL and fast on every OS,
    // with the drain's own wait (not the ladder) still the thing measured.
    // The 2000ms exit distance leaves a wide window (idle lands at 800ms,
    // the file reads fresh for the engine's 2s slack) in which the wait
    // loop observes the idle signal while the agent is provably alive.
    let _idle = registered_drain_fake_agent(
        &base,
        "drainer",
        &[
            "--linger-ms",
            "600000",
            "--idle-on-pause",
            "800:pause.sentinel:idle.sentinel",
            "--exit-on-pause",
            "2000:pause.sentinel",
        ],
        "timeout_ms = 10000\nidle_file = \"idle.sentinel\"",
    );
    // Timed-out shape: the declared idle file is NEVER touched, so the
    // manifest's 800ms bound burns and the stop happens anyway. Same
    // anchored self-exit (1200ms — comfortably past the 800ms bound, so
    // the wait genuinely times out first): the Windows ladder sees the
    // agent end on its own instead of polling its whole default window.
    let _stalled = registered_drain_fake_agent(
        &base,
        "stalled",
        &[
            "--linger-ms",
            "600000",
            "--exit-on-pause",
            "1200:pause.sentinel",
        ],
        "timeout_ms = 800\nidle_file = \"never.sentinel\"",
    );
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let drainer = format!("http://{}/v0/instances/drainer", server.addr);
    let stalled = format!("http://{}/v0/instances/stalled", server.addr);

    // Both start → running (the read-back is the read-back of TRUTH).
    for url in [&drainer, &stalled] {
        let mut response = client
            .post(format!("{url}/start"))
            .header("Authorization", &auth)
            .send("")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{url}");
        assert_eq!(json_body(&mut response)["instance"]["state"], "running");
    }

    // An out-of-range bound is a rejected REQUEST (400) — the route refuses
    // the bound BEFORE the engine gate, the same shape as stop's window_secs.
    for ms in [0u64, 600_001] {
        let mut response = client
            .post(format!("{drainer}/drain"))
            .header("Authorization", &auth)
            .send_json(serde_json::json!({"timeout_ms": ms}))
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "ms={ms}");
        let json = json_body(&mut response);
        assert_eq!(json["error"]["kind"], "invalid_body", "ms={ms}");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("1 to 600000"),
            "ms={ms}: the refusal names the rule"
        );
    }

    // COMPLETED: an EMPTY body drains on the MANIFEST tier (10s) — the
    // three-tier rule's middle step. The idle signal arrives mid-wait, so
    // `completed: true` and the instance ends `stopped`.
    let mut response = client
        .post(format!("{drainer}/drain"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["schema_version"], 2);
    assert_eq!(json["completed"], true, "{json}");
    assert_eq!(json["instance"]["state"], "stopped");
    let waited = json["waited_ms"].as_u64().unwrap();
    // The floor (review round 1, B9): the re-anchored idle fixture lands the
    // file ~800ms AFTER the drain's pause sentinel — so a genuinely waiting
    // drain always exceeds 500ms; a skip-the-wait mutant (or an idle file
    // that predated the drain) reads ~0 and fails here.
    assert!(
        waited >= 500,
        "the wait genuinely waited for the anchored idle signal (waited {waited}ms)"
    );
    assert!(
        waited < 9000,
        "the completed drain finished inside its manifest bound (waited {waited}ms)"
    );

    // A drained (stopped) instance refuses a second drain: 409, the plain
    // conflict shape (drain's running-only gate, over the wire).
    let mut response = client
        .post(format!("{drainer}/drain"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(&mut response)["error"]["kind"], "conflict");

    // TIMED-OUT: the manifest tier is 800ms, the idle file never lands, the
    // bound burns — and the drain STILL stops the instance. That is a `200`
    // (the stop happened) with `completed: false` and a waited_ms that
    // brackets the bound (honest, not fabricated).
    let mut response = client
        .post(format!("{stalled}/drain"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a timed-out drain is still a 200 — the stop is unconditional"
    );
    let json = json_body(&mut response);
    assert_eq!(json["completed"], false, "{json}");
    assert_eq!(json["instance"]["state"], "stopped");
    let waited = json["waited_ms"].as_u64().unwrap();
    assert!(
        waited >= 700,
        "the wait genuinely burned toward its 800ms bound (waited {waited}ms)"
    );
    assert!(
        waited < 5000,
        "the stop happened right after the bound expired (waited {waited}ms)"
    );

    server.stop().unwrap();
}

// ---------------------------------------------------------------------------
// The shutdown drain's paused-agent leg (the reported serve bug: a PAUSED
// attached agent was handed to the engine Drop's raw kill-on-drop teardown —
// killed without a graceful stop — because the drain plan enumerated only
// Running rows).
// ---------------------------------------------------------------------------

/// Whether a pid is alive (`kill -0`, zombie-discounted) — the drain suite's
/// helper shape: NO OS-cfg (branch on the runtime OS id and shell out);
/// `/proc` reads no-op off Linux.
fn pid_alive(pid: u32) -> bool {
    match hekma_engine::OsId::current() {
        hekma_engine::OsId::Windows => {
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output();
            match out {
                Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
                Err(_) => false,
            }
        }
        _ => {
            let exists = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            exists && !proc_pid_is_zombie(pid)
        }
    }
}

fn proc_pid_is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.split_whitespace().next() == Some("Z"))
        })
        .unwrap_or(false)
}

fn heartbeat_lines(agent_log: &Path) -> usize {
    std::fs::read_to_string(agent_log)
        .map(|c| c.lines().filter(|l| l.starts_with("heartbeat ")).count())
        .unwrap_or(0)
}

#[test]
fn the_shutdown_drain_stops_a_paused_attached_agent_gracefully() {
    // The full-stack repro of the reported bug: `serve` supervising an
    // ATTACHED agent (started through the Control API — the exact shape a
    // host deployment runs), paused (a REAL SIGSTOP suspension on Unix),
    // then the shutdown signal. The drain used to enumerate ONLY Running
    // rows, so the paused agent was skipped by every ladder pass and then
    // SIGKILLed by the engine Drop's kill-on-drop teardown — no graceful
    // window, no stop transition, and a phantom `paused` row over a dead
    // process. The drain must take Paused rows through the SAME bounded
    // ladder (which resumes the suspension so SIGTERM can be delivered),
    // leaving `stopped` + a graceful terminal cause on the record.
    //
    // Runtime-skip on Windows (POSIX `kill` probes + the real-suspension
    // proof are Unix-shaped; Windows pause is best-effort — no suspension —
    // so a paused row's process there is stopped by the ordinary running
    // ladder, which the windows matrix already proves). NO `#[cfg`.
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // Register a HEARTBEATING fake agent (the suite's fixture + heartbeat
    // args, so the suspension is provable), then drop the registering
    // engine — serve opens the same root fresh.
    let manifest_dir = tempfile::tempdir().unwrap();
    ManifestFixture::new("frozen", &["--heartbeat-ms", "50", "--linger-ms", "600000"])
        .guaranteed_on_all_oses("pause")
        .guaranteed_on_all_oses("interaction")
        .write(manifest_dir.path());
    {
        let engine = Engine::open(Some(base.clone())).expect("engine open");
        engine
            .blocking()
            .register_with_adapter(
                "frozen",
                &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
            )
            .expect("register");
    }
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base.clone(), sink_handle.clone());
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let instance = format!("http://{}/v0/instances/frozen", server.addr);

    // start → running (ATTACHED under serve's engine).
    let mut response = client
        .post(format!("{instance}/start"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(&mut response)["instance"]["state"], "running");

    let agent_log = base
        .join("agents")
        .join("frozen")
        .join("logs")
        .join("agent.log");
    let pid = {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(contents) = std::fs::read_to_string(&agent_log) {
                if let Some(line) = contents.lines().find(|l| l.contains("ready pid=")) {
                    if let Some(idx) = line.find("pid=") {
                        if let Ok(pid) = line[idx + 4..].trim().parse::<u32>() {
                            break pid;
                        }
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "agent pid never announced"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // pause → paused, and the heartbeat REALLY freezes (real SIGSTOP).
    let mut response = client
        .post(format!("{instance}/pause"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(&mut response)["instance"]["state"], "paused");
    std::thread::sleep(Duration::from_millis(200)); // SIGSTOP + in-flight line settle
    let baseline = heartbeat_lines(&agent_log);
    let watch_until = std::time::Instant::now() + Duration::from_millis(800);
    while std::time::Instant::now() < watch_until {
        let now = heartbeat_lines(&agent_log);
        assert!(
            now <= baseline,
            "heartbeat must NOT grow while paused (real SIGSTOP): baseline {baseline}, saw {now}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Shutdown: the SIGTEST signal drives serve's drain.
    server.stop().unwrap();

    // The process is gone either way (kill-on-drop also kills) — the
    // DISCRIMINATOR is the record: the drain must have run the ladder.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while pid_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the drained paused agent must be gone (pid {pid} still alive)"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let text = sink_handle.text();
    assert!(
        text.contains("stopping frozen"),
        "the drain must take the PAUSED agent through the ladder (was excluded \
         from the plan and killed by the engine-exit teardown):\n{text}"
    );
    assert!(
        text.contains("stopped gracefully"),
        "the resumed agent honors SIGTERM — the outcome is graceful:\n{text}"
    );

    // The durable record: `stopped` (never a phantom `paused` row over a
    // dead process) with a graceful terminal cause.
    {
        let engine = Engine::open(Some(base)).expect("verify engine open");
        let facade = engine.blocking();
        let fleet = facade.fleet().expect("verify fleet");
        let entry = fleet
            .iter()
            .find(|e| e.name.as_str() == "frozen")
            .expect("frozen row present");
        assert_eq!(
            format!("{:?}", entry.state),
            "Stopped",
            "the drained instance must land stopped, not stay a phantom paused row"
        );
        let events = facade.transition_events("frozen").expect("verify events");
        let terminal = events
            .iter()
            .rev()
            .find(|e| format!("{:?}", e.new_state) == "Stopped")
            .expect("a terminal stop transition must exist (the drain ran the ladder)");
        assert!(
            matches!(terminal.cause, hekma_engine::TransitionCause::StopGraceful),
            "the terminal cause must be graceful (the ladder resumed the \
             suspended agent first), got: {:?}",
            terminal.cause
        );
    }
}

// ---------------------------------------------------------------------------
// The config surface: the CLI-mirroring GET (masked values), the PUT
// round-trip with the warnings field, and the engine's write rejections.
// ---------------------------------------------------------------------------

#[test]
fn config_get_masks_secrets_and_put_round_trips_with_the_engine_rejections() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    registered_native(&base, "cfg");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let config = format!("http://{}/v0/instances/cfg/config", server.addr);

    // GET: the CLI document shape. A FRESH registration resolves to ZERO
    // leaves — the engine-defaults layer ships EMPTY (the 2-1 review
    // decision: config no longer seeds restart.policy) and neither the kind
    // nor the instance has written config — and the document still
    // self-describes its schema. An honest empty document, not a 404.
    let mut response = get_ok(&client, &server.addr, "/v0/instances/cfg/config");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(
        json["entries"].as_array().expect("entries array"),
        &Vec::<serde_json::Value>::new(),
        "a fresh registration has no leaves yet: {json}"
    );

    // PUT a known key → 200 with the read-back AND the warnings field.
    let mut response = client
        .put(&config)
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"key": "budget.tokens.per_run", "value": "1000"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["warnings"], serde_json::json!([]));
    assert!(
        json["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["key"] == "budget.tokens.per_run" && e["value"] == "1000"),
        "the read-back shows the written value: {json}"
    );

    // The write is visible on a SUBSEQUENT GET (the round trip, not just the
    // acknowledgment's read-back), with the instance layer as the winning
    // source and every leaf carrying source + unvalidated.
    let mut response = get_ok(&client, &server.addr, "/v0/instances/cfg/config");
    let json = json_body(&mut response);
    let entries = json["entries"].as_array().expect("entries array");
    assert!(
        entries.iter().any(|e| e["key"] == "budget.tokens.per_run"
            && e["value"] == "1000"
            && e["source"] == "instance"),
        "the GET round trip shows the written key at the instance layer: {json}"
    );
    assert!(
        entries
            .iter()
            .all(|e| e["source"].is_string() && e["unvalidated"].is_boolean()),
        "every leaf carries source + unvalidated: {json}"
    );

    // A secret-reference value renders MASKED on the read (the CLI display
    // path), and the secret VALUE never appears anywhere in the response.
    let mut response = client
        .put(&config)
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"key": "agent.api_key", "value": "secret:CLEARTEXT_TOKEN"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.body_mut().read_to_string().unwrap();
    assert!(
        !text.contains("CLEARTEXT_TOKEN"),
        "the cleartext secret must never ride a response: {text}"
    );
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let masked = json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "agent.api_key")
        .expect("the secret leaf is in the read-back")["value"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(masked, "secret:CLEARTEXT_TOKEN");
    assert!(masked.contains("****"), "the mask shape: {masked}");

    // The unknown-key rejection is a 400 naming the near miss.
    let mut response = client
        .put(&config)
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"key": "modle", "value": "x"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_body");
    assert!(json["error"]["message"].as_str().unwrap().contains("model"));

    // The invalid-value rejection is a 400 with the typed reason.
    let mut response = client
        .put(&config)
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"key": "budget.tokens.per_run", "value": "lots"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_body");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("non-negative"));

    // A config GET on an unknown instance is a 404 (the mapper's NotFound).
    let response = get_ok(&client, &server.addr, "/v0/instances/ghost/config");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    server.stop().unwrap();
}

// ---------------------------------------------------------------------------
// Events: the cursor pull (lossless, idempotent, truncation-guarded) and the
// SSE stream (live delivery, in-stream lag vocabulary).
// ---------------------------------------------------------------------------

#[test]
fn the_cursor_pull_is_lossless_idempotent_and_truncation_guarded() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return; // the fake agent is POSIX (`sh`); see the lifecycle test.
    }
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _manifest = registered_fake_agent(&base, "evs");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    // Commit transitions THROUGH the API (start + pause + resume = 3+).
    for verb in ["start", "pause", "resume"] {
        let mut response = client
            .post(format!("http://{}/v0/instances/evs/{verb}", server.addr))
            .header("Authorization", &auth)
            .send("")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{verb}");
        let _ = response.body_mut().read_to_string();
    }

    // Pull from START: a batch with the committed events + a cursor.
    let mut response = get_ok(&client, &server.addr, "/v0/instances/evs/events");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    let first_events = json["events"].as_array().expect("events array").clone();
    assert!(
        first_events.len() >= 3,
        "the start/pause/resume transitions are committed: {json}"
    );
    assert!(
        first_events
            .iter()
            .all(|e| e["kind"] == "transition" || e["kind"] == "usage_update"),
        "the engine's kind-tagged payloads, verbatim: {json}"
    );
    let cursor = json["cursor"].clone();
    assert_eq!(json["torn_tail_skipped"], false);
    assert!(cursor["transitions"].as_u64().unwrap() >= 3);

    // A torn `instance.log` MIRROR (the crash shape the file-backed record
    // used to surface) no longer touches the record: the events come from
    // state.db, so the batch is complete and the skip flag stays false.
    let torn_log = state
        .path()
        .join("agents")
        .join("evs")
        .join("logs")
        .join("instance.log");
    let existing = std::fs::read_to_string(&torn_log).expect("the transition mirror exists");
    let torned = format!("{existing}{{\"schema_version\":1,\"inst");
    std::fs::write(&torn_log, torned).expect("tear the mirror's tail");
    let mut response = get_ok(&client, &server.addr, "/v0/instances/evs/events");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(
        json["torn_tail_skipped"], false,
        "the record is state.db; a torn mirror is not the record: {json}"
    );
    assert!(
        json["events"].as_array().expect("events array").len() >= 3,
        "the committed events deliver: {json}"
    );

    // IDEMPOTENT continuation: the SAME cursor yields an EMPTY batch (the
    // lossless resume contract).
    let mut response = client
        .get(format!(
            "http://{}/v0/instances/evs/events?cursor={}",
            server.addr,
            percent_encode(&cursor.to_string())
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(
        json["events"],
        serde_json::json!([]),
        "a cursor already at the tail re-delivers nothing: {json}"
    );

    // A cursor PAST the family's committed count: the typed 409
    // (`cursor_invalid`) — NEVER a clamp. THE ENGINE'S truncation guard,
    // mapped (the unit pin holds the sentence; this holds the ROUTE).
    let past = serde_json::json!({"transitions": 9_999_999, "breaches": 0, "usage": 0});
    let mut response = client
        .get(format!(
            "http://{}/v0/instances/evs/events?cursor={}",
            server.addr,
            percent_encode(&past.to_string())
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "cursor_invalid");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("past this family"));

    // A malformed cursor is a 400 invalid_body naming the rule.
    let mut response = client
        .get(format!(
            "http://{}/v0/instances/evs/events?cursor={}",
            server.addr,
            percent_encode("not-json")
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&mut response)["error"]["kind"], "invalid_body");

    server.stop().unwrap();
}

/// Stage ONE valid outbox journal entry with the given seq (built through
/// the engine's OWN types, so the wire shape can never drift from what the
/// engine writes).
fn outbox_entry_line(seq: u64) -> String {
    let entry = hekma_engine::OutboxEntry {
        schema_version: hekma_engine::OUTBOX_SCHEMA_VERSION,
        seq,
        event: hekma_engine::EngineEvent::Transition(hekma_engine::TransitionEvent::new(
            "probe",
            hekma_engine::LifecycleState::Starting,
            hekma_engine::LifecycleState::Running,
            hekma_engine::TransitionCause::AdapterReady,
            "2026-10-19T00:00:00Z".to_string(),
        )),
    };
    format!("{}\n", serde_json::to_string(&entry).unwrap())
}

#[test]
fn export_pull_ack_cursor_survives_a_host_restart_and_invalid_cursors_refuse() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _manifest = registered_fake_agent(&base, "worker");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base.clone(), sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let instance = format!("http://{}/v0/instances/worker", server.addr);

    // Drive a real lifecycle so the outbox journal holds committed
    // transitions (register happened on the PRE-SERVE engine; start/stop
    // commit through THIS host).
    let response = client
        .post(format!("{instance}/start"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = client
        .post(format!("{instance}/stop"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"window_secs": 1}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // PULL (the default: resume from the durable ack — absent, so from the
    // journal's beginning): at least the start/stop transitions, in ONE
    // global order, every entry seq-stamped.
    let mut response = get_ok(&client, &server.addr, "/v0/export");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(
        json["schema_version"], 1,
        "the journal's own version: {json}"
    );
    let events = json["events"].as_array().expect("events array");
    assert!(
        events.len() >= 2,
        "the host's committed transitions are exported: {json}"
    );
    assert_eq!(
        json["torn_tail_skipped"], false,
        "a clean journal surfaces no torn-tail skip: {json}"
    );
    let max_seq = events
        .iter()
        .map(|e| e["seq"].as_u64().expect("each entry carries its seq"))
        .max()
        .expect("a non-empty batch has a max seq");

    // ACK the tail: 200, and the response returns the cursor AS PERSISTED.
    let mut response = client
        .post(format!("http://{}/v0/export/ack", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"seq": max_seq}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["seq"], max_seq, "the ack reads back persisted: {json}");

    // An ack PAST the committed tail: the typed 409 cursor_invalid — never
    // a clamp, never a fabricated advance.
    let mut response = client
        .post(format!("http://{addr}/v0/export/ack", addr = server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"seq": 9_999_999}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "cursor_invalid");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("past the journal's committed tail"));

    // THE RESTART: stop host #1, open host #2 on the SAME state root. The
    // ack cursor is DURABLE (outbox-ack.json) — the default pull resumes
    // AFTER it (an EMPTY batch: everything acked is consumed), while the
    // explicit from-zero cursor re-pulls everything still retained.
    server.stop().unwrap();
    let (sink_handle2, _diagnostics2) = SharedSink::new();
    let server2 = spawn_serve_with_api(base, sink_handle2);
    let mut response = get_ok(&client, &server2.addr, "/v0/export");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(
        json["events"],
        serde_json::json!([]),
        "the ack survived the restart — the default pull resumes AFTER it: {json}"
    );
    let explicit_zero = serde_json::json!({"seq": 0});
    let mut response = client
        .get(format!(
            "http://{}/v0/export?cursor={}",
            server2.addr,
            percent_encode(&explicit_zero.to_string())
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert!(
        json["events"].as_array().expect("events array").len() >= 2,
        "an explicit from-zero cursor re-pulls the retained journal: {json}"
    );
    server2.stop().unwrap();
}

#[test]
fn an_export_cursor_below_the_retained_floor_is_409_never_a_clamp() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // Stage the journal DIRECTLY: one entry at seq 5 (a compaction already
    // dropped 1..=4 in this scenario). registered_fake_agent's manifest dir
    // is irrelevant here — no instance is started; the floor guard is what
    // is under test.
    let journal = base.join(hekma_engine::paths::OUTBOX_JOURNAL_FILE);
    std::fs::write(&journal, outbox_entry_line(5)).unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    // BELOW the retained floor (cursor 3 would silently skip the dropped
    // 1..=4): the typed 409 cursor_invalid naming the floor.
    let below = serde_json::json!({"seq": 3});
    let mut response = client
        .get(format!(
            "http://{}/v0/export?cursor={}",
            server.addr,
            percent_encode(&below.to_string())
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "cursor_invalid");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("below the journal's retained floor"));

    // PAST the committed tail: the same 409 class, the tail sentence.
    let past = serde_json::json!({"seq": 99});
    let mut response = client
        .get(format!(
            "http://{}/v0/export?cursor={}",
            server.addr,
            percent_encode(&past.to_string())
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "cursor_invalid");

    // The floor-minus-one cursor IS servable (a fresh consumer resuming at
    // the floor gets the retained entries).
    let at_floor = serde_json::json!({"seq": 4});
    let mut response = client
        .get(format!(
            "http://{}/v0/export?cursor={}",
            server.addr,
            percent_encode(&at_floor.to_string())
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(
        json["events"].as_array().expect("events array").len(),
        1,
        "the retained entry delivers past the floor-minus-one cursor: {json}"
    );

    // A MALFORMED cursor (not JSON at all) never reaches the engine: it is
    // the export route's own parse refusal — 400 invalid_body naming the
    // wire rule — the query-parameter sibling of the body extractors' 422.
    let mut response = client
        .get(format!(
            "http://{}/v0/export?cursor={}",
            server.addr,
            percent_encode("not json at all")
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_body");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("JSON-encoded export cursor"),
        "the refusal names the cursor's wire shape: {json}"
    );
    server.stop().unwrap();
}

#[test]
fn an_invalid_instance_name_on_the_config_routes_is_400_invalid_name() {
    // The config routes carry the name through the ConfigError mapper: a
    // bogus name is the typed 400 invalid_name — never a 500, never a
    // "not found" that would imply the name was valid but absent.
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let mut response = client
        .get(format!(
            "http://{}/v0/instances/Not%20Valid%21/config",
            server.addr
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_name");
    server.stop().unwrap();
}

#[test]
fn a_state_dir_on_an_explicit_volume_holds_everything_and_refuses_a_second_host() {
    // The story-17-4 verification leg (17-2 shipped the surface): an
    // explicit non-default base — the "Host-chosen volume" — holds the LOCK,
    // the DB (which holds the outbox and its ack), the SECRETS file; and a second host on
    // the same base is REFUSED naming the live holder.
    let state = tempfile::tempdir().unwrap();
    let volume = state.path().join("mounted-volume").join("hekma-state");
    std::fs::create_dir_all(&volume).unwrap();
    let _manifest = registered_fake_agent(&volume, "worker");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(volume.clone(), sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    // Drive one committed transition so the outbox journal EXISTS on the
    // volume (the lock/db land at open; the journal at the first commit),
    // then pull and ACK the batch — the ack file lands at the first ack
    // (an absent ack file is the "nothing consumed yet" state, so the
    // assertion below needs the real write path, not a synthetic file).
    let response = client
        .post(format!("http://{}/v0/instances/worker/start", server.addr))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = client
        .post(format!("http://{}/v0/instances/worker/stop", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"window_secs": 1}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut response = client
        .get(format!("http://{}/v0/export", server.addr))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let batch = json_body(&mut response);
    let cursor = batch["cursor"]["seq"]
        .as_u64()
        .expect("the batch carries its resume cursor");
    let response = client
        .post(format!("http://{}/v0/export/ack", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({ "seq": cursor }))
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the ack of the pulled cursor lands"
    );

    for name in [
        hekma_engine::paths::HOST_LOCK_FILE,
        hekma_engine::paths::SECRETS_FILE,
    ] {
        assert!(
            volume.join(name).exists(),
            "{name} must live under the EXPLICIT base ({})",
            volume.display()
        );
    }
    // The state DB is the paths authority's `state.db` under the same root.
    let db = std::fs::read_dir(&volume)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().ends_with(".db"));
    assert!(db, "the state DB lives under the explicit base");

    // A SECOND host on the same base: refused, naming the live holder.
    let options = hekma_host::ServeOptions {
        state_base: Some(volume.clone()),
        control_listen: Some("127.0.0.1:0".parse().unwrap()),
        bearer_secret: Some(BEARER_NAME.to_string()),
        metering_listen_port: 0,
        stop_window: Some(2),
        log_capture: hekma_engine::LogCaptureMode::Full,
        diagnostics: None,
        otlp: Default::default(),
        ..Default::default()
    };
    let err = hekma_host::serve(options).expect_err("the second host must be refused");
    assert!(
        matches!(err, hekma_host::ServeError::StateDirLocked { .. }),
        "the refusal is the typed StateDirLocked: {err}"
    );
    assert!(
        err.to_string().contains("pid"),
        "the refusal names the live holder: {err}"
    );

    server.stop().unwrap();
}

#[test]
fn the_sse_stream_delivers_live_events_and_ends_on_shutdown() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return; // the fake agent is POSIX (`sh`); see the lifecycle test.
    }
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _manifest = registered_fake_agent(&base, "live");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle.clone());
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    // Subscribe FIRST (the engine's documented order), then drive a
    // transition; the stream must carry it. This binding is the STREAM — it
    // is never shadowed (shadowing would orphan the body the reader below
    // drains; the start call gets its own binding).
    let mut response = client
        .get(format!("http://{}/v0/events", server.addr))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap().to_string())
            .as_deref(),
        Some("text/event-stream")
    );

    // Drive the transition on ANOTHER connection while the stream is open —
    // a DISTINCT binding (`start`, never a shadow of `response`: the shadowed
    // stream body would be orphaned and the reader below would drain THIS
    // already-complete body — an instant EOF that would masquerade as a
    // server bug).
    let mut start = client
        .post(format!("http://{}/v0/instances/live/start", server.addr))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(start.status(), StatusCode::OK);
    let _ = start.body_mut().read_to_string();

    // Read frames until a transition payload arrives (bounded by the client
    // config's recv timeouts — a hang fails the test, not the suite).
    let mut reader = BufReader::new(response.body_mut().as_reader());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "no transition frame arrived on the SSE stream in time"
        );
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("the stream ended before any transition frame"),
            Ok(_) => {}
            Err(err) => panic!("the SSE read failed: {err}"),
        }
        if let Some(payload) = line.strip_prefix("data: ") {
            let event: serde_json::Value = serde_json::from_str(payload.trim())
                .expect("every data frame is the engine's JSON");
            if event["kind"] == "transition" {
                assert_eq!(event["instance"], "live");
                break;
            }
            // A lag frame would be the control-API vocabulary arriving in
            // the wild — accepted, surfaced, never silent.
            assert_ne!(event["kind"], "lagged", "an unexpected lag frame");
        }
    }

    // The stream ENDS on host shutdown: fire the signal; the API stops
    // accepting FIRST, the open stream ends cooperatively (read → EOF), well
    // before the drain finishes (the shutdown order, observed live). The
    // reader DRAINS to the EOF — the loop broke mid-frame (the transition
    // data line; its terminator may already be buffered), so leftover body
    // bytes are expected and EOF is the assertion.
    server.stop().expect("serve must shut down cleanly");
    let eof_deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut tail = String::new();
        match reader.read_line(&mut tail) {
            Ok(0) => break, // the body ended — the stream is closed
            Ok(_) => {}
            Err(err) => panic!("the SSE read failed: {err}"),
        }
        assert!(
            std::time::Instant::now() < eof_deadline,
            "the SSE stream must END when the host shuts down"
        );
    }

    // The notice ORDER pins the mount contract: signal → API down → drain.
    let text = sink_handle.text();
    let api_down = text
        .match_indices("control api stopped")
        .next()
        .expect("the API-down notice is on the record");
    let signal = text
        .match_indices("SIGTEST received")
        .next()
        .expect("the signal notice is on the record");
    let complete = text
        .match_indices("shutdown complete")
        .next()
        .expect("the completion notice is on the record");
    assert!(
        signal.0 < api_down.0,
        "the API stops after the signal:\n{text}"
    );
    assert!(
        api_down.0 < complete.0,
        "the drain runs after the API is down:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// The mount contract: default-off (byte-identical 17-2), the flag-pair
// validation, bind-conflict surfacing, and the unresolvable-bearer refusal.
// ---------------------------------------------------------------------------

#[test]
fn the_api_is_off_by_default_and_the_flag_pair_is_validated() {
    // Story 21-4: with no `--listen` there is no OPERATOR listener — only the
    // loopback LOCAL endpoint every serve binds for the hekma CLI (recorded
    // in control.json, removed at shutdown).
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let options = hekma_host::ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        stop_window: Some(1),
        diagnostics: Some(diagnostic_sink_of(&sink_handle)),
        ..hekma_host::ServeOptions::default()
    };
    let server = spawn_serve(options);
    // The discovery file appears once the local endpoint is bound.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let discovery = loop {
        if let Ok(discovery) = hekma_host::read_discovery(state.path()) {
            break discovery;
        }
        assert!(std::time::Instant::now() < deadline, "no control.json");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(discovery.addr.ip().is_loopback());
    assert!(!discovery.autostarted);
    assert_eq!(discovery.token.len(), 64);
    server.stop().expect("serve completes");
    assert!(
        !hekma_host::control_file(state.path()).exists(),
        "control.json is removed at shutdown"
    );
    let text = sink_handle.text();
    assert!(
        !text.contains("control api listening on"),
        "no operator listener exists by default:\n{text}"
    );
    // The notice SEQUENCE, pinned exactly (review item 27's discipline):
    // started, the shared metering proxy's address, the local endpoint,
    // signal, the API stop, the drain's zero-agent outcome, complete — in
    // order, and NOTHING else.
    let notices: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        notices.len(),
        7,
        "a listener-less serve emits exactly seven notices:\n{text}"
    );
    assert!(
        notices[0].starts_with("[hekma] host started; state root "),
        "the first notice is the started notice:\n{text}"
    );
    assert!(
        notices[1].starts_with("[hekma] the metering proxy is listening on http://127.0.0.1:"),
        "the second notice names the shared metering proxy on loopback:\n{text}"
    );
    assert!(
        notices[2].starts_with(&format!(
            "[hekma] local control endpoint on {} (loopback, for the hekma CLI;",
            discovery.addr
        )),
        "the third notice names the local endpoint:\n{text}"
    );
    assert_eq!(
        notices[3],
        "[hekma] SIGTEST received; stopping running and paused agents (graceful window 1s) ...",
        "the fourth notice is the signal notice, verbatim:\n{text}"
    );
    assert_eq!(
        notices[4],
        "[hekma] control api stopped; no requests are accepted for the rest of the shutdown",
        "the fifth notice is the API stop:\n{text}"
    );
    assert_eq!(
        notices[5], "[hekma] no running or paused agents; nothing to stop",
        "the sixth notice is the drain's zero-agent outcome:\n{text}"
    );
    assert_eq!(
        notices[6], "[hekma] shutdown complete (SIGTEST)",
        "the seventh notice is the completion notice, verbatim:\n{text}"
    );

    // --listen without --bearer-secret: a typed InvalidOptions (usage class).
    let state = tempfile::tempdir().unwrap();
    let result = hekma_host::serve_with_signal(
        hekma_host::ServeOptions {
            state_base: Some(state.path().to_path_buf()),
            control_listen: Some("127.0.0.1:0".parse().unwrap()),
            bearer_secret: None,
            ..hekma_host::ServeOptions::default()
        },
        |_| std::future::ready(Ok("SIGTEST")),
    );
    match result {
        Err(hekma_host::ServeError::InvalidOptions { message }) => {
            // Plain-worded labels (the #319/#320 CodeQL remedy): embedding
            // {message} carries the bearer-tainted startup text into the
            // panic output, which the analyzer rightly flags as cleartext
            // logging of a secret. The contains() conditions still pin the
            // full wording.
            assert!(
                message.contains("bearer"),
                "the refusal names the bearer requirement"
            );
            assert!(
                message.contains("never serves unauthenticated"),
                "the refusal says the API never serves unauthenticated"
            );
        }
        other => panic!("a half-configured API must be InvalidOptions: {other:?}"),
    }

    // --bearer-secret without --listen: same refusal, the other direction.
    let state = tempfile::tempdir().unwrap();
    let result = hekma_host::serve_with_signal(
        hekma_host::ServeOptions {
            state_base: Some(state.path().to_path_buf()),
            bearer_secret: Some(BEARER_NAME.to_string()),
            ..hekma_host::ServeOptions::default()
        },
        |_| std::future::ready(Ok("SIGTEST")),
    );
    match result {
        Err(hekma_host::ServeError::InvalidOptions { message }) => {
            assert!(
                message.contains("requires --listen"),
                "the orphan bearer name is refused with the --listen pair"
            );
        }
        other => panic!("an orphan bearer name must be InvalidOptions: {other:?}"),
    }
}

#[test]
fn a_bind_conflict_surfaces_the_address_and_refuses_the_start() {
    // Occupy a port OUTSIDE serve, then ask the API to bind it.
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap();
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    write_secrets_file(&base);
    let (sink_handle, _diagnostics) = SharedSink::new();
    let options = hekma_host::ServeOptions {
        state_base: Some(base),
        control_listen: Some(taken),
        bearer_secret: Some(BEARER_NAME.to_string()),
        diagnostics: Some(diagnostic_sink_of(&sink_handle)),
        ..hekma_host::ServeOptions::default()
    };
    let result = hekma_host::serve_with_signal(options, |_| std::future::ready(Ok("SIGTEST")));
    match result {
        Err(hekma_host::ServeError::Startup { message }) => {
            assert!(
                message.contains(&taken.to_string()),
                "the surfaced reason names the taken address"
            );
            assert!(
                message.contains("could not bind"),
                "the surfaced reason names the bind failure"
            );
        }
        other => panic!("a bind conflict must be a Startup refusal: {other:?}"),
    }
    let text = sink_handle.text();
    assert!(
        text.contains("could not bind the control api"),
        "the refusal is surfaced through the notice stream too:\n{text}"
    );
    drop(sink_handle);
}

#[test]
fn an_unresolvable_bearer_refuses_startup_before_any_engine_work() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // NO secrets file, and a name no env var holds: the composite's typed
    // Unresolved, mapped to the host's Startup refusal.
    let (sink_handle, _diagnostics) = SharedSink::new();
    let options = hekma_host::ServeOptions {
        state_base: Some(base.clone()),
        control_listen: Some("127.0.0.1:0".parse().unwrap()),
        bearer_secret: Some("no_such_bearer_anywhere".to_string()),
        diagnostics: Some(diagnostic_sink_of(&sink_handle)),
        ..hekma_host::ServeOptions::default()
    };
    let result = hekma_host::serve_with_signal(options, |_| std::future::ready(Ok("SIGTEST")));
    match result {
        Err(hekma_host::ServeError::Startup { message }) => {
            assert!(
                message.contains("bearer secret could not be resolved"),
                "the refusal names the unresolvable bearer"
            );
            assert!(
                message.contains("no_such_bearer_anywhere"),
                "the refusal names the missing secret reference"
            );
            // The remediation rides the engine's Unresolved text.
            assert!(
                message.contains("secrets file"),
                "the remediation names the secrets file"
            );
        }
        other => panic!("an unresolvable bearer must refuse startup: {other:?}"),
    }
    // NOTHING binds, NOTHING opens: no state DB was created (the refusal
    // precedes the runtime and the engine — the I/O matrix's "no engine
    // work done").
    assert!(
        !base.join(hekma_engine::paths::STATE_DB_FILE).exists(),
        "the engine must never open for an unresolvable bearer"
    );
    drop(sink_handle);
}

// ---------------------------------------------------------------------------
// The review-hardening batch (17-3 review): RFC 9110 bearer parsing, the
// envelope over extractor rejections + routing, the stop-body presence
// discipline, the committed-truth 201 + Location, the flag-target steering
// warnings over the wire, the non-loopback + empty-secret startup notices,
// and the abort backstop observed live. The raw-socket checks exist because
// ureq cannot express them (two same-name headers, no content-type, an
// unterminated body).
// ---------------------------------------------------------------------------

/// One raw-socket HTTP/1.1 exchange: send EXACT request bytes, read to EOF.
/// The request MUST carry `Connection: close` so the server closes after one
/// response (otherwise the read waits for a keep-alive that never comes).
/// Returns (status code, full response text).
fn raw_http(addr: &str, request: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(addr).expect("the raw socket connects");
    stream.write_all(request.as_bytes()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("the response reads to EOF");
    let status: u16 = response
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| response.strip_prefix("HTTP/1.0 "))
        .and_then(|rest| rest.split(' ').next())
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    (status, response)
}

/// The envelope assertions every error response must satisfy (review item
/// 9's "never a bare status" promise): a versioned body with the frozen
/// kind. `body` is the raw response text; the JSON starts at the first `{`.
fn assert_envelope(body: &str, expected_kind: &str) -> serde_json::Value {
    let start = body.find('{').expect("an error body carries JSON");
    let json: serde_json::Value =
        serde_json::from_str(&body[start..]).expect("the envelope parses");
    assert_eq!(
        json["schema_version"], 1,
        "every error carries the API schema stamp: {body}"
    );
    assert_eq!(
        json["error"]["kind"], expected_kind,
        "the frozen kind, not prose: {body}"
    );
    assert!(
        json["error"]["message"].as_str().is_some(),
        "the envelope names a reason: {body}"
    );
    json
}

#[test]
fn the_bearer_scheme_is_case_insensitive_and_ambiguous_headers_are_refused() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();

    // RFC 9110 §11: the auth-scheme token is CASE-INSENSITIVE — `bearer`,
    // `BEARER`, `bEaReR` are the same scheme and all authenticate (review
    // item 6).
    for spelling in ["bearer", "BEARER", "bEaReR"] {
        let mut response = client
            .get(format!("http://{}/v0/version", server.addr))
            .header("Authorization", format!("{spelling} {BEARER_VALUE}"))
            .call()
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the {spelling} scheme spelling must authenticate"
        );
        let _ = response.body_mut().read_to_string();
    }

    // MULTIPLE Authorization fields are ambiguous (RFC 9110's recipient
    // rule): refused 401 — NEVER first-wins, even when BOTH are valid. Only
    // the raw socket can send two same-name headers.
    let request = format!(
        "GET /v0/version HTTP/1.1\r\nHost: {}\r\n\
         Authorization: Bearer {BEARER_VALUE}\r\n\
         Authorization: Bearer {BEARER_VALUE}\r\n\
         Connection: close\r\n\r\n",
        server.addr
    );
    let (status, body) = raw_http(&server.addr, &request);
    assert_eq!(
        status, 401,
        "two Authorization fields are an ambiguous presentation, not first-wins: {body}"
    );

    server.stop().expect("serve must shut down cleanly");
}

#[test]
fn every_error_is_an_envelope_extractor_rejections_and_routing_included() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    // Routing rejections are envelopes TOO (review item 9) — and both stay
    // BEHIND the auth layer: without a bearer an unmatched path answers 401
    // (routing never leaks a route map).
    let mut response = client
        .get(format!("http://{}/v0/no-such-route", server.addr))
        .call()
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "the auth layer wraps the fallbacks"
    );
    let _ = response.body_mut().read_to_string();

    // Unmatched path with a bearer → envelope 404.
    let (status, body) = raw_http(
        &server.addr,
        &format!(
            "GET /v0/no-such-route HTTP/1.1\r\nHost: {}\r\n\
             Authorization: Bearer {BEARER_VALUE}\r\nConnection: close\r\n\r\n",
            server.addr
        ),
    );
    assert_eq!(status, 404);
    let json = assert_envelope(&body, "not_found");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/v0/no-such-route"),
        "the 404 names the path: {json}"
    );

    // Known path, wrong method → envelope 405.
    let (status, body) = raw_http(
        &server.addr,
        &format!(
            "PUT /v0/version HTTP/1.1\r\nHost: {}\r\n\
             Authorization: Bearer {BEARER_VALUE}\r\n\
             Content-Type: application/json\r\nContent-Length: 2\r\n\
             Connection: close\r\n\r\n{{}}",
            server.addr
        ),
    );
    assert_eq!(status, 405, "a wrong method is an envelope, not a bare 405");
    assert_envelope(&body, "method_not_allowed");

    // Extractor rejections are envelopes with their OWN status classes:
    // malformed JSON (content-type json) → 400.
    let bad_body = "this is not json";
    let (status, body) = raw_http(
        &server.addr,
        &format!(
            "POST /v0/instances HTTP/1.1\r\nHost: {}\r\n\
             Authorization: Bearer {BEARER_VALUE}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{bad_body}",
            server.addr,
            bad_body.len()
        ),
    );
    assert_eq!(status, 400, "malformed JSON is a 400 envelope");
    assert_envelope(&body, "invalid_body");

    // A body with NO content-type at all → 415 envelope (never silently
    // "no body" — review item 9's extractor half).
    let jsonish = r#"{"name":"x","kind":"mock"}"#;
    let (status, body) = raw_http(
        &server.addr,
        &format!(
            "POST /v0/instances HTTP/1.1\r\nHost: {}\r\n\
             Authorization: Bearer {BEARER_VALUE}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{jsonish}",
            server.addr,
            jsonish.len()
        ),
    );
    assert_eq!(
        status, 415,
        "a body without a content-type is refused, not ignored"
    );
    assert_envelope(&body, "invalid_body");

    // A JSON body that parses but mismatches the target type → 422 envelope.
    let mut response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"completely": "wrong"}))
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a typed mismatch keeps the 422 class"
    );
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_body");

    server.stop().expect("serve must shut down cleanly");
}

#[test]
fn the_stop_body_must_parse_when_present_and_only_absence_means_the_default() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return; // the fake agent is POSIX (`sh`); see the lifecycle test.
    }
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _manifest = registered_fake_agent(&base, "stopp");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let stop_url = format!("http://{}/v0/instances/stopp/stop", server.addr);
    let start_url = format!("http://{}/v0/instances/stopp/start", server.addr);
    let start = || {
        let mut response = client
            .post(&start_url)
            .header("Authorization", &auth)
            .send("")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.body_mut().read_to_string();
    };

    start();
    // A truly ABSENT body → the engine's default window (review item 10's
    // positive half).
    let mut response = client
        .post(&stop_url)
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an absent body is the default window"
    );
    let _ = response.body_mut().read_to_string();

    start();
    // A PRESENT body must PARSE — a malformed body is an envelope 400, never
    // a silent "no body" (review item 10).
    let mut response = client
        .post(&stop_url)
        .header("Authorization", &auth)
        .send("definitely not json")
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a malformed body is refused, not treated as absent"
    );
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_body");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("window_secs"),
        "the refusal names the shape: {json}"
    );

    // A PRESENT, VALID body parses regardless of the content-type spelling
    // (the rule is presence + validity, not the header).
    let mut response = client
        .post(&stop_url)
        .header("Authorization", &auth)
        .send("{}")
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an empty object body = the default window"
    );
    let _ = response.body_mut().read_to_string();

    // The bounds still hold (the presence rule's neighborhood, re-pinned on
    // the route).
    let mut response = client
        .post(&stop_url)
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"window_secs": 9_999_999}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = response.body_mut().read_to_string();

    server.stop().unwrap();
}

#[test]
fn register_answers_a_committed_201_with_location_and_refuses_relative_paths() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    // A RELATIVE manifest path is a 400 naming the rule (review item 7) —
    // it would otherwise resolve against the host's working directory, an
    // invisible dependency the API contract does not define.
    let mut response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"name": "rely", "manifest": "adapters/echo"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "invalid_body");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ABSOLUTE"),
        "the refusal names the rule: {json}"
    );

    // A committed registration answers 201 with `Location` AND the
    // read-back body (review item 13).
    let mut response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"name": "located", "kind": "mock"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("/v0/instances/located"),
        "the Location header carries the new resource"
    );
    let json = json_body(&mut response);
    assert_eq!(json["instance"]["name"], "located");

    server.stop().unwrap();
}

#[test]
fn the_config_put_surfaces_the_steering_warnings_over_the_wire() {
    let state = tempfile::tempdir().unwrap();
    // A manifest whose `[config.model]` maps onto a FLAG target — the shape
    // the engine's set-time steering fires on (the engine registry's own
    // pin, one hop up through the API; the native `mock` maps model → env
    // MODEL, the quiet path). Registration launches nothing, so this runs
    // on every OS.
    let manifest = tempfile::tempdir().unwrap();
    std::fs::write(
        manifest.path().join("adapter.toml"),
        "contract_version = \"1.0.0\"\n\n\
         [adapter]\nkind = \"flagy\"\n\n\
         [lifecycle.start]\nexec = \"the-agent\"\n\n\
         [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
         [metering]\nsource = \"self-reported\"\n\n\
         [config.model]\nflag = \"--model\"\n",
    )
    .unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    let mut response = client
        .post(format!("http://{}/v0/instances", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({
            "name": "flagy",
            "manifest": manifest.path().display().to_string()
        }))
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "the flag-target manifest registers"
    );
    let _ = response.body_mut().read_to_string();

    // A secret:NAME value steered onto the FLAG-targeted key must WARN —
    // through the REAL route (review item 23; AI-18: warnings ride the
    // body, the host's stderr is not the client's).
    let mut response = client
        .put(format!("http://{}/v0/instances/flagy/config", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"key": "model", "value": "secret:MY_SECRET"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    let warnings = json["warnings"].as_array().expect("a warnings array");
    assert!(
        !warnings.is_empty(),
        "a secret steered onto a flag target must warn over the wire: {json}"
    );
    let joined = warnings
        .iter()
        .map(|w| w.as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        joined.contains("model"),
        "the warning names the key: {joined}"
    );
    // And the read-back renders the value MASKED (no reveal path here).
    let leaf = json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "model")
        .expect("the model leaf rides the read-back");
    let rendered = leaf["value"].as_str().unwrap_or("");
    assert!(
        !rendered.contains("MY_SECRET"),
        "no secret value on the wire: {leaf}"
    );
    assert!(rendered.contains("secret:"), "the mask shape: {leaf}");

    server.stop().unwrap();
}

#[test]
fn a_non_loopback_bind_without_tls_is_refused_before_anything_binds_or_opens() {
    // Story 21-4 (AD-20 amended 2026-10-30): beyond loopback the Control API
    // REQUIRES TLS — a plain non-loopback --listen is a usage refusal before
    // the lock, the bind or the engine open (no state.db appears).
    let state = tempfile::tempdir().unwrap();
    let base = state.path().join("never-created");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let err = hekma_host::serve_with_signal(
        hekma_host::ServeOptions {
            state_base: Some(base.clone()),
            control_listen: Some("0.0.0.0:0".parse().unwrap()),
            bearer_secret: Some(BEARER_NAME.to_string()),
            diagnostics: Some(diagnostic_sink_of(&sink_handle)),
            ..Default::default()
        },
        |_sink| async { Ok("SIGTEST") },
    )
    .expect_err("a plain non-loopback listener refuses");
    assert!(
        matches!(err, hekma_host::ServeError::InvalidOptions { .. }),
        "the usage class: {err}"
    );
    assert!(err.to_string().contains("--tls-cert"), "{err}");
    assert!(!base.exists(), "nothing was created or opened");
    // One TLS flag alone is refused too, and so is TLS without --listen.
    for (cert, key, listen) in [
        (Some("c.pem"), None, Some("127.0.0.1:0")),
        (None, Some("k.pem"), Some("127.0.0.1:0")),
        (Some("c.pem"), Some("k.pem"), None),
    ] {
        let err = hekma_host::serve_with_signal(
            hekma_host::ServeOptions {
                state_base: Some(base.clone()),
                control_listen: listen.map(|addr| addr.parse().unwrap()),
                bearer_secret: listen.map(|_| BEARER_NAME.to_string()),
                tls_cert: cert.map(PathBuf::from),
                tls_key: key.map(PathBuf::from),
                ..Default::default()
            },
            |_sink| async { Ok("SIGTEST") },
        )
        .expect_err("a half TLS configuration refuses");
        assert!(
            matches!(err, hekma_host::ServeError::InvalidOptions { .. }),
            "{err}"
        );
    }
    // An unreadable PEM is refused naming the path — before the lock.
    let err = hekma_host::serve_with_signal(
        hekma_host::ServeOptions {
            state_base: Some(base.clone()),
            control_listen: Some("0.0.0.0:0".parse().unwrap()),
            bearer_secret: Some(BEARER_NAME.to_string()),
            tls_cert: Some(state.path().join("missing-cert.pem")),
            tls_key: Some(state.path().join("missing-key.pem")),
            ..Default::default()
        },
        |_sink| async { Ok("SIGTEST") },
    )
    .expect_err("a missing certificate refuses");
    assert!(err.to_string().contains("missing-cert.pem"), "{err}");
    assert!(!base.exists(), "nothing was created or opened");
    // The autostart mode refuses the explicit-listener flags.
    let err = hekma_host::serve_with_signal(
        hekma_host::ServeOptions {
            state_base: Some(base.clone()),
            control_listen: Some("127.0.0.1:0".parse().unwrap()),
            bearer_secret: Some(BEARER_NAME.to_string()),
            autostart: true,
            ..Default::default()
        },
        |_sink| async { Ok("SIGTEST") },
    )
    .expect_err("autostart with --listen refuses");
    assert!(err.to_string().contains("--autostart"), "{err}");
    assert!(!base.exists(), "nothing was created or opened");
}

/// A self-signed certificate for `127.0.0.1`, written as PEM files.
fn self_signed(dir: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
    let certified =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into(), "localhost".into()]).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.signing_key.serialize_pem()).unwrap();
    (cert, key, certified.cert.der().to_vec())
}

#[test]
fn a_tls_listener_answers_https_and_refuses_plain_http() {
    // Story 21-4: with --tls-cert/--tls-key the operator's listener serves
    // HTTPS (any address — here 0.0.0.0, the container recipe); a plain-HTTP
    // request fails the handshake and is never served. The local endpoint
    // stays loopback plain HTTP.
    let state = tempfile::tempdir().unwrap();
    let pems = tempfile::tempdir().unwrap();
    let (cert, key, der) = self_signed(pems.path());
    let (sink_handle, _diagnostics) = SharedSink::new();
    write_secrets_file(state.path());
    let options = hekma_host::ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        control_listen: Some("0.0.0.0:0".parse().unwrap()),
        bearer_secret: Some(BEARER_NAME.to_string()),
        stop_window: Some(1),
        diagnostics: Some(diagnostic_sink_of(&sink_handle)),
        tls_cert: Some(cert),
        tls_key: Some(key),
        ..Default::default()
    };
    let server = spawn_serve(options);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let addr = loop {
        let text = sink_handle.text();
        if let Some(addr) = text
            .lines()
            .find_map(|line| line.split("control api listening on ").nth(1))
            .map(|rest| rest.split(" (").next().unwrap().to_string())
        {
            break addr;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the control api never reported listening:\n{}",
            sink_handle.text()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        sink_handle.text().contains("bearer required; https)"),
        "the notice names the scheme:\n{}",
        sink_handle.text()
    );
    let port = addr.rsplit(':').next().unwrap().to_string();
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::new_with_certs(&[
            ureq::tls::Certificate::from_der(&der).to_owned(),
        ]))
        .build();
    let https: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let mut response = https
        .get(format!("https://127.0.0.1:{port}/v0/version"))
        .header("Authorization", format!("Bearer {BEARER_VALUE}"))
        .call()
        .expect("the HTTPS call is answered");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.body_mut().read_to_string().unwrap();
    assert!(body.contains("hekma-control-api"), "{body}");
    // The local operator token is the same credential class: served too.
    let discovery = hekma_host::read_discovery(state.path()).unwrap();
    let response = https
        .get(format!("https://127.0.0.1:{port}/v0/version"))
        .header("Authorization", format!("Bearer {}", discovery.token))
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // A wrong bearer over TLS is still the static 401.
    let response = https
        .get(format!("https://127.0.0.1:{port}/v0/version"))
        .header("Authorization", "Bearer wrong")
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // Plain HTTP on the TLS port is never served.
    let plain: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    assert!(
        plain
            .get(format!("http://127.0.0.1:{port}/v0/version"))
            .header("Authorization", format!("Bearer {BEARER_VALUE}"))
            .call()
            .is_err(),
        "plain HTTP must not be answered on the TLS listener"
    );
    // The local endpoint answers plain HTTP with the local token, and the
    // --bearer-secret value works there too (one resolver, two listeners).
    for token in [discovery.token.as_str(), BEARER_VALUE] {
        let response = plain
            .get(format!("http://{}/v0/version", discovery.addr))
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    server.stop().expect("serve must shut down cleanly");
}

#[test]
fn an_empty_resolved_secret_refuses_startup_naming_the_reference() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // A secrets file whose value is the EMPTY string: the composite RESOLVES
    // the name (it exists) — but an empty bearer can never authenticate, so
    // startup refuses, typed, naming the reference (review item 2).
    let secrets = base.join(hekma_engine::paths::SECRETS_FILE);
    std::fs::write(&secrets, format!("{BEARER_NAME} = \"\"\n")).unwrap();
    #[cfg(unix)]
    chmod_600(&secrets);
    let (sink_handle, _diagnostics) = SharedSink::new();
    let options = hekma_host::ServeOptions {
        state_base: Some(base.clone()),
        control_listen: Some("127.0.0.1:0".parse().unwrap()),
        bearer_secret: Some(BEARER_NAME.to_string()),
        diagnostics: Some(diagnostic_sink_of(&sink_handle)),
        ..hekma_host::ServeOptions::default()
    };
    let result = hekma_host::serve_with_signal(options, |_| std::future::ready(Ok("SIGTEST")));
    match result {
        Err(hekma_host::ServeError::Startup { message }) => {
            assert!(
                message.contains("EMPTY value"),
                "the refusal names the empty resolved secret"
            );
            assert!(
                message.contains(BEARER_NAME),
                "the refusal names the secret reference"
            );
        }
        other => panic!("an empty resolved secret must refuse startup: {other:?}"),
    }
    assert!(
        !base.join(hekma_engine::paths::STATE_DB_FILE).exists(),
        "the engine must never open for an empty bearer secret"
    );
    drop(sink_handle);
}

#[test]
fn a_stalled_client_cannot_hang_the_shutdown_the_abort_is_surfaced() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle.clone());

    // A NON-COOPERATIVE client: a register request whose body never arrives
    // (Content-Length promises more bytes than are sent). The JSON
    // extractor parks the handler waiting for the rest — the stalled
    // connection the graceful window must NOT wait on forever.
    let mut stalled = std::net::TcpStream::connect(&server.addr).unwrap();
    use std::io::Write;
    stalled
        .write_all(
            format!(
                "POST /v0/instances HTTP/1.1\r\nHost: {}\r\n\
                 Authorization: Bearer {BEARER_VALUE}\r\n\
                 Content-Type: application/json\r\nContent-Length: 64\r\n\r\n{{",
                server.addr
            )
            .as_bytes(),
        )
        .unwrap();
    stalled.flush().unwrap();
    // Let the partial request land before the shutdown races it.
    std::thread::sleep(Duration::from_millis(300));

    // The shutdown still returns — bounded (review item 25): the 1s grace
    // expires, the serve task aborts, the drain runs, serve ends.
    let started = std::time::Instant::now();
    server
        .stop()
        .expect("serve must shut down cleanly despite the stalled client");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "the shutdown must be bounded even with a stalled client (took {elapsed:?})"
    );
    // And the abort is SURFACED (review item 16 / AI-18) — never silent.
    let text = sink_handle.text();
    assert!(text.contains("control api stopped"), "{text}");
    assert!(
        text.contains("still held a connection"),
        "the abort backstop is surfaced, not silent:\n{text}"
    );
}

#[test]
fn concurrent_config_puts_serialize_per_host_and_each_read_back_reflects_its_own_write() {
    // Review item 8's pin: the host's config-WRITE lock serializes
    // write+read-back, so eight concurrent PUTs of DISTINCT values to the
    // SAME key each get a read-back carrying the value ITS OWN request
    // wrote — never a racing sibling's. (Without the lock this assertion is
    // racy: a loser's read-back can observe the winner's value; with it, it
    // is deterministic.)
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _manifest = registered_fake_agent(&base, "conc");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let url = format!("http://{}/v0/instances/conc/config", server.addr);

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let client = client.clone();
            let auth = auth.clone();
            let url = url.clone();
            std::thread::spawn(move || {
                let written = format!("{}", 1000 + i);
                let mut response = client
                    .put(&url)
                    .header("Authorization", &auth)
                    .send_json(serde_json::json!({
                        "key": "budget.tokens.per_run",
                        "value": written
                    }))
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let json = json_body(&mut response);
                let leaf = json["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["key"] == "budget.tokens.per_run")
                    .expect("the written key rides the read-back")
                    .clone();
                (written, leaf["value"].as_str().unwrap_or("").to_string())
            })
        })
        .collect();
    for handle in handles {
        let (written, read_back) = handle.join().unwrap();
        assert_eq!(
            read_back, written,
            "each PUT's read-back must reflect its own write"
        );
    }

    server.stop().unwrap();
}

#[test]
fn un_acked_events_repull_after_an_acked_prefix_rotates_away() {
    // Review finding 25 (the spec's promised host-level leg, driven END-TO-END
    // through /v0/export): a journal near the 30MB cap, an acked PREFIX, a
    // real lifecycle append that crosses the cap (the append-time compaction
    // rotates the acked prefix away) — and the default pull afterwards
    // delivers exactly the UN-ACKED tail through the API. Nothing re-delivered,
    // nothing silently skipped.
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();

    // Stage a pre-upgrade journal SYNTHETICALLY near the cap, BEFORE the
    // first engine opens the state dir (that open imports it into state.db
    // with its seqs, as an upgrade does): 400 fat transition
    // entries (a ~100KB `at` payload each ≈ 41MB — already over the cap at
    // rest, which the check-before-write contract tolerates: only an APPEND
    // compacts, and open's rederive reads without compacting). Seqs 1..=400.
    let fat = "x".repeat(100 * 1024);
    let journal = base.join(hekma_engine::paths::OUTBOX_JOURNAL_FILE);
    let mut staged = String::with_capacity(400 * (fat.len() + 300));
    for seq in 1..=400u64 {
        let entry = hekma_engine::OutboxEntry {
            schema_version: hekma_engine::OUTBOX_SCHEMA_VERSION,
            seq,
            event: hekma_engine::EngineEvent::Transition(hekma_engine::TransitionEvent::new(
                "probe",
                hekma_engine::LifecycleState::Starting,
                hekma_engine::LifecycleState::Running,
                hekma_engine::TransitionCause::AdapterReady,
                format!("2026-10-19T00:00:00Z{fat}"),
            )),
        };
        staged.push_str(&serde_json::to_string(&entry).unwrap());
        staged.push('\n');
    }
    std::fs::write(&journal, staged).unwrap();
    let _manifest = registered_fake_agent(&base, "worker");

    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let instance = format!("http://{}/v0/instances/worker", server.addr);

    // ACK a PREFIX (350 of 400): 200, persisted.
    let mut response = client
        .post(format!("http://{}/v0/export/ack", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"seq": 350}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["seq"], 350);

    // Force the rotation: a REAL lifecycle append (seq 401) crosses the cap —
    // the append-time compaction drops the ACKED prefix (1..=350) and keeps
    // the un-acked tail (351..=400) — NEVER the whole journal, never the
    // last entry.
    let response = client
        .post(format!("{instance}/start"))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The default pull resumes from the ACK (350): the UN-ACKED tail comes
    // back — 351..=tail — even though 1..=350 were physically rotated away.
    // (The start commits TWO transitions — starting→running and the ready
    // cause — so the tail is 402; the contract under test is "everything
    // after the ack, in order", not a specific count.)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let (delivered, retained_from) = loop {
        let mut response = get_ok(&client, &server.addr, "/v0/export");
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(&mut response);
        let events = json["events"].as_array().expect("events array");
        if !events.is_empty() {
            let delivered: Vec<u64> = events
                .iter()
                .map(|e| e["seq"].as_u64().expect("each entry carries its seq"))
                .collect();
            break (delivered, json["retained_from"].as_u64().unwrap());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the post-rotation pull never delivered the un-acked tail"
        );
        std::thread::sleep(std::time::Duration::from_millis(40));
    };
    assert_eq!(
        delivered.first(),
        Some(&351),
        "the pull resumes EXACTLY after the ack: {delivered:?}"
    );
    assert!(
        delivered.last() >= Some(&401),
        "the real lifecycle append(s) are in the delivered tail: {delivered:?}"
    );
    assert!(
        delivered.windows(2).all(|w| w[1] == w[0] + 1),
        "one contiguous order, no gaps: {delivered:?}"
    );
    assert_eq!(
        retained_from, 351,
        "the batch names the NEW retention floor (1..=350 rotated away)"
    );

    server.stop().unwrap();
}

#[test]
fn export_limit_parse_failures_are_versioned_400s_never_extractor_text() {
    // Review finding 16: `limit` is parsed INSIDE the handler so every
    // failure is a versioned `400 invalid_body` envelope — axum's typed
    // `Option<u32>` extractor would reject a non-numeric value as plain
    // text OUTSIDE the envelope. `limit=0` is refused outright (it would
    // otherwise silently deliver the DEFAULT batch — a lie about the
    // request; the ABSENT parameter means default).
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();

    let cases = [
        ("abc", "must be a positive whole number"),
        ("-1", "must be a positive whole number"),
        ("0", "must be >= 1"),
        ("99999999999999", "must be a positive whole number"),
    ];
    for (value, expected_fragment) in cases {
        let mut response = get_ok(&client, &server.addr, &format!("/v0/export?limit={value}"));
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "limit={value} must be a 400"
        );
        let json = json_body(&mut response);
        assert_eq!(json["schema_version"], 1, "the envelope is versioned");
        assert_eq!(json["error"]["kind"], "invalid_body", "limit={value}");
        let message = json["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("limit query parameter"),
            "the refusal names the parameter: {message}"
        );
        assert!(
            message.contains(expected_fragment),
            "limit={value}: the refusal names the rule ({expected_fragment}): {message}"
        );
    }

    // The boundary sanity: an omitted limit and a valid limit both serve.
    for query in ["", "?limit=1"] {
        let mut response = get_ok(&client, &server.addr, &format!("/v0/export{query}"));
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "limit query {query:?} serves"
        );
        let json = json_body(&mut response);
        assert_eq!(json["schema_version"], 1);
    }

    server.stop().unwrap();
}

#[test]
fn the_run_routes_open_show_and_close_a_child_run_with_typed_refusals() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        // The manifest fake agent is the POSIX shape (see the lifecycle test).
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _manifest = registered_fake_agent(&base, "host");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let runs = format!("http://{}/v0/runs", server.addr);
    let post = |url: &str, body: serde_json::Value| {
        client
            .post(url)
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .unwrap()
    };

    // A configured bound label the started Run will carry.
    let mut response = client
        .put(format!("http://{}/v0/instances/host/config", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"key": "labels.team", "value": "payments"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(json_body(&mut response));

    // No open Run yet: 409 with the stable code.
    let mut response = post(&runs, serde_json::json!({"parent_instance": "host"}));
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["kind"], "conflict");
    assert_eq!(json["error"]["code"], "no-open-run");

    let mut response = client
        .post(format!("http://{}/v0/instances/host/start", server.addr))
        .header("Authorization", &auth)
        .send("")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(json_body(&mut response));

    // Exactly one parent.
    let mut response = post(&runs, serde_json::json!({}));
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&mut response)["error"]["kind"], "invalid_body");
    let mut response = post(
        &runs,
        serde_json::json!({"parent_instance": "host", "breach_action": "explode"}),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    drop(json_body(&mut response));
    let mut response = post(
        &runs,
        serde_json::json!({"parent_instance": "host", "dollar_cap": "-1"}),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    drop(json_body(&mut response));
    // A `pause` on a child in a process with no wire path is refused.
    let mut response = post(
        &runs,
        serde_json::json!({"parent_instance": "host", "breach_action": "pause"}),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "action-unavailable"
    );

    // Open: 201 + Location + the Run.
    let mut response = post(
        &runs,
        serde_json::json!({
            "parent_instance": "host",
            "labels": {"task": "review"},
            "token_budget": 500,
            "dollar_cap": "1.50",
            "breach_action": "warn",
            "external_id": "job-1"
        }),
    );
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let json = json_body(&mut response);
    let run_id = json["run"]["run_id"].as_str().unwrap().to_string();
    assert_eq!(location, format!("/v0/runs/{run_id}"));
    assert_eq!(json["run"]["control_depth"], "announced");
    assert_eq!(json["run"]["wire_path"], false);
    assert_eq!(json["run"]["labels"][0]["source"], "caller");
    assert_eq!(json["run"]["limits"]["token_per_run"], 500);
    // A retried open with the same external id is the same Run.
    let mut response = post(
        &runs,
        serde_json::json!({"parent_instance": "host", "external_id": "job-1"}),
    );
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(json_body(&mut response)["run"]["run_id"], run_id.as_str());

    // Show it, and its parent through the parent id.
    let mut response = get_ok(&client, &server.addr, &location);
    let json = json_body(&mut response);
    let parent = json["run"]["parent_run_id"].as_str().unwrap().to_string();
    let mut response = get_ok(&client, &server.addr, &format!("/v0/runs/{parent}"));
    let json = json_body(&mut response);
    assert_eq!(json["run"]["control_depth"], "supervised");
    assert_eq!(json["run"]["open_children"], 1);

    // Bound labels (story 21-9). The parent carries its configured label
    // (origin `config`); the verb binds over the child's Caller label.
    let patch = |url: &str, body: serde_json::Value| {
        client
            .patch(url)
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .unwrap()
    };
    let mut response = get_ok(&client, &server.addr, &format!("/v0/runs/{parent}/labels"));
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["run_id"], parent.as_str());
    assert_eq!(json["labels"][0]["key"], "team");
    assert_eq!(json["labels"][0]["origin"], "config");
    let child_labels = format!("{runs}/{run_id}/labels");
    let mut response = patch(
        &child_labels,
        serde_json::json!({"set": {"task": "triage"}, "dry_run": true}),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["changed"], true);
    assert_eq!(json["labels"][0]["origin"], "label-verb");
    let mut response = get_ok(&client, &server.addr, &format!("/v0/runs/{run_id}/labels"));
    assert_eq!(
        json_body(&mut response)["labels"][0]["source"],
        "caller",
        "a dry run writes nothing"
    );
    let mut response = patch(
        &child_labels,
        serde_json::json!({"set": {"task": "triage"}}),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["changed"], true);
    assert_eq!(json["prior_labels"][0]["source"], "caller");
    assert_eq!(json["prior_labels"][0]["value"], "review");
    assert_eq!(json["labels"][0]["source"], "bound");
    let mut response = patch(
        &child_labels,
        serde_json::json!({"set": {"task": "triage"}}),
    );
    assert_eq!(json_body(&mut response)["changed"], false);
    // The configured key on the process-owning Run cannot be removed.
    let mut response = patch(
        &format!("{runs}/{parent}/labels"),
        serde_json::json!({"remove": ["team"]}),
    );
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["code"], "label-from-config");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("labels.team"),
        "{json}"
    );

    // Close the child; closing the process-owning Run is a 409.
    let mut response = post(&format!("{runs}/{run_id}/close"), serde_json::json!({}));
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["run"]["status"], "ended");
    assert_eq!(json["run"]["end_cause"], "closed");
    let mut response = post(&format!("{runs}/{parent}/close"), serde_json::json!({}));
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(&mut response)["error"]["code"], "owns-process");
    // An ended Run's labels cannot change.
    let mut response = patch(&child_labels, serde_json::json!({"remove": ["task"]}));
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(&mut response)["error"]["code"], "run-ended");
    let mut response = get_ok(&client, &server.addr, "/v0/runs/run_ghost");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_body(&mut response)["error"]["code"], "run-not-found");

    let _ = client
        .post(format!("http://{}/v0/instances/host/stop", server.addr))
        .header("Authorization", &auth)
        .send("");
}

#[test]
fn the_label_routes_refuse_malformed_changes_and_unknown_runs_on_every_os() {
    // Story 21-9: no agent process needed — the change is validated before
    // the Run is looked up, and a valid change on an unknown Run is a 404.
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let url = format!("http://{}/v0/runs/run_ghost/labels", server.addr);
    let patch = |body: &str| {
        client
            .patch(&url)
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .unwrap()
    };
    // The engine's grammar refusals are 400; a body that is not JSON is the
    // extractor's 400, and a wrong type or an unknown field (a misspelled
    // `dry_run` must never become a real write) is its typed 422.
    for (body, status) in [
        (r#"{}"#, 400),
        (r#"{"set": {"Team": "core"}}"#, 400),
        (r#"{"set": {"team": "has space"}}"#, 400),
        (r#"{"set": {"team": "core"}, "remove": ["team"]}"#, 400),
        (r#"not json"#, 400),
        (r#"{"set": "team=core"}"#, 422),
        (r#"{"set": {"team": "core"}, "dry-run": true}"#, 422),
        (r#"{"set": {"team": "core"}, "dryRun": true}"#, 422),
    ] {
        let mut response = patch(body);
        assert_eq!(response.status().as_u16(), status, "{body}");
        assert_eq!(
            json_body(&mut response)["error"]["kind"],
            "invalid_body",
            "{body}"
        );
    }
    let mut response = patch(r#"{"set": {"team": "core"}}"#);
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_body(&mut response)["error"]["code"], "run-not-found");
    let mut response = get_ok(&client, &server.addr, "/v0/runs/run_ghost/labels");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    drop(json_body(&mut response));
    server.stop().unwrap();
}

// ---------------------------------------------------------------------------
// Story 21-4: the routes every CLI write needs (send, memory, restart policy,
// remove's disposition/force, start's detach), the engine error on the wire,
// and the auto-started host's lifetime.
// ---------------------------------------------------------------------------

/// A JSON request with the valid bearer; `body: None` sends no body.
fn call(
    client: &ureq::Agent,
    method: &str,
    url: &str,
    body: Option<serde_json::Value>,
) -> ureq::http::Response<ureq::Body> {
    let auth = format!("Bearer {BEARER_VALUE}");
    match (method, body) {
        ("POST", Some(json)) => client
            .post(url)
            .header("Authorization", &auth)
            .send_json(json),
        ("POST", None) => client.post(url).header("Authorization", &auth).send_empty(),
        ("PUT", Some(json)) => client
            .put(url)
            .header("Authorization", &auth)
            .send_json(json),
        ("PATCH", Some(json)) => client
            .patch(url)
            .header("Authorization", &auth)
            .send_json(json),
        ("PATCH", None) => client
            .patch(url)
            .header("Authorization", &auth)
            .send_empty(),
        ("DELETE", _) => client.delete(url).header("Authorization", &auth).call(),
        (other, _) => panic!("unsupported test method {other}"),
    }
    .unwrap_or_else(|err| panic!("{method} {url}: {err}"))
}

/// A client that waits past the stop ladder's bound (the default 30 s
/// window plus the forced kill's confirmation): a forced remove of a running
/// instance stops it first, and on Windows — no graceful signal is sent yet —
/// the whole window elapses before the kill.
fn ladder_client() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_recv_response(Some(Duration::from_secs(90)))
        .timeout_recv_body(Some(Duration::from_secs(10)))
        .build();
    ureq::Agent::new_with_config(config)
}

#[test]
fn the_story_21_4_routes_project_every_cli_write() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // A compiled fake_agent that echoes stdin (cross-OS: no `sh`).
    let manifest_dir = tempfile::tempdir().unwrap();
    ManifestFixture::fake_agent("worker", &["--echo-stdin", "--linger-ms", "600000"])
        .write(manifest_dir.path());
    let engine = Engine::open(Some(base.clone())).expect("engine open");
    engine
        .blocking()
        .register_with_adapter(
            "worker",
            &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
        )
        .expect("register");
    engine.blocking().register("mem", "mock").expect("register");
    drop(engine);
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base.clone(), sink_handle);
    let client = client();
    let instances = format!("http://{}/v0/instances", server.addr);

    // A present start body must parse.
    let mut response = client
        .post(format!("{instances}/worker/start"))
        .header("Authorization", format!("Bearer {BEARER_VALUE}"))
        .header("Content-Type", "application/json")
        .send("not json")
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&mut response)["error"]["kind"], "invalid_body");

    // start (an explicit non-detached body) → running, session-supervised.
    let mut response = call(
        &client,
        "POST",
        &format!("{instances}/worker/start"),
        Some(serde_json::json!({"detach": false})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["instance"]["state"], "running");
    assert_eq!(json["instance"]["supervision"], "session");
    // The status read names the host's live capture of an agent it spawned
    // (a raw-log reader's dedupe question).
    let mut response = get_ok(&client, &server.addr, "/v0/instances/worker/status");
    assert_eq!(json_body(&mut response)["live_capture"], true);
    let mut response = get_ok(&client, &server.addr, "/v0/instances/mem/status");
    assert_eq!(json_body(&mut response)["live_capture"], false);

    // send → 204, and the line reaches the agent's stdin (it echoes it).
    let mut response = call(
        &client,
        "POST",
        &format!("{instances}/worker/send"),
        Some(serde_json::json!({"text": "hello-21-4"})),
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.body_mut().read_to_string().unwrap(), "");
    let agent_log = base
        .join("agents")
        .join("worker")
        .join("logs")
        .join("agent.log");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&agent_log)
        .unwrap_or_default()
        .contains("stdin: hello-21-4")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the sent line never reached the agent"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // send to an unknown instance: the 404 envelope carries the engine's
    // typed error, decodable back into the engine's own value.
    let mut response = call(
        &client,
        "POST",
        &format!("{instances}/ghost/send"),
        Some(serde_json::json!({"text": "x"})),
    );
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["engine_error"]["family"], "engine");
    match hekma_host::wire::decode(&json["error"]["engine_error"]) {
        Some(hekma_host::wire::WireError::Engine(hekma_engine::EngineError::NotFound { name })) => {
            assert_eq!(name, "ghost")
        }
        other => panic!("{other:?}"),
    }
    // A send body without `text` is the extractor's typed rejection.
    let response = call(
        &client,
        "POST",
        &format!("{instances}/worker/send"),
        Some(serde_json::json!({"txt": "x"})),
    );
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // restart-policy: set, read back, and refuse an unknown policy.
    let mut response = call(
        &client,
        "PUT",
        &format!("{instances}/worker/restart-policy"),
        Some(serde_json::json!({"policy": "never"})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["status"]["restart_policy"], "never", "{json}");
    let mut response = call(
        &client,
        "PUT",
        &format!("{instances}/worker/restart-policy"),
        Some(serde_json::json!({"policy": "sometimes"})),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&mut response)["error"]["kind"], "invalid_body");

    // remove: a running instance needs force; malformed query values are
    // envelope 400s.
    let mut response = call(&client, "DELETE", &format!("{instances}/worker"), None);
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(&mut response)["error"]["engine_error"]["variant"],
        "RunningRequiresForce"
    );
    for query in ["disposition=bogus", "force=maybe"] {
        let mut response = call(
            &client,
            "DELETE",
            &format!("{instances}/worker?{query}"),
            None,
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(json_body(&mut response)["error"]["kind"], "invalid_body");
    }

    // stop, then a DETACHED start: the handle is disarmed at spawn.
    let response = call(
        &client,
        "POST",
        &format!("{instances}/worker/stop"),
        Some(serde_json::json!({"window_secs": 2})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let mut response = call(
        &client,
        "POST",
        &format!("{instances}/worker/start"),
        Some(serde_json::json!({"detach": true})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut response)["instance"]["supervision"],
        "detached"
    );
    // force removes the running instance and deletes its home (it stops it
    // through the ladder first — the full default window on Windows).
    let response = call(
        &ladder_client(),
        "DELETE",
        &format!("{instances}/worker?disposition=delete&force=true"),
        None,
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(!base.join("agents").join("worker").exists());

    // memory: attach answers the directory and the read-back; a kind change
    // is the typed conflict; an unknown kind is a 400; detach is a 204.
    let mut response = call(
        &client,
        "PUT",
        &format!("{instances}/mem/memory"),
        Some(serde_json::json!({"kind": "filesystem"})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["kind"], "filesystem");
    assert_eq!(json["instance"], "mem");
    assert!(json.get("readback_error").is_none(), "{json}");
    assert!(
        Path::new(json["dir"].as_str().unwrap()).is_dir(),
        "the managed dir exists: {json}"
    );
    let mut response = call(
        &client,
        "PUT",
        &format!("{instances}/mem/memory"),
        Some(serde_json::json!({"kind": "native"})),
    );
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json = json_body(&mut response);
    assert_eq!(json["error"]["engine_error"]["family"], "registry");
    assert_eq!(
        json["error"]["engine_error"]["variant"],
        "MemoryBackingKindConflict"
    );
    let response = call(
        &client,
        "PUT",
        &format!("{instances}/mem/memory"),
        Some(serde_json::json!({"kind": "cloud"})),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = call(&client, "DELETE", &format!("{instances}/mem/memory"), None);
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // The default remove retains the Agent Home.
    let response = call(&client, "DELETE", &format!("{instances}/mem"), None);
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(base.join("agents").join("mem").exists());

    server.stop().unwrap();
}

#[test]
fn an_auto_started_host_exits_when_idle_and_removes_its_discovery_file() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let (sink_handle, _diagnostics) = SharedSink::new();
    // The signal never arrives: only the idle exit can end this host.
    let (_signal_tx, signal_rx) = tokio::sync::oneshot::channel::<&'static str>();
    let options = hekma_host::ServeOptions {
        state_base: Some(base.clone()),
        stop_window: Some(1),
        diagnostics: Some(diagnostic_sink_of(&sink_handle)),
        autostart: true,
        autostart_idle_override: Some(Duration::from_secs(2)),
        ..Default::default()
    };
    let thread = std::thread::spawn(move || {
        hekma_host::serve_with_signal(options, move |_sink| async move {
            signal_rx
                .await
                .map_err(|_| "the test signal source was dropped".to_string())
        })
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let discovery = loop {
        if let Ok(discovery) = hekma_host::read_discovery(&base) {
            break discovery;
        }
        assert!(std::time::Instant::now() < deadline, "no control.json");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(discovery.autostarted);
    // A request resets the idle clock (and is served with the local token).
    let response = client()
        .get(format!("http://{}/v0/version", discovery.addr))
        .header("Authorization", format!("Bearer {}", discovery.token))
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let started = std::time::Instant::now();
    thread
        .join()
        .expect("serve thread")
        .expect("the idle exit is a clean shutdown");
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the host waited out its idle window"
    );
    assert!(!hekma_host::control_file(&base).exists());
    let text = sink_handle.text();
    assert!(
        text.contains("auto-started host exiting (idle: no live agent and no request)"),
        "{text}"
    );
    assert!(text.contains("shutdown complete (idle"), "{text}");
}

#[test]
fn an_auto_started_host_leaves_when_its_state_root_disappears() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        // Windows refuses to delete a directory with open files: the idle
        // exit (above) is the auto host's way out there.
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    let base = parent.path().join("root");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let (_signal_tx, signal_rx) = tokio::sync::oneshot::channel::<&'static str>();
    let options = hekma_host::ServeOptions {
        state_base: Some(base.clone()),
        stop_window: Some(1),
        diagnostics: Some(diagnostic_sink_of(&sink_handle)),
        autostart: true,
        // A long idle window: only the vanished root ends this host.
        autostart_idle_override: Some(Duration::from_secs(3600)),
        ..Default::default()
    };
    let thread = std::thread::spawn(move || {
        hekma_host::serve_with_signal(options, move |_sink| async move {
            signal_rx
                .await
                .map_err(|_| "the test signal source was dropped".to_string())
        })
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while hekma_host::read_discovery(&base).is_err() {
        assert!(std::time::Instant::now() < deadline, "no control.json");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::remove_dir_all(&base).unwrap();
    thread
        .join()
        .expect("serve thread")
        .expect("the vanished root ends the host cleanly");
    assert!(
        sink_handle
            .text()
            .contains("auto-started host exiting (its state root was removed)"),
        "{}",
        sink_handle.text()
    );
}

// ---------------------------------------------------------------------------
// Story 21-3: the /v0/policies routes — CRUD, the reserved default, the
// matched/fallback counts, and the typed refusals.
// ---------------------------------------------------------------------------

#[test]
fn the_policy_routes_serve_crud_with_typed_refusals_on_every_os() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();
    let base = format!("http://{}/v0/policies", server.addr);

    // The seeded default Policy reads before any write: warn, `off`, no
    // ceilings, an empty Rate table (absent when empty).
    let mut response = get_ok(&client, &server.addr, "/v0/policies/default");
    assert_eq!(response.status(), StatusCode::OK);
    let document = json_body(&mut response);
    assert_eq!(document["policy"]["limits"]["breach_action"], "warn");
    assert_eq!(document["policy"]["limits"]["scent_check"], "off");

    // Create: 201 + the Location header, answering the matched counts.
    let mut response = call(
        &client,
        "POST",
        &base,
        Some(serde_json::json!({
            "name": "nightly",
            "selector": "team=research",
            "tokens_per_run": 2_000_000,
            "breach_action": "pause",
        })),
    );
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers()["location"].to_str().unwrap(),
        "/v0/policies/nightly"
    );
    let document = json_body(&mut response);
    assert_eq!(document["matched"], 0);
    assert_eq!(document["fallbacks"], 0);

    // A duplicate name is 409 policy-exists.
    let mut response = call(
        &client,
        "POST",
        &base,
        Some(serde_json::json!({"name": "nightly", "selector": "team=x"})),
    );
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(&mut response)["error"]["code"], "policy-exists");

    // The default Policy's reserved shape: delete and a Selector are 409
    // policy-default.
    let mut response = call(&client, "DELETE", &format!("{base}/default"), None);
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(&mut response)["error"]["code"], "policy-default");
    let mut response = call(
        &client,
        "PATCH",
        &format!("{base}/default"),
        Some(serde_json::json!({"selector": "team=core"})),
    );
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(&mut response)["error"]["code"], "policy-default");

    // Story 22-3: every Scent-Check Mode is accepted and stored (no more
    // scent-unavailable); the default goes back to `off` for the steps
    // below.
    for mode in ["enforce", "shadow", "off"] {
        let mut response = call(
            &client,
            "PATCH",
            &format!("{base}/default"),
            Some(serde_json::json!({"set": {"scent_check": mode}})),
        );
        assert_eq!(response.status(), StatusCode::OK, "{mode}");
        let body = json_body(&mut response);
        assert_eq!(body["policy"]["limits"]["scent_check"], mode, "{body}");
    }

    // The Rate table edits through PATCH's `rates`: set, read back, remove.
    let response = call(
        &client,
        "PATCH",
        &format!("{base}/default"),
        Some(serde_json::json!({
            "rates": {"set": {"gpt-demo": {
                "input_micros_per_1m": 2_000_000,
                "output_micros_per_1m": 8_000_000,
            }}},
        })),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let mut response = get_ok(&client, &server.addr, "/v0/policies/default");
    let document = json_body(&mut response);
    assert_eq!(
        document["policy"]["rates"]["gpt-demo"]["input_micros_per_1m"],
        2_000_000
    );
    let response = call(
        &client,
        "PATCH",
        &format!("{base}/default"),
        Some(serde_json::json!({"rates": {"remove": ["gpt-demo"]}})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let mut response = get_ok(&client, &server.addr, "/v0/policies/default");
    assert!(
        json_body(&mut response)["policy"]["rates"].is_null(),
        "the removed entry leaves an absent table"
    );

    // A dry run plans and writes nothing; unknown fields are 422.
    let mut response = call(
        &client,
        "PATCH",
        &format!("{base}/nightly"),
        Some(serde_json::json!({"set": {"tokens_per_run": 9_000_000}, "dry_run": true})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(&mut response)["dry_run"], true);
    let mut response = get_ok(&client, &server.addr, "/v0/policies/nightly");
    assert_eq!(
        json_body(&mut response)["policy"]["limits"]["tokens_per_run"],
        2_000_000,
        "the dry run wrote nothing"
    );
    let response = call(
        &client,
        "PATCH",
        &format!("{base}/nightly"),
        Some(serde_json::json!({"tokens_per_run": 9})),
    );
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Unknown fields are refused at EVERY level: a misspelled limit inside
    // `set` (or inside a Rate) would otherwise be silently dropped, and an
    // unknown `clear` name silently clear nothing.
    let response = call(
        &client,
        "PATCH",
        &format!("{base}/nightly"),
        Some(serde_json::json!({"set": {"tokens_per_rn": 5}})),
    );
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let response = call(
        &client,
        "PATCH",
        &format!("{base}/nightly"),
        Some(serde_json::json!({"set": {"rate": {
            "input_micros_per_1m": 1, "output_micros_per_1m": 1, "input_micros": 2,
        }}})),
    );
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // A Rate needs BOTH directions in the body: a missing one (or an
    // empty object) is a 422, never an implicit $0; a $0 direction is the
    // engine's 400 refusal.
    for body in [
        serde_json::json!({"rates": {"set": {"gpt-x": {"input_micros_per_1m": 3_000_000}}}}),
        serde_json::json!({"rates": {"set": {"gpt-x": {"output_micros_per_1m": 3_000_000}}}}),
        serde_json::json!({"rates": {"set": {"gpt-x": {}}}}),
        serde_json::json!({"set": {"rate": {"input_micros_per_1m": 1}}}),
    ] {
        let response = call(&client, "PATCH", &format!("{base}/nightly"), Some(body));
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    let response = call(
        &client,
        "PATCH",
        &format!("{base}/nightly"),
        Some(serde_json::json!({"set": {"rate": {
            "input_micros_per_1m": 0, "output_micros_per_1m": 1_000_000,
        }}})),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let mut response = call(
        &client,
        "PATCH",
        &format!("{base}/nightly"),
        Some(serde_json::json!({"clear": ["tokens-per-rn"]})),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "invalid-request",
        "an unknown clear name is an engine refusal"
    );

    // The list carries every Policy, `default` included.
    let mut response = get_ok(&client, &server.addr, "/v0/policies");
    let document = json_body(&mut response);
    let names: Vec<&str> = document["policies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|policy| policy["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"default"), "{names:?}");
    assert!(names.contains(&"nightly"), "{names:?}");

    // A create dry run plans and writes nothing (200, no Location).
    let response = call(
        &client,
        "POST",
        &base,
        Some(serde_json::json!({
            "name": "previewed", "selector": "team=x", "tokens_per_run": 5, "dry_run": true,
        })),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let response = get_ok(&client, &server.addr, "/v0/policies/previewed");
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "nothing written");

    // A delete dry run reports what would loosen instead of deleting.
    let response = call(
        &client,
        "DELETE",
        &format!("{base}/nightly?dry_run=true"),
        None,
    );
    assert_eq!(response.status(), StatusCode::OK);

    // A model in both rates.set and rates.remove is a contradiction
    // (400), not a silent remove.
    let response = call(
        &client,
        "PATCH",
        &format!("{base}/default"),
        Some(serde_json::json!({
            "rates": {"set": {"gpt-x": {"input_micros_per_1m": 1, "output_micros_per_1m": 1}},
                      "remove": ["gpt-x"]}
        })),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // A $0 CACHED direction prices cached tokens at nothing — the engine
    // refuses it like a $0 direction.
    let response = call(
        &client,
        "PATCH",
        &format!("{base}/default"),
        Some(serde_json::json!({
            "rates": {"set": {"gpt-x": {
                "input_micros_per_1m": 1, "output_micros_per_1m": 1, "cached_micros_per_1m": 0,
            }}}
        })),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // An unknown name is 404 policy-not-found; delete answers 204.
    let mut response = get_ok(&client, &server.addr, "/v0/policies/ghost");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "policy-not-found"
    );
    let response = call(&client, "DELETE", &format!("{base}/nightly"), None);
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = get_ok(&client, &server.addr, "/v0/policies/nightly");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    server.stop().unwrap();
}

// ---------------------------------------------------------------------------
// Story 21-6: Selector sets and set tokens, the Run-grain verbs, Run limits.
// ---------------------------------------------------------------------------

/// Stops the serve thread (draining its agents) when dropped, so a failed
/// assertion never leaves a host or a lingering agent behind.
struct StopOnDrop(Option<ServeHandle>);

impl StopOnDrop {
    fn addr(&self) -> String {
        self.0.as_ref().expect("serving").addr.clone()
    }

    fn stop(mut self) {
        if let Some(server) = self.0.take() {
            server.stop().unwrap();
        }
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(server) = self.0.take() {
            let _ = server.stop();
        }
    }
}

fn serve_guarded(base: PathBuf, ttl: Option<Duration>) -> StopOnDrop {
    let (sink, _diagnostics) = SharedSink::new();
    if ttl.is_none() {
        return StopOnDrop(Some(spawn_serve_with_api(base, sink)));
    }
    write_secrets_file(&base);
    let mut server = spawn_serve(hekma_host::ServeOptions {
        state_base: Some(base.clone()),
        control_listen: Some("127.0.0.1:0".parse().unwrap()),
        bearer_secret: Some(BEARER_NAME.to_string()),
        stop_window: Some(2),
        diagnostics: Some(diagnostic_sink_of(&sink)),
        set_token_ttl_override: ttl,
        ..Default::default()
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(addr) = sink
            .text()
            .lines()
            .find_map(|line| line.split("control api listening on ").nth(1))
            .map(|rest| rest.split(" (").next().unwrap().to_string())
        {
            server.addr = addr;
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{}", sink.text());
        std::thread::sleep(Duration::from_millis(20));
    }
    StopOnDrop(Some(server))
}

/// A JSON call with `bearer`.
fn call_as(
    client: &ureq::Agent,
    bearer: &str,
    method: &str,
    url: &str,
    body: &str,
) -> ureq::http::Response<ureq::Body> {
    let auth = format!("Bearer {bearer}");
    let request = match method {
        "POST" => client.post(url),
        "PATCH" => client.patch(url),
        other => panic!("unsupported {other}"),
    };
    request
        .header("Authorization", &auth)
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .unwrap_or_else(|err| panic!("{method} {url}: {err}"))
}

#[test]
fn set_tokens_are_single_use_credential_bound_expiring_and_forgotten_by_a_restart() {
    // Every OS, no agent process: an empty set still mints a token whose
    // whole lifecycle is observable.
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let server = serve_guarded(base.clone(), None);
    let addr = server.addr();
    let client = client();
    let local_token = hekma_host::read_discovery(&base).unwrap().token;
    let select = |bearer: &str| {
        let mut response = call_as(
            &client,
            bearer,
            "POST",
            &format!("http://{addr}/v0/runs/select"),
            r#"{"selector": "team=core"}"#,
        );
        assert_eq!(response.status(), StatusCode::OK);
        json_body(&mut response)
    };
    let act = |bearer: &str, path: &str, body: String| {
        let mut response = call_as(
            &client,
            bearer,
            if path == "labels" { "PATCH" } else { "POST" },
            &format!("http://{addr}/v0/runs/{path}"),
            &body,
        );
        let status = response.status().as_u16();
        (status, json_body(&mut response))
    };
    let document = select(BEARER_VALUE);
    let token = document["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("hkset_"), "{token}");
    assert_eq!(document["matched"], 0);
    assert_eq!(document["runs"], serde_json::json!([]));
    assert!(document["expires_at"].as_str().unwrap().ends_with('Z'));
    // Another credential cannot use it (and does not consume it).
    let (status, body) = act(
        &local_token,
        "stop",
        format!(r#"{{"set_token": "{token}"}}"#),
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "set-token-foreign");
    // The creator acts once …
    let (status, body) = act(
        BEARER_VALUE,
        "stop",
        format!(r#"{{"set_token": "{token}"}}"#),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["verb"], "stop");
    assert_eq!(body["dry_run"], false);
    assert_eq!(body["matched"], 0);
    // … and only once.
    let (status, body) = act(
        BEARER_VALUE,
        "pause",
        format!(r#"{{"set_token": "{token}"}}"#),
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "set-token-used");
    assert!(
        !body.to_string().contains(&token),
        "a refusal never echoes the token"
    );
    // An unknown token; the body's shape per verb.
    let (status, body) = act(
        BEARER_VALUE,
        "resume",
        r#"{"set_token": "hkset_nope"}"#.into(),
    );
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (404, Some("set-token-not-found"))
    );
    for (path, body) in [
        ("pause", r#"{"set_token": "x", "window_secs": 5}"#),
        ("stop", r#"{"set_token": "x", "timeout_ms": 5}"#),
        ("stop", r#"{"set_token": "x", "window_secs": 0}"#),
        ("drain", r#"{"set_token": "x", "timeout_ms": 0}"#),
        ("close", r#"{"set_token": "x", "window_secs": 5}"#),
        ("labels", r#"{"set_token": "x", "set": {"Team": "core"}}"#),
        ("limits", r#"{"set_token": "x"}"#),
        ("limits", r#"{"set_token": "x", "clear": ["rate"]}"#),
        ("limits", r#"{"set_token": "x", "dollars_per_run": "-1"}"#),
        (
            "limits",
            r#"{"set_token": "x", "breach_action": "explode"}"#,
        ),
    ] {
        let (status, response) = act(BEARER_VALUE, path, body.to_string());
        assert_eq!(status, 400, "{path} {body}: {response}");
    }
    for (path, body) in [
        ("stop", r#"{}"#),
        ("stop", r#"{"set_token": "x", "verbose": true}"#),
        ("limits", r#"{"set_token": "x", "token_per_run": 5}"#),
    ] {
        let (status, response) = act(BEARER_VALUE, path, body.to_string());
        assert_eq!(status, 422, "{path} {body}: {response}");
    }
    let mut response = call_as(
        &client,
        BEARER_VALUE,
        "POST",
        &format!("http://{addr}/v0/runs/select"),
        r#"{"selector": "team"}"#,
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    drop(json_body(&mut response));
    // Every set route consumes: labels, limits and close each once.
    for (path, body) in [
        ("labels", r#""set": {"team": "core"}"#),
        ("limits", r#""tokens_per_run": 5"#),
        ("close", ""),
        ("drain", r#""timeout_ms": 1500"#),
    ] {
        let token = select(BEARER_VALUE)["token"].as_str().unwrap().to_string();
        let comma = if body.is_empty() { "" } else { ", " };
        let payload = format!(r#"{{"set_token": "{token}"{comma}{body}}}"#);
        let (status, response) = act(BEARER_VALUE, path, payload.clone());
        assert_eq!(status, 200, "{path}: {response}");
        let (status, _) = act(BEARER_VALUE, path, payload);
        assert_eq!(status, 409, "{path}: used");
    }
    // The per-Run routes on an unknown Run are 404s.
    for (method, path, body) in [
        ("POST", "run_ghost/stop", ""),
        ("POST", "run_ghost/pause", ""),
        ("POST", "run_ghost/resume", ""),
        ("POST", "run_ghost/drain", ""),
        ("PATCH", "run_ghost/limits", r#"{"tokens_per_run": 5}"#),
    ] {
        let (status, response) = {
            let mut response = call_as(
                &client,
                BEARER_VALUE,
                method,
                &format!("http://{addr}/v0/runs/{path}"),
                body,
            );
            (response.status().as_u16(), json_body(&mut response))
        };
        assert_eq!(status, 404, "{path}: {response}");
        assert_eq!(response["error"]["code"], "run-not-found");
    }
    // Story 22-3: an unknown Scent-Check Mode is refused before any Run is
    // read.
    let mut response = call_as(
        &client,
        BEARER_VALUE,
        "PATCH",
        &format!("http://{addr}/v0/runs/run_ghost/limits"),
        r#"{"scent_check": "loud"}"#,
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(&mut response);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not a Scent-Check Mode"),
        "{body}"
    );
    let mut response = get_ok(&client, &addr, "/v0/runs/run_ghost/limits");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    drop(json_body(&mut response));
    // A token minted before a restart is forgotten (re-select).
    let stale = select(BEARER_VALUE)["token"].as_str().unwrap().to_string();
    server.stop();
    let server = serve_guarded(base.clone(), None);
    let addr = server.addr();
    let mut response = call_as(
        &client,
        BEARER_VALUE,
        "POST",
        &format!("http://{addr}/v0/runs/stop"),
        &format!(r#"{{"set_token": "{stale}"}}"#),
    );
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "set-token-not-found"
    );
    server.stop();
    // After its lifetime a token is expired (a zero lifetime: no sleeping).
    let server = serve_guarded(base, Some(Duration::ZERO));
    let addr = server.addr();
    let mut response = call_as(
        &client,
        BEARER_VALUE,
        "POST",
        &format!("http://{addr}/v0/runs/select"),
        r#"{"selector": "team=core"}"#,
    );
    let token = json_body(&mut response)["token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut response = call_as(
        &client,
        BEARER_VALUE,
        "POST",
        &format!("http://{addr}/v0/runs/stop"),
        &format!(r#"{{"set_token": "{token}"}}"#),
    );
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "set-token-expired"
    );
    server.stop();
}

#[test]
fn a_selected_set_acts_per_run_with_the_child_refused_and_limits_scoped_to_the_run() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        // The manifest fake agent is the POSIX shape (see the lifecycle test).
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _manifest = registered_fake_agent(&base, "setter");
    let server = serve_guarded(base, None);
    let addr = server.addr();
    let client = ladder_client();
    let json = |method: &str, path: &str, body: serde_json::Value| {
        let mut response = call(&client, method, &format!("http://{addr}{path}"), Some(body));
        let status = response.status().as_u16();
        (status, json_body(&mut response))
    };
    let (status, _) = json(
        "PUT",
        "/v0/instances/setter/config",
        serde_json::json!({"key": "labels.team", "value": "core"}),
    );
    assert_eq!(status, 200);
    let mut response = call(
        &client,
        "POST",
        &format!("http://{addr}/v0/instances/setter/start"),
        None,
    );
    assert_eq!(response.status(), StatusCode::OK);
    drop(json_body(&mut response));
    let (status, child) = json(
        "POST",
        "/v0/runs",
        serde_json::json!({"parent_instance": "setter", "labels": {"team": "core"}}),
    );
    assert_eq!(status, 201, "{child}");
    let child = child["run"]["run_id"].as_str().unwrap().to_string();

    // Select: the owner and the child.
    let select = || {
        let (status, document) = json(
            "POST",
            "/v0/runs/select",
            serde_json::json!({"selector": "team=core"}),
        );
        assert_eq!(status, 200, "{document}");
        document
    };
    let document = select();
    assert_eq!(document["matched"], 2, "{document}");
    // Each selected Run is its listing row (the `GET /v0/runs` item): the
    // Run's own fields flattened, plus its labels and usage.
    for run in document["runs"].as_array().unwrap() {
        assert!(run["run_id"].is_string(), "{run}");
        assert!(
            run["labels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|label| label["key"] == "team" && label["value"] == "core"),
            "{run}"
        );
        assert!(run["usage"]["input_tokens"].is_u64(), "{run}");
    }
    let owner = document["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["process_owner"] == true)
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Run limits: Run-scoped; a child's cumulative ceiling is refused.
    let (status, outcome) = json(
        "PATCH",
        &format!("/v0/runs/{owner}/limits"),
        serde_json::json!({"tokens_cumulative": 1_000_000, "dollars_per_run": "3.00"}),
    );
    assert_eq!(status, 200, "{outcome}");
    assert_eq!(outcome["run_scoped"], true);
    assert_eq!(outcome["resolution"]["sources"]["tokens_cumulative"], "run");
    let (status, refused) = json(
        "PATCH",
        &format!("/v0/runs/{child}/limits"),
        serde_json::json!({"tokens_cumulative": 5}),
    );
    assert_eq!(
        (status, refused["error"]["code"].as_str()),
        (400, Some("scope-unavailable"))
    );
    let (status, planned) = json(
        "PATCH",
        &format!("/v0/runs/{owner}/limits"),
        serde_json::json!({"clear": ["tokens-cumulative"], "dry_run": true}),
    );
    assert_eq!(status, 200);
    assert_eq!(planned["dry_run"], true);
    assert_eq!(planned["direction"], "cleared");
    let mut response = get_ok(&client, &addr, &format!("/v0/runs/{owner}/limits"));
    let view = json_body(&mut response);
    assert_eq!(
        view["limits"]["token_cumulative"], 1_000_000,
        "the dry run wrote nothing"
    );

    // Limits over the set: one result per Run.
    let token = select()["token"].as_str().unwrap().to_string();
    let (status, results) = json(
        "POST",
        "/v0/runs/limits",
        serde_json::json!({"set_token": token, "tokens_per_run": 5_000_000}),
    );
    assert_eq!(status, 200, "{results}");
    assert_eq!(results["verb"], "budget", "the CLI's verb name");
    assert_eq!(results["dry_run"], false);
    assert_eq!(results["matched"], 2);
    for row in results["results"].as_array().unwrap() {
        assert_eq!(row["limits"]["run_scoped"], true, "{row}");
    }
    // A cumulative ceiling over the set: the owner takes it, the child is
    // its own refusal.
    let token = select()["token"].as_str().unwrap().to_string();
    let (status, results) = json(
        "POST",
        "/v0/runs/limits",
        serde_json::json!({"set_token": token, "tokens_cumulative": 9_000_000}),
    );
    assert_eq!(status, 200, "{results}");
    let rows = results["results"].as_array().unwrap();
    let child_row = rows
        .iter()
        .find(|row| row["run_id"] == child.as_str())
        .unwrap();
    assert_eq!(child_row["error"]["code"], "scope-unavailable", "{results}");
    // Close over a set: a closable child closes, the owner is refused.
    let (status, extra) = json(
        "POST",
        "/v0/runs",
        serde_json::json!({"parent_instance": "setter", "labels": {"task": "x"}}),
    );
    assert_eq!(status, 201, "{extra}");
    let (status, document) = json(
        "POST",
        "/v0/runs/select",
        serde_json::json!({"selector": "task=x"}),
    );
    assert_eq!(status, 200);
    let token = document["token"].as_str().unwrap().to_string();
    let (status, results) = json(
        "POST",
        "/v0/runs/close",
        serde_json::json!({"set_token": token}),
    );
    assert_eq!(status, 200, "{results}");
    assert_eq!(results["verb"], "close");
    assert_eq!(results["results"][0]["outcome"], "closed");
    assert_eq!(results["results"][0]["run"]["status"], "ended");

    // Labels over the set: one PATCH.
    let token = select()["token"].as_str().unwrap().to_string();
    let (status, results) = json(
        "PATCH",
        "/v0/runs/labels",
        serde_json::json!({"set_token": token, "set": {"tier": "gold"}}),
    );
    assert_eq!(status, 200, "{results}");
    assert_eq!(results["verb"], "label");
    assert!(results["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["outcome"] == "labeled"));

    // Pause over the set: the owner pauses; the child is refused from its
    // own cell (its lifecycle acts on calls), never skipped.
    let token = select()["token"].as_str().unwrap().to_string();
    let (status, results) = json(
        "POST",
        "/v0/runs/pause",
        serde_json::json!({"set_token": token}),
    );
    assert_eq!(status, 200, "{results}");
    let rows = results["results"].as_array().unwrap();
    let row = |id: &str| rows.iter().find(|row| row["run_id"] == id).unwrap().clone();
    assert_eq!(row(&owner)["outcome"], "paused");
    assert_eq!(row(&owner)["instance"]["state"], "paused");
    assert_eq!(row(&child)["outcome"], "refused");
    assert_eq!(row(&child)["error"]["code"], "verb-unavailable");
    assert!(row(&child)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("announced"));
    // The token is spent.
    let (status, _) = json(
        "POST",
        "/v0/runs/resume",
        serde_json::json!({"set_token": token}),
    );
    assert_eq!(status, 409);
    // The held child's resume names its parent; the owner resumes per Run.
    let (status, refused) = json(
        "POST",
        &format!("/v0/runs/{child}/resume"),
        serde_json::json!({}),
    );
    assert_eq!(status, 409, "{refused}");
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("parent-paused"));
    let (status, resumed) = json(
        "POST",
        &format!("/v0/runs/{owner}/resume"),
        serde_json::json!({}),
    );
    assert_eq!(status, 200, "{resumed}");
    assert_eq!(resumed["instance"]["state"], "running");

    // A set whose Runs end between select and act: per-Run `run-ended`.
    let token = select()["token"].as_str().unwrap().to_string();
    let (status, stopped) = json(
        "POST",
        &format!("/v0/runs/{owner}/stop"),
        serde_json::json!({"window_secs": 2}),
    );
    assert_eq!(status, 200, "{stopped}");
    assert_eq!(stopped["instance"]["state"], "stopped");
    let (status, results) = json(
        "POST",
        "/v0/runs/stop",
        serde_json::json!({"set_token": token, "window_secs": 2}),
    );
    assert_eq!(status, 200, "{results}");
    assert!(
        results["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["error"]["code"] == "run-ended"),
        "{results}"
    );
    assert_eq!(select()["matched"], 0, "every Run ended");
    server.stop();
}

// ---------------------------------------------------------------------------
// The Run listing + the SSE ids / Selector / resume (story 21-5).
// ---------------------------------------------------------------------------

/// Seed `base` with the listing fixture BEFORE the serve opens (the same
/// shape `ps_cli` seeds): one engine open migrates the store and creates
/// the `tight` Policy (a Policy event lands in the outbox), SQL seeds the
/// Run rows and labels, and a second open's initial sweep resolves the open
/// Runs. `journal_seed` (optional) stages a pre-existing outbox floor: a
/// legacy journal file whose single entry carries `seq`, imported by the
/// v12 step of the FIRST open — so a below-floor resume is reachable.
fn seed_listing_state(base: &Path, journal_seed: Option<u64>) {
    if let Some(seq) = journal_seed {
        std::fs::write(
            base.join(hekma_engine::paths::OUTBOX_JOURNAL_FILE),
            outbox_entry_line(seq),
        )
        .unwrap();
    }
    {
        let engine = hekma_engine::Engine::open(Some(base.to_path_buf())).unwrap();
        let facade = engine.blocking();
        facade
            .create_policy(
                hekma_engine::PolicyChange {
                    name: "tight".to_string(),
                    selector: Some("team=core".parse().unwrap()),
                    set: hekma_engine::PolicyLimits {
                        tokens_per_run: Some(1_000),
                        ..Default::default()
                    },
                    clear: Vec::new(),
                    rates: Default::default(),
                },
                false,
            )
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(base.join("state.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO agent_instances (name, kind, state, agent_home, created_at, \
                 updated_at) VALUES ('demo', 'mock', 'running', '/h', 't', 't'); \
             INSERT INTO agent_instances (name, kind, state, agent_home, created_at, \
                 updated_at) VALUES ('calm', 'mock', 'running', '/h', 't', 't');",
        )
        .unwrap();
        let demo: i64 = conn
            .query_row(
                "SELECT id FROM agent_instances WHERE name = 'demo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let calm: i64 = conn
            .query_row(
                "SELECT id FROM agent_instances WHERE name = 'calm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
             instance_id, process_owner, created_at, updated_at) \
             VALUES ('run_J', 'supervised', 0, 'active', 'breach', {demo}, 1, 't', 't'); \
             INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
             parent_run_id, instance_id, process_owner, created_at, updated_at) \
             VALUES ('run_H', 'supervised', 0, 'held', '', 'run_J', {demo}, 0, 't', 't'); \
             INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
             instance_id, process_owner, created_at, updated_at) \
             VALUES ('run_K', 'supervised', 0, 'active', '', {calm}, 1, 't', 't'); \
             INSERT INTO run_labels (run_id, key, value, source, origin) VALUES \
             ('run_J', 'team', 'core', 'bound', 'config'); \
             INSERT INTO run_labels (run_id, key, value, source, origin) VALUES \
             ('run_H', 'flow', 'triage', 'caller', NULL); \
             INSERT INTO run_latches (run_id, dimension, scope, action, latched_at) VALUES \
             ('run_J', 'tokens', 'per-run', 'block', 't');"
        ))
        .unwrap();
    }
    {
        let engine = hekma_engine::Engine::open(Some(base.to_path_buf())).unwrap();
        let _ = engine.blocking().policies();
    }
}

#[test]
fn a_selection_past_the_verb_cap_is_refused_whole_never_cut() {
    // Every OS, no process: 1001 open Runs carry `team=bulk` (seeded as SQL
    // rows before the serve opens). Selecting them to act on is refused
    // whole — `400 too-many-runs` — and mints no set.
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    drop(hekma_engine::Engine::open(Some(base.clone())).unwrap());
    {
        let mut conn = rusqlite::Connection::open(base.join("state.db")).unwrap();
        let tx = conn.transaction().unwrap();
        for index in 0..=hekma_engine::MAX_SELECTED_RUNS {
            let run_id = format!("run-{index}-1");
            tx.execute(
                "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
                 process_owner, created_at, updated_at) \
                 VALUES (?1, 'observed', 0, 'active', '', 0, 't', 't')",
                [&run_id],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO run_labels (run_id, key, value, source, origin) \
                 VALUES (?1, 'team', 'bulk', 'bound', 'config')",
                [&run_id],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let server = serve_guarded(base, None);
    let addr = server.addr();
    let client = client();
    let mut response = call_as(
        &client,
        BEARER_VALUE,
        "POST",
        &format!("http://{addr}/v0/runs/select"),
        r#"{"selector": "team=bulk"}"#,
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(&mut response);
    assert_eq!(body["error"]["code"], "too-many-runs", "{body}");
    assert!(body.get("token").is_none(), "{body}");
    server.stop();
}

#[test]
fn the_runs_route_defaults_to_the_documented_page_of_fifty() {
    // No `limit`: the documented default page (50), the same first page and
    // cursor `hekma ps` prints — not the 1000-row maximum.
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    seed_listing_state(&base, None);
    {
        let conn = rusqlite::Connection::open(base.join("state.db")).unwrap();
        let demo: i64 = conn
            .query_row(
                "SELECT id FROM agent_instances WHERE name = 'demo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for index in 0..60 {
            conn.execute(
                "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
                 parent_run_id, instance_id, process_owner, created_at, updated_at) \
                 VALUES (?1, 'supervised', 0, 'active', 'failed', 'run_J', ?2, 0, 't', 't')",
                rusqlite::params![format!("run_Z{index:03}"), demo],
            )
            .unwrap();
        }
    }
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base.clone(), sink_handle);
    let client = client();
    let mut response = get_ok(&client, &server.addr, "/v0/runs");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["counts"]["listed"], 62, "{}", json["counts"]);
    assert_eq!(
        json["items"].as_array().unwrap().len(),
        hekma_engine::LISTING_DEFAULT_LIMIT,
        "the default page is the documented 50"
    );
    assert!(json["next_cursor"].is_string(), "a further page exists");
    // Parity with the CLI's default read (ListRunsQuery::exceptions()).
    let engine = hekma_engine::Engine::open_read_only(Some(base.clone())).unwrap();
    let listing = engine
        .blocking()
        .list_runs(hekma_engine::ListRunsQuery::exceptions())
        .unwrap();
    let reference =
        serde_json::to_value(listing.into_document(hekma_engine::ListingView::Exceptions, None))
            .unwrap();
    assert_eq!(json, reference);
}

#[test]
fn the_runs_route_serves_the_listing_and_refuses_bad_params() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    seed_listing_state(&base, None);
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base.clone(), sink_handle);
    let client = client();

    let mut response = get_ok(&client, &server.addr, "/v0/runs");
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(&mut response);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["view"], "exceptions");
    assert_eq!(json["counts"]["listed"], 2, "{json}");
    assert_eq!(json["counts"]["healthy_open"], 1);
    let items = json["items"].as_array().unwrap();
    assert_eq!(items[0]["run_id"], "run_J");
    assert_eq!(items[1]["run_id"], "run_H");
    assert_eq!(items[0]["status"], "active");
    assert_eq!(
        items[0]["latched"],
        serde_json::json!([{"dimension": "tokens", "scope": "per-run", "action": "block"}])
    );
    assert_eq!(items[0]["budget"]["per_run_remaining"], 1000);
    assert_eq!(
        items[0]["budget"]["sources"]["tokens_per_run"],
        serde_json::json!({"policy": "tight"})
    );

    // Byte-shape parity with the engine's own document — the same struct
    // `hekma ps --json` prints.
    let engine = hekma_engine::Engine::open_read_only(Some(base.clone())).unwrap();
    let listing = engine
        .blocking()
        .list_runs(hekma_engine::ListRunsQuery::exceptions())
        .unwrap();
    let reference =
        serde_json::to_value(listing.into_document(hekma_engine::ListingView::Exceptions, None))
            .unwrap();
    assert_eq!(json, reference);

    // The all view + a page + the cursor round-trip.
    let mut response = get_ok(&client, &server.addr, "/v0/runs?view=all&limit=2");
    assert_eq!(response.status(), StatusCode::OK);
    let page_one = json_body(&mut response);
    assert_eq!(page_one["items"].as_array().unwrap().len(), 2);
    let cursor = page_one["next_cursor"].as_str().unwrap().to_string();
    assert!(!cursor.is_empty());
    let mut response = get_ok(
        &client,
        &server.addr,
        &format!(
            "/v0/runs?view=all&limit=2&cursor={}",
            percent_encode(&cursor)
        ),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let page_two = json_body(&mut response);
    assert_eq!(page_two["items"][0]["run_id"], "run_H");
    assert!(page_two["next_cursor"].is_null());

    // A Selector composes with the view.
    let mut response = get_ok(&client, &server.addr, "/v0/runs?selector=team=core");
    let filtered = json_body(&mut response);
    assert_eq!(filtered["items"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["items"][0]["run_id"], "run_J");
    // A caller label selects too (bound and caller alike).
    let mut response = get_ok(
        &client,
        &server.addr,
        "/v0/runs?view=all&selector=flow=triage",
    );
    let filtered = json_body(&mut response);
    assert_eq!(filtered["items"][0]["run_id"], "run_H");

    // The typed refusals: 400 with the finer machine codes.
    let auth = format!("Bearer {BEARER_VALUE}");
    for (query, code) in [
        ("selector=no-equals", "invalid-selector"),
        ("cursor=!!!", "invalid-cursor"),
        ("limit=abc", "invalid-limit"),
        ("view=everything", "invalid-view"),
    ] {
        let mut response = client
            .get(format!("http://{}/v0/runs?{query}", server.addr))
            .header("Authorization", &auth)
            .call()
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        let json = json_body(&mut response);
        assert_eq!(json["error"]["code"], code, "{query}: {json}");
    }
    server.stop().unwrap();
}

#[test]
fn the_sse_stream_carries_outbox_ids_and_resumes_from_last_event_id() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // A pre-existing floor of 5 (the journal import): the below-floor 409
    // is reachable and the replay set is small and known.
    seed_listing_state(&base, Some(5));
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    // Where the outbox stands: the staged floor is 5 (the journal import);
    // the tight Policy's create landed after it. A pull from the floor
    // minus one is the lossless entry point (a cursor below the floor is
    // the typed 409 — asserted further down).
    let mut response = get_ok(
        &client,
        &server.addr,
        &format!("/v0/export?cursor={}", percent_encode("{\"seq\":4}")),
    );
    let export = json_body(&mut response);
    let seqs: Vec<u64> = export["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["seq"].as_u64().unwrap())
        .collect();
    assert!(!seqs.is_empty(), "the seeded policy event exported");
    assert_eq!(export["retained_from"].as_u64().unwrap(), 5);

    // Live: every frame carries id: <seq>, monotone with the outbox.
    let mut response = client
        .get(format!("http://{}/v0/events", server.addr))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Drive a NEW committed event on another connection.
    let mut create = client
        .post(format!("http://{}/v0/policies", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({
            "name": "later",
            "selector": "team=later",
        }))
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let _ = create.body_mut().read_to_string();

    let mut reader = BufReader::new(response.body_mut().as_reader());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let policy_id = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "no policy frame in time"
        );
        let (id, event) = read_sse_frame(&mut reader);
        if event["kind"] == "policy" {
            break id;
        }
    };
    // The id IS the outbox seq of that event: the export agrees.
    let mut response = get_ok(
        &client,
        &server.addr,
        &format!("/v0/export?cursor={}", percent_encode("{\"seq\":4}")),
    );
    let export = json_body(&mut response);
    let later = export["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["event"]["kind"] == "policy" && entry["event"]["name"] == "later")
        .expect("the later policy exported");
    assert_eq!(later["seq"].as_u64().unwrap(), policy_id);

    // Resume: a reconnect from before the `later` event replays everything
    // after the id through the same filter, then goes live — no duplicate,
    // no gap. Reading one replayed frame (bounded) proves the seam.
    let mut response = client
        .get(format!("http://{}/v0/events", server.addr))
        .header("Authorization", &auth)
        .header("Last-Event-ID", (policy_id - 1).to_string())
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut reader = BufReader::new(response.body_mut().as_reader());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    assert!(
        std::time::Instant::now() < deadline,
        "no replayed frame in time"
    );
    let (id, event) = read_sse_frame(&mut reader);
    assert_eq!(id, policy_id, "the replay starts past the id, at `later`");
    assert_eq!(event["kind"], "policy");

    // The typed 409s at connect — never a clamp.
    for (id, code) in [
        (3u64, "cursor-below-retention"),
        (999_999, "cursor-past-tail"),
    ] {
        let mut response = client
            .get(format!("http://{}/v0/events", server.addr))
            .header("Authorization", &auth)
            .header("Last-Event-ID", id.to_string())
            .call()
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "Last-Event-ID {id}"
        );
        let json = json_body(&mut response);
        assert_eq!(json["error"]["code"], code, "Last-Event-ID {id}: {json}");
    }
    // A malformed Last-Event-ID is the 400 invalid-cursor refusal.
    let mut response = client
        .get(format!("http://{}/v0/events", server.addr))
        .header("Authorization", &auth)
        .header("Last-Event-ID", "not-a-number")
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&mut response)["error"]["code"], "invalid-cursor");
    // A malformed Selector on the stream likewise.
    let mut response = client
        .get(format!("http://{}/v0/events?selector=bad", server.addr))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "invalid-selector"
    );
    server.stop().unwrap();
}

#[test]
fn the_sse_selector_filters_by_the_subject_runs_current_labels() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    seed_listing_state(&base, None);
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(base, sink_handle);
    let client = client();
    let auth = format!("Bearer {BEARER_VALUE}");

    let mut response = client
        .get(format!(
            "http://{}/v0/events?selector=team%3Dcore",
            server.addr
        ))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Two events while the stream is open: a label change on run_J (its
    // subject Run carries team=core → delivered) and a Policy create (no
    // subject Run → dropped under the Selector).
    let mut label = client
        .patch(format!("http://{}/v0/runs/run_J/labels", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"set": {"env": "prod"}}))
        .unwrap();
    assert_eq!(label.status(), StatusCode::OK);
    let _ = label.body_mut().read_to_string();
    let mut create = client
        .post(format!("http://{}/v0/policies", server.addr))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"name": "quiet", "selector": "team=quiet"}))
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let _ = create.body_mut().read_to_string();

    let mut reader = BufReader::new(response.body_mut().as_reader());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(std::time::Instant::now() < deadline, "no run frame in time");
        let (id, event) = read_sse_frame(&mut reader);
        assert!(id > 0, "every event frame carries an id");
        assert_ne!(
            event["kind"], "policy",
            "a no-Run event is dropped under a selector"
        );
        if event["kind"] == "run" && event["run_id"] == "run_J" {
            assert_eq!(event["change"], "labels-changed");
            break;
        }
    }
    server.stop().unwrap();
}

/// Read ONE SSE event frame (an `id:` line then its `data:` line); any
/// marker frame without an id panics naming it. Returns `(id, payload)`.
fn read_sse_frame<R: std::io::BufRead>(reader: &mut R) -> (u64, serde_json::Value) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut id: Option<u64> = None;
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "no complete frame in time"
        );
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("the stream ended mid-frame"),
            Ok(_) => {}
            Err(err) => panic!("the SSE read failed: {err}"),
        }
        let line = line.trim_end().to_string();
        if let Some(raw) = line.strip_prefix("id: ") {
            id = Some(raw.parse().expect("the id is numeric"));
        } else if let Some(payload) = line.strip_prefix("data: ") {
            let event: serde_json::Value = serde_json::from_str(payload.trim())
                .expect("every data frame is the engine's JSON");
            let id = id.expect("every event frame carries an id");
            return (id, event);
        } else if line.starts_with(':') {
            // A keep-alive comment.
        } else if line.is_empty() {
            // A frame terminator.
        }
    }
}

// ---------------------------------------------------------------------------
// Story 23-1: the /v0/bindings routes and /v0/proxy — CRUD, routes, the
// one-time credentials, the listing's keyset page, and the typed codes.
// ---------------------------------------------------------------------------

#[test]
fn the_binding_routes_serve_crud_credentials_and_typed_codes() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();
    let base = format!("http://{}/v0/bindings", server.addr);

    // The proxy runs and says where.
    let mut response = get_ok(&client, &server.addr, "/v0/proxy");
    let proxy = json_body(&mut response);
    assert_eq!(proxy["running"], true);
    let port = proxy["port"].as_u64().unwrap();
    assert_eq!(proxy["url"], format!("http://127.0.0.1:{port}"));
    assert!(proxy.get("error").is_none());

    // Create: 201 + Location + the binding.
    for (name, prefix) in [("alpha", "/alpha"), ("beta", "/beta"), ("gamma", "/gamma")] {
        let mut response = call(
            &client,
            "POST",
            &base,
            Some(serde_json::json!({
                "name": name,
                "prefix": prefix,
                "upstream": "https://api.example.com/v1",
                "labels": [["team", "core"]],
            })),
        );
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers()["location"].to_str().unwrap(),
            format!("/v0/bindings/{name}")
        );
        assert_eq!(json_body(&mut response)["binding"]["prefix"], prefix);
    }
    // Typed refusals carry their codes.
    for (body, status, code) in [
        (
            serde_json::json!({"name": "alpha", "prefix": "/x", "upstream": "https://e.com"}),
            StatusCode::CONFLICT,
            "binding-exists",
        ),
        (
            serde_json::json!({"name": "dup", "prefix": "/alpha", "upstream": "https://e.com"}),
            StatusCode::CONFLICT,
            "binding-prefix-taken",
        ),
        (
            serde_json::json!({"name": "bad", "prefix": "/_hekma", "upstream": "https://e.com"}),
            StatusCode::BAD_REQUEST,
            "binding-invalid",
        ),
    ] {
        let mut response = call(&client, "POST", &base, Some(body));
        assert_eq!(response.status(), status, "{code}");
        assert_eq!(json_body(&mut response)["error"]["code"], code);
    }
    let mut response = call(&client, "POST", &base, Some(serde_json::json!({"oops": 1})));
    assert!(response.status().is_client_error());
    let _ = json_body(&mut response);

    // The keyset listing: a page of two, then the rest after its cursor.
    let mut response = get_ok(&client, &server.addr, "/v0/bindings?limit=2");
    let page = json_body(&mut response);
    assert_eq!(page["bindings"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_after"], "beta");
    let mut response = get_ok(&client, &server.addr, "/v0/bindings?after=beta&limit=x");
    let rest = json_body(&mut response);
    assert_eq!(rest["bindings"][0]["name"], "gamma");

    // Show, patch (labels replaced; custodied key set then cleared), 404s.
    let mut response = get_ok(&client, &server.addr, "/v0/bindings/alpha");
    assert_eq!(json_body(&mut response)["binding"]["name"], "alpha");
    let mut response = client
        .get(format!("{base}/ghost"))
        .header("Authorization", format!("Bearer {BEARER_VALUE}"))
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "binding-not-found"
    );
    // Story 22-2: a custodied key must be pinnable to the binding's https
    // upstream — `api.example.com` is neither declared nor built in.
    let mut response = call(
        &client,
        "PATCH",
        &format!("{base}/alpha"),
        Some(serde_json::json!({"custodied_key": "secret:KEY"})),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&mut response)["error"]["code"], "binding-invalid");
    let mut response = call(
        &client,
        "PATCH",
        &format!("{base}/alpha"),
        Some(serde_json::json!({
            "labels": [],
            "upstream": "https://api.openai.com/v1",
            "custodied_key": "secret:KEY"
        })),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let patched = json_body(&mut response);
    assert_eq!(patched["binding"]["custodied_key"], "secret:KEY");
    assert_eq!(patched["binding"]["labels"].as_array().unwrap().len(), 0);
    let mut response = call(
        &client,
        "PATCH",
        &format!("{base}/alpha"),
        Some(serde_json::json!({"custodied_key": null})),
    );
    assert!(json_body(&mut response)["binding"]
        .get("custodied_key")
        .is_none());
    let mut response = call(
        &client,
        "PATCH",
        &format!("{base}/ghost"),
        Some(serde_json::json!({"prefix": "/g"})),
    );
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let _ = json_body(&mut response);

    // Routes: put, remove, and a missing route.
    let mut response = call(
        &client,
        "PUT",
        &format!("{base}/alpha/routes/backup"),
        Some(serde_json::json!({"name": "backup", "upstream": "https://b.example.com"})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut response)["binding"]["routes"][0]["name"],
        "backup"
    );
    let mut response = call(
        &client,
        "DELETE",
        &format!("{base}/alpha/routes/backup"),
        None,
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut response)["binding"]["routes"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let mut response = call(
        &client,
        "DELETE",
        &format!("{base}/alpha/routes/backup"),
        None,
    );
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "binding-route-not-found"
    );

    // Credentials: issued once, listed without values, revoked per class.
    let mut response = call(
        &client,
        "POST",
        &format!("{base}/alpha/credentials"),
        Some(serde_json::json!({"class": "proxy-access"})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let issued = json_body(&mut response);
    let token = issued["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("hk_pxy_"));
    assert_eq!(issued["class"], "proxy-access");
    assert!(!issued["note"].as_str().unwrap().contains(&token));
    let mut response = get_ok(&client, &server.addr, "/v0/bindings/alpha/credentials");
    let listing = json_body(&mut response);
    assert!(
        !listing.to_string().contains(&token),
        "a listing carries the token"
    );
    assert_eq!(listing["credentials"][0]["state"], "active");
    for (body, code) in [
        (Some(serde_json::json!({"class": "admin"})), "invalid"),
        (None, "invalid"),
    ] {
        let response = call(&client, "POST", &format!("{base}/alpha/credentials"), body);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{code}");
    }
    let response = call(
        &client,
        "POST",
        &format!("{base}/alpha/credentials/revoke"),
        Some(serde_json::json!({"class": "proxy-access"})),
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let mut response = get_ok(&client, &server.addr, "/v0/bindings/alpha/credentials");
    assert_eq!(
        json_body(&mut response)["credentials"][0]["state"],
        "revoked"
    );
    let response = call(
        &client,
        "POST",
        &format!("{base}/alpha/credentials/revoke"),
        Some(serde_json::json!({"class": "nope"})),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Delete: 204, then 404.
    let response = call(&client, "DELETE", &format!("{base}/gamma"), None);
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let mut response = call(&client, "DELETE", &format!("{base}/gamma"), None);
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let _ = json_body(&mut response);

    // The token never reached the state directory's files.
    let mut stack = vec![state.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                assert!(
                    !bytes.windows(token.len()).any(|w| w == token.as_bytes()),
                    "{} carries the token",
                    path.display()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The per-call read surface over the calls store: the listing, the by-id
// record, every typed error, and the authority matrix's calls cells.
// ---------------------------------------------------------------------------

/// Seed call records into the state directory's calls segment (direct SQL
/// through the same file the host's proxy owns — WAL allows the concurrent
/// write). Each row is (run, outcome, status, status class, model); the
/// ids are ULIDs at a controlled time so ordering and windows are
/// deterministic.
/// A seeded call row: (call id, run id, input tokens, outcome, token id).
type SeedCall<'a> = (&'a str, &'a str, Option<i64>, &'a str, Option<&'a str>);

fn seed_call_records(base: &Path, rows: &[SeedCall<'_>]) {
    let calls_dir = base.join("calls");
    std::fs::create_dir_all(&calls_dir).unwrap();
    // A fresh segment beside whatever the host's engine owns: the reader
    // merges by name order and today's (empty) segment reads clean.
    let segment = calls_dir.join("2026-01-02.db");
    let conn = rusqlite::Connection::open(&segment).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS calls (
             call_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, token_id TEXT,
             control_depth TEXT NOT NULL, binding TEXT, disposition TEXT NOT NULL,
             hold_end TEXT, outcome TEXT NOT NULL, status INTEGER,
             status_class TEXT NOT NULL, model TEXT, input_tokens INTEGER,
             output_tokens INTEGER, cached_tokens INTEGER, started_at TEXT NOT NULL,
             ended_at TEXT NOT NULL, latency_us INTEGER NOT NULL, upstream_us INTEGER);
         CREATE INDEX IF NOT EXISTS idx_calls_run_call ON calls(run_id, call_id);
         CREATE INDEX IF NOT EXISTS idx_calls_problem ON calls(call_id)
             WHERE disposition <> 'pass' OR outcome <> 'completed'
             OR status_class NOT IN ('2xx','3xx');
         PRAGMA user_version = 2;",
    )
    .unwrap();
    // 2026-01-02T00:00:00Z in Unix milliseconds; one row per second after
    // it, so the ids and the stored starts agree.
    const BASE_MS: u64 = 1_767_312_000_000;
    for (index, (run, outcome, status, class, model)) in rows.iter().enumerate() {
        let millis = BASE_MS + 1000 + 1000 * index as u64;
        let started = format!("2026-01-02T00:00:{:02}.000000Z", index + 1);
        conn.execute(
            "INSERT OR IGNORE INTO calls VALUES (?1, ?2, ?3, 'wire', NULL, ?4, NULL, ?5, ?6, ?7, \
             ?8, 10, 5, 2, ?9, ?9, 1000, 900)",
            rusqlite::params![
                format!("call_{}", ulid_at(millis, index as u8)),
                run,
                if *outcome == "refused" {
                    Some("abcdef012345".to_string())
                } else {
                    None
                },
                if *outcome == "refused" {
                    "block"
                } else {
                    "pass"
                },
                outcome,
                status,
                class,
                model,
                started,
            ],
        )
        .unwrap();
    }
}

/// A ULID at `unix_millis` whose random half varies by `nonce` (the last
/// character carries its low bits) — deterministic, order-preserving ids.
fn ulid_at(unix_millis: u64, nonce: u8) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut value: u128 = u128::from(unix_millis & ((1 << 48) - 1)) << 80;
    value |= u128::from(nonce as u64) << 40;
    let chars: Vec<char> = (0..26)
        .rev()
        .map(|group| ALPHABET[((value >> (5 * group)) & 0x1F) as usize] as char)
        .collect();
    chars.into_iter().collect()
}

#[test]
fn the_calls_routes_read_records_page_and_refuse_every_malformed_value() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle.clone());
    let client = client();
    seed_call_records(
        state.path(),
        &[
            (
                "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T",
                "completed",
                Some(200),
                "2xx",
                Some("gpt-demo"),
            ),
            (
                "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T",
                "refused",
                Some(429),
                "refused",
                None,
            ),
            (
                "run_01JZ6Q9G3S8V2X7K4M1N5P0R2V",
                "upstream-error",
                None,
                "transport-error",
                Some("gpt-demo"),
            ),
            (
                "run_01JZ6Q9G3S8V2X7K4M1N5P0R2V",
                "completed",
                Some(200),
                "2xx",
                Some("gpt-demo"),
            ),
        ],
    );

    // The unscoped listing: newest first, exactly the documented fields,
    // the echoed filters, no cursor, nothing skipped.
    let mut response = get_ok(&client, &server.addr, "/v0/calls");
    assert_eq!(response.status(), StatusCode::OK);
    let document = json_body(&mut response);
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["filters"]["view"], "all");
    assert_eq!(document["filters"]["limit"], 50);
    let items = document["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    let first = &items[0];
    assert!(first["call_id"].as_str().unwrap().starts_with("call_"));
    assert_eq!(first["run_id"], "run_01JZ6Q9G3S8V2X7K4M1N5P0R2V");
    // The refused row (index 1) presented a Run Key: the 12-hex label reads
    // back; the others are null (never a value).
    assert_eq!(items[2]["token_id"], "abcdef012345");
    assert_eq!(first["token_id"], serde_json::Value::Null);
    assert_eq!(first["hold_end"], serde_json::Value::Null);
    assert!(document["next_cursor"].is_null());
    assert_eq!(document["skipped_segments"], 0);

    // The Run filter never expands a subtree; the problems view drops the
    // clean passes.
    let mut response = get_ok(
        &client,
        &server.addr,
        "/v0/calls?run=run_01JZ6Q9G3S8V2X7K4M1N5P0R2V",
    );
    let tenant = json_body(&mut response);
    assert_eq!(tenant["items"].as_array().unwrap().len(), 2);
    let mut response = get_ok(&client, &server.addr, "/v0/calls?view=problems");
    let problems = json_body(&mut response);
    assert_eq!(problems["items"].as_array().unwrap().len(), 2);
    assert_eq!(problems["filters"]["view"], "problems");

    // limit=2 pages disjointly; 0 serves the maximum quietly; an absent
    // limit is 50; a page-sized read's cursor continues.
    let mut response = get_ok(&client, &server.addr, "/v0/calls?limit=2");
    let page = json_body(&mut response);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let cursor = page["next_cursor"].as_str().unwrap().to_string();
    let mut response = get_ok(
        &client,
        &server.addr,
        &format!("/v0/calls?limit=2&cursor={}", percent_encode(&cursor)),
    );
    let next = json_body(&mut response);
    assert_eq!(next["items"].as_array().unwrap().len(), 2);
    assert!(next["next_cursor"].is_null(), "the read is exhausted");
    for (left, right) in page["items"]
        .as_array()
        .unwrap()
        .iter()
        .zip(next["items"].as_array().unwrap().iter())
    {
        assert!(left["call_id"].as_str().unwrap() > right["call_id"].as_str().unwrap());
    }
    let mut response = get_ok(&client, &server.addr, "/v0/calls?limit=0");
    assert_eq!(
        json_body(&mut response)["items"].as_array().unwrap().len(),
        4
    );

    // The window is half-open on the call start.
    let mut response = get_ok(
        &client,
        &server.addr,
        "/v0/calls?from=2026-01-02T00:00:01Z&to=2026-01-02T00:00:03Z",
    );
    let windowed = json_body(&mut response);
    assert_eq!(windowed["items"].as_array().unwrap().len(), 2);
    assert_eq!(windowed["filters"]["from"], "2026-01-02T00:00:01Z");

    // The by-id record.
    let newest = items[0]["call_id"].as_str().unwrap().to_string();
    let mut response = get_ok(&client, &server.addr, &format!("/v0/calls/{newest}"));
    assert_eq!(response.status(), StatusCode::OK);
    let record = json_body(&mut response);
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["call"]["call_id"], newest.as_str());
    assert_eq!(record["call"]["run_id"], "run_01JZ6Q9G3S8V2X7K4M1N5P0R2V");

    // Every typed refusal, each in the versioned envelope with its code.
    let refusal = |path: &str, status: StatusCode, code: &str| {
        let mut response = get_ok(&client, &server.addr, path);
        assert_eq!(response.status(), status, "{path}");
        let body = json_body(&mut response);
        assert_eq!(body["error"]["code"], code, "{path}");
    };
    // A cursor that is not the codec's, a v2: prefix, and a RUN cursor.
    refusal(
        "/v0/calls?cursor=nonsense",
        StatusCode::BAD_REQUEST,
        "invalid-cursor",
    );
    refusal(
        &format!(
            "/v0/calls?cursor={}",
            percent_encode(&base64ish_cursor("v2:run_x"))
        ),
        StatusCode::BAD_REQUEST,
        "invalid-cursor",
    );
    refusal(
        &format!(
            "/v0/calls?cursor={}",
            percent_encode(&base64ish_cursor("v1:run_01JZ6Q9G3S8V2X7K4M1N5P0R2T"))
        ),
        StatusCode::BAD_REQUEST,
        "invalid-cursor",
    );
    // A calls cursor on the RUN listing refuses too (the cross-kind hole,
    // both directions).
    refusal(
        &format!(
            "/v0/runs?cursor={}",
            percent_encode(&base64ish_cursor("v1:call_01JZ6Q9G3S8V2X7K4M1N5P0R2T"))
        ),
        StatusCode::BAD_REQUEST,
        "invalid-cursor",
    );
    refusal(
        "/v0/calls?run=not-a-run",
        StatusCode::BAD_REQUEST,
        "invalid-run",
    );
    refusal(
        "/v0/calls?from=yesterday",
        StatusCode::BAD_REQUEST,
        "invalid-time",
    );
    refusal(
        "/v0/calls?from=2026-01-03T00:00:00Z&to=2026-01-02T00:00:00Z",
        StatusCode::BAD_REQUEST,
        "invalid-time",
    );
    refusal(
        "/v0/calls?view=everything",
        StatusCode::BAD_REQUEST,
        "invalid-view",
    );
    refusal(
        "/v0/calls?limit=five",
        StatusCode::BAD_REQUEST,
        "invalid-limit",
    );
    refusal(
        "/v0/calls?outcome=superceded",
        StatusCode::BAD_REQUEST,
        "invalid-filter",
    );
    refusal(
        "/v0/calls/not-a-call-id",
        StatusCode::BAD_REQUEST,
        "invalid-call-id",
    );
    refusal(
        "/v0/calls/call_01JZ6Q9G3S8V2X7K4M1N5P0R2T",
        StatusCode::NOT_FOUND,
        "call-not-found",
    );

    server.stop().unwrap();
}

/// A base64url-encoded cursor body over `inner` (the codec's exact form:
/// unpadded URL-safe base64).
fn base64ish_cursor(inner: &str) -> String {
    let mut out = String::new();
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    for chunk in inner.as_bytes().chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let word = (b0 << 16) | (b1 << 8) | b2;
        let quad = [
            ALPHABET[(word >> 18) as usize & 0x3f],
            ALPHABET[(word >> 12) as usize & 0x3f],
            ALPHABET[(word >> 6) as usize & 0x3f],
            ALPHABET[word as usize & 0x3f],
        ];
        match chunk.len() {
            1 => out.push_str(std::str::from_utf8(&quad[..2]).unwrap()),
            2 => out.push_str(std::str::from_utf8(&quad[..3]).unwrap()),
            _ => out.push_str(std::str::from_utf8(&quad).unwrap()),
        }
    }
    out
}

#[test]
fn the_calls_routes_refuse_every_credential_class_but_the_operator_bearer() {
    // Seed an open root Run BEFORE the host takes the state directory, so
    // the API can open a real child under it and mint real credentials.
    let state = tempfile::tempdir().unwrap();
    drop(hekma_engine::Engine::open(Some(state.path().to_path_buf())).unwrap());
    {
        let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
             process_owner, created_at, updated_at) \
             VALUES ('run_ROOT', 'announced', 0, 'active', '', 0, 't', 't');",
        )
        .unwrap();
    }
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle.clone());
    let client = client();

    // A real child Run of the seeded root: its Run Key and refresh
    // credential, through the API.
    let mut response = client
        .post(format!("http://{}/v0/runs", server.addr))
        .header("Authorization", format!("Bearer {BEARER_VALUE}"))
        .send_json(serde_json::json!({"parent_run_id": "run_ROOT"}))
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let opened = json_body(&mut response);
    let child = opened["run"]["run_id"].as_str().unwrap().to_string();
    let run_key = opened["credentials"]["run_key"]
        .as_str()
        .unwrap()
        .to_string();
    let refresh = opened["credentials"]["refresh_credential"]
        .as_str()
        .unwrap()
        .to_string();

    // One call record attributed to that very child, and one to its
    // parent.
    seed_call_records(
        state.path(),
        &[
            ("run_ROOT", "completed", Some(200), "2xx", Some("gpt-demo")),
            (
                child.as_str(),
                "completed",
                Some(200),
                "2xx",
                Some("gpt-demo"),
            ),
        ],
    );

    // The matrix's calls cells: the Run Key — INCLUDING the key of the very
    // Run whose calls are asked for — and the refresh credential are all
    // refused; no credential is a 401; the operator bearer reads
    // everything. The by-id path uses a REAL record from the listing.
    let mut response = get_ok(&client, &server.addr, &format!("/v0/calls?run={child}"));
    let listed = json_body(&mut response);
    let child_call = listed["items"][0]["call_id"].as_str().unwrap().to_string();
    let calls_paths = [
        "/v0/calls".to_string(),
        format!("/v0/calls?run={child}"),
        format!("/v0/calls/{child_call}"),
    ];
    for presented in [run_key.as_str(), refresh.as_str()] {
        for path in &calls_paths {
            let response = client
                .get(format!("http://{}{}", server.addr, path))
                .header("Authorization", format!("Bearer {presented}"))
                .call()
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{path} with {presented}"
            );
            let mut response = response;
            let body = json_body(&mut response);
            assert_eq!(body["error"]["code"], "forbidden");
            assert!(!body.to_string().contains(presented));
        }
    }
    // The binding credential classes, issued for real on a binding: an
    // announce and a proxy-access credential read no calls either (review
    // 1, F9 — the HTTP cells of the matrix, beside the auth layer's unit
    // table).
    let mut response = call(
        &client,
        "POST",
        &format!("http://{}/v0/bindings", server.addr),
        Some(serde_json::json!({
            "name": "callsauth",
            "prefix": "/callsauth",
            "upstream": "https://api.example.com/v1",
        })),
    );
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        json_body(&mut response)
    );
    for class in ["announce", "proxy-access"] {
        let mut response = call(
            &client,
            "POST",
            &format!("http://{}/v0/bindings/callsauth/credentials", server.addr),
            Some(serde_json::json!({ "class": class })),
        );
        assert_eq!(response.status(), StatusCode::OK, "{class}");
        let token = json_body(&mut response)["token"]
            .as_str()
            .unwrap()
            .to_string();
        for path in &calls_paths {
            let response = client
                .get(format!("http://{}{}", server.addr, path))
                .header("Authorization", format!("Bearer {token}"))
                .call()
                .unwrap();
            let status = response.status();
            assert!(
                status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
                "{path} with a {class} credential: {status}"
            );
            let mut response = response;
            assert!(!json_body(&mut response).to_string().contains(&token));
        }
    }
    // An unknown Run Key shape is a 401, never a 403 oracle.
    let unknown_key = format!("hk_run_{}", "9".repeat(52));
    let response = client
        .get(format!("http://{}/v0/calls", server.addr))
        .header("Authorization", format!("Bearer {unknown_key}"))
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // No credential at all: 401.
    let response = client
        .get(format!("http://{}/v0/calls", server.addr))
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // The operator bearer reads the child's calls and every path above.
    for path in &calls_paths {
        let mut response = get_ok(&client, &server.addr, path);
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let _ = json_body(&mut response);
    }

    server.stop().unwrap();
}

#[test]
fn the_reveal_route_records_a_custodied_key_and_nothing_else() {
    // Story 22-2: `POST /v0/instances/{name}/config/revealed` commits one
    // `secret_revealed` event and answers the secret names; an instance
    // that custodies nothing answers an empty list. Operator bearer only.
    let state = tempfile::tempdir().unwrap();
    let _held = registered_fake_agent(state.path(), "held");
    let _plain = registered_fake_agent(state.path(), "plain");
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle);
    let client = client();
    let base = format!("http://{}/v0/instances", server.addr);
    let mut response = call(
        &client,
        "PUT",
        &format!("{base}/held/config"),
        Some(serde_json::json!({"key": "provider.api_key", "value": "secret:HELD_KEY"})),
    );
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        json_body(&mut response)
    );
    let mut response = call(
        &client,
        "POST",
        &format!("{base}/held/config/revealed"),
        None,
    );
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response);
    assert_eq!(body["secrets"], serde_json::json!(["HELD_KEY"]));
    let mut response = call(
        &client,
        "POST",
        &format!("{base}/plain/config/revealed"),
        None,
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(&mut response)["secrets"], serde_json::json!([]));
    // An unknown instance is the config read's own 404.
    let response = call(
        &client,
        "POST",
        &format!("{base}/ghost/config/revealed"),
        None,
    );
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // A Run Key is refused on the Operator route.
    let run_key = format!("hk_run_{}", "9".repeat(52));
    let response = client
        .post(format!("{base}/held/config/revealed"))
        .header("Authorization", format!("Bearer {run_key}"))
        .send_empty()
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// One request as `bearer` (any class), no panic on a non-2xx.
fn as_bearer(
    client: &ureq::Agent,
    method: &str,
    url: &str,
    bearer: &str,
    body: Option<serde_json::Value>,
) -> ureq::http::Response<ureq::Body> {
    let auth = format!("Bearer {bearer}");
    match (method, body) {
        ("POST", Some(json)) => client
            .post(url)
            .header("Authorization", &auth)
            .send_json(json),
        ("POST", None) => client.post(url).header("Authorization", &auth).send_empty(),
        (other, _) => panic!("unsupported test method {other}"),
    }
    .unwrap_or_else(|err| panic!("{method} {url}: {err}"))
}

#[test]
fn the_announce_and_heartbeat_routes_follow_the_matrix_and_show_credentials_once() {
    // Story 23-2: `POST /v0/runs/announce` under a binding's announce
    // credential (201 with the pair once, 200 renew-only), the heartbeat
    // route (204; 403 outside the credential's creations; 409 on an ended
    // Run), and close by the creating credential — each cell over HTTP.
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle.clone());
    let client = client();
    let base = format!("http://{}", server.addr);
    let issue = |name: &str, labels: serde_json::Value| -> String {
        let mut response = call(
            &client,
            "POST",
            &format!("{base}/v0/bindings"),
            Some(serde_json::json!({
                "name": name,
                "prefix": format!("/{name}"),
                "upstream": "https://api.example.com/v1",
                "labels": labels,
            })),
        );
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "{}",
            json_body(&mut response)
        );
        let mut response = call(
            &client,
            "POST",
            &format!("{base}/v0/bindings/{name}/credentials"),
            Some(serde_json::json!({"class": "announce"})),
        );
        assert_eq!(response.status(), StatusCode::OK);
        let issued = json_body(&mut response);
        assert!(
            issued["note"]
                .as_str()
                .unwrap()
                .contains("announces Runs beneath binding"),
            "{issued}"
        );
        issued["token"].as_str().unwrap().to_string()
    };
    let ours = issue("openai", serde_json::json!([["team", "core"]]));
    let theirs = issue("other", serde_json::json!([]));
    assert!(ours.starts_with("hk_ann_"));
    let announce_url = format!("{base}/v0/runs/announce");
    let body = serde_json::json!({"external_id": "job-7", "labels": {"step": "plan"}});

    // A first announce: 201, the Run at `observed`, the pair once.
    let mut response = as_bearer(&client, "POST", &announce_url, &ours, Some(body.clone()));
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let created = json_body(&mut response);
    assert_eq!(created["created"], true);
    let run = &created["run"];
    assert_eq!(run["control_depth"], "observed");
    assert_eq!(run["wire_path"], false);
    assert!(run["lease_expires_at"].is_string(), "{run}");
    assert!(run["last_activity_at"].is_string(), "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_string();
    let run_key = created["credentials"]["run_key"]
        .as_str()
        .unwrap()
        .to_string();
    let refresh = created["credentials"]["refresh_credential"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(run_key.starts_with("hk_run_") && refresh.starts_with("hk_ref_"));
    assert!(created["note"].as_str().unwrap().contains("shown once"));
    // The Run carries the binding's labels, bound, and the Caller's.
    let mut shown = get_ok(&client, &server.addr, &format!("/v0/runs/{run_id}"));
    let shown = json_body(&mut shown).to_string();
    assert!(shown.contains("core") && shown.contains("plan"), "{shown}");
    assert!(!shown.contains(&run_key) && !shown.contains(&refresh));

    // A re-announce: 200, the same Run, no credentials.
    let mut response = as_bearer(&client, "POST", &announce_url, &ours, Some(body.clone()));
    assert_eq!(response.status(), StatusCode::OK);
    let renewed = json_body(&mut response);
    assert_eq!(renewed["created"], false);
    assert_eq!(renewed["run"]["run_id"], run_id.as_str());
    assert!(renewed.get("credentials").is_none());
    assert!(renewed["note"].as_str().unwrap().contains("no new Run Key"));
    // Another binding's credential claims its OWN namespace: a new Run.
    let mut response = as_bearer(&client, "POST", &announce_url, &theirs, Some(body.clone()));
    assert_eq!(response.status(), StatusCode::CREATED);
    let other_run = json_body(&mut response)["run"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(other_run, run_id);

    // Every other class is refused on the announce route.
    for presented in [BEARER_VALUE, run_key.as_str(), refresh.as_str()] {
        let mut response = as_bearer(
            &client,
            "POST",
            &announce_url,
            presented,
            Some(body.clone()),
        );
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{presented}");
        assert_eq!(json_body(&mut response)["error"]["code"], "forbidden");
    }
    // A malformed identity is a 400.
    let response = as_bearer(
        &client,
        "POST",
        &announce_url,
        &ours,
        Some(serde_json::json!({"external_id": "has space"})),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Heartbeat: the creating credential and the Run's own key → 204;
    // another binding's credential → 403; the operator bearer → 403.
    let beat = format!("{base}/v0/runs/{run_id}/heartbeat");
    for presented in [ours.as_str(), run_key.as_str()] {
        let response = as_bearer(&client, "POST", &beat, presented, None);
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{presented}");
    }
    let mut response = as_bearer(&client, "POST", &beat, &theirs, None);
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "run-out-of-scope"
    );
    let mut response = as_bearer(&client, "POST", &beat, BEARER_VALUE, None);
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(&mut response)["error"]["code"], "forbidden");

    // Close: another binding's credential → 403; the creator → 200; then
    // the heartbeat is 409 `run-ended`, naming the re-announce path.
    let close = format!("{base}/v0/runs/{run_id}/close");
    let response = as_bearer(&client, "POST", &close, &theirs, None);
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let mut response = as_bearer(&client, "POST", &close, &ours, None);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(&mut response)["run"]["status"], "ended");
    let mut response = as_bearer(&client, "POST", &beat, &ours, None);
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let refused = json_body(&mut response);
    assert_eq!(refused["error"]["code"], "run-ended");
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("announce again"));
    // The closed Run's key is revoked with it.
    let response = as_bearer(&client, "POST", &beat, &run_key, None);
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // Announcing the ended identity again creates a new Run.
    let mut response = as_bearer(&client, "POST", &announce_url, &ours, Some(body.clone()));
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_ne!(json_body(&mut response)["run"]["run_id"], run_id.as_str());

    // A revoked announce credential is 401 `credential-revoked`; one never
    // issued is 401 `unauthorized`.
    let response = call(
        &client,
        "POST",
        &format!("{base}/v0/bindings/openai/credentials/revoke"),
        Some(serde_json::json!({"class": "announce"})),
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let mut response = as_bearer(&client, "POST", &announce_url, &ours, Some(body.clone()));
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "credential-revoked"
    );
    let never = format!("hk_ann_{}", "9".repeat(52));
    let mut response = as_bearer(&client, "POST", &announce_url, &never, Some(body));
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let refused = json_body(&mut response);
    assert_eq!(refused["error"]["code"], "unauthorized");
    assert!(!refused.to_string().contains(&never));

    // No credential value ever reached the state directory, before or after
    // the host's shutdown checkpoint.
    let scan = |phase: &str| {
        let mut stack = vec![state.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    for secret in [&ours, &theirs, &run_key, &refresh] {
                        assert!(
                            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                            "{phase}: {} carries a credential",
                            path.display()
                        );
                    }
                }
            }
        }
    };
    scan("live");
    server.stop().unwrap();
    scan("after shutdown");
}

/// One OTLP/HTTP+JSON metrics batch for `id` with cumulative input/output.
fn otlp_batch(id: &str, input: u64, output: u64) -> serde_json::Value {
    let point = |kind: &str, value: u64| {
        serde_json::json!({
            "attributes": [{"key": "gen_ai.token.type", "value": {"stringValue": kind}}],
            "asInt": value.to_string(),
        })
    };
    serde_json::json!({"resourceMetrics": [{
        "resource": {"attributes": [
            {"key": "hekma.run.external_id", "value": {"stringValue": id}},
            {"key": "hekma.label.step", "value": {"stringValue": "plan"}},
            {"key": "service.name", "value": {"stringValue": "SENTINEL-ignored"}},
        ]},
        "scopeMetrics": [{"metrics": [
            {"name": "gen_ai.client.token_usage", "sum": {
                "aggregationTemporality": 2, "isMonotonic": true,
                "dataPoints": [point("input", input), point("output", output)]}},
            {"name": "SENTINEL-other", "gauge": {"dataPoints": [{"asInt": "1"}]}},
        ]}],
    }]})
}

#[test]
fn the_otlp_intake_accepts_announce_credentials_only_and_meters_forward_differences() {
    // Story 23-3: `POST /v1/metrics` under a binding's announce credential:
    // the class cells, the content-type and size refusals, a first and a
    // second cumulative batch (the forward difference), `partialSuccess`
    // for the unread points, no Run Key for the Run, and no client string
    // or credential in the state directory.
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle.clone());
    let client = client();
    let base = format!("http://{}", server.addr);
    let mut response = call(
        &client,
        "POST",
        &format!("{base}/v0/bindings"),
        Some(serde_json::json!({
            "name": "openai", "prefix": "/openai",
            "upstream": "https://api.example.com/v1", "labels": [["team", "core"]],
        })),
    );
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        json_body(&mut response)
    );
    let mut response = call(
        &client,
        "POST",
        &format!("{base}/v0/bindings/openai/credentials"),
        Some(serde_json::json!({"class": "announce"})),
    );
    let announce = json_body(&mut response)["token"]
        .as_str()
        .unwrap()
        .to_string();
    let url = format!("{base}/v1/metrics");

    // The first batch: 200 with partialSuccess for the unread gauge point.
    let mut response = as_bearer(
        &client,
        "POST",
        &url,
        &announce,
        Some(otlp_batch("job-7", 1200, 40)),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(&mut response);
    assert_eq!(body["partialSuccess"]["rejectedDataPoints"], "1");
    assert!(!body.to_string().contains("SENTINEL"));
    let mut listing = get_ok(&client, &server.addr, "/v0/runs?view=all");
    let listing = json_body(&mut listing);
    let run = listing["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["identity"][1] == "job-7")
        .unwrap_or_else(|| panic!("{listing}"))
        .clone();
    assert_eq!(run["control_depth"], "observed");
    assert_eq!(run["usage"]["input_tokens"], 1200);
    let run_id = run["run_id"].as_str().unwrap().to_string();
    // The second batch commits only the difference.
    let response = as_bearer(
        &client,
        "POST",
        &url,
        &announce,
        Some(otlp_batch("job-7", 1500, 40)),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let mut shown = get_ok(&client, &server.addr, &format!("/v0/runs/{run_id}"));
    let shown = json_body(&mut shown);
    assert_eq!(shown["run"]["usage"]["input_tokens"], 1500, "{shown}");
    assert_eq!(shown["run"]["usage"]["output_tokens"], 40);
    // The same batch again records nothing (a plain 200 with the unread
    // point reported), and the totals stand.
    let mut response = as_bearer(
        &client,
        "POST",
        &url,
        &announce,
        Some(otlp_batch("job-7", 1500, 40)),
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(&mut response)["partialSuccess"]["rejectedDataPoints"],
        "1"
    );
    let mut again = get_ok(&client, &server.addr, &format!("/v0/runs/{run_id}"));
    assert_eq!(json_body(&mut again)["run"]["usage"]["input_tokens"], 1500);
    let mut keys = get_ok(&client, &server.addr, &format!("/v0/runs/{run_id}/keys"));
    let keys = json_body(&mut keys);
    assert_eq!(
        keys["keys"],
        serde_json::json!([]),
        "telemetry issues no Run Key: {keys}"
    );

    // The class cells: the operator bearer is 403; nothing, or a value
    // never issued, is 401.
    let mut response = as_bearer(
        &client,
        "POST",
        &url,
        BEARER_VALUE,
        Some(otlp_batch("x", 1, 1)),
    );
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_body(&mut response)["error"]["code"], "forbidden");
    let never = format!("hk_ann_{}", "9".repeat(52));
    let response = as_bearer(&client, "POST", &url, &never, Some(otlp_batch("x", 1, 1)));
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // Content types and size.
    let raw = |content_type: &str, body: Vec<u8>| {
        client
            .post(&url)
            .header("Authorization", format!("Bearer {announce}"))
            .header("content-type", content_type)
            .send(&body[..])
            .unwrap()
    };
    let mut response = raw("application/x-protobuf", b"\x0a\x00".to_vec());
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(json_body(&mut response)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Collector"));
    let response = raw("application/json; charset=utf-8", b"{}".to_vec());
    assert_eq!(response.status(), StatusCode::OK);
    let response = raw("application/json", b"not json".to_vec());
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = raw("application/json", vec![b' '; 4 * 1024 * 1024 + 1]);
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // The binding's cap: 1,000 open identities, then `409 too-many-children`
    // and no Run for the next one.
    let batch_of = |ids: Vec<String>| {
        let resources: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| otlp_batch(id, 1, 0)["resourceMetrics"][0].clone())
            .collect();
        serde_json::json!({ "resourceMetrics": resources })
    };
    // job-7 is open already: 999 more fill the binding.
    let ids: Vec<String> = (0..999).map(|n| format!("cap-{n}")).collect();
    for chunk in ids.chunks(8) {
        let response = as_bearer(
            &client,
            "POST",
            &url,
            &announce,
            Some(batch_of(chunk.to_vec())),
        );
        assert_eq!(response.status(), StatusCode::OK);
    }
    let mut response = as_bearer(
        &client,
        "POST",
        &url,
        &announce,
        Some(batch_of(vec!["one-too-many".to_string()])),
    );
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(&mut response)["error"]["code"],
        "too-many-children"
    );

    // Revoked: 401.
    let response = call(
        &client,
        "POST",
        &format!("{base}/v0/bindings/openai/credentials/revoke"),
        Some(serde_json::json!({"class": "announce"})),
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = as_bearer(
        &client,
        "POST",
        &url,
        &announce,
        Some(otlp_batch("job-7", 1600, 40)),
    );
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Nothing the receiver does not consume, and no credential, reached
    // the state directory.
    server.stop().unwrap();
    let mut stack = vec![state.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                for needle in ["SENTINEL", announce.as_str()] {
                    assert!(
                        !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                        "{} carries {needle}",
                        path.display()
                    );
                }
            }
        }
    }
}

#[test]
fn the_hold_family_routes_act_on_wire_runs_and_refuse_everything_else() {
    // Story 23-4: POST /v0/runs/{id}/hold|release|reject|reroute and the
    // set forms — Operator only; a wire Run's reasons set and clear; an
    // observed Run, an unknown Run and a bad route are refused.
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let server = spawn_serve_with_api(state.path().to_path_buf(), sink_handle.clone());
    let client = client();
    let base = format!("http://{}", server.addr);
    let mut response = call(
        &client,
        "POST",
        &format!("{base}/v0/bindings"),
        Some(serde_json::json!({
            "name": "openai", "prefix": "/openai",
            "upstream": "http://127.0.0.1:9/v1", "labels": [["team", "core"]],
        })),
    );
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        json_body(&mut response)
    );
    // One call through the proxy creates the binding's wire Run (the
    // upstream is unreachable; the Run exists all the same).
    let mut proxy = get_ok(&client, &server.addr, "/v0/proxy");
    let proxy_url = json_body(&mut proxy)["url"].as_str().unwrap().to_string();
    let _ = client
        .post(format!("{proxy_url}/openai/chat"))
        .send_json(serde_json::json!({"model": "m", "messages": []}));
    let mut listing = get_ok(&client, &server.addr, "/v0/runs?view=all");
    let listing = json_body(&mut listing);
    let run_id = listing["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["identity"][0] == "binding")
        .unwrap_or_else(|| panic!("{listing}"))["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let runs = format!("{base}/v0/runs/{run_id}");

    let mut response = call(&client, "POST", &format!("{runs}/hold"), None);
    assert_eq!(response.status(), StatusCode::OK);
    let held = json_body(&mut response);
    assert_eq!(held["changed"], true);
    assert_eq!(held["reasons"], serde_json::json!(["hold"]));
    assert!(held["note"]
        .as_str()
        .unwrap()
        .contains("calls already sent complete"));
    let mut shown = get_ok(&client, &server.addr, &format!("/v0/runs/{run_id}"));
    let shown = json_body(&mut shown);
    assert_eq!(shown["run"]["status"], "held");
    assert_eq!(shown["run"]["dispositions"], serde_json::json!(["hold"]));
    // Reroute: an unknown route is 400; null clears.
    let response = call(
        &client,
        "POST",
        &format!("{runs}/reroute"),
        Some(serde_json::json!({"route": "openai/nope"})),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let mut response = call(
        &client,
        "POST",
        &format!("{runs}/reroute"),
        Some(serde_json::json!({"route": null})),
    );
    assert_eq!(json_body(&mut response)["changed"], false);
    let mut response = call(&client, "POST", &format!("{runs}/release"), None);
    assert_eq!(json_body(&mut response)["reasons"], serde_json::json!([]));
    // The set form, through a selected set.
    let mut response = call(
        &client,
        "POST",
        &format!("{base}/v0/runs/select"),
        Some(serde_json::json!({"selector": "team=core"})),
    );
    let token = json_body(&mut response)["token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut response = call(
        &client,
        "POST",
        &format!("{base}/v0/runs/reject"),
        Some(serde_json::json!({"set_token": token})),
    );
    assert_eq!(response.status(), StatusCode::OK);
    let results = json_body(&mut response);
    assert_eq!(results["verb"], "reject");
    assert_eq!(results["results"][0]["outcome"], "reject", "{results}");
    // A route on a non-reroute set form is 400.
    let mut response = call(
        &client,
        "POST",
        &format!("{base}/v0/runs/select"),
        Some(serde_json::json!({"selector": "team=core"})),
    );
    let token = json_body(&mut response)["token"]
        .as_str()
        .unwrap()
        .to_string();
    let response = call(
        &client,
        "POST",
        &format!("{base}/v0/runs/hold"),
        Some(serde_json::json!({"set_token": token, "route": "openai/x"})),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // Unknown Run: 404.
    let response = call(
        &client,
        "POST",
        &format!("{base}/v0/runs/run_NONE/hold"),
        None,
    );
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // A wire Run's pause and resume act on its calls (no instance).
    let mut response = call(&client, "POST", &format!("{runs}/pause"), None);
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        json_body(&mut response)
    );
}
