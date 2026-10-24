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
//! * **The per-attempt evaluators are SHARED.** `http-get` and `file-fresh`
//!   per-attempt evaluation lives in [`super::probe_kinds`], the exact code
//!   the `[liveness]` check evaluates with (fn-pointer aliases below are the
//!   non-fork pin). Only the orchestration is readiness-specific: this
//!   module's poll-to-conclusion loop, the log-contains evaluator (no
//!   liveness kind reads logs), and the engine-owned cadence.
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
//!   the `http-get` URL, so the shared authority parser reads an
//!   [`std::net::IpAddr`] directly — no `to_socket_addrs`, no resolver in
//!   the start path (which also removes the first-address-only ambiguity:
//!   a literal IP is exactly one address).
//! * **Graceful degradation.** Every per-attempt failure (connect refused,
//!   file missing, log unreadable) is a plain "not ready yet" — retried on
//!   the cadence, never an error that aborts the start outside the typed
//!   outcomes.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

use hekma_adapter_api::ReadinessProbe;

use crate::ports::{ProcessBackend, ProcessStatus};

use super::probe_kinds::{file_fresh, http_get_attempt};

/// The non-fork pin (readiness side): readiness MUST evaluate the file-fresh
/// attempt through the SHARED [`super::probe_kinds::file_fresh`], never a
/// private copy. This alias is what `probe_kinds`'s pointer-equality test
/// pins — if this module grows (or is mutated into) its own evaluator, that
/// test fails.
pub(crate) const FILE_FRESH: fn(&Path, u64) -> bool = file_fresh;

/// The non-fork pin (readiness side) for the HTTP attempt: readiness MUST
/// evaluate `http-get` through the SHARED
/// [`super::probe_kinds::http_get_attempt`], never a private copy. Pinned by
/// the same pointer-equality test.
pub(crate) const HTTP_GET_ATTEMPT: fn(&str, u16, Instant) -> bool = http_get_attempt;

/// How often the probe wait re-evaluates the declared probe. The budget is
/// the manifest's `timeout_ms`; THIS interval is engine-owned. ~100ms is
/// snappy enough that a fast probe does not visibly delay `start`, cheap
/// enough that a filesystem/stat poll is negligible, and it bounds the
/// timeout overshoot at (roughly) one interval.
pub(crate) const READINESS_PROBE_POLL: Duration = Duration::from_millis(100);

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
            ReadinessProbe::HttpGet(p) => HTTP_GET_ATTEMPT(&p.url, p.expected_status, deadline),
            ReadinessProbe::FileFresh(p) => {
                let path = if Path::new(&p.path).is_absolute() {
                    std::path::PathBuf::from(&p.path)
                } else {
                    home.join(&p.path)
                };
                FILE_FRESH(&path, p.max_age_ms)
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

// The per-attempt evaluators (`http_get_attempt`, `file_fresh`) and the URL
// plumbing (`split_http_url`, `parse_authority`, `parse_status_line`) live in
// [`super::probe_kinds`] — SHARED with the `[liveness]` check so the two
// declared polls cannot drift apart (the aliases above are the non-fork pin).
// `log_contains` stays readiness-only: no liveness kind reads logs.

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

    // -- http url splitting, status parse, file-fresh -----------------------
    // (moved to `probe_kinds` with the shared evaluators they pin)

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
