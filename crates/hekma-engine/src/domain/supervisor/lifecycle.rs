//! lifecycle family — split out of the supervisor monolith by story 13-3
//! (epic-13; boundary ratified 2026-09-19, study: Option B). Pure move:
//! additional `impl Supervisor` block, no signature or behavior change.

use super::*;

impl Supervisor {
    /// Perform ONE Restart Policy restart of a crashed instance (story 1-6, AC4).
    ///
    /// Called by the engine cadence AFTER it has waited the backoff
    /// [`RestartPlan::delay`]. Re-runs the start path (`failed → starting →
    /// running`) recording a [`TransitionCause::Restarted`] with the consecutive
    /// `attempt` + the `waited` backoff, and keeps the persisted restart count at
    /// `attempt`.
    ///
    /// Interaction with a concurrent `stop`: `restart` re-runs the start path, so
    /// its transition gate is `next_state(state, Start)`. That gate only accepts
    /// `failed` (or registered/stopped); if the instance was already restarted to
    /// `running` by an EARLIER plan, or an operator stopped it back to `stopped`,
    /// the gate rejects and this restart is a harmless no-op. NOTE: during the
    /// backoff WINDOW the instance is `failed`, and `next_state(Failed, Stop)` is
    /// an `InvalidTransition` — so an operator cannot `stop` a mid-backoff
    /// instance to pre-empt this restart (there is no `failed → stopping` edge
    /// this story; adding one is out of scope). The restart therefore proceeds; a
    /// stop is only effective once the instance is `running` again.
    pub fn restart(
        &mut self,
        registry: &Registry,
        name: &str,
        attempt: u32,
        waited: Duration,
    ) -> Result<AgentInstance, EngineError> {
        // Review round 2: a RESTART of a DETACHED agent must stay detached.
        // The write-ahead record survives the crash (only a clean stop clears
        // it), so it is the durable source of the spawn's detached-ness — read
        // it BEFORE `start_inner` rewrites the record. Dropping the flag here
        // (the pre-patch hardcoded `false`) would silently restart the agent
        // ATTACHED and downgrade the record, killing the agent at this
        // engine's clean exit — the exact surprise `--detach` promises away.
        // A missing/unreadable record falls back to the supervised (attached)
        // default, matching how the pre-12-1 records read.
        let instance_name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let detach = registry
            .spawn_record(&instance_name)
            .ok()
            .flatten()
            .map(|record| record.detach)
            .unwrap_or(false);
        self.start_inner(registry, name, Some((attempt, waited)), detach)
    }

    /// Stop a running Agent Instance (AC3/AC4).
    ///
    /// Transitions `running → stopping`, requests graceful shutdown via the
    /// backend and escalates to a forced kill after `window` (default
    /// [`DEFAULT_STOP_WINDOW`]) if needed, records the escalation in the instance
    /// log, then `stopping → stopped`. No process of the instance survives (the
    /// backend kills the whole group/job) — in the NORMAL case.
    ///
    /// **Bounded death confirmation (fix pass, review of #80 follow-up — the
    /// CRITICAL finding):** after escalating to a forced kill, the backend
    /// CONFIRMS death bounded to [`crate::ports::KILL_CONFIRM_TIMEOUT`] (see
    /// its docs for the mechanism: a fast writer can exhaust disk and enter
    /// an OS-level uninterruptible I/O wait immune to every signal, including
    /// the one just sent). If confirmation is not reached within that bound,
    /// this returns [`EngineError::StopUnconfirmed`] instead of continuing to
    /// block — the instance stays `stopping` (never a false `stopped`), and
    /// the handle is RETAINED (not dropped) so the situation can be
    /// reconciled later.
    ///
    /// **No compounding on retry:** a SUBSEQUENT `stop` call against an
    /// instance still `stopping` with a retained (unconfirmed) handle does
    /// NOT re-run the whole SIGTERM/graceful-window/SIGKILL/confirm sequence
    /// — it performs a single cheap, NON-BLOCKING liveness poll instead
    /// (`ProcessBackend::poll`, never `ProcessBackend::stop`). If the process
    /// has since actually exited (the OS condition cleared), this
    /// SELF-HEALS: it completes the stuck `stopping → stopped` transition
    /// right here. If it is still alive, this fails fast with the SAME
    /// honest [`EngineError::StopUnconfirmed`], with no new signal and no new
    /// wait. (The crash-detection reaper's own poll, `poll_once`, performs
    /// the identical reconciliation if it observes the exit first — whichever
    /// happens first, the row does not stay permanently stuck.)
    pub fn stop(
        &mut self,
        registry: &Registry,
        name: &str,
        window: Option<Duration>,
    ) -> Result<AgentInstance, EngineError> {
        self.stop_inner(registry, name, window, None)
    }

    /// Stop driven by a budget BREACH (story 3-2). Identical to [`Supervisor::stop`]
    /// (graceful → forced escalation, story 1-4) except the `running → stopping`
    /// edge carries the [`TransitionCause::BudgetExceeded`] cause instead of a plain
    /// `stop` command, so the lifecycle log explains WHY. The terminal
    /// `stopping → stopped` edge keeps its graceful/forced cause (the escalation
    /// detail). Takes `&InstanceName` (the caller already validated it).
    pub(super) fn stop_with_cause(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        cause: TransitionCause,
    ) -> Result<AgentInstance, EngineError> {
        self.stop_inner(registry, name.as_str(), None, Some(cause))
    }

    /// The shared stop driver (story 1-4 + story 3-2 cause override).
    ///
    /// `cause_override`: when `Some`, replaces the `running → stopping` cause
    /// (a budget stop records `BudgetExceeded`); `None` uses the plain `stop`
    /// command cause (an operator `kt agent stop` is unchanged). The terminal edge
    /// always records the graceful/forced escalation cause regardless.
    fn stop_inner(
        &mut self,
        registry: &Registry,
        name: &str,
        window: Option<Duration>,
        cause_override: Option<TransitionCause>,
    ) -> Result<AgentInstance, EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = registry.lookup(&name).map_err(registry_to_engine)?;

        // Fix pass (review of #80 follow-up — the CRITICAL finding): a RETRY
        // `stop` against an instance already `stopping` whose handle is
        // marked `stop_unconfirmed` means a PRIOR pass through THIS function
        // already sent SIGKILL but could not confirm death within
        // KILL_CONFIRM_TIMEOUT (`EngineError::StopUnconfirmed`) — most likely
        // because the process is stuck in an OS-level uninterruptible I/O
        // wait. The transition gate below (`next_state`) has no
        // `(Stopping, Stop)` row, so an unmodified retry would either reject
        // with a generic, non-self-healing `InvalidTransition`, or (if that
        // gate were bypassed) re-run the WHOLE SIGTERM/graceful-window/
        // SIGKILL/confirm sequence for an outcome we can already suspect is
        // unchanged — exactly the compounding wait this fix pass closes.
        // Instead: a single cheap, NON-BLOCKING poll (`ProcessBackend::poll`,
        // never `ProcessBackend::stop`) decides the outcome — self-heals if
        // the process has since actually died (the OS condition cleared), or
        // fails fast with the SAME honest error if it is still alive, with NO
        // new signal and NO new wait. Gated specifically on `stop_unconfirmed`
        // (not merely "state is `stopping`") so this new branch changes
        // behavior ONLY for the scenario it targets — an externally-forced
        // `stopping` row with no real stop attempt behind it (as
        // `poll_once_ignores_an_exit_during_a_requested_stop_not_a_crash`
        // exercises) takes the ORIGINAL, unchanged path below.
        if instance.state == LifecycleState::Stopping {
            let stuck = match self.running.get_mut(&name) {
                Some(supervised) if supervised.stop_unconfirmed => {
                    let status = self
                        .backend
                        .poll(&mut supervised.handle)
                        .map_err(|source| EngineError::Backend {
                            name: name.as_str().to_string(),
                            source,
                        })?;
                    let log_capture = self.backend.log_capture(&supervised.handle);
                    Some((status, log_capture))
                }
                _ => None,
            };
            if let Some((status, log_capture)) = stuck {
                if !status.is_exited() {
                    // Still stuck: fail fast, honestly, with no new blocking.
                    return Err(EngineError::StopUnconfirmed {
                        name: name.as_str().to_string(),
                        timeout_secs: KILL_CONFIRM_TIMEOUT.as_secs(),
                    });
                }
                // Self-healing: the process has now actually exited (the OS
                // condition that made confirmation time out has cleared).
                // Complete the stuck `stopping -> stopped` transition exactly
                // as the ordinary path below would have on confirmed death.
                self.clear_poll_error_streak(&name);
                self.running.remove(&name);
                registry
                    .clear_spawn_record(&name)
                    .map_err(registry_to_engine)?;
                self.transition_with_log_capture(
                    registry,
                    &name,
                    LifecycleState::Stopping,
                    LifecycleState::Stopped,
                    TransitionCause::stop_forced(
                        "SIGKILL was sent by an earlier stop attempt; the process's death was \
                         confirmed on a later reconciliation (it may have been stuck in an \
                         OS-level I/O wait that has since cleared)",
                    ),
                    log_capture,
                )?;
                return registry.lookup(&name).map_err(registry_to_engine);
            }
        }

        // Transition gate (AC4): stop on stopped / registered / … rejects here
        // with the uniform InvalidTransition, before touching any process.
        let stopping = next_state(instance.state, LifecycleCommand::Stop)?;

        let window = window.unwrap_or(DEFAULT_STOP_WINDOW);
        self.ensure_log_dir(registry, &name)?;

        // running → stopping (a story-3-2 budget stop overrides the cause).
        self.transition(
            registry,
            &name,
            instance.state,
            stopping,
            cause_override
                .unwrap_or_else(|| TransitionCause::command(LifecycleCommand::Stop.as_str())),
        )?;

        // Drain any final self-reported usage the agent emitted before the stop, so
        // the last batch of a Run is not lost to the race between "agent printed it"
        // and "we killed the process" (story 3-1). TERMINAL drain: the process is
        // about to be gone, so a final newline-less usage line is consumed to
        // end-of-log rather than stranded (H1). Best-effort — a drain hiccup never
        // blocks the stop.
        self.drain_usage_for(registry, &name, DrainMode::Terminal);
        // Drain any final ENGINE-OBSERVED usage still queued before the listener is
        // torn down (story 3-4): a completion the proxy parsed just before the stop
        // must land, not be lost when the `Supervised` (and its listener) is dropped
        // below. TERMINAL mode (story 12-4): a commit failure here announces the
        // loss — there is no next pass — and any parked buffer dies with the
        // instance.
        self.drain_observed_for(registry, &name, DrainMode::Terminal);

        // Ask the backend to stop the process (group/job). If we have no handle
        // for it (the row says running but this engine holds no handle AND orphan
        // adoption found no live process), the desired end state "no process of
        // the instance survives" already holds, so we treat it as a graceful
        // stop. With story 1-6 adoption, a handle for a still-live process
        // started by a PRIOR engine IS re-held (via `adopt_orphans`), so a
        // cross-restart stop now really terminates it.
        // Story 4-2, Task 4 (fix pass, review of #80): capture the
        // log_capture HERE, before `running.remove` drops the handle below
        // — the default `self.transition(...)` lookup (via `self.running`)
        // would find NOTHING by the time the terminal transition below
        // runs. By the time `backend.stop` (below) returns, the process is
        // provably dead (its raw capture files can never grow again), and
        // `send_engine_line`'s inline catch-up folds in every remaining
        // byte of agent output BEFORE the "-> stopped" line, so the engine
        // line still lands correctly ordered after it, regardless of
        // whether the process handle's `Drop` (which also signals the
        // background tailer thread to stop) has run yet.
        let (outcome, log_capture) = match self.running.get_mut(&name) {
            Some(supervised) => {
                let log_capture = self.backend.log_capture(&supervised.handle);
                let outcome = self.backend.stop(&mut supervised.handle, window).map_err(
                    |source| match source {
                        // Fix pass (review of #80 follow-up — the CRITICAL
                        // finding): mark the handle so a RETRY `stop` (or the
                        // crash reaper's own poll) recognizes this EXACT
                        // scenario and reconciles it with a cheap,
                        // non-blocking poll instead of re-running the whole
                        // bounded SIGTERM/SIGKILL/confirm sequence (see
                        // `stop_inner`'s retry-branch docs above and
                        // `poll_once`'s docs). This `?` skips
                        // `self.running.remove` below, so the handle is
                        // RETAINED, never silently dropped — the instance
                        // stays `stopping` (the terminal transition below is
                        // never reached), an honest, non-terminal state.
                        BackendError::StopUnconfirmed { timeout_secs } => {
                            supervised.stop_unconfirmed = true;
                            EngineError::StopUnconfirmed {
                                name: name.as_str().to_string(),
                                timeout_secs,
                            }
                        }
                        other => EngineError::Backend {
                            name: name.as_str().to_string(),
                            source: other,
                        },
                    },
                )?;
                (outcome, log_capture)
            }
            None => (crate::ports::StopOutcome { forced: false }, None),
        };
        // AI-63 follow-on (billing under-count at STOP, owner-approved): a FINAL
        // TERMINAL rescue drain AFTER `backend.stop` has CONFIRMED the process
        // dead and BEFORE `self.running.remove` below drops the handle + cursor.
        //
        // The pre-kill drain (above, before `backend.stop`) runs while the agent is
        // still ALIVE, so a usage line the agent flushes in the window between that
        // drain and the kill would otherwise be stranded: the cursor never advances
        // past it and the handle is removed right after with no further drain (a
        // permanent UNDER-count). Reaching HERE means death is CONFIRMED — the
        // `BackendError::StopUnconfirmed` case `?`-returned ABOVE without removing
        // the handle (it is retained for later reconciliation), so this rescue drain
        // NEVER runs on a still-live process. With the process provably dead,
        // `agent.log` is STABLE (it can never grow again), so this Terminal drain
        // reads it to its now-final EOF, capturing exactly the tail the pre-kill
        // drain missed — with NO unbounded-growth / under-lock-stall concern (the
        // file is finite and final).
        //
        // No double-count: `drain_usage_for` is cursor-based — the pre-kill drain
        // advanced `usage_cursor` to what it consumed, so this pass ingests ONLY
        // bytes that arrived AFTER it (the two drains are disjoint by cursor; the DB
        // dedup is a backstop, not the primary guard). The `Supervised` entry MUST
        // still be in `self.running` for its cursor/run_id/metering_source to be
        // read — hence strictly BEFORE `self.running.remove`. This mirrors the crash
        // reaper's proven drain-AFTER-observed-exit (see `poll_once`). Best-effort,
        // like the pre-kill drain — a drain hiccup never blocks the stop.
        self.drain_usage_for(registry, &name, DrainMode::Terminal);
        // Drop the handle (also closes the Job / releases the child on Windows) and
        // the Run's metering context — the Run ends at this terminal transition.
        self.clear_poll_error_streak(&name);
        self.running.remove(&name);

        // Clear the write-ahead spawn record (AD-5): a cleanly-stopped instance
        // must NOT be later adopted or reconciled-to-failed as an orphan. Cleared
        // BEFORE the terminal transition so the durable record leads the state.
        registry
            .clear_spawn_record(&name)
            .map_err(registry_to_engine)?;

        // stopping → stopped, recording whether escalation happened (AC3).
        let cause = if outcome.forced {
            TransitionCause::stop_forced(format!(
                "graceful window ({}s) elapsed; escalated to a forced kill of the process group/job",
                window.as_secs()
            ))
        } else {
            TransitionCause::StopGraceful
        };
        self.transition_with_log_capture(
            registry,
            &name,
            stopping,
            LifecycleState::Stopped,
            cause,
            log_capture,
        )?;

        registry.lookup(&name).map_err(registry_to_engine)
    }

    /// Pause a running Agent Instance with honest, per-OS semantics (story 1-5,
    /// AC1/AC2/AC3/AC5 — the "surfaced not silent" HONESTY command).
    ///
    /// Order mirrors [`Supervisor::stop`] — including the persist-FIRST ordering
    /// (AI-9: the transition commits before the signal, so a persist failure can
    /// never leave the process suspended while the ledger says otherwise) —
    /// except the middle step DISPATCHES on the effective (current-OS) pause
    /// `SupportLevel` read from the persisted snapshot (AC5), rather than always
    /// calling the backend:
    /// 1. name → [`InstanceName`]; look up the instance,
    /// 2. transition gate `next_state(state, Pause)?` — an invalid transition
    ///    (e.g. pause on `stopped`/`paused`) rejects HERE with the uniform
    ///    [`LifecycleError::InvalidTransition`] (AC4), before any side effect or
    ///    level read,
    /// 3. read the effective pause level (AC5) and dispatch:
    ///    * **Guaranteed** → persist `running→paused` + a plain
    ///      [`TransitionCause::Command`] (`"pause"`) — no qualifier — THEN
    ///      `backend.pause(handle)` (real SIGSTOP suspension on Unix). AI-8: when
    ///      NO in-memory handle is held (nothing can be signalled), the recorded
    ///      cause is the honest [`TransitionCause::PauseBestEffort`] qualifier
    ///      naming the missing handle instead of a plain command that would read
    ///      as a real suspension,
    ///    * **BestEffort** → persist `running→paused` + a
    ///      [`TransitionCause::PauseBestEffort`] qualifier (the machine-readable
    ///      half of "surfaced not silent"); the process may keep running,
    ///    * **Unsupported** → FAIL FAST with
    ///      [`EngineError::CapabilityUnsupported`], NO transition, NO backend
    ///      call, NOTHING persisted (AC3).
    pub fn pause(&mut self, registry: &Registry, name: &str) -> Result<AgentInstance, EngineError> {
        self.suspend_or_resume(registry, name, LifecycleCommand::Pause, None)
    }

    /// Pause driven by a budget BREACH (story 3-2 AC6). Identical to
    /// [`Supervisor::pause`] — honoring the adapter pause Capability Declaration
    /// EXACTLY (guaranteed suspends; best-effort transitions with the honest
    /// posture; UNSUPPORTED fails fast, NO fake pause, NO silent escalation) —
    /// except the resulting `running → paused` transition carries the
    /// [`TransitionCause::BudgetExceeded`] cause instead of a plain `pause` command,
    /// so the lifecycle log itself explains WHY (the standalone breach event is the
    /// AD-14 subscription payload). Takes `&InstanceName` (the caller already has
    /// the validated name inside the ingestion path).
    pub(super) fn pause_with_cause(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        cause: TransitionCause,
    ) -> Result<AgentInstance, EngineError> {
        self.suspend_or_resume(
            registry,
            name.as_str(),
            LifecycleCommand::Pause,
            Some(cause),
        )
    }

    /// Resume a paused Agent Instance (story 1-5, AC1/AC2).
    ///
    /// The symmetric counterpart of [`Supervisor::pause`]: the transition gate is
    /// `next_state(state, Resume)?` (`paused → running`; anything else rejects
    /// with the uniform invalid-transition, AC4), and the dispatch is on the same
    /// effective pause level:
    /// * **Guaranteed** → `backend.resume(handle)` (SIGCONT), then `paused→running`
    ///   + a plain `resume` command cause,
    /// * **BestEffort** → `paused→running` + a [`TransitionCause::ResumeBestEffort`]
    ///   qualifier,
    /// * **Unsupported** → fail fast with the DEDICATED
    ///   [`EngineError::ResumeUnsupported`] (AI-7 — NOT the bare pause-unsupported
    ///   error): the instance is already `paused` (the gate above guarantees it),
    ///   so a diagnostic that merely says "pause is unsupported" would strand the
    ///   operator with no way forward. The dedicated variant names the state + the
    ///   adapter's pause declaration and gives the escape hatch — `stop` works
    ///   without pause support (it never consults the pause level), so
    ///   `stop` + `start` is a real recovery, and a `resume` on an OS where the
    ///   declaration supports pause works too. NO state change, NO signal, NO
    ///   fake success. Not normally reachable within one declaration (a `paused`
    ///   row implies pause was allowed at some point), but real via
    ///   declaration/OS drift between the pause and the resume.
    pub fn resume(
        &mut self,
        registry: &Registry,
        name: &str,
    ) -> Result<AgentInstance, EngineError> {
        self.suspend_or_resume(registry, name, LifecycleCommand::Resume, None)
    }

    /// Shared pause/resume driver (the three-level dispatch), keyed on `command`
    /// (`Pause` or `Resume`). Kept as one method so the pause and resume paths
    /// cannot drift: the transition gate, the level read, and the three-way
    /// dispatch are identical; only the target state and the cause differ.
    ///
    /// `cause_override` (story 3-2): when `Some`, it REPLACES the default cause on
    /// the resulting transition for the GUARANTEED + BEST-EFFORT paths — a
    /// budget-driven pause records [`TransitionCause::BudgetExceeded`] instead of a
    /// plain `pause` command / a best-effort qualifier, so the lifecycle log
    /// explains WHY. `None` preserves the story-1-5 causes exactly (an operator
    /// `kt agent pause` is unchanged). The UNSUPPORTED fail-fast is identical
    /// regardless (no transition, nothing persisted — the override is moot).
    fn suspend_or_resume(
        &mut self,
        registry: &Registry,
        name: &str,
        command: LifecycleCommand,
        cause_override: Option<TransitionCause>,
    ) -> Result<AgentInstance, EngineError> {
        debug_assert!(
            matches!(command, LifecycleCommand::Pause | LifecycleCommand::Resume),
            "suspend_or_resume only handles Pause/Resume"
        );
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = registry.lookup(&name).map_err(registry_to_engine)?;

        // (1) Transition gate (AC4): pause on stopped/paused, resume on running,
        // etc. reject HERE with the uniform InvalidTransition, before any level
        // read or side effect.
        let new_state = next_state(instance.state, command)?;

        // (2) Read the effective (current-OS) pause level from the persisted
        // snapshot (AC5). Projected at read time onto OsId::current(); NOT
        // re-derived from the manifest, NOT frozen at register time.
        let level = registry
            .effective_support(&name, Capability::Pause)
            .map_err(registry_to_engine)?;
        let os = OsId::current();

        // (3) Dispatch on the level.
        match level {
            // FAIL FAST (AC3): no transition, no backend call, nothing persisted.
            // AI-7: a RESUME under an Unsupported PAUSE declaration gets its OWN
            // diagnostic (not the bare pause-unsupported error): the instance is
            // already `paused` (the transition gate above guarantees it), so
            // telling the operator "cannot pause" strands them. The error names
            // the state + the declaration and gives the path forward (stop works
            // without pause support). The PAUSE arm keeps the original
            // CapabilityUnsupported fail-fast verbatim (AC3).
            SupportLevel::Unsupported if command == LifecycleCommand::Resume => {
                Err(EngineError::ResumeUnsupported {
                    name: name.as_str().to_string(),
                    os: os.as_str().to_string(),
                    level: level.as_str().to_string(),
                })
            }
            SupportLevel::Unsupported => Err(EngineError::CapabilityUnsupported {
                name: name.as_str().to_string(),
                capability: Capability::Pause.as_str().to_string(),
                os: os.as_str().to_string(),
                level: level.as_str().to_string(),
            }),
            // GUARANTEED (AC1): a real suspension via the backend, then a plain
            // command-cause transition (no qualifier — it is a true suspension). A
            // story-3-2 budget pause overrides the cause with BudgetExceeded.
            //
            // AI-8 + AI-9 (order mirrors `stop_inner`): the HONEST cause is
            // decided BEFORE anything is signalled or persisted — a guaranteed
            // command with no in-memory handle signals nothing, so its cause is
            // the best-effort qualifier naming the missing handle, never a plain
            // command that would read as a real suspension — and the transition
            // (persist + log) lands FIRST, then the signal, so a failed persist
            // can never leave the process suspended while the ledger says
            // otherwise (the durable state leads, exactly like stop).
            SupportLevel::Guaranteed => {
                self.ensure_log_dir(registry, &name)?;
                let has_handle = self.running.contains_key(&name);
                let cause = match (cause_override.clone(), has_handle) {
                    // The handle is held: a story-3-2 budget pause overrides the
                    // cause with BudgetExceeded; a plain command keeps its plain
                    // command cause (a true suspension).
                    (Some(cause), true) => cause,
                    (None, true) => TransitionCause::command(command.as_str()),
                    // AI-8 (loop 1): NOTHING is held to signal — record the
                    // honest best-effort posture with the reason (the missing
                    // handle), never a plain command that would read as a real
                    // suspension. A `Some(cause_override)` (e.g. BudgetExceeded)
                    // does NOT win here either: a budget pause that suspended
                    // nothing must not read as a performed suspension, so the
                    // override is WRAPPED as the qualifier's detail (the breach
                    // event itself already carries the budget record).
                    (override_cause, false) => {
                        let detail = match override_cause {
                            Some(cause) => format!(
                                "no live process handle is held in this engine session for \
                                 '{name}', so the guaranteed {} signalled nothing; the transition \
                                 is recorded best-effort — the requested override was:{}",
                                command.as_str(),
                                cause_suffix(&cause),
                            ),
                            None => format!(
                                "no live process handle is held in this engine session for \
                                 '{name}', so the guaranteed {} signalled nothing; the transition \
                                 is recorded best-effort",
                                command.as_str(),
                            ),
                        };
                        match command {
                            LifecycleCommand::Pause => TransitionCause::pause_best_effort(detail),
                            _ => TransitionCause::resume_best_effort(detail),
                        }
                    }
                };
                // AI-9: persist FIRST (the durable state leads; a transition
                // failure aborts BEFORE any signal, so the ledger can never claim
                // `paused` around a suspension that did not happen — nor the
                // reverse), THEN signal the held process.
                self.transition(registry, &name, instance.state, new_state, cause)?;
                // AI-9 (loop 1): the transition COMMITTED — if the signal now
                // fails, the ledger and the live process DIVERGE (the row says
                // paused/running while the process did not transition). The
                // divergence must never be silent (mirrors `stop_inner`'s
                // honesty): emit the breadcrumb naming instance + committed
                // state + signal error + the real recovery, then surface the
                // error as before.
                if let Err(err) = self.signal_backend(&name, command) {
                    // AI-9 (loop 2): the remediation must be budget-safe. When
                    // the failed pause was breach-driven, the per-Run breach
                    // latch is ALREADY spent — advising `resume` would leave an
                    // over-budget agent running for the rest of the Run with no
                    // re-enforcement — so that case recommends `stop` only.
                    let breach_driven =
                        matches!(cause_override, Some(TransitionCause::BudgetExceeded { .. }));
                    let remediation = match (command, breach_driven) {
                        // Row says `paused`, process still running: `resume`
                        // realigns the ledger (the SIGCONT is a harmless no-op
                        // on a running process); `stop` ends it — but for a
                        // breach-driven pause, `stop` is the ONLY safe advice.
                        (LifecycleCommand::Pause, true) => format!(
                            "kt agent stop {name} (the pause was budget-driven and the \
                             per-Run breach latch is spent — resuming would leave the \
                             over-budget run unenforced)"
                        ),
                        (LifecycleCommand::Pause, false) => {
                            format!(
                                "kt agent resume {name} to realign the ledger, or \
                                 kt agent stop {name} to end the instance"
                            )
                        }
                        // Row says `running`, process still suspended: only
                        // `stop` applies (a resume is now the invalid
                        // transition; stop's escalation reaches a stopped
                        // process where SIGTERM cannot).
                        (LifecycleCommand::Resume, _) => {
                            format!(
                                "kt agent stop {name} (its escalation reaches a suspended process)"
                            )
                        }
                        (_, _) => format!("kt agent stop {name}"),
                    };
                    let signal_failure = format!(
                        "{}: the committed {} transition says '{}', but the signal failed: {} — \
                         the ledger and the live process may diverge; recovery: {remediation}",
                        name.as_str(),
                        command.as_str(),
                        new_state.as_str(),
                        err,
                    );
                    self.emit_diagnostic(&signal_failure);
                    return Err(err);
                }
                registry.lookup(&name).map_err(registry_to_engine)
            }
            // BEST-EFFORT (AC2): transition + a VISIBLE qualifier cause, never a
            // silent success. No backend suspension is guaranteed here (on Unix a
            // best-effort declaration is unusual, but we still do NOT SIGSTOP — the
            // declared level is the contract; the qualifier is the honesty). A
            // story-3-2 budget pause overrides the cause with BudgetExceeded (the
            // best-effort posture is captured in the standalone breach event + a
            // diagnostic, so the lifecycle cause stays the honest WHY).
            SupportLevel::BestEffort => {
                self.ensure_log_dir(registry, &name)?;
                let cause = cause_override.clone().unwrap_or_else(|| {
                    let detail = format!(
                        "{} is best-effort for '{}' on {} (adapter-cooperative); the process may keep running",
                        Capability::Pause.as_str(),
                        name.as_str(),
                        os.as_str(),
                    );
                    match command {
                        LifecycleCommand::Pause => TransitionCause::pause_best_effort(detail),
                        _ => TransitionCause::resume_best_effort(detail),
                    }
                });
                self.transition(registry, &name, instance.state, new_state, cause)?;
                registry.lookup(&name).map_err(registry_to_engine)
            }
        }
    }

    /// Signal the running process for a GUARANTEED pause/resume, via the in-memory
    /// handle map (same `self.running.get_mut(&name)` pattern as `stop`).
    ///
    /// Cross-lifetime honesty (AD-5, story 1-6: adoption re-holds handles): with
    /// orphan adoption, a still-live process started by a PRIOR engine is
    /// re-acquired at [`Engine::open`] (via [`Supervisor::adopt_orphans`]), so its
    /// handle IS in the map and this path really signals it. The no-handle branch
    /// now only occurs when the row says `running`/`paused` but adoption found NO
    /// live process — a state adoption would already have reconciled to `failed`;
    /// so a lingering no-handle case is a best-effort no-op (nothing to signal).
    /// AI-8: the CALLER records that honesty in the transition cause (the
    /// best-effort qualifier naming the missing handle — decided in
    /// [`Supervisor::suspend_or_resume`] BEFORE the persist, via the
    /// `contains_key` probe) — this method still returns `Ok` (the desired end
    /// state trivially holds; nothing to signal), never a fake plain-command
    /// success in the ledger. A real held (spawned or adopted) process IS
    /// signalled.
    fn signal_backend(
        &mut self,
        name: &InstanceName,
        command: LifecycleCommand,
    ) -> Result<(), EngineError> {
        let Some(supervised) = self.running.get_mut(name) else {
            return Ok(());
        };
        // AI-9 (loop 2) wiring seam (cfg(test)): the armed instance's signal
        // fails with an injected error even though its transition has already
        // committed — the fault-injection front for the backend's pause/resume
        // (see the `signal_fault_names` field docs). Consulted AFTER the
        // no-handle probe so an armed name with nothing held keeps the honest
        // "no handle = harmless no-op" semantics above.
        #[cfg(test)]
        if self.signal_fault_names.contains(name) {
            return Err(EngineError::Backend {
                name: name.as_str().to_string(),
                source: BackendError::Control {
                    op: match command {
                        LifecycleCommand::Pause => "pause",
                        _ => "resume",
                    },
                    detail: "injected cfg(test) signal fault (AI-9 post-commit seam)".to_string(),
                },
            });
        }
        let result = match command {
            LifecycleCommand::Pause => self.backend.pause(&mut supervised.handle),
            _ => self.backend.resume(&mut supervised.handle),
        };
        result.map_err(|source| EngineError::Backend {
            name: name.as_str().to_string(),
            source,
        })
    }
}
