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
//! The shape: the probe is pointed at a loopback port that stays DELIBERATELY
//! unbound (connects refused) until the probe's FIRST retry announcement is
//! read off its stderr — the refusal window opens and closes on observed
//! behavior, never on wall-clock. A fixed window (the first draft held the
//! port closed for a flat 400ms) is a startup race: a windows-latest process
//! spawn can outlast the window, the probe's first connect then lands on an
//! already-bound stub, nothing is refused, and the test fails its own
//! retry-evidence assert with empty stderr — deterministically, five attempts
//! across two windows runs on 2026-10-20. The assertion: all
//! `--observed-calls` calls are eventually placed (the stub serves every one
//! of them) and the probe's stderr carries the retry evidence. Pure `std`,
//! NO OS-cfg — refused-connect behavior on an unbound loopback port is
//! uniform across the three CI legs.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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
    // call 1. The stub binds the same port the moment the probe's first
    // retry announcement is READ off its stderr — inside the ladder's
    // bounded retry window (the next attempt sleeps 250ms; a thread wakeup
    // plus a loopback bind is microseconds), with no dependence on when the
    // probe's process reaches its first connect.
    let probe = TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    let addr = probe.local_addr().expect("loopback addr");
    drop(probe);

    let served = Arc::new(AtomicUsize::new(0));
    let want_calls = 3usize;

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

    // Stream the probe's stderr from a reader thread: it (a) signals the stub
    // the moment the first retry announcement arrives — that is what closes
    // the refusal window — and (b) accumulates the text the final assert
    // pins. Reading incrementally (not only after kill) is also the shape
    // least able to drop pipe content on any leg.
    let stderr_pipe = child.stderr.take().expect("stderr is piped above");
    let stderr_seen = Arc::new(Mutex::new(String::new()));
    let (first_retry_tx, first_retry_rx) = mpsc::channel::<()>();
    let reader_seen = Arc::clone(&stderr_seen);
    let reader = std::thread::spawn(move || {
        let mut lines = BufReader::new(stderr_pipe);
        let mut announced = false;
        loop {
            let mut raw = Vec::new();
            match lines.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break, // EOF once the probe is killed
                Ok(_) => {
                    let line = String::from_utf8_lossy(&raw).into_owned();
                    reader_seen.lock().expect("stderr lock").push_str(&line);
                    if !announced
                        && line.contains("fake_agent: observed call attempt")
                        && line.contains("retrying")
                    {
                        announced = true;
                        let _ = first_retry_tx.send(());
                    }
                }
            }
        }
    });

    let stub_addr = addr;
    let stub_served = Arc::clone(&served);
    let stub = std::thread::spawn(move || {
        // Hold the port refused until the probe RETRIES — proof the refusal
        // was hit AND announced. A fire-once skip-forever probe (the bug this
        // pins against) never announces, the window never closes, and the
        // serve-loop deadline below fails the test with the shortfall.
        if first_retry_rx
            .recv_timeout(Duration::from_secs(30))
            .is_err()
        {
            return Err(
                "no retry announcement reached the probe's stderr within 30s — the \
                 refused connect was skipped instead of retried"
                    .to_string(),
            );
        }
        let listener = match TcpListener::bind(stub_addr) {
            Ok(listener) => listener,
            Err(e) => return Err(format!("stub binds the reserved port: {e}")),
        };
        for _ in 0..want_calls {
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) => return Err(format!("stub accepts: {e}")),
            };
            if let Err(e) = serve_one(stream) {
                return Err(format!("stub serves one request: {e}"));
            }
            stub_served.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    });

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
    reader.join().expect("stderr reader thread");
    let stderr = stderr_seen.lock().expect("stderr lock").clone();
    assert!(
        stderr.contains("fake_agent: observed call attempt") && stderr.contains("retrying"),
        "retry attempts must be announced on stderr, got: {stderr}"
    );

    match stub.join().expect("stub thread") {
        Ok(()) => {}
        Err(e) => panic!("{e}"),
    }
}
