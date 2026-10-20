//! END-TO-END pin of the observed-call retry ladder through the REAL
//! `fake_agent` binary: a transiently-refused loopback listener — the fault
//! shape a loaded windows-latest runner produces when the engine's loopback
//! listener cannot accept (SYN backlog refusal) — must not permanently shrink
//! the call count. Before the fix, one refused connect was SKIPPED forever
//! (`post_completion` is best-effort fire-once), that call's observed row
//! never committed, and a healthy conformance run failed as `timed out
//! waiting for 3 engine-observed rows (have 2)` after burning the full
//! 30s section budget — observed twice on 2026-10-20 windows runs (nextest's
//! fresh-process retry passed both times, ~39s).
//!
//! The shape: the probe is pointed at a loopback port that is DELIBERATELY
//! unbound for the first ~400ms (connects refused), then served by a minimal
//! pure-`std` stub. The assertion: all `--observed-calls` calls are eventually
//! placed (the stub serves every one of them) and the probe's stderr carries
//! the retry evidence. Pure `std`, NO OS-cfg — refused-connect behavior on an
//! unbound loopback port is uniform across the three CI legs.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Serve ONE request on `stream`: read the request head+body (bounded — the
/// probe's bodies are tiny and fixed), answer with a minimal OpenAI-shaped
/// usage JSON, close (the probe's client sends `Connection: close` and reads
/// to EOF).
fn serve_one(stream: TcpStream) -> std::io::Result<()> {
    let mut stream = stream;
    let mut buf = [0u8; 4096];
    // Read once (the whole request fits comfortably in one TCP segment for
    // these fixed bodies); a short read is fine — the response does not depend
    // on the request's content.
    let _ = stream.read(&mut buf);
    let body = br#"{"id":"stub","object":"chat.completion","usage":{"prompt_tokens":30,"completion_tokens":70}}"#;
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

#[test]
fn observed_calls_survive_a_transiently_refused_listener() {
    // Reserve the loopback port, then UNBIND it: the probe's early connects
    // must be REFUSED (RST), not queued, so only the retry ladder can place
    // call 1. The stub binds the same port 400ms in — inside the ladder's
    // bounded retry window (5 attempts × 250ms), far from its edge.
    let probe = TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    let addr = probe.local_addr().expect("loopback addr");
    drop(probe);

    let served = Arc::new(AtomicUsize::new(0));
    let want_calls = 3usize;

    let stub_addr = addr;
    let stub_served = Arc::clone(&served);
    let stub = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        let listener = TcpListener::bind(stub_addr).expect("stub binds the reserved port");
        for _ in 0..want_calls {
            let (stream, _) = listener.accept().expect("stub accepts");
            serve_one(stream).expect("stub serves one request");
            stub_served.fetch_add(1, Ordering::SeqCst);
        }
    });

    let mut child = Command::new(hekma_conformance::fake_agent_bin())
        .arg("--observed-calls")
        .arg(want_calls.to_string())
        .arg("--linger-ms")
        .arg("600000")
        .env("OPENAI_BASE_URL", format!("http://{addr}"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fake_agent");

    // Every requested call must be served within a generous bound (the ladder
    // itself needs <1.5s worst case; the poll is not a timing dependency).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let got = served.load(Ordering::SeqCst);
        assert!(got <= want_calls, "stub served more calls than requested");
        if got == want_calls {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the probe did not place all {want_calls} observed calls through the \
             transiently-refused listener (served {got}) — a refused connect was \
             skipped instead of retried"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The retry evidence rides the probe's stderr (the engine captures it into
    // the agent log — that is what makes the next occurrence diagnosable).
    let _ = child.kill();
    let out = child.wait().expect("reap probe");
    assert!(
        !out.success(),
        "the probe lingers by design; kill must end it"
    );
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    assert!(
        stderr.contains("fake_agent: observed call attempt") && stderr.contains("retrying"),
        "retry attempts must be announced on stderr, got: {stderr}"
    );

    stub.join().expect("stub thread");
}
