//! The OTLP exporter's conformance suite (AD-21): the I/O matrix legs driven
//! against a REAL in-test HTTP collector — the structural privacy default
//! (no `--otlp-endpoint` → nothing exists, nothing dials), the end-to-end
//! payload conformance (a real fake agent commits usage + a dollar breach;
//! the POSTed body decodes against `otlp-metrics v1` verbatim), the refused
//! collector (surfaced diagnostics + the dropped counter, supervision
//! unaffected), and the unresolvable `secret:NAME` header (typed startup
//! refusal naming the secret).
//!
//! OS-neutral by construction: no OS-conditional compilation anywhere — the
//! agent fixture is the cross-OS `fake_agent` binary, the bearer
//! secret resolves through the ENV leg (never the secrets file, whose 0600
//! chmod is the platform-conditional write), and every skip is data-driven.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_conformance::test_support::ManifestFixture;
use hekma_engine::{AdapterRef, Engine};
use serde_json::Value;
use ureq::http::StatusCode;

/// The bearer secret NAME the end-to-end leg resolves (the ENV leg —
/// never the process-wide secrets file; the name is unique to this suite so
/// a parallel test in another binary cannot collide).
const BEARER_NAME: &str = "otlp_export_test_bearer";
/// The bearer token VALUE the tests present.
const BEARER_VALUE: &str = "otlp-export-bearer-value";
/// The literal header value the end-to-end leg configures — the sink must
/// NEVER see it, and the payload must NEVER carry it (the no-leak matrix).
const HEADER_LITERAL: &str = "otlp-literal-header-secret";

/// The process-global env var is SHARED STATE across this file's parallel
/// test threads: serve resolves the bearer secret ONCE at startup through
/// the env leg, so every leg that spawns serve must find the variable set —
/// including when the end-to-end leg has not run (or has long finished).
/// The guard sets the variable idempotently under a mutex and NEVER removes
/// it (a `remove_var` mid-run is what made two legs fail when the whole
/// file ran in parallel: serve startup lost the race with the removal).
fn ensure_bearer_env() {
    static GUARD: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    let lock = GUARD.get_or_init(|| Mutex::new(()));
    let _held = lock.lock().unwrap();
    if std::env::var(BEARER_NAME).as_deref() != Ok(BEARER_VALUE) {
        std::env::set_var(BEARER_NAME, BEARER_VALUE);
    }
}

/// A sink that records every notice into a shared buffer the test can
/// assert on (the serve/control suites' shape).
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

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One recorded POST at the in-test collector.
#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    content_type: Option<String>,
    body: String,
    /// The status the collector ANSWERED with (the retry leg pins the
    /// 503-then-200 sequence).
    answered: u16,
}

/// A local OTLP collector stub: accepts one HTTP/1.1 POST per connection,
/// records it, answers `status`, and closes the connection (so the
/// exporter's client never holds a pooled socket into the test's teardown).
struct Collector {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Collector {
    /// Bind on `127.0.0.1:0`; answer every POST with `status`.
    fn start(status: u16) -> Self {
        Self::start_scripted(vec![status])
    }

    /// Bind on `127.0.0.1:0`; answer the FIRST requests with the scripted
    /// statuses in order, repeating the LAST one for every request after
    /// the script runs out — the retry leg's 503-once-then-200 shape.
    fn start_scripted(statuses: Vec<u16>) -> Self {
        assert!(!statuses.is_empty(), "a scripted collector needs a status");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test collector");
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_thread = Arc::clone(&requests);
        let script: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(statuses));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let requests = Arc::clone(&requests_thread);
                let script = Arc::clone(&script);
                std::thread::spawn(move || {
                    // Pop the next scripted status; an exhausted script
                    // repeats its last entry forever.
                    let status = {
                        let mut script = script.lock().unwrap();
                        if script.len() > 1 {
                            script.remove(0)
                        } else {
                            *script.last().unwrap()
                        }
                    };
                    answer_with(stream, status, |recorded| {
                        requests.lock().unwrap().push(recorded);
                    });
                });
            }
        });
        Collector { addr, requests }
    }

    /// An endpoint URL for this collector (http-only v1, loopback).
    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Wait until `predicate` holds over the recorded requests (bounded —
    /// a missing POST fails the test with the diagnostics attached).
    fn wait_for(&self, predicate: impl Fn(&[RecordedRequest]) -> bool, bound: Duration) {
        let deadline = Instant::now() + bound;
        loop {
            if predicate(self.requests.lock().unwrap().as_slice()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the collector never received the expected POST; recorded so far: {:?}",
                self.requests.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Read ONE HTTP/1.1 request (headers + Content-Length body), hand it to
/// `record`, answer with `status`, close.
fn answer_with(mut stream: TcpStream, status: u16, record: impl FnOnce(RecordedRequest)) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut content_type = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if let Some(value) = lower.strip_prefix("content-type:") {
            content_type = Some(value.trim().trim_end_matches("\r\n").to_string());
        }
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_ok() {
        record(RecordedRequest {
            method,
            path,
            content_type,
            body: String::from_utf8_lossy(&body).to_string(),
            answered: status,
        });
    }
    let response =
        format!("HTTP/1.1 {status} ANSWER\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// A running host + its parsed Control-API address (the control suite's
/// shape; the otlp options ride in the builder closure).
struct ServeHandle {
    addr: String,
    signal: Option<tokio::sync::oneshot::Sender<&'static str>>,
    thread: std::thread::JoinHandle<Result<(), hekma_host::ServeError>>,
}

impl ServeHandle {
    /// Fire the shutdown signal, join the serve thread, and return the
    /// host's own result.
    fn stop(mut self) -> Result<Result<(), hekma_host::ServeError>, String> {
        self.signal
            .take()
            .expect("signal sent twice")
            .send("SIGTEST")
            .ok();
        self.thread
            .join()
            .map_err(|_| "the serve thread panicked".to_string())
    }
}

/// Spawn `serve_with_signal` on a thread with the channel-triggered signal
/// factory; if `with_api` is set the Control API binds on an OS-ephemeral
/// loopback port and the parsed address is waited for.
fn spawn_serve(
    options: hekma_host::ServeOptions,
    sink: &SharedSink,
    with_api: bool,
) -> ServeHandle {
    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<&'static str>();
    let thread = std::thread::spawn(move || {
        hekma_host::serve_with_signal(options, move |_sink| async move {
            match signal_rx.await {
                Ok(name) => Ok(name),
                Err(_) => Err("the test signal source was dropped".to_string()),
            }
        })
    });
    let mut handle = ServeHandle {
        addr: String::new(),
        signal: Some(signal_tx),
        thread,
    };
    if with_api {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let text = sink.text();
            if let Some(addr) = text
                .lines()
                .find_map(|line| line.split("control api listening on ").nth(1))
                .map(|rest| rest.split(" (").next().unwrap().to_string())
            {
                handle.addr = addr;
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the control api never reported listening:\n{}",
                sink.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    handle
}

/// The ureq client (status-as-error OFF; the control suite's shape).
fn client() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_recv_response(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(10)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// A fresh state root with a usage-reporting fake agent registered under
/// `name` (the registering engine is DROPPED — serve opens the same root
/// fresh). The agent emits ONE self-reported usage event (10_000 input /
/// 500 output tokens), and the config prices it past the $0.01 cumulative
/// dollar cap — so serve's first minutes commit a UsageUpdateEvent AND a
/// DOLLAR BudgetBreachEvent, both on the bus the exporter subscribes to.
fn registered_budgeted_agent(base: &Path, name: &str) -> tempfile::TempDir {
    let manifest_dir = tempfile::tempdir().unwrap();
    ManifestFixture::new(
        name,
        &[
            "--linger-ms",
            "60000",
            "--emit-usage",
            "1",
            "--usage-input-tokens",
            "10000",
            "--usage-output-tokens",
            "500",
        ],
    )
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
    // The dollar budget: $0.01 cumulative cap, priced at $3.00 per 1M input
    // tokens — 10k input tokens derive $0.03, breaching the cap.
    engine
        .blocking()
        .set_config(name, "cost.rate.input", "3.00")
        .expect("rate input");
    engine
        .blocking()
        .set_config(name, "cost.rate.output", "3.00")
        .expect("rate output");
    engine
        .blocking()
        .set_config(name, "budget.dollars.cumulative", "0.01")
        .expect("dollar cap");
    drop(engine);
    manifest_dir
}

/// Serve options over `base` with the exporter configured (or not) and the
/// Control API on.
fn options_with(
    base: PathBuf,
    sink: hekma_engine::DiagnosticSink,
    otlp: hekma_host::OtlpOptions,
) -> hekma_host::ServeOptions {
    // The bearer resolves through the ENV leg at serve startup (OS-neutral);
    // make sure the process-global variable is in place for THIS leg too,
    // regardless of which parallel sibling set or touched it last.
    ensure_bearer_env();
    hekma_host::ServeOptions {
        state_base: Some(base),
        control_listen: Some("127.0.0.1:0".parse().unwrap()),
        bearer_secret: Some(BEARER_NAME.to_string()),
        metering_listen_port: 0,
        stop_window: Some(2),
        log_capture: hekma_engine::LogCaptureMode::Full,
        diagnostics: Some(sink),
        otlp,
    }
}

// ---------------------------------------------------------------------------
// Leg 1 — the structural privacy default: NO --otlp-endpoint, NO exporter.
// ---------------------------------------------------------------------------

#[test]
fn with_no_endpoint_the_exporter_does_not_exist_and_nothing_dials() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, sink) = SharedSink::new();
    let options = options_with(
        state.path().to_path_buf(),
        sink,
        hekma_host::OtlpOptions::default(),
    );
    let server = spawn_serve(options, &sink_handle, true);
    // Give a would-be exporter every chance to misbehave: a full second of
    // serving with agents-adopting, flushing ticks, everything.
    std::thread::sleep(Duration::from_secs(1));
    let result = server.stop().expect("the serve thread joins");
    assert!(result.is_ok(), "serve must succeed: {result:?}");
    let text = sink_handle.text();
    assert!(
        !text.contains("otlp export"),
        "the exporter must not announce itself, spawn, or dial when no endpoint \
         is configured:\n{text}"
    );
}

#[test]
fn headers_without_an_endpoint_refuse_the_exporter_never_half_exists() {
    let state = tempfile::tempdir().unwrap();
    let (_sink_handle, sink) = SharedSink::new();
    let otlp =
        hekma_host::OtlpOptions::from_flags(None, Some("authorization=secret:SOMETHING"), None);
    assert!(
        otlp.is_err(),
        "headers without an endpoint are a parse-time refusal"
    );
    // The struct-level guard too (a hand-built options value): the lib
    // re-validates before anything is touched.
    let options = options_with(
        state.path().to_path_buf(),
        sink,
        hekma_host::OtlpOptions {
            headers: vec![(
                "authorization".to_string(),
                hekma_host::OtlpHeaderValue::new("secret:SOMETHING"),
            )],
            ..Default::default()
        },
    );
    let err = hekma_host::serve_with_signal(options, |_sink| std::future::ready(Ok("SIGTEST")))
        .expect_err("headers without an endpoint refuse");
    assert!(
        matches!(err, hekma_host::ServeError::InvalidOptions { .. }),
        "the refusal is the usage class: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Leg 4 — the unresolvable secret:NAME header (typed startup refusal).
// ---------------------------------------------------------------------------

#[test]
fn an_unresolvable_secret_header_is_a_typed_refusal_naming_the_secret() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, sink) = SharedSink::new();
    let otlp = hekma_host::OtlpOptions::from_flags(
        Some("http://127.0.0.1:1".to_string()),
        Some("authorization=secret:OTLP_TEST_DEFINITELY_UNRESOLVABLE"),
        None,
    )
    .expect("the flag shape parses");
    let options = options_with(state.path().to_path_buf(), sink, otlp);
    let err = hekma_host::serve_with_signal(options, |_sink| std::future::ready(Ok("SIGTEST")))
        .expect_err("an unresolvable secret refuses the startup");
    match &err {
        hekma_host::ServeError::InvalidOptions { message } => {
            assert!(
                message.contains("OTLP_TEST_DEFINITELY_UNRESOLVABLE"),
                "the refusal NAMES the secret: {message}"
            );
        }
        other => panic!("expected InvalidOptions, got {other:?}"),
    }
    let text = sink_handle.text();
    assert!(
        text.contains("OTLP_TEST_DEFINITELY_UNRESOLVABLE"),
        "surfaced through the sink too (AI-18):\n{text}"
    );
    // Nothing opened: the refusal precedes the engine (the I/O matrix).
    assert!(
        !state.path().join("state.db").exists(),
        "a refused host must not open an engine"
    );
}

// ---------------------------------------------------------------------------
// Leg 2 — end-to-end: real usage + a dollar breach, the payload decoded
// against the versioned exposition, and the no-leak matrix at the sink.
// ---------------------------------------------------------------------------

#[test]
fn the_end_to_end_post_matches_the_otlp_metrics_v1_exposition() {
    // The bearer resolves through the ENV leg (OS-neutral; the secrets
    // file's 0600 chmod is the platform-conditional write this suite
    // deliberately avoids) — `options_with` guarantees the variable is set
    // for every serve spawn; nothing here REMOVES it (a mid-run removal is
    // the process-global race that flaked legs 1 and 3).
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _manifest = registered_budgeted_agent(&base, "worker");

    let collector = Collector::start(200);
    let (sink_handle, sink) = SharedSink::new();
    let otlp = hekma_host::OtlpOptions::from_flags(
        Some(collector.endpoint()),
        Some(&format!("x-fixture={HEADER_LITERAL}")),
        Some(1),
    )
    .expect("the flags parse");
    let options = options_with(base, sink, otlp);
    let server = spawn_serve(options, &sink_handle, true);

    // Start the agent through the Control API (the suite's real-HTTP drive).
    let http = client();
    let auth = format!("Bearer {BEARER_VALUE}");
    let start_url = format!("http://{}/v0/instances/worker/start", server.addr);
    let deadline = Instant::now() + Duration::from_secs(30);
    let started = loop {
        if let Ok(mut response) = http
            .post(&start_url)
            .header("Authorization", &auth)
            .send("")
        {
            let status = response.status();
            let body = response.body_mut().read_to_string().unwrap_or_default();
            if status == StatusCode::OK {
                break body;
            }
            // Transitional refusals settle on retry within the bound.
        }
        assert!(
            Instant::now() < deadline,
            "the agent never started:\n{}",
            sink_handle.text()
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    let started: Value = serde_json::from_str(&started).expect("the start body is JSON");
    // `running` is the clean-start state; a fast breach (the usage commit
    // prices past the $0.01 cap within the start window) lands `paused` —
    // BOTH are success here: the fixture's purpose is the events, and a
    // breach-pause is the budget system working as designed.
    let start_state = started["instance"]["state"].as_str().unwrap();
    assert!(
        start_state == "running" || start_state == "paused",
        "the agent started (then may have breach-paused): {started}"
    );

    // The exporter flushes every second; the usage commit + breach land on
    // the bus within seconds of the start. Wait for a payload carrying the
    // token histogram (the proof the usage event was observed).
    collector.wait_for(
        |requests| {
            requests
                .iter()
                .any(|r| r.body.contains("gen_ai.client.token_usage"))
        },
        Duration::from_secs(30),
    );

    // --- the payload conformance assertions (decode + pin) ---
    let recorded = collector
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.body.contains("gen_ai.client.token_usage"))
        .expect("the carrying POST")
        .clone();
    assert_eq!(recorded.method, "POST", "OTLP/HTTP+JSON is a POST");
    assert!(
        recorded.path.ends_with("/v1/metrics"),
        "the endpoint path is <base>/v1/metrics: {}",
        recorded.path
    );
    assert_eq!(
        recorded.content_type.as_deref(),
        Some("application/json"),
        "Content-Type: application/json (the frozen wire rule)"
    );

    let payload: Value = serde_json::from_str(&recorded.body).expect("the body decodes");
    let metrics = &payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
    let names: Vec<&str> = metrics
        .as_array()
        .expect("metrics array")
        .iter()
        .map(|m| m["name"].as_str().expect("named metric"))
        .collect();
    for expected in [
        "gen_ai.client.token_usage",
        "hekma.budget.breaches",
        "hekma.usage.cost",
        "hekma.agent.state",
        "hekma.otlp.dropped",
    ] {
        assert!(
            names.contains(&expected),
            "the exposition names {expected}: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n.contains("operation_duration")),
        "no producer exists for operation duration — absent, never fabricated"
    );

    // camelCase + decimal-string 64-bit ints + numeric temporality.
    let find = |name: &str| -> Value {
        metrics
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} absent"))
    };
    let cost = find("hekma.usage.cost");
    assert_eq!(
        cost["sum"]["aggregationTemporality"], 1,
        "numeric CUMULATIVE on sums"
    );
    assert_eq!(cost["unit"], "USD");
    let cost_point = &cost["sum"]["dataPoints"][0];
    assert!(
        cost_point.get("asDouble").is_some() && cost_point.get("asInt").is_none(),
        "dollars export as asDouble: {cost_point}"
    );
    let attributes = cost_point["attributes"].as_array().unwrap();
    let estimate = attributes
        .iter()
        .find(|a| a["key"] == "hekma.estimate_state")
        .expect("MANDATORY estimate_state attribute — an unlabeled dollar fails here");
    assert_eq!(estimate["value"]["stringValue"], "estimated");
    // (The asInt-absence check on a DOUBLE point lives above, on the COST
    // DATA POINT itself — `cost_point.get("asInt").is_none()` — where it
    // actually asserts the wire shape. It previously sat on the attribute
    // KEY list, where it could never fail.)
    let point_strings_ints = |point: &Value, field: &str| {
        point[field]
            .as_str()
            .map(|s| s.parse::<u64>().is_ok())
            .unwrap_or(false)
    };
    assert!(
        point_strings_ints(cost_point, "timeUnixNano"),
        "timeUnixNano is a decimal STRING: {cost_point}"
    );
    assert!(
        point_strings_ints(cost_point, "startTimeUnixNano"),
        "startTimeUnixNano is a decimal STRING"
    );

    let token_usage = find("gen_ai.client.token_usage");
    let hist_point = &token_usage["histogram"]["dataPoints"][0];
    let bounds: Vec<f64> = hist_point["explicitBounds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_f64().unwrap())
        .collect();
    assert_eq!(
        bounds,
        vec![1_000.0, 10_000.0, 100_000.0, 1_000_000.0, 10_000_000.0],
        "the documented fixed histogram bounds"
    );
    assert!(
        hist_point["count"].as_str().is_some(),
        "histogram count is a decimal STRING: {hist_point}"
    );
    let token_types: Vec<&str> = token_usage["histogram"]["dataPoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            p["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == "gen_ai.token.type")
                .expect("the semconv attribute")["value"]["stringValue"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert!(token_types.contains(&"input"), "{token_types:?}");
    assert!(token_types.contains(&"output"), "{token_types:?}");

    // The resource identity.
    let resource = &payload["resourceMetrics"][0]["resource"]["attributes"];
    let resource_value = |key: &str| -> String {
        resource
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == key)
            .expect(key)["value"]["stringValue"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(resource_value("service.name"), "hekma-host");
    assert!(!resource_value("service.instance.id").is_empty());

    // --- the no-leak matrix at the export path ---
    assert!(
        !recorded.body.contains("secret:"),
        "a secret REFERENCE never rides the payload"
    );
    assert!(
        !recorded.body.contains(HEADER_LITERAL),
        "the header LITERAL is transport-only — the payload never carries it"
    );
    assert!(
        !recorded.body.contains(BEARER_VALUE),
        "the control bearer never leaks into telemetry"
    );
    let lower_body = recorded.body.to_ascii_lowercase();
    assert!(
        !lower_body.contains("authorization"),
        "no auth vocabulary leaks into the payload attributes"
    );

    // Shutdown: the exporter stops before the drain, serve exits clean.
    let result = server.stop().expect("the serve thread joins");
    assert!(result.is_ok(), "shutdown is clean: {result:?}");
    let text = sink_handle.text();
    assert!(
        text.contains("otlp export stopped"),
        "the export stop is surfaced in the shutdown sequence:\n{text}"
    );
    // The bearer env var deliberately STAYS set for the rest of the process
    // (parallel siblings' serve startups may still need it — see
    // `ensure_bearer_env`); test processes are short-lived, nothing leaks.
}

// ---------------------------------------------------------------------------
// Leg 5 — the retry leg: a 503 on the first POST is retried WITHIN the
// flush (the bounded budget + short backoff) and the 200 lands.
// ---------------------------------------------------------------------------

#[test]
fn a_retryable_503_is_retried_within_the_flush_and_lands() {
    // The script: the FIRST POST answers 503, every later one 200. The
    // exporter's delivery classifies 503 as retryable, sleeps the short
    // backoff, and re-POSTs the same batch — the retry must LAND well
    // before the next flush tick, with no "NOT delivered" note ever fired.
    let state = tempfile::tempdir().unwrap();
    let collector = Collector::start_scripted(vec![503, 200]);
    let (sink_handle, sink) = SharedSink::new();
    let otlp = hekma_host::OtlpOptions::from_flags(Some(collector.endpoint()), None, Some(1))
        .expect("the flags parse");
    let options = options_with(state.path().to_path_buf(), sink, otlp);
    let server = spawn_serve(options, &sink_handle, true);

    // Two recorded POSTs = the failed attempt + its retry (the flush
    // interval is 1s, the retry backoff 200ms — the second POST is the
    // retry, not the next cycle).
    collector.wait_for(|requests| requests.len() >= 2, Duration::from_secs(30));
    let recorded = collector.requests.lock().unwrap().clone();
    assert_eq!(recorded[0].answered, 503, "the first attempt was refused");
    assert_eq!(
        recorded[1].answered, 200,
        "the retried POST landed on the collector"
    );
    assert!(
        recorded[1].body.contains("hekma.otlp.dropped"),
        "the retried batch is the real payload: {}",
        recorded[1].body
    );

    // Delivery SUCCEEDED: no dropped-batch diagnostic ever fired.
    let text = sink_handle.text();
    assert!(
        !text.contains("NOT delivered"),
        "a retried delivery is a success — the drop note must not fire:\n{text}"
    );

    // Shutdown is clean, the stop notes name the clean stop.
    let result = server.stop().expect("the serve thread joins");
    assert!(result.is_ok(), "shutdown is clean: {result:?}");
    let text = sink_handle.text();
    assert!(
        text.contains("otlp export stopped"),
        "the export stop is surfaced:\n{text}"
    );
}

#[test]
fn a_refused_collector_surfaces_diagnostics_grows_the_counter_and_never_blocks() {
    // A bound-then-dropped port: nothing listens there anymore.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = listener.local_addr().unwrap().port();
    drop(listener);

    let state = tempfile::tempdir().unwrap();
    let (sink_handle, sink) = SharedSink::new();
    let otlp = hekma_host::OtlpOptions::from_flags(
        Some(format!("http://127.0.0.1:{dead_port}")),
        None,
        Some(1),
    )
    .expect("the flags parse");
    let options = options_with(state.path().to_path_buf(), sink, otlp);
    let server = spawn_serve(options, &sink_handle, true);

    // The Control API still answers while the collector refuses (the AC:
    // supervision and the API continue unaffected).
    let http = client();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let text = sink_handle.text();
        if text.contains("the metrics batch was NOT delivered") {
            break;
        }
        // Prove the API answers WHILE the export is failing.
        if let Some(addr) = text
            .lines()
            .find_map(|line| line.split("control api listening on ").nth(1))
            .map(|rest| rest.split(" (").next().unwrap().to_string())
        {
            let response = http
                .get(format!("http://{addr}/v0/version"))
                .header("Authorization", &format!("Bearer {BEARER_VALUE}"))
                .call();
            assert!(
                matches!(response, Ok(ref r) if r.status() == StatusCode::OK),
                "the Control API is unaffected by the export failure"
            );
        }
        assert!(
            Instant::now() < deadline,
            "the delivery failure was never surfaced:\n{}",
            sink_handle.text()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Shutdown: the final flush fails too, the in-flight/restart honesty
    // note names the outcome, and the host still exits CLEAN (exit 0 — the
    // stream, not the exit code, carries the export failure).
    let result = server.stop().expect("the serve thread joins");
    assert!(
        result.is_ok(),
        "a dead collector never fails the host: {result:?}"
    );
    let text = sink_handle.text();
    assert!(
        text.contains("otlp export stopped"),
        "the export stop is surfaced:\n{text}"
    );
    assert!(
        text.contains("dropped"),
        "the dropped counter's honesty rides the stop notes:\n{text}"
    );
}
