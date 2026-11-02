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
            || url.ends_with("/v0/export");
        let register = method == "POST" && url.ends_with("/v0/instances");
        // The ack POST reached its handler but this loop's register-shaped
        // body carries no `seq` — the Json extractor's 422 is the
        // handler-reached answer (auth passed, the request shape was wrong).
        let ack_wrong_body = method == "POST" && url.ends_with("/v0/export/ack");
        let expected = match (register, identity) {
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
    // Off by default: NO control notice, NO socket — serve is byte-identical
    // to 17-2 (same steps, same notices).
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    let options = hekma_host::ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        stop_window: Some(1),
        diagnostics: Some(diagnostic_sink_of(&sink_handle)),
        ..hekma_host::ServeOptions::default()
    };
    let server = spawn_serve(options);
    server.stop().expect("serve completes");
    let text = sink_handle.text();
    assert!(
        !text.contains("control api"),
        "no API exists by default:\n{text}"
    );
    // Byte-identity, strengthened (review item 27): the notice SEQUENCE is
    // exactly 17-2's — started, signal, the drain's zero-agent outcome,
    // complete, in order, and NOTHING else (an interleaved API notice breaks
    // the exact-count pin, which is the point: the default serve must not
    // change shape).
    let notices: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        notices.len(),
        4,
        "an API-off serve emits exactly 17-2's four notices:\n{text}"
    );
    assert!(
        notices[0].starts_with("[hekma] host started; state root "),
        "the first notice is the started notice:\n{text}"
    );
    assert_eq!(
        notices[1],
        "[hekma] SIGTEST received; stopping running and paused agents (graceful window 1s) ...",
        "the second notice is the signal notice, verbatim:\n{text}"
    );
    assert_eq!(
        notices[2], "[hekma] no running or paused agents; nothing to stop",
        "the third notice is the drain's zero-agent outcome:\n{text}"
    );
    assert_eq!(
        notices[3], "[hekma] shutdown complete (SIGTEST)",
        "the fourth notice is the completion notice, verbatim:\n{text}"
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
fn a_non_loopback_bind_warns_loudly() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, _diagnostics) = SharedSink::new();
    write_secrets_file(state.path());
    let options = hekma_host::ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        control_listen: Some("0.0.0.0:0".parse().unwrap()),
        bearer_secret: Some(BEARER_NAME.to_string()),
        metering_listen_port: 0,
        stop_window: Some(1),
        log_capture: hekma_engine::LogCaptureMode::Full,
        diagnostics: Some(diagnostic_sink_of(&sink_handle)),
        otlp: Default::default(),
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
    // The warning is LOUD and names the posture (review item 15) — the bind
    // itself is legitimate (17-5's container recipe), never refused.
    let text = sink_handle.text();
    assert!(
        text.contains("NON-LOOPBACK"),
        "a non-loopback bind warns loudly:\n{text}"
    );
    assert!(
        text.contains("no TLS and bearer-only auth"),
        "the warning names the exposure:\n{text}"
    );
    // And the API still answers — on the loopback side of 0.0.0.0.
    let port = addr.rsplit(':').next().unwrap().to_string();
    let client = client();
    let mut response = client
        .get(format!("http://127.0.0.1:{port}/v0/version"))
        .header("Authorization", format!("Bearer {BEARER_VALUE}"))
        .call()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.body_mut().read_to_string();

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
