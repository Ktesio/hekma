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
    BindingSpec, BreachAction, CallView, CallsQuery, Engine, EngineOptions, ListRunsQuery,
    ListingView, MeteringProxyOptions, PolicyChange, PolicyLimits, RunSummary,
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

/// How the upstream fake answers.
#[derive(Clone, Copy)]
enum Reply {
    /// [`COMPLETION`] as JSON.
    Completion,
    /// A streamed completion whose final frame carries the usage.
    Sse,
    /// Headers promising more body than is sent, then a close.
    Truncated,
    /// [`COMPLETION`] after a pause.
    Slow(Duration),
    /// A completion padded to about this many bytes (its usage intact).
    Padded(usize),
}

const SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n\
data: [DONE]\n\n";

fn reply_bytes(reply: Reply) -> Vec<u8> {
    match reply {
        Reply::Completion | Reply::Slow(_) => format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{COMPLETION}",
            COMPLETION.len()
        )
        .into_bytes(),
        Reply::Sse => format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{SSE}",
            SSE.len()
        )
        .into_bytes(),
        Reply::Padded(len) => {
            let body = format!(
                r#"{{"model":"gpt-demo","pad":"{}","usage":{{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}}}"#,
                "x".repeat(len)
            );
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        }
        Reply::Truncated => b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
            content-length: 500\r\nconnection: close\r\n\r\n{\"partial\""
            .to_vec(),
    }
}

impl Upstream {
    fn start() -> Self {
        Self::start_with(Reply::Completion)
    }

    fn start_with(reply: Reply) -> Self {
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
                        if let Reply::Slow(pause) = reply {
                            std::thread::sleep(pause);
                        }
                        let _ = stream.write_all(&reply_bytes(reply));
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

#[test]
fn the_binding_lifecycle_through_the_engine_with_its_refusals_and_the_proxy_in_step() {
    use hekma_engine::{BindingCredentialClass, BindingError, BindingPatch, RouteSpec};
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    let facade = engine.blocking();
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    let spec = |name: &str, prefix: &str, upstream: &str, key: Option<&str>| BindingSpec {
        name: name.into(),
        prefix: prefix.into(),
        upstream: upstream.into(),
        labels: Vec::new(),
        custodied_key: key.map(str::to_string),
    };

    // Create refusals, each with its stable code.
    for (bad, code) in [
        (
            spec("openai", "/other", "http://127.0.0.1:9", None),
            "binding-exists",
        ),
        (
            spec("dup", "/openai", "http://127.0.0.1:9", None),
            "binding-prefix-taken",
        ),
        (
            spec("inner", "/_hekma/x", "http://127.0.0.1:9", None),
            "binding-invalid",
        ),
        (
            spec("dots", "/a/../b", "http://127.0.0.1:9", None),
            "binding-invalid",
        ),
        (
            spec("userinfo", "/u", "https://u:p@example.com", None),
            "binding-invalid",
        ),
        (
            spec("query", "/q", "https://example.com/?a=1", None),
            "binding-invalid",
        ),
        (
            spec("scheme", "/s", "ftp://example.com", None),
            "binding-invalid",
        ),
        (
            spec("key", "/k", "https://example.com", Some("sk-plain")),
            "binding-invalid",
        ),
        (
            spec("Bad Name", "/n", "https://example.com", None),
            "binding-invalid",
        ),
    ] {
        let err = facade.create_binding(bad.clone()).unwrap_err();
        assert_eq!(err.code(), code, "{bad:?}: {err}");
    }
    // Story 22-2: a custodied key goes only over https to an origin it is
    // pinned to — an http upstream, and an https one no table or
    // declaration names, are refused.
    for unpinned in [
        upstream.base(),
        "https://gateway.example.com/v1".to_string(),
    ] {
        let err = facade
            .create_binding(spec("keyed", "/keyed", &unpinned, Some("secret:KEY")))
            .unwrap_err();
        assert_eq!(err.code(), "binding-invalid", "{unpinned}: {err}");
    }
    // A custodied-key reference on a pinnable upstream is stored as given.
    let keyed = facade
        .create_binding(spec(
            "keyed",
            "/keyed",
            "https://api.openai.com/v1",
            Some("secret:KEY"),
        ))
        .unwrap();
    assert_eq!(keyed.custodied_key.as_deref(), Some("secret:KEY"));
    assert_eq!(facade.bindings(None, 100).unwrap().len(), 2);
    assert_eq!(
        facade.bindings(Some("keyed"), 100).unwrap()[0].name,
        "openai"
    );

    // A first call opens the binding's Run.
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    let before = wait_for_usage(&engine, ("binding", "openai"), 10);

    // A prefix change keeps the Run; the old prefix stops routing at once.
    let changed = facade
        .update_binding(
            "openai",
            BindingPatch {
                prefix: Some("/llm".into()),
                ..BindingPatch::default()
            },
        )
        .unwrap();
    assert_eq!(changed.prefix, "/llm");
    assert_eq!(post(port, "/openai/chat", &[]).status, 404);
    assert_eq!(post(port, "/llm/chat", &[]).status, 200);
    let after = wait_for_usage(&engine, ("binding", "openai"), 20);
    assert_eq!(after.record.run_id, before.record.run_id);
    // A label change re-resolves the open Run's labels.
    facade
        .update_binding(
            "openai",
            BindingPatch {
                labels: Some(vec![("team".into(), "platform".into())]),
                ..BindingPatch::default()
            },
        )
        .unwrap();
    let relabelled = runs(&engine)
        .into_iter()
        .find(|run| run.record.run_id == before.record.run_id)
        .unwrap();
    assert!(
        relabelled
            .labels
            .iter()
            .any(|label| label.key == "team" && label.value == "platform"),
        "{:?}",
        relabelled.labels
    );
    // Set and clear the custodied-key reference; refusals on update. A key
    // on the binding's http stub upstream is refused (story 22-2: a
    // custodied key goes only over https to a pinned origin); with a
    // pinnable https upstream it is stored, and clearing it puts the stub
    // back for the calls below.
    let err = facade
        .update_binding(
            "openai",
            BindingPatch {
                custodied_key: Some(Some("secret:OPENAI".into())),
                ..BindingPatch::default()
            },
        )
        .unwrap_err();
    assert_eq!(err.code(), "binding-invalid", "{err}");
    let set = facade
        .update_binding(
            "openai",
            BindingPatch {
                upstream: Some("https://api.openai.com/v1".into()),
                custodied_key: Some(Some("secret:OPENAI".into())),
                ..BindingPatch::default()
            },
        )
        .unwrap();
    assert_eq!(set.custodied_key.as_deref(), Some("secret:OPENAI"));
    let cleared = facade
        .update_binding(
            "openai",
            BindingPatch {
                upstream: Some(upstream.base()),
                custodied_key: Some(None),
                ..BindingPatch::default()
            },
        )
        .unwrap();
    assert!(cleared.custodied_key.is_none());
    for (name, patch, code) in [
        ("ghost", BindingPatch::default(), "binding-not-found"),
        (
            "openai",
            BindingPatch {
                prefix: Some("/keyed".into()),
                ..BindingPatch::default()
            },
            "binding-prefix-taken",
        ),
        (
            "openai",
            BindingPatch {
                upstream: Some("not a url".into()),
                ..BindingPatch::default()
            },
            "binding-invalid",
        ),
        (
            "openai",
            BindingPatch {
                custodied_key: Some(Some("plain".into())),
                ..BindingPatch::default()
            },
            "binding-invalid",
        ),
    ] {
        let err = facade.update_binding(name, patch).unwrap_err();
        assert_eq!(err.code(), code, "{name}: {err}");
    }

    // Routes: add, replace, remove, refusals, and the per-binding cap.
    let route = |name: &str| RouteSpec {
        name: name.into(),
        upstream: "https://backup.example.com/v1".into(),
        custodied_key: None,
    };
    let with = facade.put_binding_route("openai", route("backup")).unwrap();
    assert_eq!(with.routes.len(), 1);
    let replaced = facade
        .put_binding_route(
            "openai",
            RouteSpec {
                upstream: "https://other.example.com".into(),
                ..route("backup")
            },
        )
        .unwrap();
    assert_eq!(replaced.routes.len(), 1);
    assert_eq!(replaced.routes[0].upstream, "https://other.example.com");
    for n in 1..16 {
        facade
            .put_binding_route("openai", route(&format!("r{n}")))
            .unwrap();
    }
    let err = facade
        .put_binding_route("openai", route("r16"))
        .unwrap_err();
    assert_eq!(err.code(), "binding-limit", "{err}");
    let err = facade
        .put_binding_route(
            "openai",
            RouteSpec {
                upstream: "ftp://x".into(),
                ..route("bad")
            },
        )
        .unwrap_err();
    assert_eq!(err.code(), "binding-invalid", "{err}");
    let fewer = facade.remove_binding_route("openai", "backup").unwrap();
    assert_eq!(fewer.routes.len(), 15);
    let err = facade.remove_binding_route("openai", "backup").unwrap_err();
    assert!(matches!(err, BindingError::RouteNotFound { .. }), "{err}");
    let err = facade.remove_binding_route("ghost", "r1").unwrap_err();
    assert_eq!(err.code(), "binding-not-found");

    // Credentials: issue rotates, revoke is per class, listing shows states.
    let first = facade
        .issue_binding_credential("openai", BindingCredentialClass::ProxyAccess)
        .unwrap();
    let second = facade
        .issue_binding_credential("openai", BindingCredentialClass::ProxyAccess)
        .unwrap();
    assert_ne!(first.token_id, second.token_id);
    assert!(second.token.expose_secret().starts_with("hk_pxy_"));
    facade
        .issue_binding_credential("openai", BindingCredentialClass::Announce)
        .unwrap();
    facade
        .revoke_binding_credential("openai", BindingCredentialClass::Announce)
        .unwrap();
    let listing = facade.binding_credentials("openai").unwrap();
    let state_of = |id: &str| {
        listing
            .iter()
            .find(|credential| credential.token_id == id)
            .map(|credential| credential.state.clone())
    };
    assert_eq!(state_of(&first.token_id).as_deref(), Some("revoked"));
    assert_eq!(state_of(&second.token_id).as_deref(), Some("active"));
    assert_eq!(
        listing
            .iter()
            .filter(|c| c.class == BindingCredentialClass::Announce && c.state == "active")
            .count(),
        0
    );
    for err in [
        facade
            .issue_binding_credential("ghost", BindingCredentialClass::Announce)
            .unwrap_err(),
        facade
            .revoke_binding_credential("ghost", BindingCredentialClass::Announce)
            .unwrap_err(),
        facade.binding_credentials("ghost").unwrap_err(),
    ] {
        assert_eq!(err.code(), "binding-not-found", "{err}");
    }

    // Delete: the binding's Run ends, its prefix stops routing.
    facade.delete_binding("openai").unwrap();
    assert!(facade.binding("openai").unwrap().is_none());
    assert_eq!(post(port, "/llm/chat", &[]).status, 404);
    let ended = runs(&engine)
        .into_iter()
        .find(|run| run.record.run_id == before.record.run_id)
        .unwrap();
    assert_eq!(ended.record.status, hekma_engine::RunStatus::Ended);
    assert_eq!(
        facade.delete_binding("openai").unwrap_err().code(),
        "binding-not-found"
    );

    // Stopping the proxy closes its port.
    facade.stop_metering_proxy(Duration::from_secs(1));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[test]
fn a_streamed_completion_is_metered_from_its_final_usage_frame() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start_with(Reply::Sse);
    let (engine, port) = open(&state, true);
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    let answer = post(port, "/openai/chat", &[]);
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(
        answer.body.contains("[DONE]"),
        "the stream is relayed whole"
    );
    let run = wait_for_usage(&engine, ("binding", "openai"), 7);
    assert_eq!(run.usage.output_tokens, 3);
}

#[test]
fn a_truncated_upstream_body_is_a_typed_502_and_nothing_is_metered() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start_with(Reply::Truncated);
    let (engine, port) = open(&state, true);
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    let answer = post(port, "/openai/chat", &[]);
    assert_eq!(answer.status, 502, "{}", answer.body);
    assert_eq!(answer.error_type(), "hekma_upstream_error");
    std::thread::sleep(Duration::from_millis(400));
    assert!(runs(&engine).iter().all(|run| run.usage.input_tokens == 0));
}

#[test]
fn an_oversized_or_over_budget_request_is_refused_before_any_dial() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_proxy: MeteringProxyOptions {
                // One KiB of buffer for every call at once.
                max_buffered_bytes: 1024,
                ..MeteringProxyOptions::default()
            },
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let port = engine.metering_proxy().unwrap().port;
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    // A declared body over the 64 MiB cap: 413 from the header alone.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .write_all(
            b"POST /openai/chat HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n\
              content-type: application/json\r\ncontent-length: 70000000\r\n\r\n{}",
        )
        .unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    assert!(raw.starts_with("HTTP/1.1 413"), "{raw}");
    assert!(raw.contains("hekma_body_too_large"), "{raw}");
    // A body larger than the whole buffer budget: busy, retryable.
    let big = format!(r#"{{"model":"gpt-demo","pad":"{}"}}"#, "x".repeat(4096));
    let answer = send(port, "POST /openai/chat HTTP/1.1", &[], Some(&big));
    assert_eq!(answer.status, 503, "{}", answer.body);
    assert_eq!(answer.error_type(), "hekma_proxy_busy");
    assert_eq!(answer.header("x-should-retry"), Some("true"));
    assert_eq!(answer.header("retry-after"), Some("1"));
    assert!(upstream.served().is_empty());
    // A small call still fits.
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
}

#[test]
fn a_client_that_disconnects_cancels_the_call_and_is_not_metered() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start_with(Reply::Slow(Duration::from_millis(1_500)));
    let (engine, port) = open(&state, true);
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let request = format!(
            "POST /openai/chat HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{CHAT}",
            CHAT.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        // Wait until the call reached the upstream, then hang up.
        let deadline = Instant::now() + Duration::from_secs(10);
        while upstream.served().is_empty() {
            assert!(
                Instant::now() < deadline,
                "the call never reached the upstream"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    std::thread::sleep(Duration::from_millis(2_500));
    assert!(
        runs(&engine).iter().all(|run| run.usage.input_tokens == 0),
        "a cancelled call was metered"
    );
    // The proxy keeps serving.
    let upstream2 = Upstream::start();
    bind(
        &engine,
        "next",
        "/next",
        &upstream2.base(),
        &[("team", "core")],
    );
    assert_eq!(post(port, "/next/chat", &[]).status, 200);
}

/// A `Write` into a shared buffer (the engine's diagnostic sink).
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn calls_left_open_by_a_dead_host_are_closed_crash_or_recovered_at_the_next_open() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (run_id, call_id) = {
        let (engine, port) = open(&state, true);
        bind(
            &engine,
            "openai",
            "/openai",
            &upstream.base(),
            &[("team", "core")],
        );
        assert_eq!(post(port, "/openai/chat", &[]).status, 200);
        let run = wait_for_usage(&engine, ("binding", "openai"), 10);
        let db = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
        let call_id: String = db
            .query_row(
                "SELECT call_id FROM usage_events WHERE run_id = ?1",
                [run.record.run_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        (run.record.run_id.as_str().to_string(), call_id)
    };
    // Two calls a host that died left open: one whose usage made it into
    // the ledger (recovered), one whose did not (crash). Their owner is a
    // process that does not exist.
    let segment = std::fs::read_dir(state.path().join("calls"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "db"))
        .expect("the calls segment");
    let calls = rusqlite::Connection::open(&segment).unwrap();
    for id in [call_id.as_str(), "call_LOST"] {
        calls
            .execute(
                "INSERT INTO open_calls (call_id, run_id, token_id, control_depth, binding, \
                 started_at, owner_pid, owner_start_time) \
                 VALUES (?1, ?2, NULL, 'wire', 'openai', 't', 2147483000, 1)",
                rusqlite::params![id, run_id],
            )
            .unwrap();
    }
    drop(calls);

    let captured: Arc<Mutex<Vec<u8>>> = Arc::default();
    let sink: hekma_engine::DiagnosticSink =
        Arc::new(Mutex::new(Box::new(Capture(Arc::clone(&captured)))));
    let engine = Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).unwrap();
    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains(
            "1 proxied call(s) in flight when the previous host stopped were closed as \
                       crashed"
        ) && text.contains("1 as recovered"),
        "{text}"
    );
    let calls = rusqlite::Connection::open(&segment).unwrap();
    let open: i64 = calls
        .query_row("SELECT count(*) FROM open_calls", [], |row| row.get(0))
        .unwrap();
    assert_eq!(open, 0);
    let lost: String = calls
        .query_row(
            "SELECT outcome FROM calls WHERE call_id = 'call_LOST'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lost, "crash");
    // Nothing was counted twice: the usage is the one real call's.
    let run = runs(&engine)
        .into_iter()
        .find(|run| run.record.run_id.as_str() == run_id)
        .unwrap();
    assert_eq!(run.usage.input_tokens, 10);
    drop(engine);
    // A second open finds nothing left to close.
    let captured: Arc<Mutex<Vec<u8>>> = Arc::default();
    let sink: hekma_engine::DiagnosticSink =
        Arc::new(Mutex::new(Box::new(Capture(Arc::clone(&captured)))));
    drop(Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).unwrap());
    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(!text.contains("proxied call(s)"), "{text}");
}

#[test]
fn a_wire_runs_usage_and_breach_reach_the_event_bus() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    let mut events = engine.subscribe();
    engine
        .blocking()
        .create_policy(
            PolicyChange {
                name: "core-warn".to_string(),
                selector: Some("team=core".parse().unwrap()),
                set: PolicyLimits {
                    tokens_per_run: Some(10),
                    breach_action: Some(BreachAction::Warn),
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
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    let deadline = Instant::now() + Duration::from_secs(15);
    let (mut usage, mut breach) = (false, false);
    while !(usage && breach) {
        assert!(
            Instant::now() < deadline,
            "usage {usage}, breach {breach}: not both published"
        );
        match events.try_recv() {
            Ok(hekma_engine::EngineEvent::UsageUpdate(_)) => usage = true,
            Ok(hekma_engine::EngineEvent::BudgetBreach(_)) => breach = true,
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[test]
fn past_the_breadth_cap_a_split_hint_stays_on_the_bindings_run_attributed_never_refused() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let captured: Arc<Mutex<Vec<u8>>> = Arc::default();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            diagnostics: Some(Arc::new(Mutex::new(Box::new(Capture(Arc::clone(
                &captured,
            )))))),
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let port = engine.metering_proxy().unwrap().port;
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    // The binding's own Run first.
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    let own = wait_for_usage(&engine, ("binding", "openai"), 10);
    for n in 0..hekma_engine::MAX_CALLER_OPEN_CHILDREN {
        let id = format!("s{n}");
        let answer = post(port, "/openai/chat", &[("x-hekma-run", &id)]);
        assert_eq!(answer.status, 200, "{id}: {}", answer.body);
    }
    // One past the cap: served, and charged to the binding's own Run.
    let answer = post(port, "/openai/chat", &[("x-hekma-run", "one-too-many")]);
    assert_eq!(answer.status, 200, "{}", answer.body);
    let run_id = upstream
        .served()
        .last()
        .unwrap()
        .header("x-hekma-run-id")
        .map(str::to_string);
    assert_eq!(run_id.as_deref(), Some(own.record.run_id.as_str()));
    // The listing pages past 1,000 Runs, so read the Run itself.
    let deadline = Instant::now() + Duration::from_secs(15);
    while engine
        .blocking()
        .run(own.record.run_id.as_str())
        .unwrap()
        .usage
        .input_tokens
        < 20
    {
        assert!(Instant::now() < deadline, "the capped call was not charged");
        std::thread::sleep(Duration::from_millis(50));
    }
    // And it is said, once (rate-limited), never per call.
    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert_eq!(
        text.matches("could not open its own Run").count(),
        1,
        "the capped hint is said once: {text}"
    );
}

#[test]
fn a_calls_store_that_cannot_open_is_said_and_calls_still_meter() {
    let state = TempDir::new().unwrap();
    // A plain file where the calls directory belongs.
    std::fs::write(state.path().join("calls"), b"not a directory").unwrap();
    let upstream = Upstream::start();
    let captured: Arc<Mutex<Vec<u8>>> = Arc::default();
    let sink: hekma_engine::DiagnosticSink =
        Arc::new(Mutex::new(Box::new(Capture(Arc::clone(&captured)))));
    let engine = Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).unwrap();
    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("the per-call records store could not be opened"),
        "{text}"
    );
    let port = engine.metering_proxy().expect("the proxy still runs").port;
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    let run = wait_for_usage(&engine, ("binding", "openai"), 10);
    assert_eq!(run.usage.output_tokens, 5);
}

/// Whether any file under `root` carries `needle`.
fn files_carrying(root: &std::path::Path, needle: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    found.push(path);
                }
            }
        }
    }
    found
}

#[test]
fn no_provider_key_or_run_key_leaks_into_state_responses_events_or_upstream() {
    const PROVIDER_KEY: &str = "sk-SENTINEL-provider-key-7f3a9c";
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    let mut events = engine.subscribe();
    engine
        .blocking()
        .create_policy(
            PolicyChange {
                name: "core-cap".to_string(),
                selector: Some("team=core".parse().unwrap()),
                set: PolicyLimits {
                    tokens_per_run: Some(25),
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
    let provider = format!("Bearer {PROVIDER_KEY}");
    assert_eq!(
        post(port, "/openai/chat", &[("authorization", &provider)]).status,
        200
    );
    let parent = wait_for_usage(&engine, ("binding", "openai"), 10);
    let mut request = hekma_engine::ChildRunRequest::under(hekma_engine::RunParent::Run(
        parent.record.run_id.clone(),
    ));
    request.issue_credentials = true;
    let opened = engine
        .blocking()
        .open_child_run_as(hekma_engine::RunActor::Operator, request)
        .unwrap();
    let run_key = opened
        .credentials
        .unwrap()
        .run_key()
        .expose_secret()
        .to_string();

    // Every path: a keyed call, hints, a refusal of each kind, the key in
    // the API-key slot and in the URL, then the fence and a revocation.
    let mut answers = Vec::new();
    let keyed = [
        ("authorization", provider.as_str()),
        ("x-hekma-run-key", run_key.as_str()),
    ];
    answers.push(post(port, "/openai/chat", &keyed));
    answers.push(post(
        port,
        "/openai/chat",
        &[("authorization", &provider), ("x-hekma-run", "split-1")],
    ));
    answers.push(post(port, "/nowhere", &[("authorization", &provider)]));
    answers.push(post(port, &format!("/openai/chat?k={run_key}"), &[]));
    let in_slot = format!("Bearer {run_key}");
    answers.push(post(port, "/openai/chat", &[("authorization", &in_slot)]));
    answers.push(post(port, "/openai/chat", &[("x-api-key", &run_key)]));
    for _ in 0..3 {
        answers.push(post(port, "/openai/chat", &keyed));
    }
    engine
        .blocking()
        .revoke_run_keys(opened.view.record.run_id.as_str(), true)
        .unwrap();
    answers.push(post(port, "/openai/chat", &keyed));
    std::thread::sleep(Duration::from_millis(600));

    for answer in &answers {
        assert!(
            !answer.body.contains(&run_key),
            "a response carries the Run Key"
        );
        assert!(
            !answer.body.contains(PROVIDER_KEY),
            "a response carries the provider key"
        );
        for (_, value) in &answer.headers {
            assert!(!value.contains(&run_key) && !value.contains(PROVIDER_KEY));
        }
    }
    // The upstream never saw the Run Key, in any header or path.
    for seen in upstream.served() {
        assert!(
            !seen.request_line.contains(&run_key),
            "{}",
            seen.request_line
        );
        for (name, value) in &seen.headers {
            assert!(
                !value.contains(&run_key),
                "the Run Key reached the upstream in {name}"
            );
        }
    }
    // No event carries either.
    while let Ok(event) = events.try_recv() {
        let text = format!("{event:?}");
        assert!(
            !text.contains(&run_key) && !text.contains(PROVIDER_KEY),
            "{text}"
        );
    }
    // Nothing under the state directory carries either, before and after
    // the engine (and its WAL) closes.
    for needle in [PROVIDER_KEY, run_key.as_str()] {
        assert_eq!(
            files_carrying(state.path(), needle),
            Vec::<std::path::PathBuf>::new()
        );
    }
    drop(engine);
    for needle in [PROVIDER_KEY, run_key.as_str()] {
        assert_eq!(
            files_carrying(state.path(), needle),
            Vec::<std::path::PathBuf>::new()
        );
    }
}

#[test]
fn a_lowered_ceiling_applies_on_the_next_proxied_call_and_wire_runs_price_by_model() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    let facade = engine.blocking();
    // The default Policy prices the reported model: $1 in, $2 out per 1M.
    let mut rates = std::collections::BTreeMap::new();
    rates.insert(
        "gpt-demo".to_string(),
        Some(hekma_engine::Rate::new(1_000_000, 2_000_000)),
    );
    facade
        .change_policy(
            PolicyChange {
                name: "default".to_string(),
                rates,
                ..PolicyChange::default()
            },
            false,
        )
        .unwrap();
    facade
        .create_policy(
            PolicyChange {
                name: "core-cap".to_string(),
                selector: Some("team=core".parse().unwrap()),
                set: PolicyLimits {
                    tokens_per_run: Some(1_000_000),
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
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    let run = wait_for_usage(&engine, ("binding", "openai"), 10);
    // 10 in × $1/1M + 5 out × $2/1M = 20 micro-dollars, priced by model.
    assert_eq!(
        run.usage.cost_micros,
        hekma_engine::Micros(20),
        "{:?}",
        run.usage
    );
    // A high ceiling: calls pass.
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    wait_for_usage(&engine, ("binding", "openai"), 20);

    // Lower the ceiling below the Run's 30 tokens: the very next call's
    // close breaches it, and the call after that is fenced.
    facade
        .change_policy(
            PolicyChange {
                name: "core-cap".to_string(),
                set: PolicyLimits {
                    tokens_per_run: Some(25),
                    ..PolicyLimits::default()
                },
                ..PolicyChange::default()
            },
            false,
        )
        .unwrap();
    // The first call after the change is evaluated against the lowered
    // ceiling at its close, which latches `block`; calls made before that
    // batch commits may pass (the documented overshoot, at most one batch
    // window), then every call is fenced.
    let lowered_at = Instant::now();
    let deadline = lowered_at + Duration::from_secs(15);
    loop {
        let answer = post(port, "/openai/chat", &[]);
        if answer.status == 402 {
            break;
        }
        assert_eq!(answer.status, 200, "{}", answer.body);
        assert!(
            Instant::now() < deadline,
            "the lowered ceiling never fenced"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        lowered_at.elapsed() < Duration::from_secs(2),
        "the fence took {:?}",
        lowered_at.elapsed()
    );
    assert_eq!(post(port, "/openai/chat", &[]).status, 402);
}

#[test]
fn concurrent_first_calls_for_one_identity_converge_on_one_run() {
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
    let barrier = Arc::new(std::sync::Barrier::new(16));
    let calls: Vec<_> = (0..16)
        .map(|n| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                // Half create the binding's Run, half one split Run.
                let headers: &[(&str, &str)] = if n % 2 == 0 {
                    &[]
                } else {
                    &[("x-hekma-run", "team-a")]
                };
                post(port, "/openai/chat", headers).status
            })
        })
        .collect();
    for call in calls {
        assert_eq!(call.join().unwrap(), 200);
    }
    wait_for_usage(&engine, ("binding", "openai"), 80);
    wait_for_usage(&engine, ("binding:openai", "team-a"), 80);
    let all = runs(&engine);
    let count = |ns: &str, id: &str| {
        all.iter()
            .filter(|run| {
                run.identity
                    .as_ref()
                    .is_some_and(|(n, i)| n == ns && i == id)
            })
            .count()
    };
    assert_eq!(count("binding", "openai"), 1);
    assert_eq!(count("binding:openai", "team-a"), 1);
}

#[test]
fn a_billed_response_over_the_byte_budget_is_recorded_and_relayed_not_refused() {
    // Review round 1, finding 1: the provider billed the call once it
    // answered, so a response the budget cannot hold is still metered and
    // relayed (a 503 would drop the usage and invite a second, billed retry).
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start_with(Reply::Padded(8 * 1024));
    let captured: Arc<Mutex<Vec<u8>>> = Arc::default();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_proxy: MeteringProxyOptions {
                max_buffered_bytes: 1024,
                ..MeteringProxyOptions::default()
            },
            diagnostics: Some(Arc::new(Mutex::new(Box::new(Capture(Arc::clone(
                &captured,
            )))))),
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let port = engine.metering_proxy().unwrap().port;
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    for _ in 0..2 {
        let answer = post(port, "/openai/chat", &[]);
        assert_eq!(answer.status, 200, "{}", answer.body);
        assert!(answer.body.contains("prompt_tokens"), "relayed whole");
    }
    let run = wait_for_usage(&engine, ("binding", "openai"), 20);
    assert_eq!(run.usage.output_tokens, 10);
    let text = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert_eq!(
        text.matches("buffered past the buffered-byte budget")
            .count(),
        1,
        "the overshoot is said once a minute: {text}"
    );
}

#[test]
fn a_streamed_request_body_over_the_budget_is_refused_before_it_is_buffered() {
    // Review round 1, finding 2: the budget is taken as the body arrives.
    // A chunked body larger than the whole budget is refused at its first
    // chunk, without waiting for (or holding) the rest, and nothing is dialled.
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_proxy: MeteringProxyOptions {
                max_buffered_bytes: 1024,
                ..MeteringProxyOptions::default()
            },
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let port = engine.metering_proxy().unwrap().port;
    bind(
        &engine,
        "openai",
        "/openai",
        &upstream.base(),
        &[("team", "core")],
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let chunk = "x".repeat(4096);
    // The body never ends: no terminating zero-length chunk is sent.
    stream
        .write_all(
            format!(
                "POST /openai/chat HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: application/json\r\n\
                 transfer-encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n",
                chunk.len()
            )
            .as_bytes(),
        )
        .unwrap();
    let mut head = [0u8; 12];
    stream
        .read_exact(&mut head)
        .expect("answered before the body ends");
    assert_eq!(&head, b"HTTP/1.1 503");
    assert!(upstream.served().is_empty());
}

/// Poll the calls listing until `done` holds for it (records commit within
/// the writer's batch window; the stream is unacknowledged).
fn wait_for_calls(
    engine: &Engine,
    done: impl Fn(&[hekma_engine::CallRecord]) -> bool,
) -> Vec<hekma_engine::CallRecord> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let items = engine
            .blocking()
            .list_calls(CallsQuery::all())
            .unwrap()
            .items;
        if done(&items) {
            return items;
        }
        assert!(
            Instant::now() < deadline,
            "the calls never landed: {items:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn recorded_calls_read_back_with_their_shapes_and_nothing_secret_anywhere() {
    // The per-call read surface over records the real proxy wrote: a clean
    // pass, an upstream transport failure, and a `block` refusal; read by
    // the proxy-owning engine and by a read-only engine on the same state
    // directory; and no provider key, prompt or upstream address in the
    // listing, the by-id record or the calls segment's bytes.
    const KEY: &str = "sk-SENTINEL-provider-4b1d";
    const PROMPT: &str = "SENTINEL-prompt-9c2e";
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
    // A binding whose upstream refuses connections: a transport failure.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_base = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    bind(&engine, "dead", "/dead", &dead_base, &[("team", "dead")]);
    let body =
        format!(r#"{{"model":"gpt-demo","messages":[{{"role":"user","content":"{PROMPT}"}}]}}"#);
    let auth = format!("Bearer {KEY}");
    let ok = send(
        port,
        "POST /openai/chat HTTP/1.1",
        &[("authorization", &auth)],
        Some(&body),
    );
    assert_eq!(ok.status, 200, "{}", ok.body);
    let failed = send(
        port,
        "POST /dead/chat HTTP/1.1",
        &[("authorization", &auth)],
        Some(&body),
    );
    assert_eq!(failed.status, 502, "{}", failed.body);
    // A block Policy on the binding's labels, then one call over it.
    wait_for_usage(&engine, ("binding", "openai"), 10);
    engine
        .blocking()
        .create_policy(
            PolicyChange {
                name: "cap".to_string(),
                selector: Some("team=core".parse().unwrap()),
                set: PolicyLimits {
                    tokens_per_run: Some(1),
                    breach_action: Some(BreachAction::Block),
                    ..PolicyLimits::default()
                },
                ..PolicyChange::default()
            },
            false,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let refused = loop {
        let answer = send(
            port,
            "POST /openai/chat HTTP/1.1",
            &[("authorization", &auth)],
            Some(&body),
        );
        if answer.status == 402 {
            break answer;
        }
        assert!(Instant::now() < deadline, "the block never fenced");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(refused.error_type(), "hekma_budget_block");

    let items = wait_for_calls(&engine, |items| {
        items
            .iter()
            .any(|call| call.disposition.as_str() == "block")
    });
    let completed = items
        .iter()
        .find(|call| call.outcome.as_str() == "completed")
        .expect("the clean pass");
    assert_eq!(completed.status, Some(200));
    assert_eq!(completed.input_tokens, Some(10));
    assert_eq!(completed.binding.as_deref(), Some("openai"));
    assert!(completed.token_id.is_none(), "no Run Key was presented");
    assert!(completed.hold_end.is_none());
    let transport = items
        .iter()
        .find(|call| call.binding.as_deref() == Some("dead"))
        .expect("the transport failure");
    assert_eq!(transport.outcome.as_str(), "upstream-error");
    assert!(transport.input_tokens.is_none(), "unknown, never 0");
    let blocked = items
        .iter()
        .find(|call| call.disposition.as_str() == "block")
        .expect("the block refusal");
    assert_eq!(blocked.status, Some(402));
    // Newest first, and the problems view drops only the clean pass.
    assert!(items.windows(2).all(|w| w[0].call_id > w[1].call_id));
    let problems = engine
        .blocking()
        .list_calls(CallsQuery {
            view: CallView::Problems,
            ..CallsQuery::all()
        })
        .unwrap();
    assert!(problems
        .items
        .iter()
        .all(|call| call.call_id != completed.call_id));
    assert!(problems.items.len() >= 2);

    // A read-only engine reads the same records while the host writes.
    let reader = Engine::open_read_only(Some(state.path().to_path_buf())).unwrap();
    let read = reader.blocking().list_calls(CallsQuery::all()).unwrap();
    assert_eq!(read.items.len(), items.len());
    let by_id = reader.blocking().call(&blocked.call_id).unwrap();
    assert_eq!(&by_id, blocked);

    // Nothing secret in any surface, nor in the segment's bytes.
    let upstream_addr = upstream.base().trim_start_matches("http://").to_string();
    let document = serde_json::to_string(
        &read
            .clone()
            .into_document(CallsQuery::all().echoed_filters()),
    )
    .unwrap();
    for needle in [KEY, PROMPT, upstream_addr.as_str()] {
        assert!(!document.contains(needle), "the listing carries {needle}");
        assert!(
            !serde_json::to_string(&by_id).unwrap().contains(needle),
            "the record carries {needle}"
        );
    }
    drop(reader);
    drop(engine);
    for entry in std::fs::read_dir(state.path().join("calls")).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        for needle in [KEY, PROMPT, upstream_addr.as_str()] {
            assert!(
                !bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "{} carries {needle}",
                path.display()
            );
        }
    }
}

/// Story 23-2: the `x-hekma-announce` header channel end to end — a first
/// announcing call is served and its response carries the Run Key and
/// refresh credential ONCE; the Run is `binding:<name>`/id at `announced`;
/// the minted key identifies the Run, is never custody-eligible, never
/// reaches the upstream or the state directory, and a revoked key wins
/// over an announce of the same identity, which never re-mints.
#[test]
fn a_header_announce_returns_its_pair_once_and_the_key_names_the_run() {
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
    let announce = [
        ("x-hekma-announce", "job-7"),
        ("x-hekma-labels", "step=plan"),
    ];
    let first = post(port, "/openai/chat", &announce);
    assert_eq!(first.status, 200, "{}", first.body);
    let key = first.header("x-hekma-run-key").unwrap().to_string();
    let refresh = first.header("x-hekma-refresh").unwrap().to_string();
    assert!(key.starts_with("hk_run_") && refresh.starts_with("hk_ref_"));
    assert_eq!(first.header("cache-control"), Some("no-store"));
    // A re-announce is served with no credentials.
    let again = post(port, "/openai/chat", &announce);
    assert_eq!(again.status, 200);
    assert!(again.header("x-hekma-run-key").is_none());
    assert!(again.header("x-hekma-refresh").is_none());
    let run = wait_for_usage(&engine, ("binding:openai", "job-7"), 20);
    assert_eq!(run.record.control_depth.as_str(), "announced");
    assert!(run.record.wire_path);
    assert!(run
        .labels
        .iter()
        .any(|label| label.key == "team" && label.value == "core"));
    assert!(run
        .labels
        .iter()
        .any(|label| label.key == "step" && label.value == "plan"));
    let shown = engine.blocking().run(run.record.run_id.as_str()).unwrap();
    assert!(
        shown.lease_expires_at.is_some(),
        "an announced Run holds a Lease"
    );
    // The minted key names the Run.
    let keyed = post(port, "/openai/chat", &[("x-hekma-run-key", &key)]);
    assert_eq!(keyed.status, 200, "{}", keyed.body);
    wait_for_usage(&engine, ("binding:openai", "job-7"), 30);
    // A header-announced key never draws custody: in the API-key slot it
    // is `custody_unavailable`, and nothing is forwarded.
    let served = upstream.served().len();
    let slot = post(
        port,
        "/openai/chat",
        &[("authorization", &format!("Bearer {key}"))],
    );
    assert_eq!(slot.status, 400, "{}", slot.body);
    assert_eq!(slot.error_type(), "hekma_custody_unavailable");
    assert_eq!(upstream.served().len(), served);
    // Neither the announce nor a credential reached the upstream.
    for seen in upstream.served() {
        assert!(seen.header("x-hekma-announce").is_none());
        assert!(seen.header("x-hekma-run-key").is_none());
        assert!(!format!("{seen:?}").contains(&refresh));
    }
    // A revoked key presented with an announce of the same identity: the
    // 401 wins, and a later announce renews without a new key.
    engine
        .blocking()
        .revoke_run_keys(run.record.run_id.as_str(), true)
        .unwrap();
    let revoked = post(
        port,
        "/openai/chat",
        &[("x-hekma-run-key", &key), ("x-hekma-announce", "job-7")],
    );
    assert_eq!(revoked.status, 401, "{}", revoked.body);
    assert_eq!(revoked.error_type(), "hekma_credential_revoked");
    let renewed = post(port, "/openai/chat", &announce);
    assert_eq!(renewed.status, 200);
    assert!(
        renewed.header("x-hekma-run-key").is_none(),
        "announcing never re-mints a revoked key"
    );
    // Nothing in the state directory carries either credential.
    drop(engine);
    for secret in [&key, &refresh] {
        assert!(files_carrying(state.path(), secret).is_empty());
    }
}

/// Story 23-2: an announce beneath an unlabelled binding claims its own
/// Run (it is never ignored like a hint there).
#[test]
fn an_announce_beneath_an_unlabelled_binding_claims_its_own_run() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port) = open(&state, true);
    bind(&engine, "plain", "/plain", &upstream.base(), &[]);
    let first = post(port, "/plain/chat", &[("x-hekma-announce", "solo")]);
    assert_eq!(first.status, 200, "{}", first.body);
    assert!(first.header("x-hekma-run-key").is_some());
    let run = wait_for_usage(&engine, ("binding:plain", "solo"), 10);
    assert_eq!(run.record.control_depth.as_str(), "announced");
    assert!(
        !runs(&engine)
            .iter()
            .any(|r| r.identity.as_ref().is_some_and(|(ns, _)| ns == "proxy")),
        "the unattributed Run was not used"
    );
}

/// Story 23-2: the fence wins over everything, and a fenced announcing
/// call still carries its credentials on the 402 — the child Run beneath a
/// blocked parent's key exists and is listed; its calls are refused by its
/// ancestor's ceiling.
#[test]
fn a_fenced_announcing_call_answers_402_with_its_credentials() {
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
    // The parent: announced, its key spends past its 10-token ceiling.
    let first = post(port, "/openai/chat", &[("x-hekma-announce", "parent")]);
    assert_eq!(first.status, 200, "{}", first.body);
    let parent_key = first.header("x-hekma-run-key").unwrap().to_string();
    wait_for_usage(&engine, ("binding:openai", "parent"), 10);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let answer = post(port, "/openai/chat", &[("x-hekma-run-key", &parent_key)]);
        if answer.status == 402 {
            break;
        }
        assert_eq!(answer.status, 200, "{}", answer.body);
        assert!(Instant::now() < deadline, "block never fenced the parent");
        std::thread::sleep(Duration::from_millis(50));
    }
    let served = upstream.served().len();
    // A child announced beneath the fenced parent's key: 402, nothing
    // forwarded, and the child's pair rides the refusal.
    let fenced = post(
        port,
        "/openai/chat",
        &[
            ("x-hekma-run-key", &parent_key),
            ("x-hekma-announce", "kid"),
        ],
    );
    assert_eq!(fenced.status, 402, "{}", fenced.body);
    assert_eq!(fenced.error_type(), "hekma_budget_block");
    assert!(fenced
        .header("x-hekma-run-key")
        .is_some_and(|key| key.starts_with("hk_run_") && key != parent_key));
    assert!(fenced.header("x-hekma-refresh").is_some());
    assert_eq!(upstream.served().len(), served);
    assert!(runs(&engine).iter().any(|run| run
        .identity
        .as_ref()
        .is_some_and(|(ns, id)| ns.starts_with("run:") && id == "kid")));
}

/// Poll until the Run `identity` has `status`, returning it.
fn wait_for_status(engine: &Engine, identity: (&str, &str), status: &str) -> RunSummary {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let found = runs(engine).into_iter().find(|run| {
            run.identity
                .as_ref()
                .is_some_and(|(ns, id)| ns == identity.0 && id == identity.1)
                && run.record.status.as_str() == status
        });
        if let Some(run) = found {
            return run;
        }
        assert!(
            Instant::now() < deadline,
            "no Run {identity:?} became {status}: {:?}",
            runs(engine)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Story 23-2: the sweeper over a real proxy, with second-scale windows —
/// an announced Run whose Lease lapses goes `silent` (served all the
/// while), a call brings it back to `active`, and past the silent window
/// it ends (`silent-window`) with its key revoked at once; the identity's
/// next announce creates a new Run.
#[test]
fn the_sweeper_silences_renews_and_ends_an_announced_run() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_proxy: MeteringProxyOptions {
                leases: hekma_engine::LeaseOptions {
                    lease_secs: 1,
                    wire_inactivity_secs: 1,
                    silent_window_secs: 3,
                    sweep_interval_secs: 1,
                },
                ..MeteringProxyOptions::default()
            },
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let port = engine.metering_proxy().unwrap().port;
    bind(&engine, "openai", "/openai", &upstream.base(), &[]);
    // The unattributed Run (an unlabelled binding's plain call) holds no
    // Lease: it ends by inactivity too, and is recreated on the next call.
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    let first = post(port, "/openai/chat", &[("x-hekma-announce", "job")]);
    assert_eq!(first.status, 200, "{}", first.body);
    let key = first.header("x-hekma-run-key").unwrap().to_string();
    let identity = ("binding:openai", "job");
    let silent = wait_for_status(&engine, identity, "silent");
    // Silence never stops anything: the next call is served and lifts it.
    let call = post(port, "/openai/chat", &[("x-hekma-run-key", &key)]);
    assert_eq!(call.status, 200, "{}", call.body);
    let active = wait_for_status(&engine, identity, "active");
    assert_eq!(active.record.run_id, silent.record.run_id);
    // Left alone: silent again, then ended past the silent window.
    let ended = wait_for_status(&engine, identity, "ended");
    assert_eq!(ended.record.run_id, silent.record.run_id);
    assert_eq!(ended.record.end_cause.as_deref(), Some("silent-window"));
    let refused = post(port, "/openai/chat", &[("x-hekma-run-key", &key)]);
    assert_eq!(refused.status, 401, "{}", refused.body);
    assert_eq!(refused.error_type(), "hekma_credential_revoked");
    // The identity's next announce is a new Run, with a new pair.
    let again = post(port, "/openai/chat", &[("x-hekma-announce", "job")]);
    assert_eq!(again.status, 200);
    assert!(again
        .header("x-hekma-run-key")
        .is_some_and(|fresh| fresh != key));
    let reborn = wait_for_status(&engine, identity, "active");
    assert_ne!(reborn.record.run_id, ended.record.run_id);
    let unattributed = ("proxy", "unattributed");
    let gone = wait_for_status(&engine, unattributed, "ended");
    assert_eq!(gone.record.end_cause.as_deref(), Some("silent-window"));
    assert_eq!(post(port, "/openai/chat", &[]).status, 200);
    let fresh = wait_for_status(&engine, unattributed, "active");
    assert_ne!(fresh.record.run_id, gone.record.run_id);
}
