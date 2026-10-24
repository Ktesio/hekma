//! reaper family — split out of the supervisor monolith by story 13-3
//! (epic-13; boundary ratified 2026-09-19, study: Option B). Pure move:
//! additional `impl Supervisor` block, no signature or behavior change.

use super::*;
use crate::domain::liveness::{self, LivenessSignal, WindowDecision};
use crate::ports::StoreError;

impl Supervisor {
    /// Clear one instance's consecutive poll-error streak (AI-12). Called at
    /// EVERY [`Supervisor::poll_once`] `running.remove` site and on every fresh
    /// handle insert (start / adopt), so a removed — or replaced — handle can
    /// never bequeath a stale error streak to the instance's next Run.
    pub(super) fn clear_poll_error_streak(&mut self, name: &InstanceName) {
        self.poll_error_streaks.remove(name);
        self.poll_last_errors.remove(name);
    }

    /// Arm the cfg(test) poll-fault seam for `pid`: every `backend.poll` of
    /// the handle with this pid fails with an injected error until this
    /// supervisor is dropped. Lib-test only.
    #[cfg(test)]
    pub(crate) fn arm_poll_fault(&mut self, pid: u32) {
        assert!(
            self.poll_fault_pids.insert(pid),
            "poll fault already armed for pid {pid}"
        );
    }

    /// The injected poll error for `pid`, when the seam is armed (cfg(test)).
    /// `pub(super)`: the stuck-stop retry branch in `lifecycle.rs` fronts the
    /// SAME seam at its own (second) `backend.poll` site, mirroring this
    /// module's phase-1 probe.
    #[cfg(test)]
    pub(super) fn injected_poll_fault(&self, pid: u32) -> Option<BackendError> {
        self.poll_fault_pids
            .contains(&pid)
            .then(|| BackendError::Control {
                op: "poll",
                detail: "injected cfg(test) poll fault (AI-12 wiring seam)".to_string(),
            })
    }

    /// Arm the cfg(test) signal-fault seam for `name`: every `signal_backend`
    /// for this instance fails with an injected error until this supervisor is
    /// dropped. Lib-test only.
    #[cfg(test)]
    pub(crate) fn arm_signal_fault(&mut self, name: InstanceName) {
        assert!(
            self.signal_fault_names.insert(name),
            "signal fault already armed for this instance"
        );
    }

    /// The crash-detection reaper pass (story 1-6, AC-A / AC3 / AC5).
    ///
    /// Polls every held handle via the EXISTING `backend.poll` and reacts to an
    /// unrequested exit: for each instance the store still shows `running` or
    /// `paused` (a `stopping` in flight means an operator stop is under way — NOT
    /// a crash, so it is skipped), applies the EVENT-driven `running → failed`
    /// edge with a [`TransitionCause::Crashed`] (AC5), removes the handle, and
    /// consults the per-instance [`RestartPolicy`] (AD-15):
    /// * [`RestartPolicy::Never`] — leave `failed`; record the crash cause; NO
    ///   restart plan.
    /// * [`RestartPolicy::OnFailure`] — increment the consecutive restart count;
    ///   if it hit the crash-loop threshold ([`is_crash_loop`]) leave `failed`
    ///   with the crash-loop reason and NO plan; otherwise persist the new count
    ///   and return a [`RestartPlan`] with the backoff delay for that attempt.
    ///
    /// Returns the [`RestartPlan`]s the engine cadence should time. SYNC +
    /// cfg-free (the engine calls it via `spawn_blocking` on an interval); it
    /// performs NO sleeping itself. Idempotent per exit: once an instance is
    /// moved to `failed` and its handle removed, a later pass will not see it in
    /// `self.running` again.
    ///
    /// **Persistent poll errors are crash input (AI-12):** a `backend.poll` error
    /// is tolerated as transient only while its PER-INSTANCE consecutive streak
    /// stays below [`MAX_CONSECUTIVE_POLL_ERRORS`] (the
    /// [`poll_verdict`] pure decision). A clean `Alive` read resets the streak,
    /// and every handle removal clears it; an error streak that reaches the
    /// threshold is treated exactly like an observed exit — the instance lands
    /// `failed` with a cause naming the persistent poll failure plus the LAST
    /// error's text (truncated; loop 1) and the Restart Policy applies — instead
    /// of the old silent `Err(_) => None` that could hide a dead handle FOREVER.
    ///
    /// **Systemic guard (AI-12, loop 1):** every held handle is polled ONCE per
    /// tick, up front. An error on MORE THAN ONE handle in the same tick is
    /// corroborated as environmental (a procfs/sysctl-style outage): no streak
    /// increment, one diagnostic, handles stay alive — a fleet-wide poll outage
    /// must never mass-crash running agents through kill-on-drop. Only a handle
    /// erroring ALONE (its peers read fine) accumulates crash-input credit.
    ///
    /// **Declared liveness (story 18-2, PHASE 4):** after crash handling, every
    /// still-held instance whose persisted state is EXACTLY `running` and whose
    /// adapter declared a `[liveness]` check gets ONE attempt per pass (the
    /// shared evaluators — heartbeat mtime read, or ONE `http-get` whose
    /// deadline is `now + HTTP_ATTEMPT_CAP`). A HUNG check therefore stretches
    /// its own pass by up to ~1s; N hung checks stretch it by up to ~Ns — the
    /// attempts run sequentially under the supervisor lock, and ONLY failing
    /// checks pay the cap (a fresh heartbeat costs one stat; an answering
    /// endpoint costs one fast loopback connect). The unifying semantic — the
    /// signal failing CONTINUOUSLY for the declared `max_age_ms` — is the PURE
    /// [`crate::domain::liveness::window_decision`]; expiry drives the EXACT
    /// crash-path sequence below with a [`TransitionCause::Unresponsive`] (the
    /// handle drop kills the hung process — the sanctioned teardown, the AI-12
    /// PersistentPollFailure precedent), and the Restart Policy applies
    /// verbatim. A paused instance is NEVER probed (a suspended process cannot
    /// heartbeat — the exact-`running` guard, silent by design), and an
    /// instance with no declared check behaves byte-identically to before.
    pub fn poll_once(&mut self, registry: &Registry) -> Vec<RestartPlan> {
        // First, INGEST self-reported usage from every running instance's captured
        // output (story 3-1): the reaper is the natural cadence for draining the
        // agent-output log into the Usage Ledger while an instance is `running`.
        // Best-effort per instance; a drain hiccup never blocks crash detection.
        self.drain_usage_all(registry);
        // Then INGEST engine-observed usage (story 3-4): drain each observed
        // instance's listener queue (the counts the loopback proxy parsed out of the
        // agent's model traffic) into the SAME `ingest_usage` choke point, minting
        // the per-Run `sequence`. This reaper cadence (~250ms) lands observed usage
        // well within the AD-7/FR-19 flush bound (≤5s) of call completion. Best-
        // effort per instance, exactly like the self-reported drain.
        self.drain_observed_all(registry);

        // Then DRAIN each running acp instance's surfaced notices (story 14-1,
        // spine AD-19): malformed lines, permission denials, usage updates,
        // unhandled messages, and stream-end facts the reader thread queued
        // WITHOUT ever taking the supervisor lock. Emitted here through
        // `emit_diagnostic` (the choke point — we hold the lock), bounded by
        // the queue cap per instance.
        self.drain_acp_notices_all();

        // Snapshot the currently-held names (we mutate self.running as we react).
        let names: Vec<InstanceName> = self.running.keys().cloned().collect();
        let mut plans = Vec::new();

        // FIRST — drain restart plans QUEUED by the start path (Epic 18): an
        // exit observed during a readiness-probe wait landed its crash
        // transition inline and queued the plan; this pass hands it to the
        // engine cadence, which times the backoff exactly as for
        // reaper-detected crashes. Draining BEFORE phase 1 keeps the restart
        // attempt nearest in time to the failure that scheduled it.
        plans.append(&mut self.drain_pending_restarts());

        // PHASE 1 — poll EVERY held handle in the SAME tick (AI-12, loop 1).
        // Liveness reads happen up front, BEFORE any crash handling, so the pass
        // can CORROBORATE: a poll error that shows up on MULTIPLE handles in one
        // tick is a backend/environment-wide condition (a procfs/sysctl-style
        // outage), never a per-handle fault — and tripping every streak then
        // would mass-crash the fleet on kill-on-drop handles (the
        // graceful-degradation gate forbids it). The chosen guard is same-tick
        // cross-handle corroboration (the error-classification alternative was
        // evaluated: the port carries no environment-vs-handle distinction an
        // OS backend could honestly report, so corroboration is the provable
        // shape). A single handle erroring alone stays on the streak path: an
        // un-pollable HANDLE amid readable peers is exactly the crash signal
        // AI-12 exists to surface.
        let mut outcomes: Vec<(InstanceName, PollOutcome)> = Vec::with_capacity(names.len());
        for name in &names {
            // AI-12 wiring seam (cfg(test)): the armed pid's poll fails with an
            // injected error — the fault-injection front for the backend's poll
            // (see the `poll_fault_pids` field docs). The pid read + the seam
            // probe run on short immutable borrows BEFORE the mutable handle
            // borrow below.
            #[cfg(test)]
            {
                let pid = self
                    .running
                    .get(name)
                    .map(|supervised| self.backend.pid(&supervised.handle));
                if let Some(err) = pid.and_then(|pid| self.injected_poll_fault(pid)) {
                    outcomes.push((name.clone(), PollOutcome::Errored(err)));
                    continue;
                }
            }
            let Some(supervised) = self.running.get_mut(name) else {
                continue;
            };
            let outcome = match self.backend.poll(&mut supervised.handle) {
                Ok(ProcessStatus::Alive) => PollOutcome::Alive,
                Ok(ProcessStatus::Exited { code }) => PollOutcome::Exited(code),
                Err(err) => PollOutcome::Errored(err),
            };
            outcomes.push((name.clone(), outcome));
        }

        // PHASE 2 — same-tick corroboration (AI-12 amendment a). Count how many
        // DISTINCT handles errored this tick; more than one ⇒ environmental.
        let errored = outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, PollOutcome::Errored(_)))
            .count();
        let mut environmental_tick = errored > 1;
        if environmental_tick {
            // AI-12 (loop 2): blanket environmental immunity is bounded. A pair
            // of handles that errors together EVERY tick (one broken, one flaky
            // — or a real outage that outlives the cap) must not keep crash
            // detection defeated forever, so past the cap the per-handle streak
            // path resumes and trips normally a few ticks later. The escalation
            // diagnostic fires ONCE, on the transition tick.
            self.consecutive_environmental_ticks =
                self.consecutive_environmental_ticks.saturating_add(1);
            if self.consecutive_environmental_ticks > MAX_CONSECUTIVE_ENVIRONMENTAL_TICKS {
                environmental_tick = false;
                if self.consecutive_environmental_ticks == MAX_CONSECUTIVE_ENVIRONMENTAL_TICKS + 1 {
                    let escalation = format!(
                        "environmental poll failure has persisted for \
                         {MAX_CONSECUTIVE_ENVIRONMENTAL_TICKS} consecutive ticks — no longer \
                         treated as a transient environment-wide condition; per-handle \
                         crash-input credit resumes (an un-pollable handle will be \
                         crash-detected again)"
                    );
                    self.emit_diagnostic(&escalation);
                }
            }
        } else {
            self.consecutive_environmental_ticks = 0;
        }
        if environmental_tick {
            // Record each error's text (a later, genuinely per-handle streak may
            // still want it in a crash cause) but grant NO crash-input credit:
            // every streak stays where it is and every handle stays alive.
            for (name, outcome) in &outcomes {
                if let PollOutcome::Errored(err) = outcome {
                    self.poll_last_errors.insert(
                        name.clone(),
                        truncate_for_cause(&err.to_string(), POLL_ERROR_CAUSE_MAX_CHARS),
                    );
                }
            }
            let errors: Vec<String> = outcomes
                .iter()
                .filter_map(|(name, outcome)| match outcome {
                    PollOutcome::Errored(err) => Some(format!(
                        "{}: {}",
                        name.as_str(),
                        truncate_for_cause(&err.to_string(), POLL_ERROR_CAUSE_MAX_CHARS)
                    )),
                    _ => None,
                })
                .collect();
            let environmental = format!(
                "environmental poll failure: {errored} of {} held handles failed backend.poll \
                 in the same tick — a backend/environment-wide condition, not a per-handle \
                 fault; treating every one as transient (no crash-input credit, no streak \
                 increment, handles stay alive). Errors: {}",
                outcomes.len(),
                errors.join("; "),
            );
            self.emit_diagnostic(&environmental);
        }

        // PHASE 3 — per-name handling (unchanged crash semantics, now fed by the
        // corroborated outcomes). `held_handles` feeds the sole-handle caveat:
        // a handle that trips with no peers could never be corroborated.
        let held_handles = names.len();
        for (name, outcome) in outcomes {
            // What the reaper treats as the crash input: a real observed exit
            // (with the code the backend reported, `None` if unknown), or — new
            // under AI-12 — a persistent poll failure (no exit code exists; the
            // recorded cause says so). Everything else keeps polling.
            let crash = match outcome {
                PollOutcome::Alive => {
                    self.clear_poll_error_streak(&name);
                    continue;
                }
                PollOutcome::Exited(code) => {
                    self.clear_poll_error_streak(&name);
                    CrashInput::Exited(code)
                }
                // Environmental tick: this error was already corroborated as
                // environment-wide above (diagnostic emitted, text recorded) —
                // grant no crash-input credit and keep the handle alive.
                PollOutcome::Errored(_) if environmental_tick => continue,
                PollOutcome::Errored(err) => {
                    // Record the error's text FIRST (AI-12b): whatever the
                    // verdict, a later persistent trip must carry this why.
                    self.poll_last_errors.insert(
                        name.clone(),
                        truncate_for_cause(&err.to_string(), POLL_ERROR_CAUSE_MAX_CHARS),
                    );
                    // A clean Alive read clears the handle's consecutive
                    // poll-error streak; an error increments it and, once it
                    // reaches MAX_CONSECUTIVE_POLL_ERRORS, becomes crash input
                    // instead of the old silent `Err(_) => None` that swallowed
                    // every error forever. Reap on exit is done inside `poll`.
                    let (verdict, streak) = poll_verdict(
                        self.poll_error_streaks.get(&name).copied().unwrap_or(0),
                        Err(err),
                    );
                    match verdict {
                        PollVerdict::TransientError => {
                            self.poll_error_streaks.insert(name.clone(), streak);
                            continue;
                        }
                        PollVerdict::PersistentError => {
                            self.poll_error_streaks.insert(name.clone(), streak);
                            CrashInput::PersistentPollFailure {
                                sole_handle: held_handles == 1,
                            }
                        }
                        // Unreachable by construction: `poll_verdict` maps an
                        // `Err` input to one of the two error verdicts only.
                        PollVerdict::Alive | PollVerdict::Exited(_) => {
                            unreachable!("an Err poll cannot yield a clean verdict")
                        }
                    }
                }
            };
            // The process exited (or the handle went permanently un-pollable):
            // drain any usage it emitted right before dying, so a final batch is
            // not lost between "agent printed it" and this reap. TERMINAL drain —
            // the process is dead, so consume a final newline-less usage line to
            // end-of-log instead of stranding it (H1). On the poll-failure path
            // the process is not PROVEN dead, but the handle is about to be
            // dropped (which kills the group), so this is the last chance to
            // capture the flushed tail; a truncated mid-write line fails the
            // sentinel parse and is skipped, and the DB dedup key backstops the
            // rest. Story 14-3 (T3): an acp instance's stderr sentinel channel
            // drains here too, under the same terminal rule (its own cursor).
            self.drain_self_reported_for(registry, &name, DrainMode::Terminal);
            // Drain any final ENGINE-OBSERVED usage still queued before the crashed
            // instance's listener is torn down (story 3-4): a completion parsed just
            // before the crash must land, not be lost when the `Supervised` is
            // removed below. TERMINAL mode (story 12-4): a commit failure here
            // announces the loss — no next pass.
            self.drain_observed_for(registry, &name, DrainMode::Terminal);
            // Story 17-4: in metadata mode the raw capture files leave the disk
            // with the Run — the final usage pass above has already parsed them
            // (metadata parses before unlink). Covers the crash, stuck-stopping,
            // and silent-drop branches below uniformly: every one of them ends
            // this Run. Best-effort, surfaced failure.
            self.unlink_raw_logs_if_metadata(registry, &name);

            // Read the store state: only an instance the store still shows
            // running/paused is an UNREQUESTED crash. A `stopping` (operator
            // stop) or any other state is not a crash — drop the (now-dead)
            // handle without a `failed` transition.
            let state = match registry.lookup(&name) {
                Ok(inst) => inst.state,
                // The row is gone (removed concurrently) — just drop the handle.
                Err(_) => {
                    self.clear_poll_error_streak(&name);
                    self.running.remove(&name);
                    continue;
                }
            };
            if !matches!(state, LifecycleState::Running | LifecycleState::Paused) {
                // Requested stop (or already-terminal) — not a crash.
                //
                // Fix pass (review of #80 follow-up — the CRITICAL finding,
                // self-healing requirement): if this handle's PRIOR stop
                // attempt sent SIGKILL but could not confirm death within
                // KILL_CONFIRM_TIMEOUT (`stop_unconfirmed` — set ONLY by
                // that specific path, see `stop_inner`'s docs) and the store
                // still shows `stopping`, THIS poll's own observed `Exited`
                // is the reconciliation event the stuck stop() call itself
                // could not wait for: finalize `stopping -> stopped` here
                // rather than silently dropping the handle, so the row does
                // not stay permanently stuck even if no operator ever
                // retries `stop` manually. This is DELIBERATELY narrower
                // than "any exit while stopping" — an ordinary in-flight
                // (non-stuck) stop() call ALWAYS finalizes this transition
                // itself upon its own return, so only the
                // stuck-then-abandoned case needs the reaper's help; every
                // OTHER "not a crash" exit (mirrored by
                // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`,
                // which never sets `stop_unconfirmed`) keeps its EXISTING,
                // unchanged silent-drop behavior.
                let stuck_stopping = state == LifecycleState::Stopping
                    && self.running.get(&name).is_some_and(|s| s.stop_unconfirmed);
                if stuck_stopping {
                    let log_capture = self
                        .running
                        .get(&name)
                        .and_then(|s| self.backend.log_capture(&s.handle));
                    // Story 14-1: surface any queued acp notices BEFORE the
                    // connection drops with the handle.
                    self.drain_acp_notices_for(&name);
                    self.clear_poll_error_streak(&name);
                    self.running.remove(&name);
                    // Story 14-2: the settle retains an acp session id (see
                    // the ordinary stop path); identical to a clear otherwise.
                    if registry.settle_spawn_record(&name).is_ok() {
                        let _ = self.transition_with_log_capture(
                            registry,
                            &name,
                            LifecycleState::Stopping,
                            LifecycleState::Stopped,
                            TransitionCause::stop_forced(
                                "SIGKILL was sent by an earlier stop attempt; the \
                                 crash-detection reaper confirmed the process's death on a \
                                 later poll (it may have been stuck in an OS-level I/O wait \
                                 that has since cleared)",
                            ),
                            log_capture,
                        );
                    }
                    continue;
                }
                // Story 14-1: surface any queued acp notices before the
                // requested-stop handle drop (the reader's EOF notice).
                self.drain_acp_notices_for(&name);
                self.clear_poll_error_streak(&name);
                self.running.remove(&name);
                continue;
            }

            // A crash. Consult the Restart Policy FIRST (so a terminal outcome —
            // `never` or crash-loop — can enrich the recorded crash cause), then
            // apply running/paused → failed with that detail (AC5).
            //
            // Story 4-2, Task 4: capture the log_capture BEFORE removing the
            // entry below — same reasoning as `stop_inner`'s terminal
            // transition (the default `self.transition(...)` lookup would
            // otherwise miss it).
            //
            // AI-13: read the `adopted` flag BEFORE the remove below — an adopted
            // handle is not the engine's child, so a `code: None` exit means "the
            // exit code is UNAVAILABLE", and the cause must say so instead of
            // asserting a signal termination it cannot prove.
            let adopted = self.running.get(&name).is_some_and(|s| s.adopted);
            let crash_log_capture = self
                .running
                .get(&name)
                .and_then(|s| self.backend.log_capture(&s.handle));
            // AI-12b: capture the last poll error's text BEFORE the bookkeeping
            // clear below (the cause build needs it).
            let last_poll_error = self.poll_last_errors.get(&name).cloned();
            // Story 14-1: surface any queued acp notices before the crashed
            // instance's connection drops with the handle (the reader's EOF /
            // malformed-line / permission facts must not die silently with it).
            self.drain_acp_notices_for(&name);
            self.clear_poll_error_streak(&name);
            self.running.remove(&name);
            let base_detail = match crash {
                // AI-12: the handle went permanently un-pollable — the recorded
                // cause names the persistent poll failure, never a fabricated
                // exit. Loop 1: it also carries the LAST poll error's text
                // (truncated) so the operator gets the actual why.
                CrashInput::PersistentPollFailure { sole_handle } => {
                    let last = last_poll_error
                        .as_deref()
                        .unwrap_or("no error text recorded");
                    // AI-12 (loop 2): a lone held handle's errors could never be
                    // cross-checked against peers, so the cause says the
                    // single-handle caveat out loud instead of asserting a
                    // per-handle fault it cannot prove.
                    let corroboration = if sole_handle {
                        " — this was the ONLY held handle, so the error could not be \
                         corroborated against peers; if it recurs across restarts, check the \
                         platform's process-table source (procfs/sysctl) before blaming the \
                         agent"
                    } else {
                        ""
                    };
                    format!(
                        "persistent poll failure: {MAX_CONSECUTIVE_POLL_ERRORS} consecutive \
                         backend.poll errors — the handle's liveness could no longer be read, \
                         so it is not trusted as alive; last error: {last}{corroboration}"
                    )
                }
                CrashInput::Exited(Some(c)) => {
                    format!("process exited unexpectedly with code {c}")
                }
                // AI-13: an adopted process's exit code is unrecoverable on
                // UNIX (it is not this engine's child — only a parent gets an
                // ExitStatus), so THIS arm is the Unix-shaped case: the cause
                // says the code is unavailable and why, instead of asserting a
                // signal termination it cannot prove. On WINDOWS this arm is
                // reachable only when the code is GENUINELY unreadable: the
                // adopted handle's poll (backends/windows reap_if_exited)
                // reads the real exit code via GetExitCodeProcess, so a
                // Windows adopted exit normally lands in the `Exited(Some)`
                // arm above carrying its true code (the story-11-5 closure of
                // the 11-1 Windows-half defer).
                CrashInput::Exited(None) if adopted => "process exited unexpectedly (exit \
                 code unavailable — adopted process is not this engine's child)"
                    .to_string(),
                CrashInput::Exited(None) => {
                    "process exited unexpectedly (terminated by signal)".to_string()
                }
            };
            let decision = self.plan_restart(registry, &name, &base_detail);
            if self.ensure_log_dir(registry, &name).is_err() {
                // If we cannot even prepare the log dir, still persist the state
                // so the durable state leads; skip the event append best-effort.
            }
            // The recorded crash cause carries the full story: the exit detail,
            // plus (on a terminal outcome) the policy conclusion (crash-loop, or
            // "policy is never — not restarting"). This is what `instance_status`
            // falls back to for the failed cause once the terminal record is
            // cleared (AC9).
            if self
                .transition_with_log_capture(
                    registry,
                    &name,
                    state,
                    LifecycleState::Failed,
                    TransitionCause::crashed(decision.crash_cause.clone()),
                    crash_log_capture,
                )
                .is_err()
            {
                // Persisting the crash transition failed; leave the record for a
                // later reconcile and move on (do not panic the reaper).
                continue;
            }

            if let Some(plan) = decision.plan {
                plans.push(plan);
            }
        }

        // PHASE 4 — DECLARED LIVENESS (story 18-2). After crash handling: any
        // handle phase 3 reaped is gone from `self.running`, so this snapshot
        // holds only the SURVIVING instances. Each one whose adapter declared
        // a `[liveness]` check and whose persisted state is EXACTLY `running`
        // gets ONE attempt per pass (see the tick-stretch honesty in this
        // fn's docs). The window bookkeeping is the PURE
        // `WindowDecision`; expiry runs the crash-path sequence verbatim.
        let surviving: Vec<InstanceName> = self.running.keys().cloned().collect();
        for name in surviving {
            // Read the declared check + the open window (if any) up front, so
            // the mutable borrows below start clean.
            let (check, failing_since) = {
                let Some(supervised) = self.running.get(&name) else {
                    continue;
                };
                match &supervised.liveness_check {
                    Some(check) => (
                        check.clone(),
                        supervised.liveness_failing_since.map(|t| t.elapsed()),
                    ),
                    // No declared check: process liveness only — the
                    // byte-identical pre-1.1.0 behavior (and the adopted
                    // instance whose resolution failed: ONE diagnostic at
                    // adoption already said so, silent thereafter).
                    None => continue,
                }
            };
            // The paused guard: ONLY an instance the store shows EXACTLY
            // `running` is probed. A paused instance is never probed (a
            // suspended process cannot heartbeat — SIGSTOP, or a Windows
            // pause that was a no-op: neither may fail the agent), and a
            // `stopping`/terminal row is somebody else's transition. SILENT
            // skip by design — a pause is a normal state, not a degradation.
            //
            // Review round 1 (P2): a store-read ERROR is its own arm — the
            // state is UNREADABLE, which is not evidence the instance is not
            // running, so the check is PAUSED for this tick (never fail an
            // instance on an unreadable state) and the degradation is
            // SURFACED once per episode: the first Err for a name emits ONE
            // diagnostic; further Errs stay silent (no per-tick spam while
            // the store is down) until a read recovers — an Ok clears the
            // episode silently and checks resume.
            #[cfg(test)]
            if self.liveness_lookup_fault_names.contains(&name) {
                // cfg(test) seam (P2's driver): behave exactly like a store
                // read failure, deterministically (a real outage cannot be
                // staged in-process). The SAME episode bookkeeping as the
                // real Err arm — the seam exercises the whole arm, not just
                // the diagnostic.
                if self.liveness_lookup_err_names.insert(name.clone()) {
                    self.emit_liveness_lookup_degraded(
                        &name,
                        "injected cfg(test) liveness-lookup fault (store read failed)",
                    );
                }
                continue;
            }
            match registry.lookup(&name) {
                Ok(instance) => {
                    // A successful read ends the degraded-read episode (if
                    // one was open) — silently; recovery is not news, the
                    // ONE degradation diagnostic already covered it.
                    self.liveness_lookup_err_names.remove(&name);
                    if instance.state != LifecycleState::Running {
                        continue;
                    }
                }
                Err(lookup_err) => {
                    if self.liveness_lookup_err_names.insert(name.clone()) {
                        // First Err THIS episode: the ONE diagnostic.
                        self.emit_liveness_lookup_degraded(&name, &lookup_err.to_string());
                    }
                    continue;
                }
            }
            let max_age_ms = check.max_age_ms();
            let failing_now =
                liveness::evaluate(&check, &registry.agent_home(&name)) == LivenessSignal::Failing;
            match liveness::window_decision(failing_since, failing_now, max_age_ms) {
                WindowDecision::Healthy => {}
                WindowDecision::WithinWindow { .. } => {
                    // A transient blip inside the window: silent — it is none
                    // of the operator's business until it persists.
                }
                WindowDecision::StartWindow => {
                    // FIRST failing attempt: open the window, emit the ONE
                    // first-failure diagnostic (surfaced, not silent — the
                    // operator sees the window open while there is still time
                    // to care).
                    //
                    // Review round 1 (P6): the pair is BOUNDED. Once this Run
                    // has flapped through the diagnostic budget (3 full
                    // open→recover cycles), further window-opens open
                    // SILENTLY — the ONE suppression note (emitted at the
                    // third recovery) already said the signal is flapping.
                    // The window itself always opens and always still
                    // expires; only the chatter is bounded.
                    let suppressed = {
                        let Some(supervised) = self.running.get_mut(&name) else {
                            continue;
                        };
                        supervised.liveness_failing_since = Some(std::time::Instant::now());
                        supervised.liveness_reopens >= LIVENESS_REOPEN_DIAGNOSTIC_BUDGET
                    };
                    if !suppressed {
                        self.emit_diagnostic(&format!(
                            "{}: declared liveness check failing ({}) — the window is open; \
                             failing continuously for {}ms fails the instance",
                            name.as_str(),
                            liveness::describe(&check),
                            max_age_ms,
                        ));
                    }
                }
                WindowDecision::Recovered => {
                    // The signal came back inside the window: close the
                    // window, emit the ONE recovery diagnostic (a failing
                    // agent that came back is exactly what an operator wants
                    // to know about).
                    //
                    // Review round 1 (P6): count the completed flap cycle.
                    // Recoveries inside the diagnostic budget (the first 3)
                    // each emit; AT the third, ONE flap note explains that
                    // further reopen diagnostics are suppressed; later
                    // recoveries are silent (the operator has been told).
                    let reopens = {
                        let Some(supervised) = self.running.get_mut(&name) else {
                            continue;
                        };
                        supervised.liveness_failing_since = None;
                        supervised.liveness_reopens = supervised.liveness_reopens.saturating_add(1);
                        supervised.liveness_reopens
                    };
                    if reopens <= LIVENESS_REOPEN_DIAGNOSTIC_BUDGET {
                        self.emit_diagnostic(&format!(
                            "{}: declared liveness check recovered ({}) — the failure window \
                             closed before expiring",
                            name.as_str(),
                            liveness::describe(&check),
                        ));
                    }
                    if reopens == LIVENESS_REOPEN_DIAGNOSTIC_BUDGET {
                        // Exactly once (the count never revisits this value).
                        self.emit_diagnostic(&format!(
                            "{}: further liveness-window reopen diagnostics suppressed \
                             (the signal is flapping; the window still never expires while \
                             it recovers)",
                            name.as_str()
                        ));
                    }
                }
                WindowDecision::Expired { failing_for_ms } => {
                    // UNRESPONSIVE. The crash-path sequence verbatim (phase 3's
                    // terminal drains → capture → notices → remove → policy
                    // consult → transition), with the `unresponsive` cause and
                    // the guaranteed-Running prior state — plus the review
                    // round 1 hardenings: the detached-handle force-kill (P4),
                    // the onset honesty (P7), the detail-carrying cause (P8),
                    // and the GUARDED commit (P3).
                    let check_desc = liveness::describe(&check);
                    self.drain_self_reported_for(registry, &name, DrainMode::Terminal);
                    self.drain_observed_for(registry, &name, DrainMode::Terminal);
                    self.unlink_raw_logs_if_metadata(registry, &name);
                    // Capture BEFORE the remove — the drop of the (possibly
                    // hung) handle is the sanctioned teardown (kill-on-drop,
                    // the AI-12 PersistentPollFailure precedent: a wedged
                    // agent will not stop on request, and a bounded-stop
                    // ladder must not wedge the reaper).
                    let liveness_log_capture = self
                        .running
                        .get(&name)
                        .and_then(|s| self.backend.log_capture(&s.handle));
                    self.drain_acp_notices_for(&name);
                    self.clear_poll_error_streak(&name);
                    // P4 (review round 1): a DETACHED handle has NO
                    // kill-on-drop — dropping it below would release the
                    // handle and leave the wedged process RUNNING (holding
                    // its ports, still unresponsive — the "failure" would be
                    // bookkeeping, not teardown). The disarm flag's single
                    // source is the spawn record (spawn and adopt both pass
                    // it); when it says detached and the process is still
                    // ALIVE, escalate to the stop ladder's final leg (a
                    // zero-window stop is SIGKILL + confirm on unix) BEFORE
                    // the removal — the same escalation shape the
                    // readiness-timeout teardown uses (18-1 review E2). A
                    // non-detached (owned) handle keeps the kill-on-drop
                    // drop, unchanged.
                    let detached = registry
                        .spawn_record(&name)
                        .ok()
                        .flatten()
                        .map(|record| record.detach)
                        .unwrap_or(false);
                    if detached {
                        if let Some(supervised) = self.running.get_mut(&name) {
                            if matches!(
                                self.backend.poll(&mut supervised.handle),
                                Ok(ProcessStatus::Alive)
                            ) {
                                if let Err(kill_err) =
                                    self.backend.stop(&mut supervised.handle, Duration::ZERO)
                                {
                                    // Surfaced (AI-18): the kill failed — the
                                    // operator must know the teardown was not
                                    // completed, not silently drop a live
                                    // handle.
                                    self.emit_diagnostic(&format!(
                                        "{}: liveness-expiry teardown could not force-kill \
                                         the detached, unresponsive process: {kill_err}",
                                        name.as_str()
                                    ));
                                }
                            }
                        }
                    }
                    self.running.remove(&name);
                    // P7 (review round 1): the honest onset note.
                    // `failing_for_ms` counts from the FIRST OBSERVED
                    // failure; for a heartbeat file the engine CAN say how
                    // much staler the file already was at that observation
                    // (stat the file now, compare its age to the observed
                    // streak) — the true staleness is at least their sum,
                    // and the observation is the lower bound, not the whole
                    // truth. An http-get has no statable onset (a refused
                    // connection leaves no artifact) — the note is
                    // heartbeat-file-only.
                    let onset_note = liveness_expiry_onset_note(
                        &check,
                        &registry.agent_home(&name),
                        failing_for_ms,
                    );
                    let mut detail = format!(
                        "declared liveness check unresponsive: {check_desc} failing for \
                         {failing_for_ms}ms (window {max_age_ms}ms)"
                    );
                    if let Some(note) = &onset_note {
                        detail.push_str("; ");
                        detail.push_str(note);
                    }
                    // P8 (review round 1): the policy consult happens BEFORE
                    // the commit — the phase-3 order. On a TERMINAL outcome
                    // (policy `never`, or the crash-loop threshold) the
                    // verdict folds into the PERSISTED cause's `detail`
                    // (combined with the onset note when both exist), so the
                    // event log and `show`'s failed-cause fold carry the
                    // "will never restart" conclusion — not only the one
                    // console diagnostic this arm keeps below.
                    let decision = self.plan_restart(registry, &name, &detail);
                    let terminal_verdict = if decision.plan.is_none() {
                        // `plan_restart` prefixes the verdict with the base
                        // detail it was handed; strip that back off so the
                        // cause's `detail` field carries JUST the verdict —
                        // the cause already renders the check + streak, and
                        // re-duplicating them would garble `show`.
                        Some(
                            decision
                                .crash_cause
                                .strip_prefix(&format!("{detail}; "))
                                .unwrap_or(&decision.crash_cause)
                                .to_string(),
                        )
                    } else {
                        None
                    };
                    let cause = match (&onset_note, terminal_verdict) {
                        (Some(note), Some(verdict)) => TransitionCause::unresponsive_with_detail(
                            check_desc.clone(),
                            failing_for_ms,
                            format!("{note}; {verdict}"),
                        ),
                        (Some(note), None) => TransitionCause::unresponsive_with_detail(
                            check_desc.clone(),
                            failing_for_ms,
                            note.clone(),
                        ),
                        (None, Some(verdict)) => TransitionCause::unresponsive_with_detail(
                            check_desc.clone(),
                            failing_for_ms,
                            verdict,
                        ),
                        (None, None) => {
                            TransitionCause::unresponsive(check_desc.clone(), failing_for_ms)
                        }
                    };
                    if self.ensure_log_dir(registry, &name).is_err() {
                        // Same posture as the crash path: the state persists
                        // even if the log dir cannot be prepared; the event
                        // append below stays best-effort.
                    }
                    // P3 (review round 1): the GUARDED commit. The phase-4
                    // lookup read `running` a moment ago, but a concurrent
                    // command (another CLI engine on the same state root)
                    // may have legitimately moved the row since — the write
                    // commits ONLY when the row is still `running`. A
                    // refusal is the ONE skip-diagnostic with no failure and
                    // NO restart plan: the state moved, so whoever drove
                    // that transition owns this instance's lifecycle now.
                    // HONEST ORDER NOTE (P8): the consult above has ALREADY
                    // charged the counter / settled the record by the time
                    // this commit runs — the same consult-then-persist order
                    // the crash path uses — so a `StateConflict` refusal
                    // leaves that charge in place. The failure was real,
                    // whoever drove the concurrent transition owns the row,
                    // and the charge stands.
                    if let Err(err) = self.transition_if_prior(
                        registry,
                        &name,
                        LifecycleState::Running,
                        LifecycleState::Failed,
                        cause,
                        liveness_log_capture,
                    ) {
                        match err {
                            EngineError::Store(StoreError::StateConflict { .. }) => {
                                self.emit_diagnostic(&format!(
                                    "{}: liveness expiry skipped — the instance state \
                                     moved before the failure committed; the instance is \
                                     left to whoever drove that transition",
                                    name.as_str()
                                ));
                            }
                            _ => {
                                // Persisting the transition failed (event
                                // append / compensation); leave the record
                                // for a later reconcile and move on (never
                                // panic the reaper).
                            }
                        }
                        continue;
                    }
                    match decision.plan {
                        Some(plan) => plans.push(plan),
                        None => {
                            // Terminal (policy `never`, or the crash-loop
                            // threshold): keep the ONE immediate-console
                            // diagnostic — the verdict ALSO rides the
                            // persisted cause above (P8), so the event log
                            // and `show` carry it durably; this is the
                            // operator-console surface, emitted exactly once
                            // (surfaced, not silent).
                            self.emit_diagnostic(&format!(
                                "{}: {}",
                                name.as_str(),
                                decision.crash_cause
                            ));
                        }
                    }
                }
            }
        }
        plans
    }

    /// Decide the Restart Policy action for a just-crashed instance (AC4).
    ///
    /// Reads the per-instance record (policy + current consecutive count) and
    /// returns a [`RestartDecision`]: the crash cause to record in the event log
    /// (enriched with the policy conclusion on a terminal outcome) and, when a
    /// restart is scheduled, the [`RestartPlan`]. Side effects (all best-effort —
    /// a store hiccup is never a panic):
    /// * `on-failure`, below the crash-loop threshold → increment the persisted
    ///   restart count; the plan carries the backoff delay for that attempt.
    /// * `on-failure`, at the crash-loop threshold ([`is_crash_loop`]) → TERMINAL:
    ///   CLEAR the write-ahead record (F-Low-2: no needless adopt-attempt against
    ///   a dead/reused PID on a later open) and enrich the crash cause with the
    ///   crash-loop reason; no plan.
    /// * `never` → TERMINAL: clear the write-ahead record and note the policy in
    ///   the crash cause; no plan.
    ///
    /// Also called by the START path (spawn.rs) when the readiness probe
    /// observes the process EXIT mid-wait: the write-ahead record exists by
    /// then (committed before the probe), so the policy consult is identical.
    pub(super) fn plan_restart(
        &self,
        registry: &Registry,
        name: &InstanceName,
        crash_detail: &str,
    ) -> RestartDecision {
        let record = registry.spawn_record(name).ok().flatten();
        let policy = record
            .as_ref()
            .map(|r| r.restart_policy)
            .unwrap_or_default();
        let current = record.as_ref().map(|r| r.restart_count).unwrap_or(0);

        if !policy.restarts_on_crash() {
            // `never`: TERMINAL. Settle the record so a later open does not
            // adopt-attempt a dead PID; the crash cause names the policy.
            self.settle_terminal_record(registry, name, policy);
            return RestartDecision {
                crash_cause: format!("{crash_detail}; restart policy is 'never' — not restarting"),
                plan: None,
            };
        }

        let next = current.saturating_add(1);
        if is_crash_loop(next) {
            // Crash loop: TERMINAL. Settle the record (F-Low-2), leave `failed`
            // with the reason STATED in the crash cause.
            self.settle_terminal_record(registry, name, policy);
            return RestartDecision {
                crash_cause: format!(
                    "{crash_detail}; crash-loop: {} consecutive failures reached — \
                     not restarting, inspect the agent and start it manually",
                    MAX_CONSECUTIVE_FAILURES,
                ),
                plan: None,
            };
        }

        // Schedule a restart: persist the incremented count + the crash cause,
        // and return the plan with the backoff delay for this attempt.
        let _ = registry.set_restart_count(name, next, Some(crash_detail));
        let delay = self.backoff.delay_for(next);
        RestartDecision {
            crash_cause: crash_detail.to_string(),
            plan: Some(RestartPlan {
                name: name.clone(),
                attempt: next,
                delay,
            }),
        }
    }

    /// Settle the write-ahead record on a TERMINAL `failed` outcome (F-Low-2).
    ///
    /// Drops the record's LIVE fingerprint (so a later [`Supervisor::adopt_orphans`]
    /// does NOT adopt-attempt the dead/reused PID — the reconcile skips a pid-0
    /// record, exactly like a policy-only config seed), while RE-SEEDING the
    /// per-instance policy so `kt agent show` still reports the active Restart
    /// Policy for the failed instance (AC9). Concretely: settle the record
    /// (clearing it, and — story 14-2 — retaining an established acp session
    /// id on the pid-0 seed row so a later start can offer it via
    /// `session/load`), then re-persist the policy as a pid-0 seed. The failed
    /// CAUSE is not kept in the record — it rides in the event log, which
    /// `instance_status` falls back to. Best-effort (a store hiccup here is
    /// never a panic).
    fn settle_terminal_record(
        &self,
        registry: &Registry,
        name: &InstanceName,
        policy: RestartPolicy,
    ) {
        let _ = registry.settle_spawn_record(name);
        let _ = registry.set_restart_policy(name, policy);
    }

    /// Adopt orphaned processes on engine start (story 1-6, AC-B / AC7 / AI-7 /
    /// AI-8) — the HONEST cross-lifetime reconcile.
    ///
    /// Reads EVERY write-ahead [`SpawnRecord`] (AD-5) and, for each, asks the
    /// backend to re-acquire a live process matching the fingerprint
    /// (`backend.adopt`):
    /// * `Some(handle)` — a live process whose start-time matches: ADOPT it
    ///   (re-hold the handle so `stop`/`pause`/`poll` work again); the persisted
    ///   state stays as-is (`running`/`paused` — AI-7: a live paused process is
    ///   re-held so a later `resume` works).
    /// * `None` — no live match (PID gone, or reused by a different process):
    ///   reconcile HONESTLY to `failed` with an "orphan not found" cause + the
    ///   last-known cause (AI-8: never leave a phantom `running`/`paused` row),
    ///   and clear the record.
    ///
    /// Called from [`Engine::open`]. Best-effort per record: a single
    /// adopt/persist failure does not abort the whole reconcile; it leaves that
    /// record for the next open. Returns the number of processes adopted (for
    /// diagnostics/tests).
    pub fn adopt_orphans(&mut self, registry: &Registry) -> usize {
        let records = match registry.list_spawn_records() {
            Ok(records) => records,
            Err(_) => return 0,
        };
        // The LIVE-OWNER adoption boundary: THIS process's own fingerprint,
        // read once — the identity a record's owner is compared against (a
        // record this process itself spawned or adopted is always adoptable
        // by this process; the boundary only guards OTHER live engines).
        let self_owner = self.backend.self_fingerprint();
        let mut adopted = 0;
        for record in records {
            let name = record.name.clone();
            // A pid-0 record is a policy-only config SEED (set via
            // `set_restart_policy` before the instance was ever started), NOT a
            // supervised process — skip it (it names no real process to adopt or
            // fail, and clearing it would wipe the persisted policy).
            if record.fingerprint.pid == 0 {
                continue;
            }
            // The live-owner gate (NON-detached records only — a detached
            // record's adoption is disarmed by its detach flag, so a live
            // owner costs nothing there and the durable-detach promise is
            // untouched): a record whose recorded SUPERVISING OWNER is still
            // ALIVE in another process is supervised elsewhere — re-holding
            // it HERE would arm a second kill-on-drop handle whose teardown
            // at this command's exit stops another engine's agent (the
            // `hekma agent list`-kills-`serve`'s-agent bug). SKIP it: the
            // row and record stay exactly as the live owner left them, and
            // its owner keeps the sole handle (crash detection, budgets,
            // stops all stay ITS). An owner that is DEAD (the story 1-6
            // crashed engine — the exact case adoption exists for), REUSED
            // (start-time mismatch), ABSENT (pre-boundary record), or THIS
            // process adopts exactly as before. `probe_alive == None`
            // (indeterminate) also adopts — the fail-open posture that
            // keeps NFR-1 intact on a flaky read. Surfaced, not silent
            // (AI-18): the skip is named so a confused operator reads WHY
            // `stop` from this command will refuse.
            if !record.detach {
                if let Some(owner) = record.owner {
                    let owner_is_self = self_owner.is_some_and(|me| me.matches(&owner));
                    if !owner_is_self && self.backend.probe_alive(&owner) == Some(true) {
                        self.emit_diagnostic(&format!(
                            "{}: supervised by another live engine (pid {}) — its spawn \
                             record is left untouched and THIS engine holds no handle for \
                             it; an explicit stop re-acquires the process on demand, and \
                             every benign command leaves it alone",
                            name.as_str(),
                            owner.pid,
                        ));
                        continue;
                    }
                }
            }
            // The record's detach flag (story 12-1 AMENDMENT) decides HOW the
            // handle is re-held: a detached spawn's handle comes back DISARMED
            // (its Drop does not kill), so this engine's clean exit leaves the
            // agent alive for the command after it; a supervised spawn's
            // handle keeps the story 1-6 kill-on-drop guarantee.
            match self.backend.adopt(&record.fingerprint, record.detach) {
                Ok(Some(handle)) => {
                    // Live match: re-hold the handle. State stays as persisted
                    // (running/paused). AI-7: a paused process is now resumable.
                    //
                    // Metering across a crash/adoption (story 3-1, documented
                    // assumption; story 15-2 AMENDED): the Run id now RIDES the
                    // spawn record, so the adopted instance RESUMES the previous
                    // engine's Run instead of opening a new one — per-run totals
                    // keep accumulating under the same id across commands. The
                    // ingestion cursor still anchors at the CURRENT end of the
                    // agent-output log (skipping pre-adoption lines), so nothing
                    // is re-attributed or double-counted: the DB dedup key
                    // includes the run id, and the pre-adoption lines were either
                    // already committed under this same Run or were never
                    // ingested by the dead engine (the park-loss note above
                    // names that window).
                    //
                    // AI-18 (surfaced-not-silent, 2026-09-22 hardening): the cursor
                    // re-derivation skips everything the previous engine had NOT
                    // yet flushed — its in-memory park (the observed pending buffer
                    // and the self-reported parked tail die with the process). An
                    // adoption previously noted only the detach/session facts; the
                    // un-flushed window went unmentioned (the AI-18 comment-only-
                    // caveat shape). This diagnostic names it, once per successful
                    // adoption, through the choke point.
                    self.emit_diagnostic(&format!(
                        "{}: adopted from a previous engine; metering resumes at the \
                         current end of the instance logs, so usage the previous engine \
                         had read but not yet recorded when it exited is not counted",
                        name.as_str(),
                    ));
                    // Story 15-2: RESUME the persisted Run id (the durable Run
                    // identity on the spawn record, schema v9) instead of
                    // minting — a detached instance is ONE Run across the N
                    // commands of its life, so per-run token/dollar totals keep
                    // accumulating under the same id and an already-recorded
                    // breach for this Run is spent (the latch below seeds from
                    // breaches.log). A record with NO id is a pre-v9 record
                    // (pid-0 policy seeds were skipped above): mint, PERSIST the
                    // mint back onto the record, and surface a ONE-TIME note —
                    // the next command resumes the id, so both are genuinely
                    // once-per-upgrade, not once-per-command.
                    let run_id = match record.run_id.clone() {
                        Some(persisted) => persisted,
                        None => {
                            // Review round 1: the mint note comes AFTER the
                            // successful write-back (the Err arm carries the
                            // failure) — a note printed before the write
                            // could claim "persisted" of an id that never
                            // landed.
                            let minted = RunId::mint();
                            match registry.set_spawn_run_id(&name, &minted, &record.fingerprint) {
                                Ok(()) => {
                                    self.emit_diagnostic(&format!(
                                        "{}: the spawn record predates durable Run ids — a fresh \
                                         Run id was minted for it and persisted; per-run totals and \
                                         breach state continue under it from the next command on",
                                        name.as_str(),
                                    ));
                                }
                                // Surfaced-not-silent (AI-18): without the
                                // write-back every later command mints AGAIN
                                // (the pre-v9 behavior) — the operator must
                                // hear that, not infer it from resetting
                                // per-run totals. (The CAS names the rarer
                                // concurrent stop→start loss itself, where
                                // the winner's fresh Run id — not a fresh
                                // mint — is what the next command resumes.)
                                Err(err) => {
                                    self.emit_diagnostic(&format!(
                                        "{}: could not persist the minted Run id onto the \
                                         spawn record ({err}) — each later command mints a \
                                         fresh Run until a write succeeds (or, if a concurrent \
                                         stop→start replaced the record, resumes ITS fresh Run)",
                                        name.as_str(),
                                    ));
                                }
                            }
                            minted
                        }
                    };
                    // Story 15-2: the DURABLE latch — seed the in-memory
                    // per-Run breach latch from the breaches ALREADY RECORDED
                    // for the resumed Run (breaches.log keys every event by
                    // run_id; the append-only durable record IS the spent
                    // check). The adoption-time enforce_budget call below
                    // still evaluates — but an already-recorded
                    // (dimension, scope) short-circuits, so a breach that
                    // fired before the engine change does NOT re-record a
                    // duplicate breaches.log line or re-fire the action
                    // (Fibery 205), while a genuinely un-recorded crossing
                    // still fires exactly once (AI-44, now durable: the
                    // crash-between-record-and-action window self-heals on
                    // the FIRST adoption, not the next usage event).
                    //
                    // Review round 1, ACTION FILTER: only events whose ACTION
                    // equals the CURRENTLY-CONFIGURED action for their
                    // dimension seed the latch — a reconfigured action
                    // (e.g. `warn` → `pause`) must fire ONCE under the
                    // resumed Run, not stay suppressed forever by the idempotence
                    // latch keyed only on (dimension, scope). A config read
                    // that fails degrades to an EMPTY latch (fail-open toward
                    // one extra enforcement pass — the same direction as the
                    // corrupt-log arm below; the enforcement call itself
                    // returns early on a failed config read, so the seed is
                    // moot in exactly that case).
                    let (token_action, cost_action) = registry
                        .effective_config(&name, ConfigLayer::empty())
                        .map(|effective| {
                            let (_budget, token_action) = config::resolve_token_budget(&effective);
                            let (_rate, _cap, cost_action) = config::resolve_cost(&effective);
                            (token_action, cost_action)
                        })
                        .unwrap_or((BreachAction::Pause, BreachAction::Pause));
                    let breach_log_events =
                        super::read_breach_events_from(&registry.instance_breach_log_path(&name));
                    let breach_log_events = match breach_log_events {
                        Ok(events) => events,
                        Err(detail) => {
                            // Review round 1 (AI-18): the corrupt log's
                            // fail-open is SURFACED, never silent — the
                            // operator hears that the durable spent-check
                            // was unreadable and the latch degrades to
                            // empty (one extra enforcement pass may re-fire,
                            // a duplicate breaches.log line may land).
                            self.emit_diagnostic(&format!(
                                "{}: the instance's breach log could not be read \
                                 ({detail}) — the breach latch degrades to empty, so an \
                                 already-recorded breach may fire (and re-record) ONCE \
                                 more under the resumed Run",
                                name.as_str(),
                            ));
                            Vec::new()
                        }
                    };
                    let breached_scopes = breach_log_events
                        .into_iter()
                        .filter(|event| event.run_id == run_id.as_str())
                        .filter(|event| match event.dimension {
                            BreachDimension::Tokens => event.action == token_action,
                            BreachDimension::Dollars => event.action == cost_action,
                        })
                        .map(|event| (event.dimension, event.scope))
                        .collect::<std::collections::HashSet<_>>();
                    let usage_cursor = self.agent_log_len(registry, &name);
                    // Story 14-3 (T3): the acp kind's stderr sentinel channel
                    // anchors the same way (at the CURRENT end of the captured
                    // stderr log, skipping pre-crash lines — the documented
                    // adoption posture above, per channel).
                    let stderr_usage_cursor = self.agent_stderr_log_len(registry, &name);
                    // Story 14-3 (T2): the acp kind's ACTIVE source resolves
                    // from its effective config (the same resolution the start
                    // seam and the Fleet read use), so an adopted instance that
                    // opted into the observed channel keeps surfacing that
                    // source. A degraded config read falls back to the snapshot
                    // value already resolved above (the AI-46 note stays the
                    // loud path for a snapshot failure).
                    let adopted_is_acp = registry
                        .lookup(&name)
                        .map(|instance| crate::acp::is_acp_kind(&instance.kind))
                        .unwrap_or(false);
                    let metering_source = if adopted_is_acp {
                        registry
                            .effective_config(&name, ConfigLayer::empty())
                            .map(|effective| {
                                crate::acp::resolve_acp_metering_source(&effective).to_string()
                            })
                            .unwrap_or_else(|err| {
                                let unclear = format!(
                                    "{}: the adopted acp instance's effective config could \
                                     not be read ({err}); its metering source stays the \
                                     snapshot value until the next start",
                                    name.as_str(),
                                );
                                self.emit_diagnostic(&unclear);
                                registry
                                    .metering_source(&name)
                                    .unwrap_or_else(|_| "self-reported".to_string())
                            })
                    } else {
                        registry.metering_source(&name).unwrap_or_else(|err| {
                            // AI-46 (review loop 1): a registry read hiccup must
                            // not SILENCE the stranded-listener diagnostic —
                            // defaulting to `self-reported` here would skip the
                            // one announcement an actually-observed orphan
                            // needs. Announce the ambiguity loudly, then use
                            // the neutral fallback for bookkeeping.
                            let unclear = format!(
                                "{}: the adopted instance's metering source could not be \
                                     read ({err}); if it is engine-observed, its injected \
                                     'metering.base_url' points at the PREVIOUS engine's dead \
                                     loopback listener — stop the instance and start it again \
                                     to re-anchor the listener",
                                name.as_str(),
                            );
                            self.emit_diagnostic(&unclear);
                            "self-reported".to_string()
                        })
                    };
                    // Clone the Run context into `Supervised` — the AI-44
                    // enforcement call below borrows the same values afterwards.
                    // Story 17-2 (the AI-46 closure, the `metering.listen`
                    // seam): when THIS engine opened with a FIXED metering
                    // port, an adopted engine-observed instance gets a LIVE
                    // listener re-bound on that port before it is re-held —
                    // the injected `base_url` of an instance whose spawner ran
                    // the same fixed port is live again with NO stop→start.
                    // Port 0 (every pre-17-2 open) keeps the story-11-3
                    // strand diagnostic below unchanged (a re-bind cannot
                    // guess the previous engine's ephemeral port). The
                    // DECISION itself is the pure
                    // [`adoption_rebind_decision`] seam (review finding 14:
                    // unit-tested on every OS — the process legs skip on the
                    // linux CI runner).
                    let adopted_listener =
                        match adoption_rebind_decision(&metering_source, self.metering_listen_port)
                        {
                            AdoptionRebind::Attempt => {
                                match registry.effective_config(&name, ConfigLayer::empty()) {
                                    Ok(effective) => {
                                        match self.start_observed_listener(
                                            &name,
                                            &metering_source,
                                            &effective,
                                        ) {
                                            // `start_observed_listener` returns Some only for
                                            // an engine-observed instance — which this is.
                                            Ok(Some(listener)) => {
                                                self.emit_diagnostic(&format!(
                                            "{}: re-bound the observed-metering listener on the \
                                             fixed loopback port {} — an instance started under \
                                             the SAME fixed port is live again (no stop/start \
                                             needed); one inherited from an ephemeral-port engine \
                                             still needs stop→start to re-inject its base_url",
                                            name.as_str(),
                                            listener.local_addr().port(),
                                        ));
                                                Some(listener)
                                            }
                                            Ok(None) => None,
                                            Err(err) => {
                                                // AI-18: a failed re-bind is SURFACED (the
                                                // EngineError detail is traffic-free by
                                                // construction — bind/upstream shape only),
                                                // never a silent strand; the strand note
                                                // below still applies and fires too.
                                                self.emit_diagnostic(&format!(
                                                    "{}: could not re-bind the observed-metering \
                                             listener on the fixed port {} ({}) — the adopted \
                                             instance keeps NO listener; free the port (or stop \
                                             and start the instance) to restore its model \
                                             traffic",
                                                    name.as_str(),
                                                    self.metering_listen_port,
                                                    err,
                                                ));
                                                None
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        // A config-read failure degrades to the
                                        // pre-17-2 strand posture (no listener),
                                        // SURFACED — the strand note below fires too.
                                        self.emit_diagnostic(&format!(
                                            "{}: the adopted engine-observed instance's effective \
                                     config could not be read ({err}); its metering listener \
                                     could not be re-bound on the fixed port {} — stop and \
                                     start the instance to restore its model traffic",
                                            name.as_str(),
                                            self.metering_listen_port,
                                        ));
                                        None
                                    }
                                }
                            }
                            AdoptionRebind::SkipNotObserved | AdoptionRebind::SkipNoFixedPort => {
                                None
                            }
                        };
                    let adopted_observed_source = adopted_listener
                        .as_ref()
                        .map(|_| ObservedUsageSource::new());
                    // Read BEFORE the listener moves into `Supervised` (the
                    // strand note below must know whether a live listener landed).
                    let rebind_landed = adopted_listener.is_some();
                    // Story 18-2: re-resolve the adopted instance's declared
                    // liveness check from ITS adapter manifest (the same
                    // resolution the start seam runs). The window itself does
                    // NOT survive the previous engine — the prior failure
                    // streak lived in the dead engine's `Supervised` — so an
                    // adopted instance starts a FRESH window (the honest
                    // degradation, documented: up to one extra window of
                    // grace). A resolution failure (manifest drifted invalid,
                    // or unreadable since registration) is SURFACED (AI-18)
                    // as ONE diagnostic and the adopted instance is re-held
                    // with NO declared check — process liveness only — never
                    // a failed adoption (graceful degradation: the agent is
                    // running and owned either way).
                    let adopted_liveness_check = match registry.adapter_launch_facts(&name) {
                        Ok((kind, manifest_path, _launch)) => {
                            match crate::adapter::resolve_liveness_check(
                                &kind,
                                manifest_path.as_deref(),
                            ) {
                                Ok(check) => check,
                                Err(err) => {
                                    self.emit_diagnostic(&format!(
                                        "{}: the adopted instance's declared liveness check \
                                         could not be resolved ({err}); supervising PROCESS \
                                         liveness only for this Run — stop and start the \
                                         instance to restore the declared check",
                                        name.as_str(),
                                    ));
                                    None
                                }
                            }
                        }
                        Err(err) => {
                            self.emit_diagnostic(&format!(
                                "{}: the adopted instance's adapter snapshot could not be \
                                 read ({err}); its declared liveness check (if any) is NOT \
                                 supervised this Run — stop and start the instance to \
                                 restore it",
                                name.as_str(),
                            ));
                            None
                        }
                    };
                    self.clear_poll_error_streak(&name);
                    self.running.insert(
                        name.clone(),
                        Supervised {
                            handle,
                            run_id: run_id.clone(),
                            metering_source: metering_source.clone(),
                            usage_cursor,
                            usage_park_attempts: None,
                            // Story 14-3 (T3): the adopted instance's stderr
                            // sentinel channel anchors fresh (no parked state
                            // survives the prior engine).
                            stderr_usage_cursor,
                            stderr_usage_park_attempts: None,
                            // A fresh (adopted) Run has seen no sentinel lines.
                            sentinel_lines_seen: false,
                            // Story 14-3 (T2): gates the stderr sentinel drain.
                            is_acp: adopted_is_acp,
                            // Story 12-4: an adopted instance starts with no parked
                            // observed events (its prior engine's park died with it).
                            observed_park: None,
                            // Story 15-2: the DURABLE latch — seeded above from
                            // the breaches already recorded for the RESUMED Run
                            // (breaches.log keys events by run_id). The adopted
                            // Run is the SAME Run the previous engine held (the
                            // id resumed from the spawn record), so a breach
                            // that already fired for it is spent here — once per
                            // RUN, not once per command (AI-44's intent, made
                            // durable). A fresh Run (stop→start) still begins
                            // with an empty latch at the spawn site.
                            breached_scopes,
                            // ENGINE-OBSERVED across a crash/adoption (story 3-4,
                            // tracked follow-up — NOT just a metering gap): the pre-crash
                            // listener died with the crashed engine, but the already-
                            // running agent's `base_url` STILL points at that now-DEAD
                            // loopback port. So the adopted agent's MODEL TRAFFIC ITSELF
                            // breaks — its completion calls hit the dead port and fail
                            // with a connection-refused error (not merely un-metered).
                            // This fails LOUD (a transport error the agent surfaces),
                            // never a corrupt/silent-wrong output. We cannot rebind the
                            // old port to a fresh listener here (the agent chose no port;
                            // the OS did), so we leave it un-observed with no listener;
                            // RECOVERY is an operator stop→start, which relaunches the
                            // agent pointed at a fresh listener. The full fix (re-launch
                            // an adopted observed instance / a stable per-instance listener
                            // port / the Epic-7 daemon owning the listener) is a tracked
                            // follow-up, not done here. A self-reported instance's
                            // log-tail drain is unaffected (it needs no listener).
                            // Story 17-2: an adopted engine-observed instance
                            // whose spawner ran the SAME fixed metering port
                            // is re-bound live above; `None` (port-0 opens,
                            // failed re-binds) keeps the story-11-3 strand
                            // posture, and the strand diagnostic below says
                            // so honestly.
                            observed_listener: adopted_listener,
                            observed_source: adopted_observed_source,
                            // An adopted instance's stop attempt has not
                            // happened yet in THIS engine session.
                            stop_unconfirmed: false,
                            // AI-13: this handle was re-acquired, not spawned.
                            // On UNIX its exit code is unrecoverable (the
                            // adopted-exit crash cause says so); on WINDOWS
                            // the adopted handle's poll still reads the real
                            // exit code via GetExitCodeProcess
                            // (backends/windows reap_if_exited), so only a
                            // genuinely unreadable code falls to the
                            // unavailable-code cause there.
                            adopted: true,
                            // Story 17-4 (review finding 11): an adopted
                            // Run's capture posture is the ORIGINATING
                            // engine's, which this engine cannot know —
                            // `None` keeps the host's mode from ever
                            // unlinking another engine's raw logs, and the
                            // capture-read refusal qualifies the unknown
                            // provenance instead of claiming a shape.
                            capture_mode: None,
                            // Story 14-1 (spine AD-19): an ADOPTED acp instance
                            // is re-held as a bare process — its ACP pipe
                            // halves died with the engine that spawned it, so
                            // NO connection is re-established here (the honest
                            // note below says so; `session/load` resume is
                            // story 14-2).
                            acp: None,
                            // Story 18-2: the adopted instance's RE-RESOLVED
                            // liveness check (above) with a FRESH window —
                            // the previous engine's failure streak is not
                            // observable, so adoption grants a whole new
                            // window (the honest ≤-one-extra-window grace,
                            // documented in the manifest docs). The reopen
                            // counter starts fresh too (P6): this engine has
                            // seen zero flaps.
                            liveness_check: adopted_liveness_check,
                            liveness_failing_since: None,
                            liveness_reopens: 0,
                        },
                    );
                    adopted += 1;
                    // NOTE (the live-owner boundary, deliberately NOT done
                    // here): the adopter does NOT re-stamp itself as the
                    // record's owner. A re-stamp would make every LONG-LIVED
                    // bystander that adopted a dead owner's record (a
                    // `logs --follow`, an embedding's read engine) the
                    // record's "owner", blocking explicit stops from other
                    // engines — the boundary's protection is scoped to the
                    // case it exists for: the ORIGINAL spawning owner being
                    // ALIVE (a running `serve`, an embedding). Dead-owner
                    // records keep the pre-boundary semantics byte-for-byte.
                    // Story 14-1 + 14-2: surface the acp adoption honesty —
                    // the instance is alive but this engine holds no live ACP
                    // connection for it (the pipe pair died with the previous
                    // engine; the adopted process is not re-piped), so `send`
                    // refuses until a stop→start re-establishes the transport.
                    // The note ALSO names the persisted session state: the
                    // recorded id (if any) will be offered via `session/load`
                    // at the next start. Surfaced-not-silent (AI-18); the
                    // diagnostic is emitted under the supervisor lock via the
                    // choke point.
                    let kind = registry
                        .lookup(&name)
                        .map(|instance| instance.kind)
                        .unwrap_or_default();
                    if crate::acp::is_acp_kind(&kind) {
                        let note = crate::acp::adopted_acp_note(
                            name.as_str(),
                            record.acp_session_id.as_deref(),
                        );
                        self.emit_diagnostic(&note);
                    }
                    // AI-46 (story 11-3): an adopted ENGINE-OBSERVED instance is
                    // stranded — the paragraph on `observed_listener: None`
                    // above documents it, but until now the engine said it
                    // NOWHERE an operator could hear. The engine cannot rewrite
                    // the already-running child's injected `base_url` (the env
                    // went in at the previous engine's spawn), so the honest
                    // fix is to ANNOUNCE the condition: name the instance, the
                    // stranded observed listener, and the stop→start
                    // remediation. The diagnostic names the CONDITION without
                    // the dead port number: the spawn record carries no launch
                    // facts (the write-ahead record is
                    // {fingerprint, policy, count, cause} only), and the
                    // registration snapshot's launch predates the start-time
                    // injection, so no base_url host/port is recoverable here.
                    // Semantics are untouched: the instance stays marked
                    // un-observed exactly as below, and no process is
                    // relaunched (adoption keeps-them-running).
                    // Story 17-2: SKIPPED when the fixed-port re-bind above
                    // landed a live listener (`adopted_listener` is Some) —
                    // the instance is NOT stranded, and a strand warning would
                    // lie. Port-0 opens (and failed re-binds) still fire it.
                    if metering_source == "engine-observed" && !rebind_landed {
                        let strand = format!(
                            "{}: adopted an engine-observed instance; its injected \
                             'metering.base_url' still points at the PREVIOUS engine's \
                             loopback forward listener, which died with that engine — this \
                             engine holds NO listener for the adopted process, so its model \
                             calls hit the dead port and fail with connection errors \
                             (stranded observed listener). The already-injected environment \
                             of a running process cannot be rewritten. Remediation: stop \
                             the instance and start it again; the fresh start binds a new \
                             listener and re-injects a live base_url.",
                            name.as_str(),
                        );
                        self.emit_diagnostic(&strand);
                    }
                    // Story 15-2: the stranded-paused adoption note (AI-18 +
                    // Fibery 178): an adopted PAUSED row is held paused —
                    // adoption NEVER auto-resumes (the pinned AI-7
                    // stays-paused contract) — and until now that fact went
                    // unsaid: a paused row whose previous engine died mid-pause
                    // looks exactly like a healthy operator pause on every
                    // surface. Name it, and name the way out. For a DETACHED
                    // row the note also states the release semantics — which
                    // REVIEW ROUND 1 now splits by pause kind (the release
                    // pass's classifier, shared here): an OPERATOR pause is
                    // healed at the releasing session's exit, while a
                    // BUDGET-BREACH pause deliberately holds into the
                    // enforcement-free between-commands window (resuming it
                    // would run the agent unsupervised while the seeded latch
                    // suppresses re-enforcement — un-doing AI-44's
                    // spend-stop).
                    if registry
                        .lookup(&name)
                        .map(|inst| inst.state)
                        .is_ok_and(|state| state == LifecycleState::Paused)
                    {
                        let resume_hint = if record.detach {
                            if super::lifecycle::classify_pause_kind(registry, &name)
                                == super::lifecycle::PauseKind::Breach
                            {
                                "it was paused by a BUDGET BREACH, so the freeze holds \
                                 between commands too — this engine's exit does NOT resume it \
                                 (the between-commands window is enforcement-free; resuming it \
                                 there would run it unsupervised past its ceiling); it stays \
                                 suspended until a later command resumes or stops it"
                            } else {
                                "it stays paused only while THIS engine session holds \
                                 it; on this session's exit the child is resumed \
                                 (SIGCONT) and the row moves to running — never left \
                                 frozen and unsupervised"
                            }
                        } else {
                            "it stays paused until an engine session resumes or \
                             stops it"
                        };
                        self.emit_diagnostic(&format!(
                            "{}: adopted a PAUSED instance (its previous engine died \
                             mid-pause, or it was paused before that engine exited) — \
                             adoption never auto-resumes; {}; `hekma agent resume {}` \
                             (or `stop`) is the explicit path",
                            name.as_str(),
                            resume_hint,
                            name.as_str(),
                        ));
                    }
                    // AI-44: re-evaluate budgets for the JUST-ADOPTED instance
                    // NOW, before returning — the durable ledger survived the
                    // engine crash, so an instance already past its ceiling must
                    // be enforced at startup, not left running unconstrained
                    // until the NEXT usage event happens to arrive (which may be
                    // never for a quiet agent). This is the SAME AD-7
                    // enforcement stage `ingest_usage` runs: a live config read,
                    // the committed per-run + cumulative ledger totals, the pure
                    // evaluators, record-first-then-act. Story 15-2: the call is
                    // deliberately NOT gated — an over-budget adopted Run is
                    // still ACTED UPON here — but the latch seeded above makes a
                    // breach already recorded for the RESUMED Run a no-op, so
                    // the action fires once per RUN, not once per command. The
                    // action (default pause) goes through the normal lifecycle
                    // path with the handle just re-held. Best-effort by
                    // contract: an enforcement error is a diagnostic, never an
                    // adoption failure.
                    self.enforce_budget(registry, &name, &run_id, &metering_source);
                }
                Ok(None) => {
                    // No live match — reconcile to `failed` HONESTLY (AI-8).
                    self.reconcile_orphan_failed(registry, &record);
                }
                Err(_) => {
                    // A backend adopt error is treated as "cannot confirm live" —
                    // reconcile to failed rather than leave a phantom row (AI-8).
                    self.reconcile_orphan_failed(registry, &record);
                }
            }
        }
        adopted
    }

    /// Reconcile a non-adopted orphan record to `failed` (AI-8): the process is
    /// gone (or unconfirmable), so a persisted `running`/`paused` row must NOT be
    /// left implying supervision that does not exist. Records a `Crashed` cause
    /// naming the orphan + the last-known cause, then clears the record. If the
    /// current state is already terminal (`failed`/`stopped`) we only clear the
    /// stale record. Best-effort — a persist failure leaves the record for the
    /// next open.
    fn reconcile_orphan_failed(&self, registry: &Registry, record: &SpawnRecord) {
        let name = &record.name;
        let state = match registry.lookup(name) {
            Ok(inst) => inst.state,
            Err(_) => {
                // Row gone — just drop the stale record.
                let _ = registry.clear_spawn_record(name);
                return;
            }
        };
        if matches!(state, LifecycleState::Running | LifecycleState::Paused) {
            let last = record
                .last_known_cause
                .as_deref()
                .unwrap_or("no prior cause recorded");
            let detail = format!(
                "orphan not found on engine restart (process pid {} is gone or was reused); \
                 last known: {last}",
                record.fingerprint.pid,
            );
            let _ = self.ensure_log_dir(registry, name);
            let _ = self.transition(
                registry,
                name,
                state,
                LifecycleState::Failed,
                TransitionCause::crashed(detail),
            );
        }
        // Settle the stale record either way (its process is gone). Story
        // 14-2: the settle retains an established acp session id on a pid-0
        // seed row — the crash the reconcile names is exactly when the NEXT
        // start's `session/load` resume matters — and is byte-identical to a
        // plain clear for records without one.
        let _ = registry.settle_spawn_record(name);
    }

    /// The ONE degraded-read diagnostic of the phase-4 liveness lookup
    /// (review round 1, P2) — shared by the real store-error path and the
    /// cfg(test) seam so both emit byte-identically.
    fn emit_liveness_lookup_degraded(&self, name: &InstanceName, reason: &str) {
        self.emit_diagnostic(&format!(
            "{}: declared liveness check could not consult the instance state \
             (store read failed): {reason} — checks paused until reads recover",
            name.as_str(),
        ));
    }

    // ---- internals ----
}

/// How many complete liveness flap cycles (window open → recovery) emit
/// their first-failure/recovery diagnostic PAIRS within one Run before the
/// pairs go silent (review round 1, P6). A flapping signal that never
/// expires is contract-correct — persistence is the window's job — but an
/// unbounded diagnostic pair per flap is per-tick spam in slow motion: three
/// pairs tell the operator everything, then ONE suppression note says the
/// quiet part, and the window keeps expiring on schedule regardless.
const LIVENESS_REOPEN_DIAGNOSTIC_BUDGET: u32 = 3;

/// The expiry-verdict onset note (review round 1, P7): for a HEARTBEAT-FILE
/// check, how much staler the file already was at the first OBSERVED failure
/// than the observed streak itself — the engine can stat the file at expiry
/// and subtract what the window measured, surfacing the pre-existing
/// staleness the observation could not see. `None` for an http-get check (a
/// refused connection leaves no statable artifact), for a file that cannot
/// be stat'ed honestly, and when the file's current age does not exceed the
/// observed streak (nothing pre-existing to name). Best-effort by design:
/// a note is additive honesty, never a gate on the failure itself.
fn liveness_expiry_onset_note(
    check: &LivenessCheck,
    home: &std::path::Path,
    failing_for_ms: u64,
) -> Option<String> {
    let LivenessCheck::HeartbeatFile(h) = check else {
        return None;
    };
    // The SAME home-relative resolution the evaluator uses (evaluate's
    // HeartbeatFile arm), so the note stats the exact file that was judged
    // stale.
    let path = if std::path::Path::new(&h.path).is_absolute() {
        std::path::PathBuf::from(&h.path)
    } else {
        home.join(&h.path)
    };
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata.modified().ok()?;
    let age_now_ms = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or(std::time::Duration::ZERO)
        .as_millis() as u64;
    if age_now_ms > failing_for_ms {
        Some(format!(
            "the heartbeat file was already ~{}ms stale at first observation",
            age_now_ms - failing_for_ms
        ))
    } else {
        None
    }
}
