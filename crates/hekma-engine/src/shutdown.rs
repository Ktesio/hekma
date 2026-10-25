//! The shutdown drain — the engine-owned facade that stops every live agent
//! through the bounded stop ladder before the engine is dropped.
//!
//! This is the embedder-facing half of issue #328 item 3: a library embedder
//! that simply drops an [`Engine`] gets the raw kill-on-drop teardown for its
//! attached agents — no graceful window, no stop transition. Hosts that run
//! `serve` never see that (they drain first), so the drain that used to live
//! in `hekma-host`'s `drain` module moved HERE: a plain embedding now gets
//! the identical ladder through [`Engine::shutdown`] without depending on the
//! host crate, and the host's `drain_and_stop` is a thin delegate. One drain
//! implementation, engine-owned — AD-20's "the engine owns the stop" taken to
//! its conclusion: the fleet-wide drain IS stops.
//!
//! Semantics (unchanged from the host-era drain; the story 17-2 review
//! findings survive the move):
//!
//! * **Finding 4 — every live row's stop runs through the ladder.** The stops
//!   are issued one per agent in fleet order. The WALL CLOCK, honestly
//!   stated: the engine's stop ladder runs under its supervision lock (the
//!   ratified coarse two-mutex model), so instances that must run out their
//!   full graceful window before the force-kill escalation QUEUE behind each
//!   other — worst case Σ(window) over the fleet. (The host-era drain issued
//!   the stops through a `JoinSet`; under that lock the ladders serialized
//!   anyway, so the wall clock is identical — the sequential loop just makes
//!   each agent's `stopping …` notice and its outcome adjacent, which is the
//!   more readable shutdown stream.)
//! * **Finding 5 — the plan is re-enumerated**: an instance that enters a
//!   live state after the first snapshot (a concurrent `start`, a restart
//!   policy firing mid-drain) gets ONE extra ladder pass; anything still
//!   alive after that cap is SURFACED honestly (AI-18 — never silently
//!   handed to the engine `Drop`'s un-graceful kill-on-drop).
//! * **Finding 6 — an already-exited instance is honest**: a stop that errors
//!   because the instance ended ON ITS OWN mid-drain (its state now reads
//!   stopped/failed) is classified `ExitedOnItsOwn`, not `Failed` — the
//!   drain's goal (no surviving process) is met, and the record says so
//!   without claiming the ladder did it.
//!
//! Outcome discipline (AI-18 surfaced-not-silent): the drain NEVER aborts on
//! a per-instance failure — one un-stoppable agent must not strand the
//! others' stops — and every outcome (graceful / forced / unverified /
//! exited-on-its-own / failed) is both returned to the caller and, when a
//! [`DiagnosticSink`] was supplied, emitted as a notice. A failed stop
//! degrades the SHUTDOWN's tidiness, never its honesty: the caller still
//! gets every outcome on the record.

use std::collections::HashSet;
use std::io::Write;
use std::time::Duration;

use crate::engine::Engine;
use crate::{DiagnosticSink, FleetEntry, LifecycleState, TransitionCause};

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
/// can be delivered) instead of being handed to the engine `Drop`'s raw
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

/// The drain's notice emitter — the drain-side twin of the supervisor's
/// `emit_diagnostic` discipline: `[hekma] `-prefixed, one line,
/// `\n`-terminated, best-effort writes with poisoned-mutex recovery and a
/// caught write/flush panic (a broken sink writer must never kill a
/// shutdown). `None` (an embedder who wants return values only) emits
/// nothing.
#[derive(Clone, Default)]
pub(crate) struct Notes {
    sink: Option<DiagnosticSink>,
}

impl Notes {
    pub(crate) fn over(sink: Option<&DiagnosticSink>) -> Self {
        Notes {
            sink: sink.cloned(),
        }
    }

    /// Emit ONE `[hekma] `-prefixed notice line (best-effort write + flush).
    fn note(&self, text: &str) {
        let Some(sink) = &self.sink else {
            return;
        };
        let line = format!("[hekma] {text}\n");
        let mut writer = match sink.lock() {
            Ok(writer) => writer,
            // Poisoned by a panicking writer: take the guard anyway. The
            // data is the stream; a torn line is better than NO line.
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.flush();
        }));
    }
}

/// The drain (see the module docs and [`Engine::shutdown`]): fleet → plan →
/// bounded stops, one agent at a time in fleet order → a re-enumeration pass
/// for newcomers (finding 5, capped at one extra pass) → an honest
/// still-alive roll-up (finding 5). Every outcome is noted through `notes`
/// AND returned. Returns after the LAST stop settles; the caller drops the
/// engine only afterwards.
pub(crate) async fn drain_and_stop(
    engine: &Engine,
    window: Duration,
    notes: &Notes,
) -> Vec<DrainOutcome> {
    let mut attempted: HashSet<String> = HashSet::new();
    let mut outcomes = Vec::new();

    // Pass 1 — the snapshot plan.
    let found_any = stop_pass(engine, window, notes, &mut attempted, &mut outcomes).await;

    // Finding 5: re-enumerate ONCE — a newcomer (entered a live state after
    // the snapshot: a concurrent start, a restart policy firing mid-drain)
    // gets the ladder too. The cap is deliberate: two passes bound the
    // drain's wall clock (≈ 2 × window worst case); beyond that the honest
    // roll-up below reports the stragglers instead of looping forever on an
    // un-stoppable instance.
    let newcomer_found = stop_pass(engine, window, notes, &mut attempted, &mut outcomes).await;

    if !found_any && !newcomer_found {
        notes.note("no running or paused agents; nothing to stop");
    }

    // Finding 5's honesty cap: anything STILL alive (running OR paused)
    // after both passes is surfaced (AI-18) — it will be handed to the
    // engine `Drop`'s kill-on-drop teardown WITHOUT the bounded ladder, and
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

/// One drain PASS: enumerate the fleet, stop EVERY live instance through the
/// bounded ladder, and collect the outcomes. Returns whether ANY stop was
/// issued. Instances already attempted by an earlier pass (tracked in
/// `attempted`) are skipped — a failed stop is re-surfaced by the final
/// roll-up, never retried into duplicate noise.
async fn stop_pass(
    engine: &Engine,
    window: Duration,
    notes: &Notes,
    attempted: &mut HashSet<String>,
    outcomes: &mut Vec<DrainOutcome>,
) -> bool {
    let fleet = match engine.fleet().await {
        Ok(fleet) => fleet,
        Err(err) => {
            // Surfaced degradation (AI-18): an unreadable fleet means the
            // drain cannot even enumerate what to stop. The engine `Drop`
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
    for name in &plan {
        notes.note(&format!(
            "stopping {name} (graceful window {}s) ...",
            window.as_secs_f32()
        ));
        // Each outcome is noted the moment it settles (AI-18: the
        // per-instance stop result is an operator notice, not just a return
        // value — a direct shutdown caller sees the same stream serve does).
        // The ladders serialize under the supervision lock regardless (see
        // the module doc's wall-clock statement), so a sequential loop costs
        // nothing over concurrent issuance.
        let outcome = stop_one(engine, name, window).await;
        notes.note(&outcome.to_string());
        outcomes.push(outcome);
    }
    true
}

/// ONE instance's bounded stop → its classified outcome. The classification
/// is honest by construction (finding 6): a stop error against an instance
/// the engine now reports as ended (stopped/failed) is `ExitedOnItsOwn`,
/// not `Failed`.
async fn stop_one(engine: &Engine, name: &str, window: Duration) -> DrainOutcome {
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
    use crate::{InstanceName, RestartPolicy, UsageTotals, UsageView};
    use std::sync::{Arc, Mutex};

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

    #[test]
    fn notes_without_a_sink_emit_nothing_and_never_panic() {
        // The None-sink embedder path: return values only, no operator
        // stream — and a poisoned or panicking sink writer must never take
        // the shutdown down (the supervisor's emission discipline, ported).
        let none = Notes::over(None);
        none.note("silent by contract");

        struct PanicWriter;
        impl Write for PanicWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                panic!("host writer bug");
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(PanicWriter)));
        let notes = Notes::over(Some(&sink));
        notes.note("a panicking writer is swallowed, not propagated");
    }

    #[test]
    fn notes_recover_from_a_poisoned_sink_mutex() {
        // The poisoned-lock arm: a writer that panicked while HOLDING the
        // sink mutex poisons it; the note-taker takes the poisoned guard
        // anyway (a torn line is better than NO line) and its own
        // catch_unwind swallows the panicking writer — the shutdown survives
        // a poisoned, panicking sink, never a panic out of the drain.
        struct AlwaysPanicWriter;
        impl Write for AlwaysPanicWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                panic!("host writer bug");
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(AlwaysPanicWriter)));
        let holder = sink.clone();
        // Panic while the guard is alive INSIDE the closure: the unwind
        // drops the guard mid-panic and the mutex poisons.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = holder.lock().unwrap();
            let _ = guard.write_all(b"x");
        }));
        assert!(
            sink.lock().is_err(),
            "the writer's mid-hold panic must have poisoned the mutex"
        );
        let notes = Notes::over(Some(&sink));
        notes.note("the drain survives a poisoned, panicking sink");
    }

    // ------------------------------------------------------------------
    // The drain facade's failure arms (epic 20-1 coverage pass): driven
    // through a real Engine — the FAILED stop (staged through the cfg(test)
    // stop-fault seam, the codebase's established fault-injection idiom)
    // and the UNREADABLE fleet (the store's table dropped out from under
    // an open registry connection).
    // ------------------------------------------------------------------

    /// A recording sink (the host suites' SharedSink shape): every note the
    /// drain emits lands in a shared buffer the test asserts on.
    #[derive(Clone)]
    struct RecordingSink(Arc<Mutex<Vec<u8>>>);

    impl RecordingSink {
        fn new() -> (Self, crate::DiagnosticSink) {
            let shared = Self(Arc::new(Mutex::new(Vec::new())));
            let sink: crate::DiagnosticSink = Arc::new(Mutex::new(Box::new(shared.clone())));
            (shared, sink)
        }

        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl std::io::Write for RecordingSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Register + start a `fake_agent` instance named `name` under a fresh
    /// state root, through the engine's own blocking facade (the same seam
    /// the CLI's register/start drive). The short linger means a child the
    /// teardown cannot reach still self-exits promptly.
    fn started_fake_agent(name: &str) -> (tempfile::TempDir, crate::Engine) {
        let state = tempfile::tempdir().unwrap();
        let manifest = tempfile::tempdir().unwrap();
        let bin = hekma_conformance::fake_agent_bin();
        let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"fake\"\n\n\
             [lifecycle.start]\nexec = {exec:?}\nargs = [\"--linger-ms\", \"5000\"]\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n",
            exec = bin.to_string_lossy(),
        );
        std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();
        let engine = crate::Engine::open(Some(state.path().to_path_buf())).unwrap();
        let facade = engine.blocking();
        facade
            .register_with_adapter(
                name,
                &crate::AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        let started = facade.start(name).unwrap();
        assert_eq!(started.state, LifecycleState::Running);
        (state, engine)
    }

    #[test]
    fn a_failed_drain_stop_reports_failed_and_leaves_the_row_honestly_stopping() {
        // The stop-failure arms (finding 6's Err side): a stop that errors
        // against a still-live process — staged through the cfg(test)
        // stop-fault seam, because a real unstoppable process cannot be
        // hermetic — must surface a `Failed` outcome naming the reason and
        // the remediation, leave the row in the honest `stopping` state
        // (never a fabricated `stopped` over a live process), and never
        // abort the drain (the roll-up still runs and says nothing false).
        // Driven through the blocking facade: the Engine owns its runtime,
        // so it cannot live inside an async test context.
        let (_state, engine) = started_fake_agent("wedge");
        engine.arm_test_stop_fault("wedge");
        let (sink_handle, sink) = RecordingSink::new();
        let outcomes = engine
            .blocking()
            .shutdown(Duration::from_secs(2), Some(&sink));
        assert_eq!(outcomes.len(), 1, "exactly one drain outcome");
        let text = sink_handle.text();
        assert!(
            text.contains("stopping wedge (graceful window"),
            "the pass notes the ladder attempt:\n{text}"
        );
        assert!(
            text.contains("wedge:") && text.contains("NOT stopped"),
            "the failed outcome is surfaced honestly (AI-18):\n{text}"
        );
        assert!(
            text.contains("injected cfg(test) stop fault"),
            "the outcome carries the backend's reason:\n{text}"
        );
        // The row is left `stopping`: the ladder really started, the backend
        // failed, and nothing was fabricated over the live process.
        let fleet = engine.blocking().fleet().unwrap();
        let wedge = fleet
            .iter()
            .find(|entry| entry.name.as_str() == "wedge")
            .expect("the wedged instance is still listed");
        assert_eq!(
            wedge.state,
            LifecycleState::Stopping,
            "no phantom stopped/failed row over a live process"
        );
        // The roll-up's honesty cap stays quiet: a `stopping` row is not a
        // "still running or paused" straggler.
        assert!(
            !text.contains("still running or paused after the shutdown drain"),
            "the honesty cap names only rows that are really still live:\n{text}"
        );
    }

    #[test]
    fn an_unreadable_fleet_surfaces_every_enumeration_refusal() {
        // The fleet-read failure arms: a registry whose backing table is
        // dropped from a second connection (the registry's own connection
        // discovers the loss on its next read) makes EVERY fleet read fail —
        // both passes surface the enumeration refusal (naming that NO
        // instance went through the ladder), the nothing-to-stop note still
        // fires, and the final roll-up says the drain's completion is
        // UNVERIFIED — a surfaced degradation, never a silent Ok. Driven
        // through the blocking facade (the Engine owns its runtime).
        let state = tempfile::tempdir().unwrap();
        let engine = crate::Engine::open(Some(state.path().to_path_buf())).unwrap();
        let db = state.path().join("state.db");
        rusqlite::Connection::open(&db)
            .expect("open the state store directly")
            .execute("DROP TABLE agent_instances", [])
            .expect("drop the fleet's table");
        let (sink_handle, sink) = RecordingSink::new();
        let outcomes = engine
            .blocking()
            .shutdown(Duration::from_secs(2), Some(&sink));
        assert!(outcomes.is_empty(), "no stop was even attempted");
        let text = sink_handle.text();
        assert!(
            text.contains("could not enumerate the fleet for the shutdown drain"),
            "the pass's enumeration failure is surfaced:\n{text}"
        );
        assert!(
            text.contains("no instance went through the bounded stop ladder"),
            "the first-pass note names that the ladder never ran:\n{text}"
        );
        assert!(
            text.contains("no running or paused agents; nothing to stop"),
            "the nothing-to-stop note still fires:\n{text}"
        );
        assert!(
            text.contains("could not re-read the fleet to confirm the drain completed"),
            "the roll-up names the unverified completion:\n{text}"
        );
    }
}
