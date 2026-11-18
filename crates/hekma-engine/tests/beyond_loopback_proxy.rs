//! The metering proxy bound beyond loopback (story 22-5): TLS plus a
//! credential. The proxy binds `0.0.0.0` with a self-signed rcgen
//! certificate; a loopback peer reaches it on `127.0.0.1` (plain HTTP, as a
//! supervised agent does, or TLS), and a non-loopback peer reaches it on
//! this machine's own routable address (the remote legs are skipped, and
//! say so, on a runner with no routable address).
//!
//! Every OS: no child process; every socket read is bounded.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hekma_engine::{
    BindingCredentialClass, BindingSpec, CallsQuery, Engine, EngineOptions, MeteringProxyOptions,
    ProxyTls,
};
use tempfile::TempDir;

const COMPLETION: &str =
    r#"{"id":"c","model":"m","usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
const CHAT: &str = r#"{"model":"m","messages":[]}"#;

/// A loopback upstream recording each request's header block.
struct Upstream {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    let head = read_head(&mut stream);
                    let length = content_length(&head);
                    let mut body = vec![0u8; length];
                    let _ = stream.read_exact(&mut body);
                    log.lock().unwrap().push(head);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{COMPLETION}",
                        COMPLETION.len()
                    );
                });
            }
        });
        Upstream { addr, seen }
    }

    fn base(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn served(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn read_head(stream: &mut impl Read) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&head).to_string()
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

/// This machine's own routable address, when it has one (the source a
/// connect toward a documentation-range address would use; nothing is
/// sent).
fn routable_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        return None;
    }
    // Some hosts (a firewall, a network extension) drop a connection to
    // their own routable address: the remote legs cannot run there.
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    listener.set_nonblocking(true).ok()?;
    let target = SocketAddr::new(ip, listener.local_addr().ok()?.port());
    let _client = TcpStream::connect_timeout(&target, Duration::from_secs(2)).ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if listener.accept().is_ok() {
            return Some(ip);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// One HTTP/1.1 exchange over `stream`; the status and the whole answer
/// (`None` when the connection closed with nothing).
fn exchange(mut stream: impl Read + Write, request: &str) -> Option<(u16, String)> {
    stream.write_all(request.as_bytes()).ok()?;
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    let answer = String::from_utf8_lossy(&answer).to_string();
    let status = answer.split(' ').nth(1)?.parse().ok()?;
    Some((status, answer))
}

fn request(path: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut out = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nconnection: close\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!(
        "content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    ));
    out
}

struct Tls {
    config: Arc<rustls::ClientConfig>,
}

impl Tls {
    fn connect(
        &self,
        addr: SocketAddr,
    ) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let conn = rustls::ClientConnection::new(Arc::clone(&self.config), name).unwrap();
        rustls::StreamOwned::new(conn, tcp)
    }
}

fn plain(addr: SocketAddr) -> TcpStream {
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    tcp
}

/// A proxy bound on `0.0.0.0` over TLS, a binding `openai` (and `other`)
/// to the upstream fake, and the client TLS config trusting its cert.
fn open(state: &TempDir, upstream: &Upstream) -> (Engine, u16, Tls) {
    open_as(state, upstream, false)
}

/// [`open`], every peer treated as remote when `remote` (the test seam).
fn open_as(state: &TempDir, upstream: &Upstream, remote: bool) -> (Engine, u16, Tls) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certified.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into()),
        )
        .unwrap();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_proxy: MeteringProxyOptions {
                bind: Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                tls: Some(ProxyTls(Arc::new(server))),
                public_host: Some("proxy.example".into()),
                peers_as_remote: remote,
                ..MeteringProxyOptions::default()
            },
            ..EngineOptions::default()
        },
    )
    .expect("open with the proxy beyond loopback");
    let info = engine.metering_proxy().expect("the proxy runs");
    assert!(info.tls && info.beyond_loopback());
    assert_eq!(info.url, format!("http://127.0.0.1:{}", info.port));
    assert_eq!(
        info.public_url.as_deref(),
        Some(format!("https://proxy.example:{}", info.port).as_str())
    );
    for (name, prefix) in [("openai", "/openai"), ("other", "/other")] {
        engine
            .blocking()
            .create_binding(BindingSpec {
                name: name.into(),
                prefix: prefix.into(),
                upstream: upstream.base(),
                labels: vec![("team".into(), "core".into())],
                custodied_key: None,
                identity_headers: None,
            })
            .unwrap();
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certified.cert.der().clone()).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    (
        engine,
        info.port,
        Tls {
            config: Arc::new(client),
        },
    )
}

#[test]
fn a_bind_beyond_loopback_without_tls_does_not_start() {
    let state = TempDir::new().unwrap();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_proxy: MeteringProxyOptions {
                bind: Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                ..MeteringProxyOptions::default()
            },
            ..EngineOptions::default()
        },
    )
    .unwrap();
    assert!(engine.metering_proxy().is_none());
    let error = engine.metering_proxy_error().expect("says why");
    assert!(
        error.contains("without transport encryption") && error.contains("--proxy-tls-cert"),
        "{error}"
    );
}

#[test]
fn loopback_peers_keep_todays_rules_in_plain_http_or_tls() {
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port, tls) = open(&state, &upstream);
    let local = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    // Plain HTTP from loopback (a supervised agent's base URL): served, no
    // credential needed.
    let (status, _) = exchange(plain(local), &request("/openai/chat", &[], CHAT)).unwrap();
    assert_eq!(status, 200);
    // TLS from loopback: served too.
    let (status, _) = exchange(tls.connect(local), &request("/openai/chat", &[], CHAT)).unwrap();
    assert_eq!(status, 200);
    assert_eq!(upstream.served().len(), 2);
    // A proxy-access credential is honoured from loopback too, on its own
    // prefix only.
    let issued = engine
        .blocking()
        .issue_binding_credential("openai", BindingCredentialClass::ProxyAccess)
        .unwrap();
    let bearer = format!("Bearer {}", issued.token.expose_secret());
    let (status, answer) = exchange(
        plain(local),
        &request("/other/chat", &[("authorization", &bearer)], CHAT),
    )
    .unwrap();
    assert_eq!(status, 403, "{answer}");
    assert!(answer.contains("hekma_credential_out_of_scope"), "{answer}");
    assert!(engine.proxy_access_active());
}

#[test]
fn a_remote_peer_must_authenticate_and_a_proxy_access_credential_is_scoped_stripped_and_revocable()
{
    // Every peer is treated as remote (the test seam), so this runs over
    // 127.0.0.1 on any host; the next test proves a real routable peer.
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (engine, port, tls) = open_as(&state, &upstream, true);
    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let remote = SocketAddr::new(ip, port);
    let facade = engine.blocking();
    assert!(!engine.proxy_access_active());

    // Plain HTTP from a remote peer is never served.
    assert!(
        exchange(plain(remote), &request("/openai/chat", &[], CHAT)).is_none(),
        "plain HTTP beyond loopback must not be answered"
    );
    // No credential: 401 before the body is read — a 50 MiB body is
    // announced, never sent, and the answer still comes.
    let big = "POST /openai/chat HTTP/1.1\r\nHost: proxy.example\r\n\
               content-type: application/json\r\ncontent-length: 52428800\r\n\r\n{";
    let (status, answer) = exchange(tls.connect(remote), big).unwrap();
    assert_eq!(status, 401, "{answer}");
    assert!(answer.contains("hekma_authentication_required"), "{answer}");
    assert!(answer.to_ascii_lowercase().contains("connection: close"));
    assert!(upstream.served().is_empty(), "nothing reached the upstream");

    // The binding's proxy-access credential: served, stripped, recorded.
    let issued = facade
        .issue_binding_credential("openai", BindingCredentialClass::ProxyAccess)
        .unwrap();
    assert!(engine.proxy_access_active());
    let token = issued.token.expose_secret().to_string();
    let bearer = format!("Bearer {token}");
    for headers in [
        vec![("authorization", bearer.as_str())],
        vec![("x-api-key", token.as_str())],
    ] {
        let (status, answer) = exchange(
            tls.connect(remote),
            &request("/openai/chat", &headers, CHAT),
        )
        .unwrap();
        assert_eq!(status, 200, "{answer}");
    }
    let served = upstream.served();
    assert_eq!(served.len(), 2);
    for head in &served {
        assert!(
            !head.contains("hk_"),
            "an hk_ token reached the upstream:\n{head}"
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let listed = facade
            .list_calls(CallsQuery {
                token_id: Some(issued.token_id.clone()),
                limit: 10,
                ..CallsQuery::default()
            })
            .unwrap();
        if listed.items.len() == 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the calls carry its token id"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Another binding's prefix: 403, nothing forwarded (a Run route is in
    // the identification table).
    let (status, answer) = exchange(
        tls.connect(remote),
        &request("/other/chat", &[("authorization", &bearer)], CHAT),
    )
    .unwrap();
    assert_eq!(status, 403, "{answer}");
    assert!(answer.contains("hekma_credential_out_of_scope"), "{answer}");
    assert_eq!(upstream.served().len(), 2);

    // Revoked: the very next call is told so (no sleep), and a rotation
    // revokes the first in the same job.
    facade
        .revoke_binding_credential("openai", BindingCredentialClass::ProxyAccess)
        .unwrap();
    let (status, answer) = exchange(
        tls.connect(remote),
        &request("/openai/chat", &[("authorization", &bearer)], CHAT),
    )
    .unwrap();
    assert_eq!(status, 401);
    assert!(answer.contains("hekma_credential_revoked"), "{answer}");
    let first = facade
        .issue_binding_credential("openai", BindingCredentialClass::ProxyAccess)
        .unwrap();
    let second = facade
        .issue_binding_credential("openai", BindingCredentialClass::ProxyAccess)
        .unwrap();
    let rotated = format!("Bearer {}", first.token.expose_secret());
    let (status, _) = exchange(
        tls.connect(remote),
        &request("/openai/chat", &[("authorization", &rotated)], CHAT),
    )
    .unwrap();
    assert_eq!(status, 401);
    let current = format!("Bearer {}", second.token.expose_secret());
    let (status, _) = exchange(
        tls.connect(remote),
        &request("/openai/chat", &[("authorization", &current)], CHAT),
    )
    .unwrap();
    assert_eq!(status, 200);

    // A reopen keeps the revoked answer (it is read from the store).
    drop(engine);
    let (engine, port, tls) = open_again(&state);
    let remote = SocketAddr::new(ip, port);
    let (status, answer) = exchange(
        tls.connect(remote),
        &request("/openai/chat", &[("authorization", &bearer)], CHAT),
    )
    .unwrap();
    assert_eq!(status, 401);
    assert!(answer.contains("hekma_credential_revoked"), "{answer}");
    drop(engine);
}

/// Reopen the same state directory beyond loopback (the bindings exist).
fn open_again(state: &TempDir) -> (Engine, u16, Tls) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certified.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into()),
        )
        .unwrap();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_proxy: MeteringProxyOptions {
                bind: Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                tls: Some(ProxyTls(Arc::new(server))),
                peers_as_remote: true,
                ..MeteringProxyOptions::default()
            },
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let port = engine.metering_proxy().unwrap().port;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certified.cert.der().clone()).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    (
        engine,
        port,
        Tls {
            config: Arc::new(client),
        },
    )
}

#[test]
fn a_real_routable_peer_is_remote_without_the_seam() {
    let Some(ip) = routable_ip() else {
        eprintln!(
            "skipped: this runner has no routable address that reaches itself, to act as a \
             remote peer"
        );
        return;
    };
    let state = TempDir::new().unwrap();
    let upstream = Upstream::start();
    let (_engine, port, tls) = open(&state, &upstream);
    let remote = SocketAddr::new(ip, port);
    assert!(exchange(plain(remote), &request("/openai/chat", &[], CHAT)).is_none());
    let (status, answer) =
        exchange(tls.connect(remote), &request("/openai/chat", &[], CHAT)).unwrap();
    assert_eq!(status, 401, "{answer}");
    assert!(answer.contains("hekma_authentication_required"), "{answer}");
    assert!(upstream.served().is_empty());
}
