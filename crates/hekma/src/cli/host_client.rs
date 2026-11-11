//! The CLI's client for the local Service Host (story 21-4, AD-20's one
//! data-path owner): every `hekma agent` WRITE is one Control API call to the
//! host that owns the state root.
//!
//! Finding the owner: read `<state-root>/control.json` and call
//! `GET /v0/version` with its token (a 1 s connect bound). On any failure —
//! no file, refused, `401`, a different `api` id — start one:
//! `<this exe> serve --autostart --state-dir <root>` DETACHED (its own process
//! group, stdin null, output appended to `<state-root>/serve.log`), then poll
//! `control.json` every 50 ms for up to 10 s. Two commands that race both
//! spawn; one host wins `host.lock`, the other exits into `serve.log`, and
//! both commands connect to the winner. A host from another hekma build gets
//! one stderr note and is used.
//!
//! Errors keep the CLI's messages and exit codes: the host answers an engine
//! failure with the engine's TYPED error on the wire (`error.engine_error`),
//! decoded here back into the engine's own value
//! ([`hekma_host::wire::decode`]) so the caller renders it through its usual
//! mapper. Anything else — a host that cannot be found or started, a
//! transport failure after it was found, an envelope with no engine error —
//! is [`ServiceHostUnavailable`] / [`ServiceHostRefused`] (exit 1).
//!
//! The engine notes a write raised in the host (its `x-hekma-engine-notes`
//! response header) are written to this command's stderr verbatim — the
//! lines the in-process engine used to print.
//!
//! Synchronous by design (the CLI carries no async tokens): `ureq` over plain
//! HTTP to 127.0.0.1, no proxy, no connection pooling.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hekma_engine::{ConfigError, EngineError, RegistryError};
use serde_json::Value;

use crate::error::{ServiceHostRefused, ServiceHostUnavailable};
use crate::ui;

/// The connect bound for every call (a loopback host answers in microseconds;
/// a dead one refuses at once — this only bounds a wedged accept queue).
const CONNECT_BOUND: Duration = Duration::from_secs(1);

/// The bound on the discovery probe (`GET /v0/version`).
const PROBE_BOUND: Duration = Duration::from_secs(2);

/// How long a command waits for the host it started to publish
/// `control.json`.
const AUTOSTART_WAIT: Duration = Duration::from_secs(10);

/// How often it re-reads `control.json` while waiting.
const AUTOSTART_POLL: Duration = Duration::from_millis(50);

/// The Control API's self-description id (`GET /v0/version`'s `api`).
const API_ID: &str = "hekma-control-api";

/// A failed call: the engine's own error (decoded from the wire), or a
/// host-level failure carrying its surfaced message.
#[derive(Debug)]
pub(crate) enum CallError {
    /// An [`EngineError`] the host's engine raised.
    Engine(EngineError),
    /// A [`RegistryError`] the host's engine raised.
    Registry(RegistryError),
    /// A [`ConfigError`] the host's engine raised.
    Config(ConfigError),
    /// The host refused the request with an envelope that carries no engine
    /// error (a body rejection, an unknown route — or an engine error this
    /// build cannot decode): its message.
    Refused(String),
    /// The host refused a Run route with a stable `error.code` (story 21-2:
    /// `run-not-found`, `parent-ended`, …) — the code and the message, for
    /// the caller to map onto its exit classes.
    Coded {
        /// The envelope's `error.code`.
        code: String,
        /// The envelope's `error.message`.
        message: String,
    },
    /// The host could not be reached, started, or stopped answering.
    Unavailable(String),
    /// The connection was refused before anything was sent (the host just
    /// exited): safe to find or start the owner again and retry once.
    NotSent(String),
}

impl CallError {
    /// Map a host-level failure into its CLI diagnostic; the engine
    /// families are the caller's (`map_engine_error` / `map_error` /
    /// `map_config_error`).
    pub(crate) fn host_diagnostic(message: String, refused: bool) -> Box<dyn std::error::Error> {
        if refused {
            ServiceHostRefused { message }.into()
        } else {
            ServiceHostUnavailable { message }.into()
        }
    }
}

/// A connected client for one state root's Service Host.
pub(crate) struct HostClient {
    base: String,
    token: String,
    agent: ureq::Agent,
    /// The host's process id (from `control.json`).
    pub(crate) pid: u32,
    /// The hekma build that wrote `control.json`.
    hekma_version: String,
    state_root: PathBuf,
}

/// The `ureq` agent every call uses: status codes are answers (never
/// transport errors), no proxy (the host is loopback), no pooled
/// connections, a bounded connect, and no overall timeout — a `stop` with a
/// long window legitimately holds its request for the whole ladder.
fn agent(global: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(None)
        .max_idle_connections(0)
        .timeout_connect(Some(CONNECT_BOUND))
        .timeout_global(global)
        .build()
        .into()
}

/// Whether this build may auto-start a host (`<current exe> serve
/// --autostart`). Any `hekma` binary may, whatever its file is named (a
/// packager's suffix, a renamed copy); a unit-test build of this crate never
/// does — its `current_exe` is the test harness, which has no `serve`.
fn may_autostart() -> bool {
    !cfg!(test)
}

impl HostClient {
    /// Find the state root's Service Host, or start one and wait for it.
    pub(crate) fn connect(state_root: &Path) -> Result<Self, CallError> {
        if let Some(client) = Self::discover(state_root) {
            client.note_version_skew();
            return Ok(client);
        }
        let exe = std::env::current_exe().map_err(|err| {
            CallError::Unavailable(format!(
                "no Service Host serves the state root {} and this command could not locate \
                 its own binary to start one: {err}",
                state_root.display()
            ))
        })?;
        if !may_autostart() {
            return Err(CallError::Unavailable(format!(
                "no Service Host serves the state root {}; start one with: hekma serve",
                state_root.display()
            )));
        }
        std::fs::create_dir_all(state_root).map_err(|source| {
            CallError::Registry(RegistryError::Io {
                name: "<state-dir>".to_string(),
                path: state_root.to_string_lossy().into_owned(),
                source,
            })
        })?;
        let log = hekma_host::serve_log_file(state_root);
        let args = [
            "serve".into(),
            "--autostart".into(),
            "--state-dir".into(),
            state_root.as_os_str().to_os_string(),
        ];
        hekma_host::spawn_detached(&exe, &args, &log).map_err(|err| {
            CallError::Unavailable(format!(
                "could not start a local Service Host for the state root {}: {err}",
                state_root.display()
            ))
        })?;
        let deadline = Instant::now() + AUTOSTART_WAIT;
        loop {
            if let Some(client) = Self::discover(state_root) {
                client.note_version_skew();
                return Ok(client);
            }
            if Instant::now() >= deadline {
                return Err(CallError::Unavailable(format!(
                    "the local Service Host for the state root {} did not come up within {}s; \
                     see {}",
                    state_root.display(),
                    AUTOSTART_WAIT.as_secs(),
                    log.display()
                )));
            }
            std::thread::sleep(AUTOSTART_POLL);
        }
    }

    /// The live Service Host serving the state root right now — found
    /// through `control.json` and answering, NEVER started (the read-only
    /// commands' question).
    pub(crate) fn find(state_root: &Path) -> Option<Self> {
        Self::discover(state_root)
    }

    /// The live host's build when it differs from this command's (the skew
    /// note's subject).
    pub(crate) fn skewed_version(&self) -> Option<&str> {
        (self.hekma_version != env!("CARGO_PKG_VERSION")).then_some(self.hekma_version.as_str())
    }

    /// One stderr note when the host is from another hekma build (it is used
    /// as-is).
    fn note_version_skew(&self) {
        if self.hekma_version != env!("CARGO_PKG_VERSION") {
            ui::note(format!(
                "the local Service Host (pid {pid}) is from hekma {}, this command is hekma {}; \
                 it is used as-is — restart it to match: stop its agents, then end pid {pid}",
                self.hekma_version,
                env!("CARGO_PKG_VERSION"),
                pid = self.pid,
            ));
        }
    }

    /// Read `control.json` and probe the host it names. `None` on any
    /// failure (no file, refused, 401, another API).
    fn discover(state_root: &Path) -> Option<Self> {
        let discovery = hekma_host::read_discovery(state_root).ok()?;
        let base = format!("http://{}", discovery.addr);
        let probe = agent(Some(PROBE_BOUND));
        let answered = probe
            .get(format!("{base}/v0/version"))
            .header("Authorization", format!("Bearer {}", discovery.token))
            .call();
        match answered {
            Ok(response) => {
                if response.status().as_u16() != 200 {
                    return None;
                }
                let body: Value = response.into_body().read_json().ok()?;
                if body.get("api").and_then(Value::as_str) != Some(API_ID) {
                    return None;
                }
            }
            // A connect that never completed — refused, or timed out (Windows
            // retries a SYN to a closed loopback port instead of refusing at
            // once): nothing listens there, the file is stale — absent.
            Err(err) if never_sent(&err) => return None,
            // The host ACCEPTED the connection but has not answered within
            // the bound: it is alive and busy (its runtime serves a long
            // request) — never mistaken for an absent host, which would
            // start a second one that can only lose `host.lock`.
            Err(ureq::Error::Timeout(_)) => {}
            // Garbled or otherwise broken: absent.
            Err(_) => return None,
        }
        Some(HostClient {
            base,
            token: discovery.token,
            agent: agent(None),
            pid: discovery.pid,
            hekma_version: discovery.hekma_version,
            state_root: state_root.to_path_buf(),
        })
    }

    /// One Control API call. `Ok(Some(json))` for a 2xx with a body,
    /// `Ok(None)` for a 2xx without one; a failure decodes as [`CallError`].
    pub(crate) fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<Value>, CallError> {
        let url = format!("{}{path}", self.base);
        let auth = format!("Bearer {}", self.token);
        let sent = match (method, body) {
            ("GET", _) => self.agent.get(&url).header("Authorization", &auth).call(),
            ("DELETE", _) => self
                .agent
                .delete(&url)
                .header("Authorization", &auth)
                .call(),
            ("POST", Some(json)) => self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .send_json(json),
            ("PATCH", Some(json)) => self
                .agent
                .patch(&url)
                .header("Authorization", &auth)
                .send_json(json),
            ("POST", None) => self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .send_empty(),
            (_, Some(json)) => self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .send_json(json),
            (_, None) => self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .send_empty(),
        };
        let response = sent.map_err(|err| self.transport_error(err))?;
        // The engine's own notes raised while the request ran (a start's
        // delivery note, an adoption or resume note) — written to THIS
        // command's stderr verbatim, before its own output, as the
        // in-process engine wrote them.
        if let Some(notes) = response
            .headers()
            .get(hekma_host::engine_notes::ENGINE_NOTES_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            use std::io::Write;
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(&hekma_host::engine_notes::decode(notes));
            let _ = stderr.flush();
        }
        let status = response.status().as_u16();
        let text = response
            .into_body()
            .read_to_string()
            .map_err(|err| self.transport_error(err))?;
        let json = if text.trim().is_empty() {
            None
        } else {
            Some(serde_json::from_str::<Value>(&text).map_err(|err| {
                CallError::Unavailable(format!(
                    "the local Service Host (pid {}) answered {status} with a body that is not \
                     JSON: {err}",
                    self.pid
                ))
            })?)
        };
        if (200..300).contains(&status) {
            return Ok(json);
        }
        Err(decode_failure(status, json))
    }

    fn transport_error(&self, err: ureq::Error) -> CallError {
        let not_sent = never_sent(&err);
        let message = format!(
            "the local Service Host (pid {}) stopped answering: {err}; see {}",
            self.pid,
            hekma_host::serve_log_file(&self.state_root).display()
        );
        if not_sent {
            CallError::NotSent(message)
        } else {
            CallError::Unavailable(message)
        }
    }
}

/// Whether a transport failure happened before the request left this
/// process: the connection was refused, failed, or did not complete within
/// the connect bound (name resolution included). Such a request was never
/// sent, so finding or starting the owner again and retrying is safe. A
/// timeout AFTER the connect (sending, or waiting for the answer) is not:
/// the host may have acted on it.
fn never_sent(err: &ureq::Error) -> bool {
    match err {
        ureq::Error::ConnectionFailed => true,
        ureq::Error::Timeout(ureq::Timeout::Connect | ureq::Timeout::Resolve) => true,
        ureq::Error::Io(io) => io.kind() == std::io::ErrorKind::ConnectionRefused,
        _ => false,
    }
}

/// Decode a non-2xx answer: the engine's typed error when the envelope
/// carries one this build understands, else the envelope's own message.
fn decode_failure(status: u16, json: Option<Value>) -> CallError {
    let Some(json) = json else {
        return CallError::Refused(format!("the local Service Host answered {status}"));
    };
    let error = &json["error"];
    if let Some(decoded) = error.get("engine_error").and_then(hekma_host::wire::decode) {
        return match decoded {
            hekma_host::wire::WireError::Engine(err) => CallError::Engine(err),
            hekma_host::wire::WireError::Registry(err) => CallError::Registry(err),
            hekma_host::wire::WireError::Config(err) => CallError::Config(err),
        };
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("the local Service Host answered {status}"));
    if let Some(code) = error.get("code").and_then(Value::as_str) {
        return CallError::Coded {
            code: code.to_string(),
            message,
        };
    }
    CallError::Refused(message)
}

/// Percent-encode one URL path segment (an instance name the CLI passes
/// through verbatim — the engine validates it).
pub(crate) fn segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_test_build_never_may_autostart() {
        // The real binary links this crate without `cfg(test)`; this test
        // build is the harness, which must never spawn itself as a host.
        assert!(!may_autostart());
    }

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(segment("worker-1_a"), "worker-1_a");
        assert_eq!(segment("Bad Name"), "Bad%20Name");
        assert_eq!(segment("a/b"), "a%2Fb");
        assert_eq!(segment(""), "");
    }

    #[test]
    fn a_failure_decodes_to_the_engine_error_or_the_envelope_message() {
        let engine = serde_json::json!({
            "schema_version": 1,
            "error": {
                "kind": "not_found",
                "message": "no Agent Instance named 'w' is registered",
                "engine_error": hekma_host::wire::encode_engine(&EngineError::NotFound {
                    name: "w".into()
                }),
            }
        });
        assert!(matches!(
            decode_failure(404, Some(engine)),
            CallError::Engine(EngineError::NotFound { .. })
        ));
        let registry = serde_json::json!({"error": {"engine_error":
            hekma_host::wire::encode_registry(&RegistryError::DuplicateName { name: "w".into() })}});
        assert!(matches!(
            decode_failure(409, Some(registry)),
            CallError::Registry(RegistryError::DuplicateName { .. })
        ));
        let config = serde_json::json!({"error": {"engine_error":
            hekma_host::wire::encode_config(&ConfigError::NotFound { name: "w".into() })}});
        assert!(matches!(
            decode_failure(404, Some(config)),
            CallError::Config(ConfigError::NotFound { .. })
        ));
        // An engine error from the future falls back to the envelope message.
        let future = serde_json::json!({"error": {"kind": "conflict", "message": "later",
            "engine_error": {"schema_version": 9}}});
        match decode_failure(409, Some(future)) {
            CallError::Refused(message) => assert_eq!(message, "later"),
            other => panic!("{other:?}"),
        }
        match decode_failure(400, Some(serde_json::json!({"error": {}}))) {
            CallError::Refused(message) => assert!(message.contains("400"), "{message}"),
            other => panic!("{other:?}"),
        }
        match decode_failure(502, None) {
            CallError::Refused(message) => assert!(message.contains("502"), "{message}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_unit_test_never_autostarts_a_host() {
        let dir = tempfile::tempdir().unwrap();
        match HostClient::connect(dir.path()) {
            Err(CallError::Unavailable(message)) => {
                assert!(message.contains("hekma serve"), "{message}")
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("no host can serve a fresh temp root"),
        }
        assert!(
            !hekma_host::control_file(dir.path()).exists(),
            "nothing was started"
        );
    }

    /// A scripted loopback "host": answers each request with the
    /// `(status, body)` the script picks from its method and path (`None`:
    /// accept and never answer). Runs until the test process exits.
    fn fake_host(script: fn(&str, &str) -> Option<(u16, &'static str)>) -> std::net::SocketAddr {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    let _ = reader.read_line(&mut request_line);
                    let mut length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; length];
                    let _ = reader.read_exact(&mut body);
                    let mut parts = request_line.split_whitespace();
                    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    match script(method, path) {
                        Some((status, body)) => {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                        }
                        None => std::thread::sleep(Duration::from_secs(30)),
                    }
                });
            }
        });
        addr
    }

    /// Point a fresh state root's `control.json` at `addr`.
    fn discovery_at(addr: std::net::SocketAddr, version: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        hekma_host::write_discovery(
            dir.path(),
            &hekma_host::Discovery {
                schema_version: hekma_host::DISCOVERY_SCHEMA_VERSION,
                pid: 4242,
                addr,
                token: "ab".repeat(32),
                hekma_version: version.to_string(),
                autostarted: true,
            },
        )
        .unwrap();
        dir
    }

    #[test]
    fn discovery_takes_only_a_200_from_the_control_api() {
        let refused = fake_host(|_, _| Some((401, r#"{"error":{}}"#)));
        assert!(HostClient::find(discovery_at(refused, "x").path()).is_none());
        let foreign = fake_host(|_, _| Some((200, r#"{"api":"something-else"}"#)));
        assert!(HostClient::find(discovery_at(foreign, "x").path()).is_none());
        let garbled = fake_host(|_, _| Some((200, "not json")));
        assert!(HostClient::find(discovery_at(garbled, "x").path()).is_none());
        let host = fake_host(|_, _| Some((200, r#"{"api":"hekma-control-api"}"#)));
        let same = discovery_at(host, env!("CARGO_PKG_VERSION"));
        let client = HostClient::find(same.path()).expect("a live host");
        assert_eq!(client.pid, 4242);
        assert_eq!(client.skewed_version(), None);
        let other = discovery_at(host, "0.0.0-older");
        let skewed = HostClient::find(other.path()).expect("a live host");
        assert_eq!(skewed.skewed_version(), Some("0.0.0-older"));
        // The skew note goes to stderr and the host is used as-is.
        skewed.note_version_skew();
    }

    #[test]
    fn a_host_that_accepts_but_has_not_answered_is_alive_not_absent() {
        let busy = fake_host(|_, _| None);
        let root = discovery_at(busy, env!("CARGO_PKG_VERSION"));
        assert!(
            HostClient::find(root.path()).is_some(),
            "a probe timeout never reads as an absent host"
        );
    }

    #[test]
    fn calls_decode_bodies_and_name_transport_failures() {
        let host = fake_host(|method, path| match (method, path) {
            ("GET", "/v0/version") => Some((200, r#"{"api":"hekma-control-api"}"#)),
            ("PUT", "/v0/with-body") => Some((200, r#"{"ok":true}"#)),
            ("PUT", "/v0/empty") => Some((204, "")),
            ("GET", "/v0/garbled") => Some((200, "not json")),
            _ => Some((
                404,
                r#"{"error":{"kind":"not_found","message":"no such route"}}"#,
            )),
        });
        let root = discovery_at(host, env!("CARGO_PKG_VERSION"));
        let client = HostClient::find(root.path()).expect("a live host");
        let body = serde_json::json!({"key": "value"});
        assert_eq!(
            client.call("PUT", "/v0/with-body", Some(&body)).unwrap(),
            Some(serde_json::json!({"ok": true}))
        );
        assert_eq!(client.call("PUT", "/v0/empty", None).unwrap(), None);
        match client.call("GET", "/v0/garbled", None) {
            Err(CallError::Unavailable(message)) => {
                assert!(message.contains("not JSON"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        match client.call("DELETE", "/v0/nowhere", None) {
            Err(CallError::Refused(message)) => assert_eq!(message, "no such route"),
            other => panic!("{other:?}"),
        }
        // The host went away after it was found: nothing was sent.
        let gone = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let stale = HostClient {
            base: format!("http://{gone}"),
            ..client
        };
        match stale.call("POST", "/v0/instances/w/start", None) {
            Err(CallError::NotSent(message)) => {
                assert!(message.contains("serve.log"), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// A loopback "host" that breaks every connection MID-FLIGHT: it accepts,
    /// reads the request line, then DROPS the connection without ever
    /// answering — the request was sent, the answer never came (distinct
    /// from a refused port's pre-send failure and from a stalling host's
    /// timeout). Runs until the test process exits.
    fn breaking_host() -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    let _ = reader.read_line(&mut request_line);
                    // Drop the connection: accepted, read, then broken.
                });
            }
        });
        addr
    }

    /// A loopback "host" that answers the FIRST connection as the Control
    /// API (the discovery probe) and breaks every later one mid-flight —
    /// a host that is found alive and then dies on the first real call.
    fn probe_then_breaking_host() -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            let mut connections = 0usize;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                connections += 1;
                let answer_version = connections == 1;
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    let _ = reader.read_line(&mut request_line);
                    if answer_version {
                        let body = r#"{"api":"hekma-control-api"}"#;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                    }
                    // Later connections: read the request, then drop it.
                });
            }
        });
        addr
    }

    #[test]
    fn a_discovery_connection_that_breaks_after_accept_reads_as_absent() {
        // The probe's connection was ACCEPTED and then broken without an
        // answer: not never-sent (the connect completed), not a timeout
        // (nothing is stalling — a stalling host is alive, not absent). The
        // catch-all reads a garbled/broken probe as an absent host, so the
        // command moves on to starting one instead of trusting a dead file.
        let broken = breaking_host();
        assert!(HostClient::find(discovery_at(broken, env!("CARGO_PKG_VERSION")).path()).is_none());
    }

    #[test]
    fn a_call_that_breaks_after_the_request_was_sent_is_unavailable_not_unsent() {
        // The host answered the probe, then broke the connection AFTER the
        // real call's request left this process: it may have acted on it,
        // so the failure is Unavailable ("stopped answering") — NEVER
        // NotSent, whose retry-once contract would risk a duplicated write.
        let host = probe_then_breaking_host();
        let client = HostClient::find(discovery_at(host, env!("CARGO_PKG_VERSION")).path())
            .expect("a live host");
        match client.call("POST", "/v0/instances/w/start", None) {
            Err(CallError::Unavailable(message)) => {
                assert!(message.contains("stopped answering"), "{message}");
                assert!(message.contains("serve.log"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn only_a_connect_phase_failure_counts_as_never_sent() {
        // Refused, failed or timed out while connecting: never sent (the
        // Windows shape of a closed loopback port is the connect timeout).
        assert!(never_sent(&ureq::Error::ConnectionFailed));
        assert!(never_sent(&ureq::Error::Timeout(ureq::Timeout::Connect)));
        assert!(never_sent(&ureq::Error::Timeout(ureq::Timeout::Resolve)));
        assert!(never_sent(&ureq::Error::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        ))));
        // After the connect the host may have acted: not retryable.
        for timeout in [
            ureq::Timeout::SendRequest,
            ureq::Timeout::SendBody,
            ureq::Timeout::RecvResponse,
            ureq::Timeout::RecvBody,
            ureq::Timeout::Global,
            ureq::Timeout::PerCall,
        ] {
            assert!(!never_sent(&ureq::Error::Timeout(timeout)), "{timeout:?}");
        }
        assert!(!never_sent(&ureq::Error::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionReset
        ))));
    }

    #[test]
    fn host_diagnostics_split_refused_from_unavailable() {
        let refused = CallError::host_diagnostic("r".into(), true);
        assert!(refused.downcast_ref::<ServiceHostRefused>().is_some());
        let unavailable = CallError::host_diagnostic("u".into(), false);
        assert!(unavailable
            .downcast_ref::<ServiceHostUnavailable>()
            .is_some());
    }
}
