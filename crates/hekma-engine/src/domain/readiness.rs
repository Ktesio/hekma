//! The readiness-probe runner (contract `1.1.0`, the readiness-gate story).
//!
//! A manifest's optional `[readiness]` section declares ONE probe
//! ([`ReadinessGate`]) the engine must satisfy between spawn and the
//! `starting → running` transition. This module is the RUNNER the start path
//! calls (synchronously, under the supervisor lock — the same bounded-wait
//! posture as the ACP handshake): poll the probe on the ENGINE-owned cadence
//! until it passes, the process exits, or the declared budget runs out.
//!
//! Deliberate boundaries:
//! * **Sync, dependency-free probes.** The supervisor's start path holds no
//!   async runtime handle it may block on, so the `http-get` probe is a raw
//!   [`std::net::TcpStream`] HTTP/1.1 client (one GET, `Connection: close`,
//!   status-line parse) — the same std-only posture as the conformance fake
//!   agent's observed-calls client. No new dependencies, no async in the
//!   spawn path (the AD-3 embed gate).
//! * **Engine owns the cadence.** The manifest declares the BUDGET
//!   (`timeout_ms`); the poll interval is [`READINESS_PROBE_POLL`] here. A
//!   manifest never tunes engine timing.
//! * **The declared budget is a HARD wall.** The loop checks liveness FIRST
//!   (an exit during the probe is the existing crash path, never a probe
//!   failure), then the probe, then the deadline — and reports the ACTUAL
//!   elapsed wait, never the declared one. The wall is enforced, not
//!   observed: one `deadline = started + timeout_ms` is computed up front
//!   and EVERY socket operation's timeout is `min(time-to-deadline,
//!   HTTP_ATTEMPT_CAP)`, with the deadline re-checked inside the HTTP read
//!   loop — so a dribbling peer that never stops sending cannot hold the
//!   wait past its budget. What remains of an overshoot is scheduling-only
//!   (thread wake-up jitter); the number surfaced is the truth.
//! * **No DNS.** Manifest validation admits only LITERAL loopback IPs in
//!   the `http-get` URL, so the runner parses the authority as an
//!   [`std::net::IpAddr`] directly — no `to_socket_addrs`, no resolver in
//!   the start path (which also removes the first-address-only ambiguity:
//!   a literal IP is exactly one address).
//! * **Graceful degradation.** Every per-attempt failure (connect refused,
//!   file missing, log unreadable) is a plain "not ready yet" — retried on
//!   the cadence, never an error that aborts the start outside the typed
//!   outcomes.

use std::io::{Read, Seek, SeekFrom};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use hekma_adapter_api::ReadinessProbe;

use crate::ports::{ProcessBackend, ProcessStatus};

/// How often the probe wait re-evaluates the declared probe. The budget is
/// the manifest's `timeout_ms`; THIS interval is engine-owned. ~100ms is
/// snappy enough that a fast probe does not visibly delay `start`, cheap
/// enough that a filesystem/stat poll is negligible, and it bounds the
/// timeout overshoot at (roughly) one interval.
pub(crate) const READINESS_PROBE_POLL: Duration = Duration::from_millis(100);

/// The per-attempt cap on ONE http-get attempt's connect/read/write timeouts.
/// A connect is retried on the poll cadence, so a single attempt never needs
/// the whole budget; the cap keeps each op short even when the remaining
/// budget is minutes. The probe's declared `timeout_ms` is the HARD wall
/// (the deadline below); this cap only paces individual socket ops within
/// it.
const HTTP_ATTEMPT_CAP: Duration = Duration::from_secs(1);

/// How much of a response body the http-get probe reads before giving up on
/// the parse. The probe reads only the status line; the cap bounds a
/// pathologically chatty (or lying) peer.
const HTTP_READ_CAP_BYTES: usize = 8 * 1024;

/// How ONE probe wait ended — the three outcomes the start path branches on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// The probe passed within its budget: proceed to `running`. Carries the
    /// ACTUAL elapsed wait (review round 1: the start path emits ONE
    /// "probe passed after Nms" diagnostic beside its wait-begins note —
    /// the observable ordering evidence that the pass PRECEDED the
    /// `starting → running` transition).
    Ready {
        /// The honest wall-clock wait until the pass, in milliseconds.
        waited_ms: u64,
    },
    /// The budget ran out. Carries the probe's rendered description (for the
    /// diagnostic + the transition cause), the ACTUAL elapsed wait, and how
    /// many liveness polls ERRORED during the wait (review round 1: the
    /// runner tolerates a poll error as alive, so the timeout's honesty
    /// must name that the death-check was degraded — the start path appends
    /// the fact to the failure detail).
    TimedOut {
        /// The rendered `describe` of the probe that never passed.
        description: String,
        /// The honest wall-clock wait, in milliseconds.
        waited_ms: u64,
        /// How many liveness polls errored while the wait ran.
        poll_errors: u64,
    },
    /// The supervised process EXITED while the probe waited. This is the
    /// existing crash path (an exit during startup), never a probe failure —
    /// the start path drives the same `Crashed` transition + Restart Policy
    /// consult a reaper-detected exit gets. Carries the reported exit code.
    ProcessExited(Option<i32>),
}

/// Run ONE readiness probe to a conclusion (the start path's synchronous
/// wait). Polls — on [`READINESS_PROBE_POLL`], bounded by the gate's
/// `timeout_ms` — in strict order: liveness, probe, deadline.
///
/// * `backend` + `handle` — the just-spawned process, for the liveness poll
///   (a poll ERROR is tolerated as alive, the same posture as the startup
///   survival watch: the reaper surfaces persistent poll failures later).
/// * `log_path` + `log_start_offset` — the engine-owned agent output log and
///   the pre-spawn cursor for a `log-contains` probe. `None` (capture off)
///   makes that probe unsatisfiable — it will run out its budget honestly
///   and the timeout names why.
/// * `home` — the Agent Home, which a relative `file-fresh` path resolves
///   against.
pub(crate) fn run_probe(
    gate: &hekma_adapter_api::ReadinessGate,
    backend: &crate::backends::Backend,
    handle: &mut crate::backends::Handle,
    log_path: Option<&Path>,
    log_start_offset: u64,
    home: &Path,
) -> ProbeOutcome {
    let budget = Duration::from_millis(gate.timeout_ms);
    let started = Instant::now();
    // Review round 1 (A2): the declared budget is a HARD wall, so ONE
    // deadline is computed up front and handed to every attempt — each
    // socket op is capped at min(time-to-deadline, HTTP_ATTEMPT_CAP) and
    // the deadline is re-checked inside the read loop. The old shape handed
    // each attempt a fresh `min(remaining, 1s)` and checked the deadline
    // only BETWEEN attempts, so a dribbling peer could reset its read
    // timer per read and hold one attempt far past the budget.
    let deadline = started + budget;
    // Review round 1 (E5): poll errors are tolerated as alive (the survival
    // watch's posture) — COUNTED here so the timeout can say honestly that
    // the death-check was degraded during the wait.
    let mut poll_errors: u64 = 0;
    loop {
        // (1) LIVENESS FIRST. An exit during the probe is the crash path, not
        // a probe failure; a poll ERROR is tolerated as alive (the same
        // posture as the startup survival watch — the reaper surfaces a
        // persistently un-pollable handle later) and counted for the
        // timeout's honesty note (E5).
        match backend.poll(handle) {
            Ok(ProcessStatus::Exited { code }) => return ProbeOutcome::ProcessExited(code),
            Ok(ProcessStatus::Alive) => {}
            Err(_) => poll_errors += 1,
        }
        // (2) THE PROBE. One per-attempt evaluation; any per-attempt failure
        // is a plain not-ready. The http-get attempt gets the shared
        // deadline (A2), never a fresh per-attempt budget.
        let ready = match &gate.probe {
            ReadinessProbe::HttpGet(p) => http_ready(&p.url, p.expected_status, deadline),
            ReadinessProbe::FileFresh(p) => {
                let path = if Path::new(&p.path).is_absolute() {
                    std::path::PathBuf::from(&p.path)
                } else {
                    home.join(&p.path)
                };
                file_fresh(&path, p.max_age_ms)
            }
            ReadinessProbe::LogContains(p) => match log_path {
                // Capture off (or no recoverable log): the probe cannot ever
                // pass — it runs out its budget honestly and the timeout says
                // why (the rendered description + the capture-off note the
                // start path already emitted).
                Some(log) => log_contains(log, log_start_offset, &p.line),
                None => false,
            },
        };
        if ready {
            return ProbeOutcome::Ready {
                waited_ms: started.elapsed().as_millis() as u64,
            };
        }
        // (3) THE DEADLINE. The reported wait is the ACTUAL elapsed time.
        let elapsed = started.elapsed();
        if elapsed >= budget {
            return ProbeOutcome::TimedOut {
                description: describe(&gate.probe),
                waited_ms: elapsed.as_millis() as u64,
                poll_errors,
            };
        }
        std::thread::sleep(READINESS_PROBE_POLL.min(budget - elapsed));
    }
}

/// The rendered, operator-facing description of a probe — what was watched
/// and what counts as ready. One line, greppable, no timestamps (the event
/// log adds them). Used in the probe-wait diagnostic and the
/// `readiness-timeout` transition cause.
pub(crate) fn describe(probe: &ReadinessProbe) -> String {
    match probe {
        ReadinessProbe::HttpGet(p) => format!(
            "readiness probe http-get {} (expected {})",
            p.url, p.expected_status
        ),
        ReadinessProbe::FileFresh(p) => format!(
            "readiness probe file-fresh {} (fresh within {}ms)",
            p.path, p.max_age_ms
        ),
        ReadinessProbe::LogContains(p) => {
            format!("readiness probe log-contains \"{}\"", p.line)
        }
    }
}

/// Evaluate ONE http-get attempt: connect (bounded), send one HTTP/1.1 GET,
/// read the status line. ANY failure — resolve, connect, write, read, parse —
/// is `false` (not ready yet), never an error: a listener that is not up yet
/// is the normal pre-ready state.
///
/// `deadline` is the WHOLE probe wait's hard wall (review round 1, A2): every
/// socket operation's timeout is `min(time-to-deadline, HTTP_ATTEMPT_CAP)`
/// and the read loop re-checks the deadline, so one attempt can never hold
/// the wait past the budget — no matter how the peer dribbles bytes.
fn http_ready(url: &str, expected_status: u16, deadline: Instant) -> bool {
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
/// (review round 1, A4) rejects FRAGMENTS at validation, so the runner never
/// sees a `#...` suffix to strip or mis-send: this stays total anyway (an
/// unsplitable URL is a per-attempt `false`).
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
/// directly. No `to_socket_addrs` — no DNS in the engine's start path, and
/// no first-address-only ambiguity (a literal IP is one address). An IPv6
/// literal with a port takes the `[addr]:port` form. A hostname (e.g.
/// `localhost`) parses as None — a per-attempt not-ready, though validation
/// refuses it long before the runner runs.
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
/// let a not-yet-ready agent's churning work directory pass), and its
/// modification age (SystemTime deltas only) is within `max_age_ms`. A
/// missing file, a non-file, an unavailable mtime, or an un-readable path
/// is a plain `false`. A future mtime (clock skew, an agent that stamps
/// ahead) counts as age zero — freshly written, which is exactly what the
/// probe asserts.
fn file_fresh(path: &Path, max_age_ms: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    // B6 (review round 1): only a regular FILE proves readiness. A directory
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

/// Evaluate ONE log-contains attempt: does `needle` appear in the engine-
/// owned agent output log AFTER the spawn cursor? Reads incrementally from
/// `start_offset`, carrying the last `needle.len() - 1` bytes across chunk
/// boundaries so a needle split across two reads still matches. Any read
/// failure (no log — e.g. capture off, rotated away) is a plain `false`.
///
/// Review round 1 (G1): if the log SHRANK below the spawn cursor (truncated
/// or rotated under the same name while this Run waited), the cursor no
/// longer addresses any byte and a scan from it would scan nothing forever
/// — so the scan resets to 0. That rescan is HONEST and named: a rotation
/// that dropped history can now match a needle that existed only
/// PRE-truncation (a line from before the reset was never part of this
/// Run's guaranteed window). The alternative — keeping the dead cursor —
/// was a permanently unsatisfiable probe with no signal at all.
fn log_contains(path: &Path, start_offset: u64, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        // Unreachable via the manifest validation (a non-empty `line` is
        // required) — but the honest answer to "does an empty substring
        // appear" must not be a panic.
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let start_offset = match file.metadata().map(|m| m.len()) {
        Ok(len) if len < start_offset => 0,
        _ => start_offset,
    };
    if file.seek(SeekFrom::Start(start_offset)).is_err() {
        return false;
    }
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return false,
            Ok(n) => {
                let mut window = std::mem::take(&mut carry);
                window.extend_from_slice(&buf[..n]);
                if window.windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
                let keep = (needle.len() - 1).min(window.len());
                carry = window[window.len() - keep..].to_vec();
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    // -- describe ----------------------------------------------------------

    #[test]
    fn describe_renders_each_kind_one_line_greppable() {
        assert_eq!(
            describe(&ReadinessProbe::HttpGet(hekma_adapter_api::HttpGetProbe {
                url: "http://127.0.0.1:8080/health".to_string(),
                expected_status: 200,
            })),
            "readiness probe http-get http://127.0.0.1:8080/health (expected 200)"
        );
        assert_eq!(
            describe(&ReadinessProbe::FileFresh(
                hekma_adapter_api::FileFreshProbe {
                    path: "ready.marker".to_string(),
                    max_age_ms: 250,
                }
            )),
            "readiness probe file-fresh ready.marker (fresh within 250ms)"
        );
        assert_eq!(
            describe(&ReadinessProbe::LogContains(
                hekma_adapter_api::LogContainsProbe {
                    line: "listening on".to_string(),
                }
            )),
            "readiness probe log-contains \"listening on\""
        );
    }

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
        // None (validation refuses it long before the runner runs).
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
            "hekma-readiness-test-{}-{}",
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
        // keeps 1ms the smallest legal budget; the pure helper stays total).
        let mtime = std::time::SystemTime::now()
            .checked_sub(Duration::from_secs(10))
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
        // whenever its entries change, which must not read as a readiness
        // signal (`metadata().is_file()` gate).
        assert!(!file_fresh(&dir, 60_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- log-contains ----------------------------------------------------------

    #[test]
    fn log_contains_matches_within_and_across_chunk_boundaries() {
        let dir = std::env::temp_dir().join(format!(
            "hekma-readiness-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let log = dir.join("agent.log");
        // 4093 filler bytes so the needle straddles the 4096-byte read
        // boundary — the carry path.
        let mut body = vec![b'x'; 4093];
        body.extend_from_slice(b"READY");
        {
            let mut f = std::fs::File::create(&log).expect("create log");
            f.write_all(&body).expect("write log");
        }
        assert!(log_contains(&log, 0, "READY"));
        // Behind the cursor: the PRIOR Run's bytes stay invisible.
        assert!(!log_contains(&log, 4096, "READY"));
        // Absent needle never matches.
        assert!(!log_contains(&log, 0, "UNREADY"));
        // A missing log (capture off) is a plain false.
        assert!(!log_contains(&dir.join("absent.log"), 0, "READY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_contains_ignores_bytes_before_the_spawn_cursor() {
        let dir = std::env::temp_dir().join(format!(
            "hekma-readiness-cursor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let log = dir.join("agent.log");
        std::fs::write(&log, b"READY from a prior run\n").expect("write");
        // A cursor at the file END sees nothing new.
        let len = std::fs::metadata(&log).unwrap().len();
        assert!(!log_contains(&log, len, "READY"));
        // The same content IS found from 0.
        assert!(log_contains(&log, 0, "READY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_contains_rescans_from_zero_when_the_log_shrank_below_the_cursor() {
        // G1 (review round 1): a log TRUNCATED (or rotated under the same
        // name) below the spawn cursor leaves the cursor addressing no byte
        // — a scan from it would scan nothing forever. The runner resets to
        // 0 and finds the needle (the honest rescan; a needle that existed
        // only pre-truncation can now match — named on the fn, not silent).
        let dir = std::env::temp_dir().join(format!(
            "hekma-readiness-trunc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let log = dir.join("agent.log");
        // A long first Run, so the spawn cursor sits far into the file...
        std::fs::write(&log, vec![b'x'; 8192]).expect("write");
        let cursor = std::fs::metadata(&log).unwrap().len();
        assert!(cursor > 4096);
        // ...then a truncation (the rotate/reopen shape): the file is now
        // SHORTER than the cursor and carries a fresh needle.
        std::fs::write(&log, b"READY after truncate\n").expect("truncate + rewrite");
        // The dead cursor would never see it; the reset does.
        assert!(log_contains(&log, cursor, "READY"));
        // A cursor still INSIDE the file's length keeps its position (the
        // healthy path never rescans): this needle lives entirely before
        // offset 5, so a scan from there finds nothing.
        assert!(!log_contains(&log, 5, "READY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- run_probe (the start path's synchronous wait) ----------------------

    #[test]
    fn run_probe_times_out_within_its_declared_budget_on_an_unsatisfiable_probe() {
        // C3 (review round 1): the spec-named `run_probe` unit drill — it had
        // NO unit test of its own (only integration shapes exercised it).
        // An unsatisfiable probe (capture-off log-contains: no log path) with
        // a 250ms budget must return `TimedOut` promptly: the waited time is
        // the honest ~250ms, bounded well under a second (the A2 deadline
        // wall makes the declared budget literally true).
        let backend = crate::backends::current();
        let home = std::env::temp_dir().join(format!(
            "hekma-readiness-drill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).expect("drill home");
        // A REAL lingering handle: the liveness poll must see Alive so the
        // timeout (not the crash path) ends the wait.
        let spec = crate::ports::SpawnSpec {
            exec: hekma_conformance::fake_agent_bin()
                .to_string_lossy()
                .into_owned(),
            args: vec!["--never-ready".to_string(), "10000".to_string()],
            env: std::collections::BTreeMap::new(),
            working_dir: home.clone(),
            log_file: None,
            attributed_log_path: None,
            stderr_log_file: None,
            instance_name: "probe-drill".to_string(),
            pipe_stdin: false,
            pipe_stdout: false,
            detach: false,
            inherit_env: true,
            capture_mode: crate::ports::LogCaptureMode::Off,
        };
        let mut handle = backend.spawn(&spec).expect("spawn the drill agent");
        let gate = hekma_adapter_api::ReadinessGate {
            timeout_ms: 250,
            probe: ReadinessProbe::LogContains(hekma_adapter_api::LogContainsProbe {
                line: "never printed".to_string(),
            }),
        };
        let outcome = run_probe(&gate, &backend, &mut handle, None, 0, &home);
        match outcome {
            ProbeOutcome::TimedOut {
                description,
                waited_ms,
                poll_errors,
            } => {
                assert!(
                    description.contains("log-contains"),
                    "the timeout names the probe: {description}"
                );
                assert!(
                    (200..1000).contains(&waited_ms),
                    "the wait is the honest ~250ms budget, got {waited_ms}ms"
                );
                // A healthy handle never errored a poll — the honesty count
                // stays zero (the E5 note is appended only when degraded).
                assert_eq!(poll_errors, 0, "a healthy poll track has no errors");
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
        // Teardown: the bounded stop ladder (which escalates internally) —
        // never a lingering drill agent.
        let _ = backend.stop(&mut handle, Duration::from_millis(500));
        drop(handle);
        let _ = std::fs::remove_dir_all(&home);
    }
}
