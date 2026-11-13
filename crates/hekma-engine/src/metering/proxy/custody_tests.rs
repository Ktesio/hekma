//! Story 22-2: key custody end to end — a supervised custodied agent's Run
//! Key through the real proxy to a TLS fake provider that records what it
//! receives and echoes the presented key back.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{AdapterRef, Engine, EngineOptions, MeteringProxyOptions};

/// The custodied key (the agent must never see it; the provider must).
const SENTINEL: &str = "sk-e2e-custody-sentinel-0123456789";
const SECRET: &str = "HEKMA_E2E_CUSTODY_KEY";

/// One request the fake provider received: its request line and headers.
type Seen = Arc<Mutex<Vec<String>>>;

/// A TLS fake provider on `localhost`: accepts `Bearer <SENTINEL>` only
/// (anything else is `401`), records every request, and echoes the
/// presented credential in a header and in the body (the redaction probe).
fn tls_provider() -> (u16, Vec<u8>, Seen) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = certified.cert.der().to_vec();
    let key = certified.signing_key.serialize_der();
    let config = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(cert.clone())],
                rustls::pki_types::PrivateKeyDer::Pkcs8(key.into()),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = Arc::clone(&config);
            let record = Arc::clone(&record);
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let Ok(conn) = rustls::ServerConnection::new(config) else {
                    return;
                };
                let mut tls = rustls::StreamOwned::new(conn, stream);
                let mut raw = Vec::new();
                let mut buffer = [0u8; 4096];
                let head_end = loop {
                    match tls.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => raw.extend_from_slice(&buffer[..n]),
                    }
                    if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at;
                    }
                };
                let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while raw.len() < head_end + 4 + length {
                    match tls.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buffer[..n]),
                    }
                }
                record.lock().unwrap().push(head.clone());
                let presented = head
                    .lines()
                    .find_map(|l| {
                        let lower = l.to_ascii_lowercase();
                        lower
                            .starts_with("authorization:")
                            .then(|| l["authorization:".len()..].trim().to_string())
                    })
                    .unwrap_or_default();
                let (status, body) = if presented == format!("Bearer {SENTINEL}") {
                    (
                        "200 OK",
                        format!(
                            r#"{{"model":"gpt-demo","usage":{{"prompt_tokens":10,"completion_tokens":5}},"echo":"{presented}"}}"#
                        ),
                    )
                } else {
                    (
                        "401 Unauthorized",
                        r#"{"error":"invalid api key"}"#.to_string(),
                    )
                };
                let _ = tls.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nx-echo: {presented}\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = tls.flush();
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    (port, cert, seen)
}

/// A raw HTTP/1.1 request to the proxy; `(status, headers, body)`.
fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String, String) {
    let body = r#"{"model":"gpt-demo","messages":[]}"#;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    ));
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    (status, head.to_string(), body.to_string())
}

/// One HTTPS call straight to the fake provider with `Bearer <key>`,
/// trusting `root`; the response status.
fn direct_call(port: u16, root: &[u8], key: &str) -> u16 {
    let config = crate::metering::relay::tls_config_with_root(root);
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(config, name).unwrap();
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    tls.write_all(
        format!(
            "POST /v1/chat/completions HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {key}\r\n\
             connection: close\r\ncontent-length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    String::from_utf8_lossy(&raw)
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn write_manifest(dir: &Path, dump: &Path) {
    let bin = hekma_conformance::fake_agent_bin();
    let body = format!(
        "contract_version = \"1.0.0\"\n\n[adapter]\nkind = \"custody-e2e\"\n\n\
         [lifecycle.start]\nexec = {exec:?}\nargs = [\"--linger-ms\", \"600000\", \"--dump\", {dump:?}]\n\n\
         [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
         [metering]\nsource = \"engine-observed\"\n\n\
         [config.\"metering.base_url\"]\nenv = \"OPENAI_BASE_URL\"\n\n\
         [config.\"provider.api_key\"]\nenv = \"OPENAI_API_KEY\"\n",
        exec = bin.to_string_lossy(),
        dump = dump.to_string_lossy(),
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

fn dumped_env(path: &Path, var: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Some(value) = text
                .lines()
                .find_map(|line| line.strip_prefix(&format!("env={var}=")))
            {
                return value.to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "{var} never appeared in {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_custodied_call_carries_the_held_key_only_to_its_pinned_origin_and_masks_it_on_the_way_back() {
    let (provider_port, root, seen) = tls_provider();
    let root_copy = root.clone();
    let origin = format!("https://localhost:{provider_port}");
    let state = tempfile::tempdir().unwrap();
    // The key resolves from the environment; its pins sit beside it in
    // the secrets file.
    std::env::set_var(SECRET, SENTINEL);
    let secrets = state.path().join("secrets.toml");
    std::fs::write(
        &secrets,
        format!(
            "[custody.{SECRET}]\norigins = [\"{origin}\"]\npaths = [\"/v1/chat/completions\"]\n"
        ),
    )
    .unwrap();
    crate::backends::restrict_to_owner(&secrets).unwrap();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_proxy: MeteringProxyOptions {
                test_root_der: Some(root.clone()),
                ..MeteringProxyOptions::default()
            },
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let proxy_port = engine.metering_proxy().unwrap().port;
    let manifest = tempfile::tempdir().unwrap();
    let dump = manifest.path().join("dump.txt");
    write_manifest(manifest.path(), &dump);
    let facade = engine.blocking();
    facade
        .register_with_adapter("e2e", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade
        .set_config("e2e", "provider.api_key", &format!("secret:{SECRET}"))
        .unwrap();
    facade
        .set_config("e2e", "metering.upstream_base_url", &format!("{origin}/v1"))
        .unwrap();
    facade.start("e2e").unwrap();
    let run_key = dumped_env(&dump, "OPENAI_API_KEY");
    let base = dumped_env(&dump, "OPENAI_BASE_URL");
    assert!(run_key.starts_with("hk_run_"), "{run_key}");
    let route = base
        .split_once(&format!(":{proxy_port}"))
        .unwrap()
        .1
        .to_string();
    let bearer = format!("Bearer {run_key}");

    // The swap: the provider sees the held key once, in its own scheme,
    // and never a Run Key; the agent's answer has the key masked.
    let (status, head, body) = send(
        proxy_port,
        "POST",
        &format!("{route}/chat/completions"),
        &[("authorization", &bearer)],
    );
    assert_eq!(status, 200, "{head}\n{body}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let request = &seen[0];
        assert!(request.contains(&format!("Bearer {SENTINEL}")), "{request}");
        assert!(
            !request.contains("hk_"),
            "a Run Key reached the provider: {request}"
        );
        assert!(
            !request.to_ascii_lowercase().contains("x-hekma-run-key"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("accept-encoding: identity"),
            "{request}"
        );
    }
    assert!(!body.contains(SENTINEL), "the held key came back: {body}");
    assert!(!head.contains(SENTINEL), "the held key came back: {head}");
    assert!(body.contains("hekma_redacted_key"), "{body}");
    assert!(head.contains("hekma_redacted_key"), "{head}");

    // A path outside the key's list: refused, nothing forwarded.
    let (status, _, body) = send(
        proxy_port,
        "POST",
        &format!("{route}/files"),
        &[("authorization", &bearer)],
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("hekma_path_not_allowed"), "{body}");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "nothing reached the provider"
    );

    // A rebinding-shaped Host: refused before any dial.
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    stream
        .write_all(
            format!(
                "POST {route}/chat/completions HTTP/1.1\r\nhost: evil.example\r\n\
                 authorization: {bearer}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    assert!(raw.starts_with("HTTP/1.1 403"), "{raw}");
    assert!(raw.contains("hekma_bad_host"), "{raw}");
    assert_eq!(seen.lock().unwrap().len(), 1);

    // Fail closed: the Run Key dialled straight at the provider (around
    // the proxy) is not a provider key — the provider refuses it.
    let status = direct_call(provider_port, &root_copy, &run_key);
    assert_eq!(status, 401, "the provider accepted a Run Key");
    facade.stop("e2e", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn revealing_a_custodied_key_records_one_event_naming_it_and_never_its_value() {
    // AC (R2.15): the reveal of a custodied key commits a `secret_revealed`
    // event naming the secret — on the bus, never the value — before the
    // value is shown; an opted-out instance custodies nothing and records
    // nothing.
    let state = tempfile::tempdir().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let manifest = tempfile::tempdir().unwrap();
    write_manifest(manifest.path(), &manifest.path().join("unused.txt"));
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "revealer",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    std::env::set_var(SECRET, SENTINEL);
    facade
        .set_config("revealer", "provider.api_key", &format!("secret:{SECRET}"))
        .unwrap();
    let mut events = facade.subscribe();
    let event = facade
        .note_secret_revealed("revealer")
        .unwrap()
        .expect("a custodied key's reveal is recorded");
    assert_eq!(event.instance, "revealer");
    assert_eq!(event.secrets, vec![SECRET.to_string()]);
    let published = events.try_recv().expect("the event is on the bus");
    let json = serde_json::to_string(&published).unwrap();
    assert!(json.contains("\"kind\":\"secret_revealed\""), "{json}");
    assert!(json.contains(SECRET), "{json}");
    assert!(
        !json.contains(SENTINEL),
        "the value reached the event: {json}"
    );

    facade
        .set_config("revealer", "provider.custody", "off")
        .unwrap();
    assert!(facade.note_secret_revealed("revealer").unwrap().is_none());
    assert!(
        events.try_recv().is_err(),
        "nothing recorded when nothing is custodied"
    );
}
