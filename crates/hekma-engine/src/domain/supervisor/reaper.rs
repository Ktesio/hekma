//! reaper family — split out of the supervisor monolith by story 13-3
//! (epic-13; boundary ratified 2026-09-19, study: Option B). Pure move:
//! additional `impl Supervisor` block, no signature or behavior change.

use super::*;

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
    #[cfg(test)]
    fn injected_poll_fault(&self, pid: u32) -> Option<BackendError> {
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
                    if registry.clear_spawn_record(&name).is_ok() {
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
    fn plan_restart(
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
    /// Policy for the failed instance (AC9). Concretely: clear the record, then
    /// re-persist the policy as a pid-0 seed. The failed CAUSE is not kept in the
    /// record — it rides in the event log, which `instance_status` falls back to.
    /// Best-effort (a store hiccup here is never a panic).
    fn settle_terminal_record(
        &self,
        registry: &Registry,
        name: &InstanceName,
        policy: RestartPolicy,
    ) {
        let _ = registry.clear_spawn_record(name);
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
                    // assumption): the pre-crash Run id lived only in the crashed
                    // engine's memory, so the adopted instance opens a NEW Run and
                    // begins ingestion at the CURRENT end of its agent-output log
                    // (skipping pre-crash lines). This keeps per-run totals honest for
                    // the post-adoption span without re-attributing (or double-
                    // counting) the old Run's already-captured usage; the DB dedup key
                    // includes the run id, so even an overlapping sequence is safe.
                    let run_id = RunId::mint();
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
                            // The adopted instance opens a NEW Run (the pre-crash
                            // run_id died with the crashed engine), so its breach latch
                            // starts empty too (story 3-2).
                            breached_scopes: std::collections::HashSet::new(),
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
                            observed_listener: None,
                            observed_source: None,
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
                            // Story 14-1 (spine AD-19): an ADOPTED acp instance
                            // is re-held as a bare process — its ACP pipe
                            // halves died with the engine that spawned it, so
                            // NO connection is re-established here (the honest
                            // note below says so; `session/load` resume is
                            // story 14-2).
                            acp: None,
                        },
                    );
                    adopted += 1;
                    // Story 14-1: surface the acp adoption honesty — the
                    // instance is alive but this engine holds no ACP session
                    // for it, so `send` refuses (the ordinary
                    // adopted-instance interaction error) until a stop→start
                    // re-establishes the transport. Surfaced-not-silent
                    // (AI-18); the diagnostic is emitted under the supervisor
                    // lock via the choke point.
                    let kind = registry
                        .lookup(&name)
                        .map(|instance| instance.kind)
                        .unwrap_or_default();
                    if crate::acp::is_acp_kind(&kind) {
                        let note = crate::acp::adopted_acp_note(name.as_str());
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
                    if metering_source == "engine-observed" {
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
                    // AI-44: re-evaluate budgets for the JUST-ADOPTED instance
                    // NOW, before returning — the durable ledger survived the
                    // engine crash, so an instance already past its ceiling must
                    // be enforced at startup, not left running unconstrained
                    // until the NEXT usage event happens to arrive (which may be
                    // never for a quiet agent). This is the SAME AD-7
                    // enforcement stage `ingest_usage` runs: a live config read,
                    // the committed per-run + cumulative ledger totals, the pure
                    // evaluators, record-first-then-act. The fresh Run's breach
                    // latch is empty (inserted above), so a surviving breach
                    // fires exactly once here; the action (default pause) goes
                    // through the normal lifecycle path with the handle just
                    // re-held. Best-effort by contract: an enforcement error is
                    // a diagnostic, never an adoption failure.
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
        // Clear the stale record either way (its process is gone).
        let _ = registry.clear_spawn_record(name);
    }

    // ---- internals ----
}
