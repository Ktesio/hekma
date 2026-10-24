//! The DRAIN composite (story 18-4): `stop new work → wait, bounded → stop`.
//!
//! [`Supervisor::drain`] is the engine-side implementation of the `drain`
//! verb: it runs the adapter's declared `[lifecycle.pause]` COMMAND (the
//! cooperative pause — never an OS signal, OS-independent by construction),
//! waits — BOUNDED — for the adapter's optional declared idle signal to beat,
//! then drives the ordinary stop ladder. The bound is the three-tier rule:
//! the caller's override, else the manifest `[drain].timeout_ms`, else
//! [`super::DRAIN_DEFAULT_BOUND`]. A drain that outlives its bound STILL
//! stops (the last step is unconditional); the outcome is recorded honestly
//! (`completed: false`) on the terminal `DrainStopped` cause.
//!
//! Race-free BY CONSTRUCTION: one `Instant` deadline is fixed at the drain's
//! start and every phase (the pause command's bound, the wait poll) measures
//! against that deadline — nothing compares a measured elapsed against a flat
//! value twice, so slow machines stretch the wait but never slip past the
//! contract. All duration arithmetic is saturating (`checked_add` /
//! `saturating_duration_since`); a bound that cannot be represented ends
//! immediately rather than panicking.
//!
//! No OS-specific code anywhere in this module: the command runner is
//! `std::process` (spawn, poll, kill — the same portable surface the
//! contract's launch uses), the idle probe is the SHARED
//! [`super::super::probe_kinds::file_fresh`] evaluator the readiness/liveness
//! probes use, and the stop is the ordinary ladder. The signal path
//! (`suspend_or_resume`'s level dispatch) is simply never consulted here.

use super::*;
use crate::adapter::StartLaunch;
use std::time::Instant;

/// The DRAIN composite's durable outcome (story 18-4) — what actually
/// happened inside the bounded wait. `completed` is TRUE when the drain
/// observed its declared wait (or had none to do: the degenerate
/// no-command-no-signal drain) before its bound; FALSE when the bound burned
/// first (the drain STILL stops — an honest `false`, never a fabricated
/// success). `waited_ms` is the measured wall clock of the WHOLE bounded
/// phase (the pause command included), as-spent — the honest accounting the
/// terminal `DrainStopped` cause and the CLI surface carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrainOutcome {
    /// Whether the bounded wait COMPLETED before the bound (or no wait was
    /// declared).
    pub completed: bool,
    /// The measured wall clock of the whole bounded phase, in milliseconds.
    pub waited_ms: u64,
}

/// Why a bounded lifecycle command ended (story 18-4). `Failed` carries the
/// honest why (nonzero exit with its status, spawn failure with the OS
/// reason, wait error) — the caller turns it into the typed
/// [`EngineError::PauseCommandFailed`] rejection with state untouched. An
/// overrun KILLS the child first (the bound is real) and reports `Overran`;
/// for the STANDALONE pause that rejects the pause, and inside a drain that
/// IS the timed-out path (the drain still stops).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BoundedCommandOutcome {
    /// The command spawned, ran, and exited 0.
    Exited0,
    /// The command could not run to a zero exit: spawn failure, nonzero
    /// exit, or a wait error — `detail` names which and why.
    Failed { detail: String },
    /// The command outlived its bound; it was KILLED (and reaped) — the
    /// bound is real, never advisory.
    Overran,
}

/// Render a [`StartLaunch`] as the human-readable command line the causes
/// and errors record: exec + args joined with single spaces. The
/// environment is deliberately NOT rendered (NFR-6 no-leak: env values are
/// exactly where secrets land).
pub(super) fn render_command(launch: &StartLaunch) -> String {
    let mut rendered = launch.exec.clone();
    for arg in &launch.args {
        rendered.push(' ');
        rendered.push_str(arg);
    }
    rendered
}

/// Run ONE bounded lifecycle command (story 18-4): spawn `launch` with the
/// Agent `home` as its working directory, its declared envs overlaid on the
/// inherited environment (the same overlay rule the start launch uses),
/// stdio discarded (the command's output is the adapter's business — the
/// side effects on disk are the contract), and poll `try_wait` on `poll`
/// cadence against the `bound`. On overrun the child is KILLED and reaped
/// before returning [`BoundedCommandOutcome::Overran`] — a timed-out
/// command never survives its bound.
///
/// Portable by construction (`std::process` only — no libc, no job objects),
/// which is exactly the cooperative model's point: the manifest's own
/// command does the suspending, on every OS the same way.
pub(super) fn run_bounded_command(
    home: &Path,
    launch: &StartLaunch,
    bound: Duration,
    poll: Duration,
) -> BoundedCommandOutcome {
    // The Agent Home normally exists by now (the start path created it); the
    // idempotent create is defensive only — a missing home would otherwise
    // fail the spawn with a confusing cwd error.
    let _ = std::fs::create_dir_all(home);
    let mut command = std::process::Command::new(&launch.exec);
    command
        .args(&launch.args)
        .current_dir(home)
        .envs(&launch.env)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(source) => {
            return BoundedCommandOutcome::Failed {
                detail: format!(
                    "could not spawn '{}': {source} — check the exec path and its permissions",
                    launch.exec
                ),
            }
        }
    };
    // Saturating deadline (race-free by construction: ONE Instant, fixed
    // before the loop — never a measured-vs-flat comparison). A bound that
    // cannot be represented ends immediately: the deadline IS now.
    let deadline = Instant::now().checked_add(bound);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return BoundedCommandOutcome::Exited0;
                }
                return BoundedCommandOutcome::Failed {
                    detail: format!("exited {status} — the lifecycle change was not performed"),
                };
            }
            Ok(None) => {
                let expired = match deadline {
                    Some(deadline) => Instant::now() >= deadline,
                    None => true,
                };
                if expired {
                    // The bound is real: kill and reap before reporting.
                    let _ = child.kill();
                    let _ = child.wait();
                    return BoundedCommandOutcome::Overran;
                }
                std::thread::sleep(poll);
            }
            Err(source) => {
                // The wait itself failed (the child is gone in a way we
                // cannot read). Kill-and-reap is best-effort; report
                // honestly — this is NOT a zero exit.
                let _ = child.kill();
                let _ = child.wait();
                return BoundedCommandOutcome::Failed {
                    detail: format!("waiting on the command failed: {source}"),
                };
            }
        }
    }
}

/// The bounded phase's decision (story 18-4) — PURE, so the deadline
/// arithmetic is unit-pinned without staging a real drain. One arm table:
///
/// * the idle signal beat AND the pause command finished →
///   [`DrainDecision::Completed`] (the drain's wait is satisfied);
/// * the deadline passed → [`DrainDecision::TimedOut`] (the command phase's
///   overrun lands here too — a still-running command at the deadline is a
///   timeout, which is why `pause_done` gates only the COMPLETED arm);
/// * anything else → [`DrainDecision::Continue`] (keep polling).
pub(super) enum DrainDecision {
    /// The wait is satisfied — proceed to the stop.
    Completed,
    /// The bound burned first — stop anyway, honestly (`completed: false`).
    TimedOut,
    /// Keep polling (bounded by the same deadline).
    Continue,
}

pub(super) fn drain_decision(
    now: Instant,
    deadline: Instant,
    pause_done: bool,
    idle_fresh: bool,
) -> DrainDecision {
    if pause_done && idle_fresh {
        return DrainDecision::Completed;
    }
    if now >= deadline {
        return DrainDecision::TimedOut;
    }
    DrainDecision::Continue
}

impl Supervisor {
    /// The DRAIN composite (story 18-4): pause (command) → wait (bounded) →
    /// stop. Returns the stopped instance plus the composite's durable
    /// [`DrainOutcome`].
    ///
    /// Order (every fallible step precedes any state change):
    /// 1. the RUNNING-ONLY gate ([`EngineError::DrainRefused`]) — before the
    ///    bound validation and the plan resolution (the transition gate
    ///    before any side effect);
    /// 2. the bound override's validation ([`EngineError::DrainBoundInvalid`]
    ///    — zero, or past the adapter-contract ceiling);
    /// 3. the plan resolution (one manifest read, `resolve_drain_plan` — a
    ///    drifted pair/section refuses the drain);
    /// 4. the DEGENERATE check — no pause command AND no idle signal means
    ///    there is no declared wait at all: stop DIRECTLY, recording
    ///    `DrainStopped { completed: true, waited_ms: 0 }` with a diagnostic
    ///    saying so (surfaced, not silent — AI-18);
    /// 5. step (a): the pause COMMAND under the drain's own bound (it shares
    ///    the deadline — the three-tier bound governs the command too). A
    ///    failed command (nonzero / spawn failure) rejects the drain with the
    ///    state UNTOUCHED ([`EngineError::PauseCommandFailed`] — never a fake
    ///    pause); an OVERRUN command is killed and IS the timed-out path.
    /// 6. step (b): the bounded wait — the shared
    ///    [`super::super::probe_kinds::file_fresh`] evaluator polls the
    ///    declared idle signal (agent-home-relative; freshness with a small
    ///    engine-owned slack, absent ≡ stale) on
    ///    [`super::DRAIN_WAIT_POLL`] cadence against the single deadline.
    /// 7. step (c): the ordinary stop ladder —
    ///    `stop_inner(cause_override = command("drain"),
    ///    terminal_drain = Some(outcome))` — so the terminal edge carries
    ///    [`TransitionCause::DrainStopped`] (graceful) or the outcome folded
    ///    into `StopForced`'s detail (forced honesty outranks; the outcome is
    ///    durable either way). A timed-out drain STILL stops.
    ///
    /// The pause's transition (step a) persists FIRST (AI-9) with the
    /// [`TransitionCause::PauseCooperative`] cause naming the rendered
    /// command; the in-memory cooperative flag is set only AFTER the commit.
    /// If the commit itself fails after the command ran, the divergence is
    /// SURFACED (one diagnostic) and the error returned — the same honesty
    /// the signal path's post-commit breadcrumb carries.
    pub(crate) fn drain(
        &mut self,
        registry: &Registry,
        name: &str,
        timeout_override: Option<Duration>,
    ) -> Result<(AgentInstance, DrainOutcome), EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = registry.lookup(&name).map_err(registry_to_engine)?;

        // (1) The RUNNING-ONLY gate, before the bound validation and the plan
        // resolution (the transition gate before any side effect).
        if instance.state != LifecycleState::Running {
            return Err(EngineError::DrainRefused {
                name: name.as_str().to_string(),
                state: instance.state.as_str().to_string(),
            });
        }

        // (2) The bound override's validation: `1..=600_000ms` (the same
        // ceiling the adapter contract pins for the manifest tier). The CLI
        // validates too — this is the engine-side defense of the same rule.
        if let Some(bound) = timeout_override {
            let ms = bound.as_millis();
            if ms == 0 || ms > 600_000 {
                let bound_ms = u64::try_from(ms).unwrap_or(u64::MAX);
                return Err(EngineError::DrainBoundInvalid { bound_ms });
            }
        }

        // (3) Resolve the plan (one manifest read per call — the 18-3 D1
        // defer stands). A drifted pair/section refuses the drain before any
        // state change.
        let (kind, manifest_path, _) = registry
            .adapter_launch_facts(&name)
            .map_err(registry_to_engine)?;
        let plan = crate::adapter::resolve_drain_plan(&kind, manifest_path.as_deref())
            .map_err(|e| launch_to_engine(&name, e))?;

        // The three-tier bound: override → section.timeout_ms (REQUIRED when
        // the section exists) → the engine default. The default tier is only
        // reachable with a declared pause command and no section — the
        // no-command-no-signal shape is the degenerate drain below, which
        // needs no bound at all.
        let bound = timeout_override.unwrap_or_else(|| {
            plan.drain
                .as_ref()
                .map(|spec| Duration::from_millis(spec.timeout_ms))
                .unwrap_or(DRAIN_DEFAULT_BOUND)
        });

        // The idle signal, resolved to the Agent Home (its paths are
        // home-RELATIVE by contract).
        let idle_file = plan
            .drain
            .as_ref()
            .and_then(|spec| spec.idle_file.as_ref())
            .map(|rel| registry.agent_home(&name).join(rel));

        // (4) The DEGENERATE drain: nothing to run, nothing to wait for —
        // there is no declared wait at all. Stop DIRECTLY and say so
        // (surfaced, not silent); the outcome is honestly "completed, zero
        // wait", because there was nothing to complete.
        if plan.pause_command.is_none() && idle_file.is_none() {
            self.emit_diagnostic(&format!(
                "{}: drain has no declared wait — no `[lifecycle.pause]` command and no \
                 `[drain].idle_file` — so there is nothing to pause or wait for; stopping \
                 directly",
                name.as_str(),
            ));
            let stopped = self.stop_inner(
                registry,
                name.as_str(),
                None,
                Some(TransitionCause::command("drain")),
                Some((true, 0)),
            )?;
            return Ok((
                stopped,
                DrainOutcome {
                    completed: true,
                    waited_ms: 0,
                },
            ));
        }

        // The SINGLE deadline (race-free by construction): fixed once, here,
        // governing the pause command AND the wait alike. Saturating: a
        // bound that cannot be represented ends immediately.
        let started = Instant::now();
        let deadline = started.checked_add(bound).unwrap_or(started);

        let mut timed_out = false;

        // (5) Step (a): the pause COMMAND, under the drain's own bound (the
        // command shares the deadline — the three-tier bound governs it too).
        // Persist FIRST (AI-9), then the flag; a failed command rejects with
        // the state untouched.
        if let Some(launch) = &plan.pause_command {
            let rendered = render_command(launch);
            let remaining = deadline.saturating_duration_since(Instant::now());
            let outcome = run_bounded_command(
                &registry.agent_home(&name),
                launch,
                remaining,
                BOUNDED_COMMAND_POLL,
            );
            match outcome {
                BoundedCommandOutcome::Exited0 => {}
                BoundedCommandOutcome::Failed { detail } => {
                    // NEVER a fake pause: the command spoke, the drain is
                    // rejected, the durable state is untouched.
                    return Err(EngineError::PauseCommandFailed {
                        name: name.as_str().to_string(),
                        direction: "pause".to_string(),
                        command: rendered,
                        detail,
                    });
                }
                BoundedCommandOutcome::Overran => {
                    // The command outlived the drain's own bound: killed,
                    // and this IS the timed-out path — the drain still
                    // stops (the last step is unconditional), recording
                    // `completed: false`. Not a rejection: the bound, not
                    // the adapter, ended the wait.
                    timed_out = true;
                }
            }
            // The command RAN — commit the cooperative pause (persist first,
            // AI-9), with the rendered command recorded on the cause. A
            // commit failure AFTER the command ran is a surfaced divergence
            // (the agent was told to pause; the ledger refused) — the same
            // post-commit honesty the signal path carries.
            self.ensure_log_dir(registry, &name)?;
            if let Err(err) = self.transition(
                registry,
                &name,
                LifecycleState::Running,
                LifecycleState::Paused,
                TransitionCause::pause_cooperative(rendered),
            ) {
                self.emit_diagnostic(&format!(
                    "{}: the drain's pause command already ran, but recording the \
                     running→paused transition failed ({err}) — the ledger and the agent \
                     may diverge; the drain proceeds to the stop either way",
                    name.as_str(),
                ));
                return Err(err);
            }
            if let Some(supervised) = self.running.get_mut(&name) {
                supervised.cooperatively_paused = true;
            }
        }

        // (6) Step (b): the bounded wait — poll the idle signal (freshness,
        // small engine-owned slack; absent ≡ stale) against the SAME
        // deadline. No idle signal declared (command-only drain): the wait
        // is already satisfied — the command's zero exit was the wait.
        if !timed_out {
            if let Some(idle_path) = &idle_file {
                let slack_ms = u64::try_from(DRAIN_IDLE_FRESH_SLACK.as_millis())
                    .unwrap_or(DRAIN_IDLE_FRESH_SLACK.as_millis() as u64);
                loop {
                    match drain_decision(
                        Instant::now(),
                        deadline,
                        true,
                        crate::domain::probe_kinds::file_fresh(idle_path, slack_ms),
                    ) {
                        DrainDecision::Completed => break,
                        DrainDecision::TimedOut => {
                            timed_out = true;
                            break;
                        }
                        DrainDecision::Continue => std::thread::sleep(DRAIN_WAIT_POLL),
                    }
                }
            }
        }

        // The measured wall clock of the WHOLE bounded phase (the pause
        // command included) — saturating, as-spent, honest.
        let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let outcome = DrainOutcome {
            completed: !timed_out,
            waited_ms,
        };

        // (7) Step (c): the ordinary stop ladder. The `drain` command cause
        // names the act on the (paused|running) → stopping edge; the
        // terminal edge carries the DURABLE drain outcome — `DrainStopped`
        // when the ladder finished gracefully, the outcome folded into
        // `StopForced`'s detail when escalation fired (forced honesty
        // outranks, but the outcome still lands durably). A timed-out drain
        // STILL stops.
        let stopped = self.stop_inner(
            registry,
            name.as_str(),
            None,
            Some(TransitionCause::command("drain")),
            Some((outcome.completed, outcome.waited_ms)),
        )?;
        Ok((stopped, outcome))
    }
}

#[cfg(test)]
mod tests {
    //! The PURE pieces of the drain composite, unit-pinned without staging a
    //! real drain (story 18-4): the deadline arithmetic
    //! ([`drain_decision`]), the rendered-command surface the causes record
    //! ([`render_command`]), and the pinned constants the three-tier bound
    //! and the wait cadence hang off.

    use super::*;
    use std::time::Duration;

    // -- The drain_decision matrix (race-free by construction: ONE deadline) --

    #[test]
    fn drain_decision_completes_only_when_pause_done_and_idle_fresh() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(1_000);
        // Both conditions hold before the deadline: the wait is satisfied.
        assert!(matches!(
            drain_decision(now, deadline, true, true),
            DrainDecision::Completed
        ));
        // A missing condition keeps polling, whatever the other does.
        assert!(matches!(
            drain_decision(now, deadline, false, true),
            DrainDecision::Continue
        ));
        assert!(matches!(
            drain_decision(now, deadline, true, false),
            DrainDecision::Continue
        ));
        assert!(matches!(
            drain_decision(now, deadline, false, false),
            DrainDecision::Continue
        ));
    }

    #[test]
    fn drain_decision_times_out_at_the_deadline_unless_already_complete() {
        let now = Instant::now();
        let deadline = now; // the deadline IS now
                            // The COMPLETED arm is checked first: both conditions at the deadline
                            // is still a completion (the poll that noticed the last condition may
                            // land exactly at the deadline — it must not be reported as a
                            // timeout).
        assert!(matches!(
            drain_decision(now, deadline, true, true),
            DrainDecision::Completed
        ));
        // Any unmet condition at the deadline is a timeout — the bound is
        // real, and the caller still stops (honestly, `completed: false`).
        assert!(matches!(
            drain_decision(now, deadline, true, false),
            DrainDecision::TimedOut
        ));
        assert!(matches!(
            drain_decision(now, deadline, false, true),
            DrainDecision::TimedOut
        ));
        assert!(matches!(
            drain_decision(now, deadline, false, false),
            DrainDecision::TimedOut
        ));
        // PAST the deadline: the same arms (saturating clocks never rewind).
        let past = now + Duration::from_millis(5);
        assert!(matches!(
            drain_decision(past, deadline, false, false),
            DrainDecision::TimedOut
        ));
        assert!(matches!(
            drain_decision(past, deadline, true, true),
            DrainDecision::Completed
        ));
    }

    // -- render_command: exec + args, env NEVER rendered (NFR-6 no-leak) --

    #[test]
    fn render_command_joins_exec_and_args_and_never_the_env() {
        let launch = StartLaunch {
            exec: "fake-agent".to_string(),
            args: vec!["--touch".to_string(), "pause.sentinel".to_string()],
            env: std::collections::BTreeMap::from([(
                "SECRET_TOKEN".to_string(),
                "super-secret-value".to_string(),
            )]),
        };
        assert_eq!(
            render_command(&launch),
            "fake-agent --touch pause.sentinel",
            "exec + args, single-space joined"
        );
        let rendered = render_command(&launch);
        assert!(
            !rendered.contains("SECRET_TOKEN") && !rendered.contains("super-secret-value"),
            "the environment is NEVER rendered (NFR-6 no-leak): {rendered}"
        );
        // No args: the bare exec.
        let bare = StartLaunch {
            exec: "only".to_string(),
            args: vec![],
            env: std::collections::BTreeMap::new(),
        };
        assert_eq!(render_command(&bare), "only");
    }

    // -- The pinned constants the composite's bounds hang off --

    #[test]
    fn the_bound_constants_keep_their_pinned_values() {
        // The STANDALONE pause-command cap: 30s (the spec's `PAUSE_COMMAND_CAP`).
        assert_eq!(PAUSE_COMMAND_CAP, Duration::from_secs(30));
        // The DEFAULT drain bound (the third tier): 30s.
        assert_eq!(DRAIN_DEFAULT_BOUND, Duration::from_secs(30));
        // The idle-freshness slack (engine-owned, above the poll cadence).
        assert_eq!(DRAIN_IDLE_FRESH_SLACK, Duration::from_millis(2_000));
        // The poll cadences: bounded latency, never part of any bound.
        assert_eq!(DRAIN_WAIT_POLL, Duration::from_millis(25));
        assert_eq!(BOUNDED_COMMAND_POLL, Duration::from_millis(20));
    }
}
