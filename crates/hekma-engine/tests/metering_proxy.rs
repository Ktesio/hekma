//! The shared Metering Proxy end to end (story 23-1): a real engine with
//! its proxy on an OS-picked loopback port, an upstream fake that records
//! every request it is sent, and raw HTTP/1.1 calls. No child processes,
//! so it runs on every OS.
//!
//! Each test proves what reaches the upstream (and what never does), the
//! typed `proxy-wire v1` answers, attribution to the right Run, and the
//! usage that lands in that Run's ledger rows.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_engine::{
    BindingSpec, BreachAction, Engine, EngineOptions, ListRunsQuery, ListingView,
    MeteringProxyOptions, PolicyChange, PolicyLimits, RunSummary,
};
use tempfile::TempDir;

/// The completion every upstream answer carries: 10 in, 5 out.
const COMPLETION: &str = r#"{"id":"cmpl-1","object":"chat.completion","model":"gpt-demo","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;

/// One request as the upstream fake saw it.
#[derive(Clone, Debug)]
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A loopback upstream that answers every request with [`COMPLETION`] and
/// records what it was sent.
struct Upstream {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Upstream {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let record = Arc::clone(&record);
                std::thread::spawn(move || {
                    if let Some(request) = read_request(&mut stream) {
                        record.lock().unwrap().push(request);
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{COMPLETION}",
                            COMPLETION.len()
                        );
                        let _ = stream.write_all(reply.as_bytes());
                    }
                });
            }
        });
        Self { port, seen }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn served(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Read one request's head and its `content-length` body.
fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break index;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Seen {
        request_line,
        headers,
    })
}

/// One answer from the proxy.
#[derive(Debug)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }

    /// The `proxy-wire v1` refusal's `error.type`.
    fn error_type(&self) -> String {
        let json = self.json();
        assert_eq!(json["error"]["proxy_wire"], "v1", "{}", self.body);
        json["error"]["type"].as_str().unwrap().to_string()
    }
}

/// Send one raw request line with `headers` and a JSON body; `None` sends
/// no body (a `GET`).
fn send(port: u16, request_line: &str, headers: &[(&str, &str)], body: Option<&str>) -> Answer {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut request = format!("{request_line}\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\n");
    for (key, value) in headers {
        request.push_str(&format!("{key}: {value}\r\n"));
    }
    if let Some(body) = body {
        request.push_str(&format!(
            "content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ));
    } else {
        request.push_str("\r\n");
    }
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in: {text}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    Answer {
        status,
        headers,
        body: body.to_string(),
    }
}

const CHAT: &str = r#"{"model":"gpt-demo","messages":[{"role":"user","content":"hi"}]}"#;

fn post(port: u16, path: &str, headers: &[(&str, &str)]) -> Answer {
    send(port, &format!("POST {path} HTTP/1.1"), headers, Some(CHAT))
}

fn open(state: &TempDir, identity_headers: bool) -> (Engine, u16) {
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_proxy: MeteringProxyOptions {
                identity_headers,
                ..MeteringProxyOptions::default()
            },
            ..EngineOptions::default()
        },
    )
    .expect("open with the proxy");
    let port = engine.metering_proxy().expect("the proxy runs").port;
    (engine, port)
}

fn bind(engine: &Engine, name: &str, prefix: &str, upstream: &str, labels: &[(&str, &str)]) {
    engine
        .blocking()
        .create_binding(BindingSpec {
            name: name.to_string(),
            prefix: prefix.to_string(),
            upstream: upstream.to_string(),
            labels: labels
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            custodied_key: None,
        })
        .unwrap();
}

/// Every Run in the listing.
fn runs(engine: &Engine) -> Vec<RunSummary> {
    engine
        .blocking()
        .list_runs(ListRunsQuery {
            view: ListingView::All,
            limit: 0,
            ..ListRunsQuery::default()
        })
        .unwrap()
        .items
}

/// Poll until a Run with `identity` has `tokens` of input, returning it
/// (proxied usage commits within the writer's batch window).
fn wait_for_usage(engine: &Engine, identity: (&str, &str), input: u64) -> RunSummary {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let found = runs(engine).into_iter().find(|run| {
            run.identity
                .as_ref()
                .is_some_and(|(ns, id)| ns == identity.0 && id == identity.1)
        });
        if let Some(run) = &found {
            if run.usage.input_tokens >= input {
                return found.unwrap();
            }
        }
        assert!(
            Instant::now() < deadline,
            "no Run {identity:?} with {input} input tokens: {found:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_binding_call_is_forwarded_stripped_labelled_and_metered_once() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );

    let answer = post(
        port,
        "/openai/chat/completions?x=1",
        &[
            ("authorization", "Bearer sk-provider-key"),
            ("x-hekma-anything", "never-forwarded"),
        ],
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.json()["usage"]["prompt_tokens"], 10);
    // No breach is latched, so no breach header.
    assert!(answer.header("x-hekma-breach").is_none());

    let served = upstream.served();
    assert_eq!(served.len(), 1);
    let seen = &served[0];
    // The prefix is replaced by the upstream's path; the query rides along.
    assert_eq!(seen.request_line, "POST /v1/chat/completions?x=1 HTTP/1.1");
    // The provider key passes; every `x-hekma-*` request header is stripped,
    // and the identity headers are added.
    assert_eq!(seen.header("authorization"), Some("Bearer sk-provider-key"));
    assert!(seen.header("x-hekma-anything").is_none());
    assert_eq!(seen.header("x-hekma-labels"), Some("team=core"));
    let run_id = seen.header("x-hekma-run-id").expect("the Run id rides out");
    assert!(run_id.starts_with("run_"), "{run_id}");

    // Attributed to the binding's own Run, which carries its labels.
    let run = wait_for_usage(&engine, ("binding", "openai"), 10);
    assert_eq!(run.record.run_id.as_str(), run_id);
    assert_eq!(run.usage.output_tokens, 5);
    assert!(
        run.labels
            .iter()
            .any(|label| label.key == "team" && label.value == "core"),
        "{:?}",
        run.labels
    );
    // A second call lands on the same Run, counted once more (no dedup
    // across distinct calls, no double count of one call).
    assert_eq!(post(port, "/openai/chat/completions", &[]).status, 200);
    let run = wait_for_usage(&engine, ("binding", "openai"), 20);
    assert_eq!(run.usage.input_tokens, 20);
    assert_eq!(run.usage.output_tokens, 10);
}

#[test]
fn identity_headers_can_be_turned_off() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, false);
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    let seen = &upstream.served()[0];
    assert!(seen.header("x-hekma-run-id").is_none());
    assert!(seen.header("x-hekma-labels").is_none());
}

#[test]
fn every_refusal_is_typed_and_never_reaches_the_upstream() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    let unknown = format!("hk_run_{}", "A".repeat(52));
    let bearer = format!("Bearer {unknown}");
    let cases: Vec<(Answer, u16, &str)> = vec![
        (post(port, "/elsewhere/v1", &[]), 404, "hekma_no_route"),
        (
            post(port, "/_hekma/runs/run_GONE/v1", &[]),
            404,
            "hekma_no_route",
        ),
        (
            post(port, &format!("/openai/v1?key={unknown}"), &[]),
            400,
            "hekma_credential_in_url",
        ),
        (
            post(port, "/openai/v1", &[("x-hekma-run-key", &unknown)]),
            401,
            "hekma_unknown_credential",
        ),
        (
            post(port, "/openai/v1", &[("authorization", &bearer)]),
            401,
            "hekma_unknown_credential",
        ),
        (
            send(port, "CONNECT api.example.com:443 HTTP/1.1", &[], None),
            400,
            "hekma_bad_target",
        ),
        (
            send(
                port,
                "POST http://evil.example/v1 HTTP/1.1",
                &[],
                Some(CHAT),
            ),
            400,
            "hekma_bad_target",
        ),
    ];
    for (answer, status, error_type) in &cases {
        assert_eq!(answer.status, *status, "{error_type}: {}", answer.body);
        assert_eq!(answer.error_type(), *error_type);
        assert_eq!(answer.header("content-type"), Some("application/json"));
        assert_eq!(answer.header("x-should-retry"), Some("false"));
        // No body carries the presented token.
        assert!(!answer.body.contains(&unknown), "{}", answer.body);
    }
    assert!(
        upstream.served().is_empty(),
        "a refused call reached the upstream: {:?}",
        upstream.served()
    );
}

#[test]
fn an_unreachable_upstream_is_a_typed_502_with_the_sdk_default_retry() {
    let state = TempDir::new().unwrap();
    // A port nothing listens on.
    let dead = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    };
    let (engine, port) = open(&state, true);
    bind(
        &engine,
        "dead",
        "/dead",
        &format!("http://127.0.0.1:{dead}"),
        &[("team", "core")],
    );
    let answer = post(port, "/dead/v1/chat", &[]);
    assert_eq!(answer.status, 502, "{}", answer.body);
    assert_eq!(answer.error_type(), "hekma_upstream_error");
    assert!(answer.header("x-should-retry").is_none());
}

#[test]
fn unlabelled_bindings_charge_the_unattributed_run_and_hints_split_labelled_ones() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    bind(&engine, "plain", "/plain", &upstream.base(), &[]);
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );

    // An unlabelled binding: the unattributed Run; a hint there is ignored.
    assert_eq!(
        post(port, "/plain/chat", &[("x-hekma-run", "anyone")]).status,
        200
    );
    let unattributed = wait_for_usage(&engine, ("proxy", "unattributed"), 10);
    assert!(unattributed.record.instance.is_none());
    assert!(
        !runs(&engine)
            .iter()
            .any(|run| run.identity.as_ref().is_some_and(|(_, id)| id == "anyone")),
        "a hint beneath the unattributed Run minted a Run"
    );

    // A labelled binding with hints: one split Run per id, each carrying
    // the binding's labels, each metered on its own.
    for split in ["team-a", "team-b", "team-a"] {
        assert_eq!(
            post(port, "/openai/chat", &[("x-hekma-run", split)]).status,
            200
        );
    }
    let a = wait_for_usage(&engine, ("binding:openai", "team-a"), 20);
    let b = wait_for_usage(&engine, ("binding:openai", "team-b"), 10);
    assert_ne!(a.record.run_id, b.record.run_id);
    assert_eq!(b.usage.input_tokens, 10);
    for run in [&a, &b] {
        assert!(run.record.parent_run_id.is_none(), "a split is top-level");
        assert!(
            run.labels
                .iter()
                .any(|label| label.key == "team" && label.value == "core"),
            "{:?}",
            run.labels
        );
    }
    // Every hint header was stripped before forwarding.
    assert!(upstream
        .served()
        .iter()
        .all(|seen| seen.header("x-hekma-run").is_none()));
}

#[test]
fn block_refuses_a_wire_runs_next_call_and_lets_safe_methods_pass() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    engine
        .blocking()
        .create_policy(
            PolicyChange {
                name: "core-cap".to_string(),
                selector: Some("team=core".parse().unwrap()),
                set: PolicyLimits {
                    tokens_per_run: Some(10),
                    breach_action: Some(BreachAction::Block),
                    ..PolicyLimits::default()
                },
                ..PolicyChange::default()
            },
            false,
        )
        .unwrap();
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );

    // The first call crosses the 10-token per-run ceiling (15 tokens).
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    wait_for_usage(&engine, ("binding", "openai"), 10);
    // The breach latches `block` for the wire Run.
    let deadline = Instant::now() + Duration::from_secs(15);
    let refused = loop {
        let answer = post(port, "/openai/chat", &[]);
        if answer.status == 402 {
            break answer;
        }
        assert_eq!(answer.status, 200, "{}", answer.body);
        assert!(Instant::now() < deadline, "block never fenced the Run");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(refused.error_type(), "hekma_budget_block");
    assert_eq!(refused.header("x-should-retry"), Some("false"));
    let served_at_block = upstream.served().len();

    // Further non-safe calls stay refused and never reach the upstream.
    assert_eq!(post(port, "/openai/chat", &[]).status, 402);
    assert_eq!(upstream.served().len(), served_at_block);
    // A safe method still passes, and carries the breach header.
    let listing = send(port, "GET /openai/models HTTP/1.1", &[], None);
    assert_eq!(listing.status, 200, "{}", listing.body);
    assert!(
        listing.header("x-hekma-breach").is_some(),
        "{:?}",
        listing.headers
    );
    assert_eq!(upstream.served().len(), served_at_block + 1);
}

#[test]
fn a_child_runs_key_charges_its_own_run_and_is_refused_once_revoked() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    // The binding's Run exists after its first call.
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    let parent = wait_for_usage(&engine, ("binding", "openai"), 10);

    // A child Run beneath it, with its Run Key.
    let mut request = hekma_engine::ChildRunRequest::under(hekma_engine::RunParent::Run(
        parent.record.run_id.clone(),
    ));
    request.issue_credentials = true;
    let opened = engine
        .blocking()
        .open_child_run_as(hekma_engine::RunActor::Operator, request)
        .unwrap();
    let credentials = opened.credentials.expect("the pair is issued");
    let key = credentials.run_key().expose_secret().to_string();
    let child = opened.view.record.run_id.clone();

    // The key in x-hekma-run-key charges the child, on the binding's route.
    let answer = post(port, "/openai/chat", &[("x-hekma-run-key", &key)]);
    assert_eq!(answer.status, 200, "{}", answer.body);
    let served = upstream.served();
    let seen = served.last().unwrap();
    assert!(
        seen.header("x-hekma-run-key").is_none(),
        "the key went upstream"
    );
    assert_eq!(seen.header("x-hekma-run-id"), Some(child.as_str()));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let usage = runs(&engine)
            .into_iter()
            .find(|run| run.record.run_id == child)
            .map(|run| run.usage.input_tokens)
            .unwrap_or_default();
        if usage >= 10 {
            break;
        }
        assert!(Instant::now() < deadline, "the child was never charged");
        std::thread::sleep(Duration::from_millis(50));
    }

    // The same key in the API-key slot would reach the provider: refused.
    let bearer = format!("Bearer {key}");
    let answer = post(port, "/openai/chat", &[("authorization", &bearer)]);
    assert_eq!(answer.status, 400, "{}", answer.body);
    assert_eq!(answer.error_type(), "hekma_custody_unavailable");

    // Revoked: the next call is told so, and is not forwarded.
    let served_before = upstream.served().len();
    engine
        .blocking()
        .revoke_run_keys(child.as_str(), true)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let answer = loop {
        let answer = post(port, "/openai/chat", &[("x-hekma-run-key", &key)]);
        if answer.status != 200 {
            break answer;
        }
        assert!(Instant::now() < deadline, "the revoked key still served");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(answer.status, 401, "{}", answer.body);
    assert_eq!(answer.error_type(), "hekma_credential_revoked");
    assert!(!answer.body.contains(&key));
    let served_after = upstream.served().len();
    assert!(
        served_after <= served_before + 1,
        "revoked calls reached the upstream"
    );
    // Once refused, it stays refused.
    assert_eq!(
        post(port, "/openai/chat", &[("x-hekma-run-key", &key)]).status,
        401
    );
    assert_eq!(upstream.served().len(), served_after);
}
