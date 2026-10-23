//! The shutdown drain (story 17-2, reworked by the review findings):
//! enumerate the Fleet, stop every LIVE agent — `Running` AND `Paused` —
//! through the engine's OWN bounded stop ladder, surface each per-instance
//! outcome, and never drop the engine before every stop has settled (the
//! `Drop` teardown — never before the window ends). A paused agent is
//! resumed-then-stopped by the same ladder (the engine's stop path resumes a
//! real suspension first, so the graceful signal can be delivered) — never
//! handed to the engine Drop's raw kill-on-drop teardown, which was the
//! "paused agents killed without a graceful stop" bug.
//!
//! The host owns the SIGNAL; the engine owns the STOP (AD-20): every stop is
//! `Engine::stop(name, Some(window))` — the same graceful-then-escalate
//! ladder every `kt agent stop` uses, with the host's `--stop-window`. The
//! host never signals a process itself.
//!
//! "Drain" here is the bounded stop ladder only; the AD-15 drain composite
//! (in-flight request draining) arrives with Epic 18 (16-4) and is
//! deliberately NOT this.
//!
//! Review findings, in this module:
//!
//! * **Finding 4 — the stops are ISSUED CONCURRENTLY**: one bounded stop per
//!   running agent, all issued, all joined (`JoinSet`) — the per-instance
//!   outcomes settle and surface as they finish, never in a fixed
//!   issue-order lockstep. The WALL CLOCK, honestly stated: the engine's
//!   stop ladder runs under its supervision lock (the ratified coarse
//!   two-mutex model — one lock held per whole ladder, measured and
//!   re-ratified by the serve-shape scenario), so instances that must run
//!   out their full graceful window before the force-kill escalation QUEUE
//!   behind each other — worst case Σ(window) over the fleet. Agents that
//!   exit promptly on the graceful signal hold the lock briefly and settle
//!   fast. Parallel ladders (≈ one window total) need the engine's
//!   stop-ladder locking to change — the archived per-instance lock-model
//!   plan, an architecture decision, not a host-layer change; this
//!   issue-all/join structure is the seam that becomes parallel the moment
//!   the engine's stops do.
//! * **Finding 5 — the plan is re-enumerated**: an instance that enters
//!   `Running` after the first snapshot (a concurrent `agent start`, a
//!   restart policy firing mid-drain) gets ONE extra ladder pass; anything
//!   still `Running` after that cap is SURFACED honestly (AI-18 — never
//!   silently handed to the engine Drop's un-graceful kill-on-drop).
//! * **Finding 6 — an already-exited instance is honest**: a stop that
//!   errors because the instance ended ON ITS OWN mid-drain (its state now
//!   reads stopped/failed) is classified `ExitedOnItsOwn`, not `Failed` —
//!   the drain's goal (no surviving process) is met, and the record says so
//!   without claiming the ladder did it.
//!
//! Outcome discipline (AI-18 surfaced-not-silent): the drain NEVER aborts on
//! a per-instance failure — one un-stoppable agent must not strand the
//! others' stops — and every outcome (graceful / forced / unverified /
//! exited-on-its-own / failed) is both returned to the caller and emitted as
//! a notice. A failed stop degrades the SHUTDOWN's tidiness, never its
//! honesty: `serve` still exits 0 (the drain ran), with the failures on the
//! record.

use std::collections::HashSet;
use std::sync::Arc;

use hekma_engine::{DiagnosticSink, Engine, FleetEntry, LifecycleState, TransitionCause};

/// How one instance's drain-time stop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopResult {
    /// Exited on its own within the graceful window
    /// (`TransitionCause::StopGraceful`).
    Graceful,
    /// The window elapsed and the ladder force-killed it
    /// (`TransitionCause::StopForced`).
    Forced,
    /// The engine reports the stop completed, but the graceful-vs-forced
    /// flavor could not be verified from the instance's event log (the
    /// surfaced reason rides along).
    Unverified(String),
    /// The instance ended ON ITS OWN mid-drain (its state read
    /// stopped/failed when the stop errored): the ladder was not needed and
    /// did not run — the honest classification, never a fabricated `Failed`
    /// (review finding 6). The state it ended in rides along.
    ExitedOnItsOwn {
        /// The instance's state at re-read (`"stopped"` / `"failed"`).
        state: String,
    },
    /// The stop itself failed (the surfaced reason rides along); the agent
    /// may still be running.
    Failed(String),
}

/// One instance's drain outcome: who, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainOutcome {
    /// The instance name.
    pub name: String,
    /// How its stop ended.
    pub result: StopResult,
}

impl std::fmt::Display for DrainOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.result {
            StopResult::Graceful => write!(f, "{}: stopped gracefully", self.name),
            StopResult::Forced => write!(
                f,
                "{}: stopped (the graceful window elapsed; the stop ladder force-killed it)",
                self.name
            ),
            StopResult::Unverified(detail) => write!(
                f,
                "{}: stopped (graceful-vs-forced outcome unreadable: {detail})",
                self.name
            ),
            StopResult::ExitedOnItsOwn { state } => write!(
                f,
                "{}: already ended on its own ({state}) — the stop ladder was not \
                 needed and did not run",
                self.name
            ),
            StopResult::Failed(detail) => write!(
                f,
                "{}: NOT stopped ({detail}) — it may still be running; check the fleet \
                 and stop it manually",
                self.name
            ),
        }
    }
}

/// The pure drain planner: WHICH instances the drain will stop — exactly the
/// live-process rows, `LifecycleState::Running` AND `LifecycleState::Paused`,
/// in fleet order. A paused agent is stopped through the same bounded ladder
/// (the engine's stop resumes the suspension first, so the graceful signal
/// can be delivered) instead of being handed to the engine Drop's raw
/// kill-on-drop teardown — at shutdown the engine dies with the agent either
/// way, and the ladder is the only graceful path. Everything else
/// (registered/stopped/failed rows) is not the drain's business: no process
/// to stop (the stays-paused contract governs ADOPTION — a stranded paused
/// row stays paused — not shutdown). Pure so the selection rule is
/// unit-testable without an engine.
pub fn drain_plan(entries: &[FleetEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.state,
                LifecycleState::Running | LifecycleState::Paused
            )
        })
        .map(|entry| entry.name.as_str().to_string())
        .collect()
}

/// The drain (see the module docs): fleet → plan → CONCURRENT bounded stops
/// (one window's wall clock, finding 4) → a re-enumeration pass for
/// newcomers (finding 5, capped at one extra pass) → an honest still-running
/// roll-up (finding 5). Every outcome is noted through `sink` AND returned.
/// Returns after the LAST stop settles; the caller drops the engine only
/// afterwards. The engine is shared by the concurrent stop tasks, hence the
/// `Arc`.
pub async fn drain_and_stop(
    engine: &Arc<Engine>,
    window: std::time::Duration,
    sink: &DiagnosticSink,
) -> Vec<DrainOutcome> {
    // The notice emitter over the SAME sink the host and engine use (the
    // caller's sink, so host + engine + drain form one interleaved stream).
    let notes = super::Notifier::over(sink.clone());
    let mut attempted: HashSet<String> = HashSet::new();
    let mut outcomes = Vec::new();

    // Pass 1 — the snapshot plan.
    let found_any = stop_pass(engine, window, &notes, &mut attempted, &mut outcomes).await;

    // Finding 5: re-enumerate ONCE — a newcomer (entered Running after the
    // snapshot: a concurrent start, a restart policy firing mid-drain) gets
    // the ladder too. The cap is deliberate: two passes bound the drain's
    // wall clock (≈ 2 × window worst case); beyond that the honest roll-up
    // below reports the stragglers instead of looping forever on an
    // un-stoppable instance.
    let newcomer_found = stop_pass(engine, window, &notes, &mut attempted, &mut outcomes).await;

    if !found_any && !newcomer_found {
        notes.note("no running or paused agents; nothing to stop");
    }

    // Finding 5's honesty cap: anything STILL alive (Running OR Paused)
    // after both passes is surfaced (AI-18) — it will be handed to the
    // engine Drop's kill-on-drop teardown WITHOUT the bounded ladder, and
    // the operator reads exactly that here, never a silent exit.
    match engine.fleet().await {
        Ok(fleet) => {
            let still_alive = drain_plan(&fleet);
            if !still_alive.is_empty() {
                notes.note(&format!(
                    "still running or paused after the shutdown drain: {} — the engine's exit \
                     teardown will terminate them WITHOUT the bounded ladder; their \
                     stops did not settle within the drain's two passes",
                    still_alive.join(", ")
                ));
            }
        }
        Err(err) => {
            notes.note(&format!(
                "could not re-read the fleet to confirm the drain completed ({err}) — \
                 whether every stop settled is unverified"
            ));
        }
    }
    outcomes
}

/// One drain PASS: enumerate the fleet, issue EVERY running instance's stop
/// concurrently (`JoinSet` — finding 4), and collect the outcomes as they
/// settle. Returns whether ANY stop was issued. Instances already attempted
/// by an earlier pass (tracked in `attempted`) are skipped — a failed stop
/// is re-surfaced by the final roll-up, never retried into duplicate noise.
async fn stop_pass(
    engine: &Arc<Engine>,
    window: std::time::Duration,
    notes: &super::Notifier,
    attempted: &mut HashSet<String>,
    outcomes: &mut Vec<DrainOutcome>,
) -> bool {
    let fleet = match engine.fleet().await {
        Ok(fleet) => fleet,
        Err(err) => {
            // Surfaced degradation (AI-18): an unreadable fleet means the
            // host cannot even enumerate what to stop. The engine Drop
            // teardown still kills supervised handles (kill-on-drop), but
            // the BOUNDED ladder did not run — say exactly that. (Only the
            // FIRST pass's failure says "no instance went through the
            // ladder"; a later pass's failure is covered by the final
            // roll-up's own unreadable-fleet note.)
            if attempted.is_empty() && outcomes.is_empty() {
                notes.note(&format!(
                    "could not enumerate the fleet for the shutdown drain ({err}) — no \
                     instance went through the bounded stop ladder; supervised handles \
                     still die with the engine"
                ));
            }
            return false;
        }
    };
    let plan: Vec<String> = drain_plan(&fleet)
        .into_iter()
        .filter(|name| attempted.insert(name.clone()))
        .collect();
    if plan.is_empty() {
        return false;
    }
    // Finding 4: ALL stops issued, then joined — each outcome surfaces the
    // moment its ladder settles. (The engine's ladders serialize under the
    // supervision lock — see the module doc for the honest wall-clock
    // statement; this structure is what turns parallel the moment the
    // engine's stops do.)
    let mut set = tokio::task::JoinSet::new();
    for name in &plan {
        notes.note(&format!(
            "stopping {name} (graceful window {}s) ...",
            window.as_secs_f32()
        ));
        let engine = Arc::clone(engine);
        let name = name.clone();
        set.spawn(async move { stop_one(&engine, &name, window).await });
    }
    while let Some(joined) = set.join_next().await {
        let outcome = match joined {
            Ok(outcome) => outcome,
            Err(err) => {
                // A JoinError means the stop TASK panicked — the outcome for
                // that instance is honest `Failed` with the reason, and the
                // other instances' outcomes are unaffected.
                DrainOutcome {
                    name: "<unknown>".to_string(),
                    result: StopResult::Failed(format!("the stop task failed: {err}")),
                }
            }
        };
        // Each outcome is noted the moment it settles (AI-18: the
        // per-instance stop result is a host notice, not just a return
        // value — a direct drain caller sees the same stream serve does).
        notes.note(&outcome.to_string());
        outcomes.push(outcome);
    }
    true
}

/// ONE instance's bounded stop → its classified outcome. The classification
/// is honest by construction (finding 6): a stop error against an instance
/// the engine now reports as ended (stopped/failed) is `ExitedOnItsOwn`,
/// not `Failed`.
async fn stop_one(engine: &Engine, name: &str, window: std::time::Duration) -> DrainOutcome {
    match engine.stop(name, Some(window)).await {
        Ok(_instance) => {
            // The stop committed; read WHICH ladder arm ran from the
            // instance's committed transition log (the same committed-state
            // seam every consumer uses — never a guess).
            let result = match engine.transition_events(name).await {
                Ok(events) => {
                    match events.into_iter().rev().find(|event| {
                        matches!(
                            event.cause,
                            TransitionCause::StopGraceful | TransitionCause::StopForced { .. }
                        )
                    }) {
                        Some(event) => match event.cause {
                            TransitionCause::StopGraceful => StopResult::Graceful,
                            TransitionCause::StopForced { .. } => StopResult::Forced,
                            _ => StopResult::Unverified(
                                "the newest stop transition was neither graceful nor forced"
                                    .to_string(),
                            ),
                        },
                        None => StopResult::Unverified(
                            "no stop transition found in the instance's event log".to_string(),
                        ),
                    }
                }
                Err(err) => StopResult::Unverified(err.to_string()),
            };
            DrainOutcome {
                name: name.to_string(),
                result,
            }
        }
        Err(err) => {
            // Finding 6: the stop errored — but WHY? If the instance ended
            // on its own mid-drain (state now stopped/failed), the honest
            // outcome is "already ended on its own; the ladder was not
            // needed" (the orphan-adoption honesty pattern), NOT a `Failed`
            // that reads as if an agent survived the drain. Any other error
            // (unreadable fleet included) is `Failed` with the reason.
            let state_now = match engine.fleet().await {
                Ok(fleet) => fleet
                    .into_iter()
                    .find(|entry| entry.name.as_str() == name)
                    .map(|entry| entry.state),
                Err(_) => None,
            };
            let ended_on_its_own = matches!(
                state_now,
                Some(LifecycleState::Stopped) | Some(LifecycleState::Failed)
            );
            let result = if ended_on_its_own {
                StopResult::ExitedOnItsOwn {
                    state: format!(
                        "{:?}",
                        state_now.expect("ended_on_its_own implies Some(state)")
                    ),
                }
            } else {
                StopResult::Failed(err.to_string())
            };
            DrainOutcome {
                name: name.to_string(),
                result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hekma_engine::{InstanceName, RestartPolicy, UsageTotals, UsageView};

    /// A minimal FleetEntry with just the fields `drain_plan` reads; every
    /// other field is the honest zero/None default (the planner is the unit
    /// under test, not the Fleet schema).
    fn entry(name: &str, state: LifecycleState) -> FleetEntry {
        FleetEntry {
            name: InstanceName::new(name).expect("test instance name"),
            kind: "test".to_string(),
            state,
            restart_count: 0,
            restart_policy: RestartPolicy::OnFailure,
            failed_cause: None,
            budget: None,
            usage: UsageView::new(UsageTotals::zero(), UsageTotals::zero()),
            metering_source: "self-reported".to_string(),
            acp_context_usage: None,
            usage_gap: None,
            supervision: None,
            agent_home: "/tmp/test-home".to_string(),
        }
    }

    #[test]
    fn the_plan_is_exactly_the_live_process_rows_in_fleet_order() {
        let entries = vec![
            entry("idle", LifecycleState::Registered),
            entry("busy", LifecycleState::Running),
            entry("frozen", LifecycleState::Paused),
            entry("second", LifecycleState::Running),
            entry("dead", LifecycleState::Failed),
            entry("done", LifecycleState::Stopped),
        ];
        assert_eq!(
            drain_plan(&entries),
            vec![
                "busy".to_string(),
                "frozen".to_string(),
                "second".to_string()
            ],
            "Running AND Paused rows (the live-process rows), preserving fleet order — \
             a paused agent must go through the bounded ladder, never the engine \
             Drop's kill-on-drop teardown"
        );
    }

    #[test]
    fn an_empty_or_all_quiescent_fleet_plans_nothing() {
        assert!(drain_plan(&[]).is_empty(), "empty fleet → empty plan");
        let entries = vec![
            entry("a", LifecycleState::Registered),
            entry("b", LifecycleState::Failed),
            entry("c", LifecycleState::Stopped),
        ];
        assert!(
            drain_plan(&entries).is_empty(),
            "no live-process row → empty plan"
        );
    }

    #[test]
    fn outcome_wording_names_the_instance_and_is_honest_about_failures() {
        // The AI-18 pin: a failed stop's wording says NOT stopped and points
        // at the remediation; a forced stop says the ladder escalated; a
        // graceful stop is plain. No fabricated success anywhere.
        let graceful = DrainOutcome {
            name: "a".to_string(),
            result: StopResult::Graceful,
        };
        assert_eq!(graceful.to_string(), "a: stopped gracefully");
        let forced = DrainOutcome {
            name: "b".to_string(),
            result: StopResult::Forced,
        };
        assert!(
            forced.to_string().contains("b:") && forced.to_string().contains("force-killed"),
            "forced wording names the instance and the escalation: {}",
            forced
        );
        let failed = DrainOutcome {
            name: "c".to_string(),
            result: StopResult::Failed("backend refused".to_string()),
        };
        let failed_text = failed.to_string();
        assert!(
            failed_text.contains("c:") && failed_text.contains("NOT stopped"),
            "failed wording must NOT claim success: {failed_text}"
        );
        assert!(
            failed_text.contains("backend refused") && failed_text.contains("check the fleet"),
            "failed wording carries the reason + remediation: {failed_text}"
        );
        let unverified = DrainOutcome {
            name: "d".to_string(),
            result: StopResult::Unverified("event log unreadable".to_string()),
        };
        let unverified_text = unverified.to_string();
        assert!(
            unverified_text.contains("d:") && unverified_text.contains("unreadable"),
            "unverified wording must say the flavor could not be read: {unverified_text}"
        );
    }

    #[test]
    fn an_exited_on_its_own_outcome_never_reads_as_a_failed_stop() {
        // Finding 6's pin: the classification exists so a mid-drain
        // self-exit is NOT reported as "NOT stopped" (which would be false —
        // the instance is GONE), and NOT as a graceful/forced stop (which
        // would be false — the ladder did not run). The wording names the
        // instance, the state it ended in, and that the ladder stayed out.
        let outcome = DrainOutcome {
            name: "selfexit".to_string(),
            result: StopResult::ExitedOnItsOwn {
                state: "Stopped".to_string(),
            },
        };
        let text = outcome.to_string();
        assert!(
            text.contains("selfexit:") && text.contains("ended on its own"),
            "the wording must say the instance ended on its own: {text}"
        );
        assert!(
            text.contains("Stopped") && text.contains("was not needed"),
            "the wording names the state and that the ladder did not run: {text}"
        );
        assert!(
            !text.contains("NOT stopped") && !text.contains("force-killed"),
            "the wording must NOT borrow the failed/forced shapes: {text}"
        );
    }
}
