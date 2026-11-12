//! Run Keys over the Control API (story 22-1), over REAL HTTP: the class ×
//! route-family matrix end to end, the one-time credential bodies, refresh
//! and reuse, revoke-keys with and without the cascade (refused on the very
//! next request), the URL rule, the typed static refusals, the rate-limited
//! diagnostic, and — on Unix — a supervised agent's `HEKMA_RUN_KEY`.
//!
//! Every OS: the Run tree is seeded as SQL rows (no process), so nothing
//! here can hang on Windows; the supervised case spawns the POSIX fake
//! agent and returns early elsewhere. Every HTTP call has a bounded
//! timeout, and nothing reads stdin.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ureq::http::StatusCode;

const BEARER_NAME: &str = "run_keys_test_bearer";
const BEARER_VALUE: &str = "test-bearer-token-22-1";

#[derive(Clone)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl SharedSink {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
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

fn write_secrets_file(base: &Path) {
    let path = base.join(hekma_engine::paths::SECRETS_FILE);
    std::fs::write(&path, format!("{BEARER_NAME} = \"{BEARER_VALUE}\"\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// A running host; [`Server::stop`] fires the signal and joins the thread
/// (also on drop, so a failing assertion never leaves a host behind).
struct Server {
    addr: String,
    sink: SharedSink,
    signal: Option<tokio::sync::oneshot::Sender<&'static str>>,
    thread: Option<std::thread::JoinHandle<Result<(), hekma_host::ServeError>>>,
}

impl Server {
    fn start(base: PathBuf) -> Self {
        write_secrets_file(&base);
        let sink = SharedSink::new();
        let diagnostics: hekma_engine::DiagnosticSink =
            Arc::new(Mutex::new(Box::new(sink.clone())));
        let options = hekma_host::ServeOptions {
            state_base: Some(base),
            control_listen: Some("127.0.0.1:0".parse().unwrap()),
            bearer_secret: Some(BEARER_NAME.to_string()),
            metering_listen_port: 0,
            stop_window: Some(2),
            log_capture: hekma_engine::LogCaptureMode::Full,
            diagnostics: Some(diagnostics),
            otlp: Default::default(),
            ..Default::default()
        };
        let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<&'static str>();
        let thread = std::thread::spawn(move || {
            hekma_host::serve_with_signal(options, move |_sink| async move {
                match signal_rx.await {
                    Ok(name) => Ok(name),
                    Err(_) => Err("the test signal source was dropped".to_string()),
                }
            })
        });
        let mut server = Server {
            addr: String::new(),
            sink,
            signal: Some(signal_tx),
            thread: Some(thread),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(addr) = server
                .sink
                .text()
                .lines()
                .find_map(|line| line.split("control api listening on ").nth(1))
                .map(|rest| rest.split(" (").next().unwrap().to_string())
            {
                server.addr = addr;
                return server;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the control api never reported listening:\n{}",
                server.sink.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(signal) = self.signal.take() {
            let _ = signal.send("SIGTEST");
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn client() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// One answered request: status and body text.
struct Answer {
    status: StatusCode,
    body: String,
}

impl Answer {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }

    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

fn call(method: &str, url: &str, bearer: &str, body: Option<serde_json::Value>) -> Answer {
    let agent = client();
    let auth = format!("Bearer {bearer}");
    let request = ureq::http::Request::builder()
        .method(method)
        .uri(url)
        .header("Authorization", &auth)
        .header("Content-Type", "application/json")
        .body(body.map(|b| b.to_string()).unwrap_or_default())
        .unwrap();
    let mut response = agent.run(request).unwrap();
    Answer {
        status: response.status(),
        body: response.body_mut().read_to_string().unwrap_or_default(),
    }
}

/// A state dir with one open root Run (`run_ROOT`, no process) on an
/// instance `host`, seeded as rows.
fn seeded_state() -> tempfile::TempDir {
    let state = tempfile::tempdir().unwrap();
    drop(hekma_engine::Engine::open(Some(state.path().to_path_buf())).unwrap());
    let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
    conn.execute_batch(
        "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
         process_owner, created_at, updated_at) \
         VALUES ('run_ROOT', 'announced', 0, 'active', '', 0, 't', 't');",
    )
    .unwrap();
    state
}

/// The Operator opens a child of `parent` with credentials; returns (run id,
/// run key, refresh credential).
fn operator_open(server: &Server, parent: &str) -> (String, String, String) {
    let answer = call(
        "POST",
        &server.url("/v0/runs"),
        BEARER_VALUE,
        Some(serde_json::json!({ "parent_run_id": parent })),
    );
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    let json = answer.json();
    (
        json["run"]["run_id"].as_str().unwrap().to_string(),
        json["credentials"]["run_key"].as_str().unwrap().to_string(),
        json["credentials"]["refresh_credential"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

#[test]
fn the_class_by_route_family_matrix_holds_end_to_end() {
    let state = seeded_state();
    let server = Server::start(state.path().to_path_buf());
    let (child, key, refresh) = operator_open(&server, "run_ROOT");
    let (sibling, _, _) = operator_open(&server, "run_ROOT");
    assert!(key.starts_with("hk_run_") && refresh.starts_with("hk_ref_"));

    // RunOpen: under its own Run (Caller labels), and under a descendant;
    // the child's own pair comes back once.
    let answer = call(
        "POST",
        &server.url("/v0/runs"),
        &key,
        Some(serde_json::json!({ "parent_run_id": child, "labels": {"task": "x"} })),
    );
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    let json = answer.json();
    let grandchild = json["run"]["run_id"].as_str().unwrap().to_string();
    assert_eq!(json["run"]["labels"][0]["source"], "caller");
    assert!(json["credentials"]["run_key"]
        .as_str()
        .unwrap()
        .starts_with("hk_run_"));
    let answer = call(
        "POST",
        &server.url("/v0/runs"),
        &key,
        Some(serde_json::json!({ "parent_run_id": grandchild, "issue_credentials": false })),
    );
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    assert!(answer.json().get("credentials").is_none());
    let deeper = answer.json()["run"]["run_id"].as_str().unwrap().to_string();

    // RunLimits: tighten as the Caller; a raise is refused.
    let answer = call(
        "PATCH",
        &server.url(&format!("/v0/runs/{child}/limits")),
        &key,
        Some(serde_json::json!({ "tokens_per_run": 500 })),
    );
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let answer = call(
        "PATCH",
        &server.url(&format!("/v0/runs/{child}/limits")),
        &key,
        Some(serde_json::json!({ "tokens_per_run": 900 })),
    );
    assert_eq!(answer.status, StatusCode::FORBIDDEN, "{}", answer.body);
    assert_eq!(answer.code(), "raise-forbidden");

    // RunClose: a descendant.
    let answer = call(
        "POST",
        &server.url(&format!("/v0/runs/{deeper}/close")),
        &key,
        None,
    );
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);

    // Out of scope: a sibling, the parent.
    for target in [sibling.as_str(), "run_ROOT"] {
        for (method, path, body) in [
            (
                "POST",
                "/v0/runs".to_string(),
                Some(serde_json::json!({ "parent_run_id": target })),
            ),
            ("POST", format!("/v0/runs/{target}/close"), None),
            (
                "PATCH",
                format!("/v0/runs/{target}/limits"),
                Some(serde_json::json!({ "tokens_per_run": 1 })),
            ),
        ] {
            let answer = call(method, &server.url(&path), &key, body);
            assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {path}");
            assert_eq!(answer.code(), "run-out-of-scope", "{method} {path}");
        }
    }

    // Every Operator-family route refuses the Run Key (and the refresh
    // credential) with 403 forbidden; the operator bearer is let through.
    let operator_routes: Vec<(&str, String, Option<serde_json::Value>)> = vec![
        ("GET", "/v0/version".into(), None),
        ("GET", "/v0/fleet".into(), None),
        ("GET", "/v0/instances/host".into(), None),
        ("GET", "/v0/instances/host/config".into(), None),
        ("GET", "/v0/runs".into(), None),
        ("GET", format!("/v0/runs/{child}"), None),
        ("GET", format!("/v0/runs/{child}/labels"), None),
        (
            "PATCH",
            format!("/v0/runs/{child}/labels"),
            Some(serde_json::json!({"set": {"a": "b"}})),
        ),
        ("GET", format!("/v0/runs/{child}/limits"), None),
        ("GET", "/v0/policies".into(), None),
        ("GET", "/v0/export".into(), None),
        ("POST", format!("/v0/runs/{sibling}/revoke-keys"), None),
        ("GET", format!("/v0/runs/{child}/keys"), None),
        (
            "POST",
            "/v0/runs/select".into(),
            Some(serde_json::json!({"selector": "a=b"})),
        ),
        ("POST", format!("/v0/runs/{child}/stop"), None),
    ];
    for (method, path, body) in &operator_routes {
        for presented in [&key, &refresh] {
            let answer = call(method, &server.url(path), presented, body.clone());
            assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {path}");
            assert_eq!(answer.code(), "forbidden", "{method} {path}");
            assert!(!answer.body.contains(presented.as_str()));
        }
        let answer = call(method, &server.url(path), BEARER_VALUE, body.clone());
        assert_ne!(answer.status, StatusCode::FORBIDDEN, "{method} {path}");
        assert_ne!(answer.status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
    // The refresh credential on the Caller families: forbidden.
    let answer = call(
        "POST",
        &server.url("/v0/runs"),
        &refresh,
        Some(serde_json::json!({ "parent_run_id": child })),
    );
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    // RunRefresh: the operator bearer and a Run Key are forbidden.
    for presented in [BEARER_VALUE, key.as_str()] {
        let answer = call("POST", &server.url("/v0/runs/refresh"), presented, None);
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{}", answer.body);
        assert_eq!(answer.code(), "forbidden");
    }
    // An unknown token of any class: 401 unauthorized, static.
    for unknown in [
        format!("hk_run_{}", "A".repeat(52)),
        format!("hk_ref_{}", "B".repeat(52)),
        format!("hk_ann_{}", "C".repeat(52)),
    ] {
        let answer = call(
            "POST",
            &server.url("/v0/runs"),
            &unknown,
            Some(serde_json::json!({ "parent_run_id": child })),
        );
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(answer.code(), "unauthorized");
        assert!(!answer.body.contains(&unknown));
    }
    let unknown_refresh = format!("hk_ref_{}", "D".repeat(52));
    let answer = call(
        "POST",
        &server.url("/v0/runs/refresh"),
        &unknown_refresh,
        None,
    );
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert!(!answer.body.contains(&unknown_refresh));
}

#[test]
fn credentials_are_shown_once_refresh_rotates_and_reuse_revokes() {
    let state = seeded_state();
    let server = Server::start(state.path().to_path_buf());
    let (child, key, refresh) = operator_open(&server, "run_ROOT");
    // No read surface returns either value.
    for path in [
        format!("/v0/runs/{child}"),
        format!("/v0/runs/{child}/keys"),
        "/v0/runs?all=true".to_string(),
        "/v0/export?max=1000".to_string(),
    ] {
        let answer = call("GET", &server.url(&path), BEARER_VALUE, None);
        assert_eq!(answer.status, StatusCode::OK, "{path}: {}", answer.body);
        assert!(!answer.body.contains(&key), "{path}");
        assert!(!answer.body.contains(&refresh), "{path}");
    }
    let keys = call(
        "GET",
        &server.url(&format!("/v0/runs/{child}/keys")),
        BEARER_VALUE,
        None,
    )
    .json();
    assert_eq!(keys["keys"].as_array().unwrap().len(), 2);
    assert_eq!(keys["keys"][0]["class"], "run-key");
    assert_eq!(keys["keys"][0]["state"], "active");

    // Refresh: a new pair once; the old key is refused on the next request.
    let answer = call("POST", &server.url("/v0/runs/refresh"), &refresh, None);
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let json = answer.json();
    assert_eq!(json["run_id"], child);
    let new_key = json["credentials"]["run_key"].as_str().unwrap().to_string();
    let open = |presented: &str| {
        call(
            "POST",
            &server.url("/v0/runs"),
            presented,
            Some(serde_json::json!({ "parent_run_id": child, "issue_credentials": false })),
        )
    };
    let refused = open(&key);
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.code(), "credential-revoked");
    assert!(!refused.body.contains(&key));
    assert_eq!(open(&new_key).status, StatusCode::CREATED);

    // Reuse: the used refresh credential again — the current pair is
    // revoked, the Anomaly raised, credential-revoked answered.
    let answer = call("POST", &server.url("/v0/runs/refresh"), &refresh, None);
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{}", answer.body);
    assert_eq!(answer.code(), "credential-revoked");
    assert_eq!(open(&new_key).code(), "credential-revoked");
    let run = call(
        "GET",
        &server.url(&format!("/v0/runs/{child}")),
        BEARER_VALUE,
        None,
    )
    .json();
    assert!(run["run"]["anomalies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a == "credential-reuse"));
    // The Operator's revoke-keys clears it.
    let answer = call(
        "POST",
        &server.url(&format!("/v0/runs/{child}/revoke-keys")),
        BEARER_VALUE,
        Some(serde_json::json!({})),
    );
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.json()["reuse_cleared"], true);
}

#[test]
fn revoke_keys_is_refused_on_the_very_next_request_and_cascades_by_default() {
    let state = seeded_state();
    let server = Server::start(state.path().to_path_buf());
    let (child, key, _) = operator_open(&server, "run_ROOT");
    let opened = call(
        "POST",
        &server.url("/v0/runs"),
        &key,
        Some(serde_json::json!({ "parent_run_id": child })),
    );
    assert_eq!(opened.status, StatusCode::CREATED);
    let grandchild_key = opened.json()["credentials"]["run_key"]
        .as_str()
        .unwrap()
        .to_string();
    let grandchild = opened.json()["run"]["run_id"].as_str().unwrap().to_string();
    // --no-cascade: the grandchild's key stays valid.
    let answer = call(
        "POST",
        &server.url(&format!("/v0/runs/{child}/revoke-keys")),
        BEARER_VALUE,
        Some(serde_json::json!({ "cascade": false })),
    );
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.json()["revoked"].as_array().unwrap().len(), 2);
    // No sleep: the next request presenting the revoked key is refused.
    let next = call(
        "POST",
        &server.url("/v0/runs"),
        &key,
        Some(serde_json::json!({ "parent_run_id": child })),
    );
    assert_eq!(next.code(), "credential-revoked");
    let deeper = call(
        "POST",
        &server.url("/v0/runs"),
        &grandchild_key,
        Some(serde_json::json!({ "parent_run_id": grandchild, "issue_credentials": false })),
    );
    assert_eq!(deeper.status, StatusCode::CREATED, "{}", deeper.body);
    // The cascade (an empty body) reaches the grandchild's pair.
    let answer = call(
        "POST",
        &server.url(&format!("/v0/runs/{child}/revoke-keys")),
        BEARER_VALUE,
        None,
    );
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let revoked = answer.json()["revoked"].as_array().unwrap().clone();
    assert_eq!(revoked.len(), 2);
    assert!(revoked.iter().all(|r| r["cascaded"] == true));
    assert!(!answer.body.contains(&grandchild_key));
    let next = call(
        "POST",
        &server.url("/v0/runs"),
        &grandchild_key,
        Some(serde_json::json!({ "parent_run_id": grandchild })),
    );
    assert_eq!(next.code(), "credential-revoked");
    // A bad body is a typed 400; a missing Run a 404.
    let bad = call(
        "POST",
        &server.url(&format!("/v0/runs/{child}/revoke-keys")),
        BEARER_VALUE,
        Some(serde_json::json!({ "cascade": "yes" })),
    );
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let missing = call(
        "POST",
        &server.url("/v0/runs/run_none/revoke-keys"),
        BEARER_VALUE,
        None,
    );
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let missing = call(
        "GET",
        &server.url("/v0/runs/run_none/keys"),
        BEARER_VALUE,
        None,
    );
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[test]
fn a_token_in_a_url_is_refused_before_auth_and_revoked() {
    let state = seeded_state();
    let server = Server::start(state.path().to_path_buf());
    let (child, key, _) = operator_open(&server, "run_ROOT");
    let (_, key2, _) = operator_open(&server, "run_ROOT");
    let encoded = key2.replace('_', "%5F");
    for (path, bearer) in [
        (format!("/v0/runs/{key}"), BEARER_VALUE),
        (format!("/v0/runs?key={encoded}"), "nonsense"),
        (format!("/v0/fleet?x=y&token={}", &key[..20]), ""),
    ] {
        let answer = call("GET", &server.url(&path), bearer, None);
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(answer.code(), "credential-in-url", "{path}");
        assert!(!answer.body.contains(&key) && !answer.body.contains(&key2));
    }
    // The complete tokens seen are revoked (exposed-in-url).
    for exposed in [&key, &key2] {
        let answer = call(
            "POST",
            &server.url("/v0/runs"),
            exposed,
            Some(serde_json::json!({ "parent_run_id": child })),
        );
        assert_eq!(answer.code(), "credential-revoked");
    }
    let keys = call(
        "GET",
        &server.url(&format!("/v0/runs/{child}/keys")),
        BEARER_VALUE,
        None,
    )
    .json();
    assert_eq!(keys["keys"][0]["revoke_cause"], "exposed-in-url");
}

#[test]
fn unresolved_presentations_produce_one_diagnostic_and_no_token_in_any_log() {
    let state = seeded_state();
    let server = Server::start(state.path().to_path_buf());
    let sentinel = format!("hk_run_{}", "S".repeat(52));
    for _ in 0..200 {
        let answer = call("GET", &server.url("/v0/fleet"), &sentinel, None);
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert!(!answer.body.contains(&sentinel));
    }
    for _ in 0..200 {
        let answer = call("GET", &server.url("/v0/fleet"), "wrong-secret", None);
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    }
    let text = server.sink.text();
    let lines = text
        .lines()
        .filter(|line| line.contains("unrecognised credential presentation"))
        .count();
    assert_eq!(lines, 1, "{text}");
    assert!(!text.contains(&sentinel), "{text}");
    assert!(!text.contains("wrong-secret"), "{text}");
}

#[test]
fn credential_events_reach_sse_through_the_selector_and_carry_no_token() {
    use std::io::BufRead;
    let state = seeded_state();
    let server = Server::start(state.path().to_path_buf());
    // A child carrying a caller label the Selector matches.
    let opened = call(
        "POST",
        &server.url("/v0/runs"),
        BEARER_VALUE,
        Some(serde_json::json!({ "parent_run_id": "run_ROOT", "labels": {"flow": "sse"} })),
    );
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.body);
    let child = opened.json()["run"]["run_id"].as_str().unwrap().to_string();
    let refresh = opened.json()["credentials"]["refresh_credential"]
        .as_str()
        .unwrap()
        .to_string();
    let agent = client();
    let mut stream = agent
        .get(server.url("/v0/events?selector=flow=sse"))
        .header("Authorization", format!("Bearer {BEARER_VALUE}"))
        .call()
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    // Rotate: four credential events on the child's Run.
    let rotated = call("POST", &server.url("/v0/runs/refresh"), &refresh, None);
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.body);
    let new_key = rotated.json()["credentials"]["run_key"]
        .as_str()
        .unwrap()
        .to_string();
    let mut reader = std::io::BufReader::new(stream.body_mut().as_reader());
    let mut changes = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while changes.len() < 4 {
        assert!(
            std::time::Instant::now() < deadline,
            "credential frames never arrived"
        );
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        assert!(
            !line.contains(&refresh) && !line.contains(&new_key),
            "{line}"
        );
        if let Some(payload) = line.trim_end().strip_prefix("data: ") {
            let event: serde_json::Value = serde_json::from_str(payload).unwrap();
            if event["kind"] == "credential" {
                assert_eq!(event["run_id"], child.as_str());
                assert!(event.get("token_hash").is_none());
                changes.push(event["change"].as_str().unwrap().to_string());
            }
        }
    }
    changes.sort();
    assert_eq!(changes, vec!["issued", "issued", "rotated", "rotated"]);
}

#[test]
fn the_caller_breadth_cap_answers_409_over_http() {
    let state = seeded_state();
    let server = Server::start(state.path().to_path_buf());
    let (child, key, _) = operator_open(&server, "run_ROOT");
    drop(server);
    {
        let mut conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
        let tx = conn.transaction().unwrap();
        for index in 0..hekma_engine::MAX_CALLER_OPEN_CHILDREN {
            tx.execute(
                "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
                 parent_run_id, process_owner, created_at, updated_at) \
                 VALUES (?1, 'announced', 0, 'active', '', ?2, 0, 't', 't')",
                rusqlite::params![format!("run_CAP{index}"), child],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let server = Server::start(state.path().to_path_buf());
    let answer = call(
        "POST",
        &server.url("/v0/runs"),
        &key,
        Some(serde_json::json!({ "parent_run_id": child })),
    );
    assert_eq!(answer.status, StatusCode::CONFLICT, "{}", answer.body);
    assert_eq!(answer.code(), "too-many-children");
    // The Operator is not capped.
    let answer = call(
        "POST",
        &server.url("/v0/runs"),
        BEARER_VALUE,
        Some(serde_json::json!({ "parent_run_id": child, "issue_credentials": false })),
    );
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
}

#[test]
fn a_supervised_agent_receives_its_run_key_in_the_environment_only() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        // The manifest fake agent is the POSIX shape; issuance and
        // revocation are proven at the job level on every OS.
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let dump_dir = tempfile::tempdir().unwrap();
    let dump = dump_dir.path().join("dump.txt");
    let marker = dump_dir.path().join("ready");
    let manifest = tempfile::tempdir().unwrap();
    hekma_conformance::test_support::ManifestFixture::new(
        "keyed",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            dump.to_str().unwrap(),
            "--marker",
            marker.to_str().unwrap(),
        ],
    )
    .guaranteed_on_all_oses("interaction")
    .write(manifest.path());
    {
        let engine = hekma_engine::Engine::open(Some(base.clone())).unwrap();
        engine
            .blocking()
            .register_with_adapter(
                "keyed",
                &hekma_engine::AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
    }
    let server = Server::start(base.clone());
    let answer = call(
        "POST",
        &server.url("/v0/instances/keyed/start"),
        BEARER_VALUE,
        None,
    );
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let text = loop {
        if let Ok(text) = std::fs::read_to_string(&dump) {
            if !text.is_empty() {
                break text;
            }
        }
        assert!(std::time::Instant::now() < deadline, "no dump");
        std::thread::sleep(Duration::from_millis(20));
    };
    let key = text
        .lines()
        .find_map(|line| line.strip_prefix("env=HEKMA_RUN_KEY="))
        .expect("HEKMA_RUN_KEY is in the environment")
        .to_string();
    assert!(key.starts_with("hk_run_") && key.len() == 59);
    // Not on the argv.
    assert!(
        !text
            .lines()
            .filter(|line| line.starts_with("arg="))
            .any(|line| line.contains(&key)),
        "{text}"
    );
    // It authenticates as a Run Key of the agent's Run: a child opens.
    let answer = call(
        "POST",
        &server.url("/v0/runs"),
        &key,
        Some(serde_json::json!({ "parent_instance": "keyed" })),
    );
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    // Neither the Agent Home, nor the effective config, nor any config read
    // carries it.
    let config = call(
        "GET",
        &server.url("/v0/instances/keyed/config"),
        BEARER_VALUE,
        None,
    );
    assert!(!config.body.contains(&key));
    assert_no_sentinel(&base, &key);
    // Writing the reserved key is refused.
    let answer = call(
        "PUT",
        &server.url("/v0/instances/keyed/config"),
        BEARER_VALUE,
        Some(serde_json::json!({ "key": "run.key", "value": key })),
    );
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.body);
    assert!(!answer.body.contains(&key));
    // A stop revokes it.
    let answer = call(
        "POST",
        &server.url("/v0/instances/keyed/stop"),
        BEARER_VALUE,
        Some(serde_json::json!({ "window_secs": 2 })),
    );
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let answer = call(
        "POST",
        &server.url("/v0/runs"),
        &key,
        Some(serde_json::json!({ "parent_instance": "keyed" })),
    );
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{}", answer.body);
    assert_eq!(answer.code(), "credential-revoked");
    drop(server);
    assert_no_sentinel(&base, &key);
}

/// A byte scan of every file under `root` (state.db, its -wal and -shm, the
/// logs, every Agent Home, control.json) for `sentinel`.
fn assert_no_sentinel(root: &Path, sentinel: &str) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let found = bytes
                    .windows(sentinel.len())
                    .any(|window| window == sentinel.as_bytes());
                assert!(!found, "{} carries the token", path.display());
            }
        }
    }
}
