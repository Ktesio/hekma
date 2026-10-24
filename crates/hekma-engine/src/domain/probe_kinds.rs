//! The shared per-attempt probe evaluators (contract `1.1.0`).
//!
//! Both declared polls — the `[readiness]` gate (evaluated between spawn and
//! the `starting → running` transition) and the `[liveness]` check (evaluated
//! by the reaper against the `running` agent) — rest on the SAME per-attempt
//! primitives, lifted here so the two surfaces cannot drift apart:
//!
//! * [`file_fresh`] — the heartbeat / file-fresh evaluator (regular file,
//!   mtime age within the window).
//! * [`http_get_attempt`] — ONE bounded raw-HTTP GET whose status must match
//!   (renamed from the readiness-only `http_ready` when liveness joined).
//! * the URL plumbing ([`split_http_url`], [`parse_authority`],
//!   [`parse_status_line`]) and the attempt caps ([`HTTP_ATTEMPT_CAP`],
//!   [`HTTP_READ_CAP_BYTES`]).
//!
//! Deliberate boundaries (inherited from the readiness-gate story, now the
//! shared posture):
//! * **Sync, dependency-free probes.** Both call sites run on engine-owned
//!   threads holding no async runtime handle they may block on, so the HTTP
//!   check is a raw [`std::net::TcpStream`] HTTP/1.1 client (one GET,
//!   `Connection: close`, status-line parse) — the same std-only posture as
//!   the conformance fake agent's observed-calls client. No new
//!   dependencies, no async (the AD-3 embed gate).
//! * **No DNS.** Manifest validation admits only LITERAL loopback IPs in
//!   either `http-get` URL, so the authority is parsed as an
//!   [`std::net::IpAddr`] directly — no `to_socket_addrs`, no resolver, and
//!   no first-address-only ambiguity (a literal IP is exactly one address).
//! * **Total, never panicking.** Any per-attempt failure (connect refused,
//!   file missing, unparseable head) is a plain `false` / `None` — a "not
//!   (yet) passing" answer the caller retries on its own cadence, never an
//!   error that aborts supervision.
//!
//! **The non-fork pin.** Readiness and liveness MUST call THESE symbols, not
//! private copies: both modules expose `pub(crate)` fn-pointer aliases
//! (`FILE_FRESH`, `HTTP_GET_ATTEMPT`) targeting this module's functions, and
//! a test here asserts pointer equality across all four aliases. A fork (a
//! copied evaluator that then evolves alone) fails that test.

use std::io::Read;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// The per-attempt cap on ONE http-get attempt's connect/read/write timeouts.
/// A connect is retried on the caller's cadence, so a single attempt never
/// needs the whole budget; the cap keeps each op short even when the
/// remaining budget is minutes. The readiness gate's declared `timeout_ms`
/// (or the reaper tick that carries a liveness attempt) is the pacing
/// context; this cap only bounds individual socket ops within it.
pub(crate) const HTTP_ATTEMPT_CAP: Duration = Duration::from_secs(1);

/// How much of a response body the http-get attempt reads before giving up
/// on the parse. The attempt reads only the status line; the cap bounds a
/// pathologically chatty (or lying) peer.
pub(crate) const HTTP_READ_CAP_BYTES: usize = 8 * 1024;

/// Evaluate ONE http-get attempt: connect (bounded), send one HTTP/1.1 GET,
/// read the status line. ANY failure — split, parse, connect, write, read —
/// is `false` (not passing), never an error: a listener that is not up yet
/// is the normal pre-ready state, and a hung one is the reaper's window
/// problem, not a panic here.
///
/// `deadline` is the caller's hard wall (the readiness gate's whole-wait
/// deadline; the reaper passes `Instant::now() + HTTP_ATTEMPT_CAP`): every
/// socket operation's timeout is `min(time-to-deadline, HTTP_ATTEMPT_CAP)`
/// and the read loop re-checks the deadline, so one attempt can never hold
/// its caller past the wall — no matter how the peer dribbles bytes.
pub(crate) fn http_get_attempt(url: &str, expected_status: u16, deadline: Instant) -> bool {
    let Some((authority, path)) = split_http_url(url) else {
        return false;
    };
    let Some(addr) = parse_authority(&authority) else {
        return false;
    };
    // Per-op cap: never more than the time left on the wall, never more than
    // one attempt's pacing slice. A deadline already due resolves to ZERO —
    // every op then fails immediately and the caller's deadline check ends
    // the wait.
    let op_cap = |deadline: Instant| -> Duration {
        deadline
            .saturating_duration_since(Instant::now())
            .min(HTTP_ATTEMPT_CAP)
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, op_cap(deadline)) else {
        return false;
    };
    if stream.set_write_timeout(Some(op_cap(deadline))).is_err() {
        return false;
    }
    let request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    if std::io::Write::write_all(&mut stream, request.as_bytes()).is_err() {
        return false;
    }
    // Review round 1 (A1): parse the status line AS SOON AS it is available.
    // The old loop waited for EOF (or the byte cap), which a healthy
    // HTTP/1.1 KEEP-ALIVE peer never sends after its response head — the
    // probe then always burned its whole budget against a passing 200. Now:
    // accumulate bytes, and after EACH read check whether a complete first
    // line (`\r\n`) is present; once it is, parse and return immediately.
    // No EOF and no full-header read are required.
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        // A2: the deadline is enforced INSIDE the read loop, and each read's
        // timeout re-shrinks with the remaining time — a peer dribbling one
        // byte per interval cannot reset its own timer forever.
        if stream.set_read_timeout(Some(op_cap(deadline))).is_err() {
            return false;
        }
        if Instant::now() >= deadline {
            return false;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                head.extend_from_slice(&chunk[..n]);
                // A complete status line is parseable the moment its `\r\n`
                // landed — a keep-alive peer answering 200 passes here.
                if head.windows(2).any(|w| w == b"\r\n") {
                    return parse_status_line(&head) == Some(expected_status);
                }
                // The cap bounds a peer that never stops talking but never
                // completes a line either.
                if head.len() >= HTTP_READ_CAP_BYTES {
                    break;
                }
            }
            Err(_) => return false,
        }
    }
    parse_status_line(&head) == Some(expected_status)
}

/// Split an `http://` URL into its `(authority, request-path)` pair. The
/// manifest validation guarantees the scheme + a non-empty authority — and
/// (review round 1, A4) rejects FRAGMENTS at validation, so neither caller
/// ever sees a `#...` suffix to strip or mis-send: this stays total anyway
/// (an unsplitable URL is a per-attempt `false`).
fn split_http_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }
    Some((authority.to_string(), path.to_string()))
}

/// Parse a URL authority (`host` or `host:port`) into a socket address with
/// NO name resolution (review round 1, A3): the manifest validation admits
/// only LITERAL loopback IPs, so the host is parsed as an [`IpAddr`]
/// directly. No `to_socket_addrs` — no DNS in the engine, and no
/// first-address-only ambiguity (a literal IP is one address). An IPv6
/// literal with a port takes the `[addr]:port` form. A hostname (e.g.
/// `localhost`) parses as None — a per-attempt not-passing, though
/// validation refuses it long before any runner runs.
fn parse_authority(authority: &str) -> Option<SocketAddr> {
    let (host, port_str) = if let Some(rest) = authority.strip_prefix('[') {
        // `[addr]:port` — the bracketed IPv6-literal form (validation
        // requires the brackets whenever an IPv6 literal carries a port).
        let (host, rest) = rest.split_once(']')?;
        match rest.strip_prefix(':') {
            Some(port) => (host, port),
            None if rest.is_empty() => (host, ""),
            None => return None,
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                (host, port)
            }
            _ => (authority, ""),
        }
    };
    let ip: IpAddr = host.parse().ok()?;
    let port = match port_str.parse::<u16>() {
        Ok(port) => port,
        Err(_) if port_str.is_empty() => 80,
        Err(_) => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Parse the HTTP status code out of a response head: the second
/// whitespace-separated token of the first line. `None` when the head holds
/// no parseable status line (truncated, non-HTTP, garbage).
fn parse_status_line(head: &[u8]) -> Option<u16> {
    let line = head.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split_whitespace();
    let _version = parts.next()?;
    parts.next()?.parse().ok()
}

/// Evaluate ONE file-fresh attempt: the file exists, is a REGULAR FILE
/// (review round 1, B6: a DIRECTORY's mtime must not read as fresh — a
/// directory's metadata changes whenever its entries change, which would
/// let a not-yet-ready agent's churning work directory pass — or a churning
/// Agent Home read as a beating heart), and its modification age
/// (SystemTime deltas only) is within `max_age_ms`. A missing file, a
/// non-file, an unavailable mtime, or an un-readable path is a plain
/// `false`. A future mtime (clock skew, an agent that stamps ahead) counts
/// as age zero — freshly written, which is exactly what the signal asserts.
pub(crate) fn file_fresh(path: &Path, max_age_ms: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    // B6 (review round 1): only a regular FILE proves freshness. A directory
    // (or any non-regular entry) is never fresh, whatever its mtime says.
    if !metadata.is_file() {
        return false;
    }
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    age.as_millis() as u64 <= max_age_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::liveness::LivenessSignal;
    use hekma_adapter_api::{HeartbeatFileCheck, LivenessCheck};
    use std::time::Duration as _Duration;

    // -- http url splitting + status parse ---------------------------------

    #[test]
    fn split_http_url_handles_bare_authority_and_paths() {
        assert_eq!(
            split_http_url("http://127.0.0.1:8080/health"),
            Some(("127.0.0.1:8080".to_string(), "/health".to_string()))
        );
        assert_eq!(
            split_http_url("http://localhost:9000"),
            Some(("localhost:9000".to_string(), "/".to_string()))
        );
        assert_eq!(split_http_url("http:///path"), None);
        assert_eq!(split_http_url("https://host/health"), None);
    }

    #[test]
    fn parse_authority_is_literal_ip_only_with_default_and_bracketed_ports() {
        // Review round 1 (A3): the authority parses as an IpAddr DIRECTLY —
        // no to_socket_addrs, no DNS. A bare literal gets the HTTP default
        // port; an IPv6 literal takes the `[addr]:port` form; a hostname is
        // None (validation refuses it long before any runner runs).
        let addr = parse_authority("127.0.0.1:1").expect("numeric authority parses");
        assert_eq!(addr.port(), 1);
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        let addr = parse_authority("127.0.0.1").expect("bare literal parses with :80");
        assert_eq!(addr.port(), 80);
        let addr = parse_authority("[::1]:8080").expect("bracketed IPv6 parses");
        assert_eq!(addr.port(), 8080);
        assert_eq!(addr.ip().to_string(), "::1");
        let addr = parse_authority("[::1]").expect("bracketed IPv6 without a port");
        assert_eq!(addr.port(), 80);
        // A hostname does not resolve (DNS eliminated) — a per-attempt
        // not-ready, never a lookup.
        assert_eq!(parse_authority("localhost:9000"), None);
        // A hostname that cannot exist stays None (a per-attempt not-ready).
        assert_eq!(parse_authority("this host has a space"), None);
    }

    #[test]
    fn parse_status_line_reads_the_second_token_and_tolerates_junk() {
        assert_eq!(
            parse_status_line(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"),
            Some(200)
        );
        assert_eq!(parse_status_line(b"HTTP/1.0 404 Not Found\n"), Some(404));
        assert_eq!(parse_status_line(b"garbage"), None);
        assert_eq!(parse_status_line(b"HTTP/1.1 not-a-number"), None);
        assert_eq!(parse_status_line(b""), None);
    }

    // -- file-fresh ----------------------------------------------------------

    #[test]
    fn file_fresh_accepts_a_recent_file_and_refuses_a_stale_or_missing_one() {
        let dir = std::env::temp_dir().join(format!(
            "hekma-probe-kinds-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let fresh = dir.join("fresh.marker");
        std::fs::write(&fresh, b"ready").expect("write marker");

        // A just-written file is fresh within a generous window...
        assert!(file_fresh(&fresh, 60_000));
        // ...and NOT fresh within a zero-length one (the validation floor
        // keeps 1ms the smallest legal window; the pure helper stays total).
        let mtime = std::time::SystemTime::now()
            .checked_sub(_Duration::from_secs(10))
            .expect("10s ago");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&fresh)
            .expect("reopen");
        file.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .expect("set mtime");
        drop(file);
        assert!(!file_fresh(&fresh, 1_000));
        // A missing file is never fresh.
        assert!(!file_fresh(&dir.join("absent.marker"), 60_000));
        // B6 (review round 1): a DIRECTORY is never fresh — its mtime moves
        // whenever its entries change, which must not read as the declared
        // signal (`metadata().is_file()` gate).
        assert!(!file_fresh(&dir, 60_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- the non-fork pin ------------------------------------------------------

    #[test]
    fn readiness_and_liveness_alias_the_shared_evaluators_no_fork() {
        // The non-fork pin (the evaluator-fork mutation killer): both call
        // sites expose fn-pointer aliases targeting THIS module's functions,
        // and ADDRESS equality proves neither surface kept (or grew) a
        // private copy that could evolve alone. A forked evaluator — the
        // "fix it in one place, silently not the other" defect — flips this
        // test the moment the copies diverge in identity. (`std::ptr::fn_addr_eq`
        // is the blessed comparison — clippy rightly flags bare `==` on fn
        // pointers; comparing ADDRESSES of one specific non-generic fn is
        // exactly the identity check this pin wants.)
        let file_fresh_ptr = file_fresh as fn(&Path, u64) -> bool;
        let http_ptr = http_get_attempt as fn(&str, u16, Instant) -> bool;
        assert!(
            std::ptr::fn_addr_eq(crate::domain::readiness::FILE_FRESH, file_fresh_ptr),
            "readiness must alias the shared file_fresh, not a copy"
        );
        assert!(
            std::ptr::fn_addr_eq(crate::domain::liveness::FILE_FRESH, file_fresh_ptr),
            "liveness must alias the shared file_fresh, not a copy"
        );
        assert!(
            std::ptr::fn_addr_eq(crate::domain::readiness::HTTP_GET_ATTEMPT, http_ptr),
            "readiness must alias the shared http_get_attempt, not a copy"
        );
        assert!(
            std::ptr::fn_addr_eq(crate::domain::liveness::HTTP_GET_ATTEMPT, http_ptr),
            "liveness must alias the shared http_get_attempt, not a copy"
        );
    }

    #[test]
    fn readiness_file_fresh_and_liveness_heartbeat_answer_identically() {
        // The behavioral-equivalence canary (review round 1, V2): the same
        // on-disk fixture, fed through each surface's EXACT call shape, must
        // answer identically — the two sections share one evaluator, so a
        // heartbeat that is fresh to liveness is fresh to readiness and vice
        // versa. The readiness side replicates `run_probe`'s file-fresh arm
        // verbatim (relative resolves against the home; the readiness alias
        // evaluates), the liveness side goes through
        // `liveness::evaluate(HeartbeatFile)`. The alias pin above proves
        // identity; this proves BEHAVIOR through the real call shapes (a
        // changed call shape on either surface — a different resolution, a
        // flipped comparison — flips this canary).
        let dir = std::env::temp_dir().join(format!(
            "hekma-probe-kinds-canary-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let home = &dir;

        // Helper: one fixture's answer through BOTH call shapes.
        let both_answers = |declared_path: &str, max_age_ms: u64| -> (bool, bool) {
            // READINESS's exact shape (run_probe's file-fresh arm).
            let readiness = {
                let probe = hekma_adapter_api::FileFreshProbe {
                    path: declared_path.to_string(),
                    max_age_ms,
                };
                let path = if Path::new(&probe.path).is_absolute() {
                    std::path::PathBuf::from(&probe.path)
                } else {
                    home.join(&probe.path)
                };
                crate::domain::readiness::FILE_FRESH(&path, probe.max_age_ms)
            };
            // LIVENESS's exact shape (evaluate's HeartbeatFile arm).
            let liveness = {
                let check = LivenessCheck::HeartbeatFile(HeartbeatFileCheck {
                    path: declared_path.to_string(),
                    max_age_ms,
                });
                crate::domain::liveness::evaluate(&check, home) == LivenessSignal::Fresh
            };
            (readiness, liveness)
        };

        // 1. FRESH (just written): both true.
        let fresh = dir.join("fresh.marker");
        std::fs::write(&fresh, b"beat").expect("write marker");
        let (r, l) = both_answers("fresh.marker", 60_000);
        assert_eq!((r, l), (true, true), "fresh file: both surfaces say fresh");

        // 2. ABSENT: both false (absent ≡ stale, for BOTH sections).
        let (r, l) = both_answers("absent.marker", 60_000);
        assert_eq!(
            (r, l),
            (false, false),
            "absent file: both surfaces say stale"
        );

        // 3. DIRECTORY: both false (B6 — only a regular file proves
        //    freshness; the home itself is never a heartbeat).
        let (r, l) = both_answers(".", 60_000);
        assert_eq!((r, l), (false, false), "directory: both surfaces refuse");

        // 4. FUTURE MTIME (clock skew): both true — age clamps to zero, the
        //    freshly-written posture.
        let future = dir.join("future.marker");
        std::fs::write(&future, b"beat").expect("write marker");
        let future_mtime = std::time::SystemTime::now()
            .checked_add(_Duration::from_secs(10))
            .expect("+10s");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&future)
            .expect("reopen");
        file.set_times(std::fs::FileTimes::new().set_modified(future_mtime))
            .expect("set future mtime");
        drop(file);
        let (r, l) = both_answers("future.marker", 60_000);
        assert_eq!(
            (r, l),
            (true, true),
            "future mtime: both surfaces clamp to fresh"
        );

        // Backdate one file for the stale cases (10s old).
        let stale = dir.join("stale.marker");
        std::fs::write(&stale, b"beat").expect("write marker");
        let old_mtime = std::time::SystemTime::now()
            .checked_sub(_Duration::from_secs(10))
            .expect("-10s");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .expect("reopen");
        file.set_times(std::fs::FileTimes::new().set_modified(old_mtime))
            .expect("set old mtime");
        drop(file);

        // 5. OLD vs 5s window: both false (10s > 5s — stale).
        let (r, l) = both_answers("stale.marker", 5_000);
        assert_eq!((r, l), (false, false), "10s-old file vs 5s window: stale");

        // 6. OLD vs 60s window: both true (10s <= 60s — still fresh).
        let (r, l) = both_answers("stale.marker", 60_000);
        assert_eq!((r, l), (true, true), "10s-old file vs 60s window: fresh");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
