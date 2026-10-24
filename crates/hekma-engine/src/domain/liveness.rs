//! The liveness-check evaluator (contract `1.1.0`, the unresponsive story).
//!
//! A manifest's optional `[liveness]` section declares ONE check
//! ([`LivenessCheck`]) the engine's reaper evaluates against the RUNNING
//! agent. This module is the evaluator: ONE per-attempt probe per reaper
//! tick, plus the PURE window decision that turns a streak of failing
//! attempts into the `running → failed` transition.
//!
//! The unifying semantic (the story's frozen core): **the declared signal
//! failing CONTINUOUSLY for `max_age_ms` fails the instance.** A transient
//! failure — a heartbeat that skipped a beat, an HTTP check that got one
//! refused connection — recovers silently (one diagnostic); only an
//! UNRESPONSIVE signal (a hung agent that neither writes its heartbeat nor
//! answers its health endpoint) crosses the window and fails the instance
//! under the ordinary Restart Policy. Two boundaries follow:
//! * **Paused instances are never probed.** A suspended (SIGSTOP'd, or
//!   Windows-pause-no-op) process cannot heartbeat; probing it would fail
//!   it for being paused. The reaper only evaluates instances whose
//!   persisted state is EXACTLY `running`.
//! * **The window lives in supervision, reset on restart.** Adopted
//!   instances re-derive their window fresh (the honest degradation: an
//!   adopted agent gets up to one extra window of grace, named in the
//!   docs), because the previous engine's failure streak is not observable.
//!
//! Deliberate boundaries:
//! * **The per-attempt evaluators are SHARED** with the `[readiness]` gate
//!   ([`super::probe_kinds`]; the fn-pointer aliases below are the non-fork
//!   pin). Only the orchestration differs: readiness polls to a conclusion
//!   inside one start wait; liveness takes ONE attempt per reaper tick.
//! * **The window decision is PURE.** [`window_decision`] maps
//!   (open-window, failing-now, max_age) to an outcome the reaper can act
//!   on without touching the clock — the unit tests pin every arm and the
//!   exact `elapsed >= max_age_ms` expiry boundary.
//! * **The HTTP attempt is capped inline.** The reaper passes
//!   `Instant::now() + attempt_cap(max_age_ms)` as the deadline — the shared
//!   1s cap bounded by the declared window itself — so ONE hung check
//!   stretches its tick by at most one attempt cap (never past the window it
//!   serves; a 250ms window's attempt is capped at 250ms). Only FAILING
//!   checks pay any of it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hekma_adapter_api::LivenessCheck;

use super::probe_kinds::{file_fresh, http_get_attempt, HTTP_ATTEMPT_CAP};

/// The non-fork pin (liveness side): liveness MUST evaluate the heartbeat
/// through the SHARED [`super::probe_kinds::file_fresh`], never a private
/// copy. This alias is what `probe_kinds`'s pointer-equality test pins — if
/// this module grew (or was mutated into) its own evaluator, that test
/// fails.
pub(crate) const FILE_FRESH: fn(&Path, u64) -> bool = file_fresh;

/// The non-fork pin (liveness side) for the HTTP attempt: liveness MUST
/// evaluate `http_get` through the SHARED
/// [`super::probe_kinds::http_get_attempt`], never a private copy. Pinned by
/// the same pointer-equality test.
pub(crate) const HTTP_GET_ATTEMPT: fn(&str, u16, Instant) -> bool = http_get_attempt;

/// How ONE per-attempt liveness evaluation landed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LivenessSignal {
    /// The signal passed this attempt (fresh heartbeat / expected status).
    Fresh,
    /// The signal failed this attempt (stale or missing heartbeat, refused
    /// connection, wrong status). One failing attempt opens — or continues —
    /// the failure window; it is NOT itself a failure of the instance.
    Failing,
}

/// The per-attempt HTTP deadline for a check whose window is `max_age_ms`
/// (review round 1, P5b): the shared [`HTTP_ATTEMPT_CAP`] bounded by the
/// window itself. ONE attempt can never outlive the window it serves — under
/// the old `now + HTTP_ATTEMPT_CAP` shape, a 250ms window's attempt could
/// legally hold for a full second, running the tick PAST the moment the
/// window should have expired. Pure (no clock reads), so the unit tests pin
/// both ends.
pub(crate) fn attempt_cap(max_age_ms: u64) -> Duration {
    HTTP_ATTEMPT_CAP.min(Duration::from_millis(max_age_ms))
}

/// Evaluate ONE liveness attempt against the running agent.
///
/// * `heartbeat_file` — the path, resolved against `home` (the Agent Home)
///   when relative, then handed to the shared freshness evaluator.
/// * `http_get` — ONE bounded attempt: the deadline is
///   `Instant::now() + attempt_cap(max_age_ms)` — the shared cap bounded by
///   the declared window (see [`attempt_cap`]), so the reaper's tick
///   stretches by at most one attempt cap and never past the window (the
///   window is the reaper's patience, not a per-attempt budget).
pub(crate) fn evaluate(check: &LivenessCheck, home: &Path) -> LivenessSignal {
    match check {
        LivenessCheck::HeartbeatFile(h) => {
            let path = if Path::new(&h.path).is_absolute() {
                PathBuf::from(&h.path)
            } else {
                home.join(&h.path)
            };
            if FILE_FRESH(&path, h.max_age_ms) {
                LivenessSignal::Fresh
            } else {
                LivenessSignal::Failing
            }
        }
        LivenessCheck::HttpGet(h) => {
            let deadline = Instant::now() + attempt_cap(h.max_age_ms);
            if HTTP_GET_ATTEMPT(&h.url, h.expected_status, deadline) {
                LivenessSignal::Fresh
            } else {
                LivenessSignal::Failing
            }
        }
    }
}

/// The rendered, operator-facing description of a liveness check — what is
/// watched and what counts as alive. One line, greppable, no timestamps
/// (the event log adds them). Used in the unresponsive transition cause and
/// the diagnostics the reaper emits around the window.
///
/// Both the display name (`heartbeat-file` / `http-get`) and the WIRE TAG
/// (`heartbeat_file` / `http_get` — the serde tag a `--json` consumer sees)
/// appear, so an operator can grep the line and a script can match the exact
/// wire kind off the same string (review round 1, D3).
pub(crate) fn describe(check: &LivenessCheck) -> String {
    match check {
        LivenessCheck::HeartbeatFile(h) => format!(
            "liveness check heartbeat-file (heartbeat_file) {} (stale after {}ms)",
            h.path, h.max_age_ms
        ),
        LivenessCheck::HttpGet(h) => format!(
            "liveness check http-get (http_get) {} (expected {})",
            h.url, h.expected_status
        ),
    }
}

/// The outcome of applying the unifying semantic to ONE reaper tick's
/// observation. Pure: `(failing_since, failing_now, max_age_ms)` → outcome —
/// no clock reads, so the unit tests pin every arm deterministically.
///
/// `failing_since` is the OPEN failure window (how long the signal has been
/// failing continuously, `None` when the last attempt passed); `failing_now`
/// is this tick's attempt result.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WindowDecision {
    /// The signal passes and no window is open — the steady healthy state.
    /// The reaper does nothing.
    Healthy,
    /// The signal fails and the window just OPENED — the reaper stamps
    /// `failing_since = now` and emits its ONE first-failure diagnostic.
    StartWindow,
    /// The signal fails, a window is open, and it has NOT yet reached
    /// `max_age_ms` — the reaper stays silent (a transient blip inside the
    /// window is none of the operator's business).
    WithinWindow {
        /// How long the signal has been failing continuously.
        failing_for_ms: u64,
    },
    /// The signal PASSES with a window open — the agent recovered. The
    /// reaper clears the window and emits its ONE recovery diagnostic (a
    /// failing agent that came back is exactly what an operator wants to
    /// know about — surfacing it, not silencing it).
    Recovered,
    /// The signal fails and the open window has reached `max_age_ms`
    /// (`elapsed >= max_age_ms`, the exact boundary): the instance is
    /// UNRESPONSIVE. `failing_for_ms` counts from the FIRST OBSERVED
    /// failure (the tick the engine saw the signal down), not from whenever
    /// the signal actually began failing — the engine cannot see the past,
    /// so this is a lower bound on true staleness (a heartbeat file may have
    /// gone stale long before the first failing tick; the reaper names that
    /// gap in its expiry detail when it can). The reaper drives the
    /// `running → failed` transition (cause `unresponsive`) and the Restart
    /// Policy applies.
    Expired {
        /// How long the signal had been failing when the window expired,
        /// measured since first OBSERVED failure.
        failing_for_ms: u64,
    },
}

/// The pure window decision (see [`WindowDecision`]).
pub(crate) fn window_decision(
    failing_since: Option<Duration>,
    failing_now: bool,
    max_age_ms: u64,
) -> WindowDecision {
    match (failing_since, failing_now) {
        (None, false) => WindowDecision::Healthy,
        (None, true) => WindowDecision::StartWindow,
        (Some(_), false) => WindowDecision::Recovered,
        (Some(elapsed), true) => {
            let failing_for_ms = elapsed.as_millis() as u64;
            if failing_for_ms >= max_age_ms {
                WindowDecision::Expired { failing_for_ms }
            } else {
                WindowDecision::WithinWindow { failing_for_ms }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hekma_adapter_api::{HeartbeatFileCheck, HttpGetCheck};

    // -- describe ----------------------------------------------------------

    #[test]
    fn describe_renders_each_kind_one_line_greppable() {
        // D3 (review round 1): BOTH the display name and the WIRE TAG appear,
        // so an operator greps the line and a script matches the wire kind
        // off the same string.
        assert_eq!(
            describe(&LivenessCheck::HeartbeatFile(HeartbeatFileCheck {
                path: "heartbeat.marker".to_string(),
                max_age_ms: 5000,
            })),
            "liveness check heartbeat-file (heartbeat_file) heartbeat.marker (stale after 5000ms)"
        );
        assert_eq!(
            describe(&LivenessCheck::HttpGet(HttpGetCheck {
                url: "http://127.0.0.1:8080/health".to_string(),
                expected_status: 200,
                max_age_ms: 5000,
            })),
            "liveness check http-get (http_get) http://127.0.0.1:8080/health (expected 200)"
        );
    }

    // -- the per-attempt HTTP cap (P5b) --------------------------------------

    #[test]
    fn attempt_cap_is_the_shared_cap_bounded_by_the_window() {
        // The two ends, pinned: a floor-sized window (250 ms — the manifest
        // validation minimum) caps the attempt AT the window (an attempt can
        // never outlive the window it serves), and a generous window caps at
        // the shared 1s HTTP_ATTEMPT_CAP. Pure — no clock reads.
        assert_eq!(attempt_cap(250), Duration::from_millis(250));
        assert_eq!(attempt_cap(60_000), Duration::from_secs(1));
        // Exactly at the cap boundary, both orderings agree.
        assert_eq!(attempt_cap(1_000), Duration::from_secs(1));
        assert_eq!(attempt_cap(1_001), Duration::from_secs(1));
    }

    // -- the pure window decision -------------------------------------------

    #[test]
    fn window_decision_covers_every_arm() {
        // The five arms, pinned directly: healthy, window-open, transient
        // blip inside the window, recovery, and expiry.
        assert_eq!(window_decision(None, false, 1000), WindowDecision::Healthy);
        assert_eq!(
            window_decision(None, true, 1000),
            WindowDecision::StartWindow
        );
        assert_eq!(
            window_decision(Some(Duration::from_millis(999)), true, 1000),
            WindowDecision::WithinWindow {
                failing_for_ms: 999
            }
        );
        assert_eq!(
            window_decision(Some(Duration::from_millis(400)), false, 1000),
            WindowDecision::Recovered
        );
        assert_eq!(
            window_decision(Some(Duration::from_millis(1500)), true, 1000),
            WindowDecision::Expired {
                failing_for_ms: 1500
            }
        );
    }

    #[test]
    fn window_expiry_boundary_is_exact_max_age() {
        // The boundary pin: `elapsed >= max_age_ms` expires — AT exactly the
        // window (not one tick after), and one millisecond BEFORE it the
        // blip is still inside the window. Off-by-one here would either
        // fail an agent a millisecond early or grace a hung one a
        // millisecond long.
        assert_eq!(
            window_decision(Some(Duration::from_millis(1000)), true, 1000),
            WindowDecision::Expired {
                failing_for_ms: 1000
            }
        );
        assert_eq!(
            window_decision(Some(Duration::from_millis(999)), true, 1000),
            WindowDecision::WithinWindow {
                failing_for_ms: 999
            }
        );
    }

    // -- evaluate ------------------------------------------------------------

    #[test]
    fn evaluate_heartbeat_resolves_relative_paths_against_home() {
        // A fresh heartbeat in the Agent Home is Fresh; an absent one is
        // Failing (absent ≡ stale — an agent that never wrote its heartbeat
        // has never proven liveness).
        let home = std::env::temp_dir().join(format!(
            "hekma-liveness-eval-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).expect("temp home");
        let check = LivenessCheck::HeartbeatFile(HeartbeatFileCheck {
            path: "heartbeat.marker".to_string(),
            max_age_ms: 60_000,
        });
        assert_eq!(evaluate(&check, &home), LivenessSignal::Failing);
        std::fs::write(home.join("heartbeat.marker"), b"beat").expect("write heartbeat");
        assert_eq!(evaluate(&check, &home), LivenessSignal::Fresh);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn evaluate_http_get_reports_a_dead_listener_as_failing() {
        // No listener on a literal loopback port: the attempt is a plain
        // Failing (connect refused), never an error — the reaper's window,
        // not a panic, is what consumes repeated failures.
        let check = LivenessCheck::HttpGet(HttpGetCheck {
            url: "http://127.0.0.1:1/health".to_string(),
            expected_status: 200,
            max_age_ms: 60_000,
        });
        assert_eq!(evaluate(&check, Path::new("/")), LivenessSignal::Failing);
    }
}
