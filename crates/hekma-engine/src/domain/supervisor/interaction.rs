//! interaction family — split out of the supervisor monolith by story 13-3
//! (epic-13; boundary ratified 2026-09-19, study: Option B). Pure move:
//! additional `impl Supervisor` block, no signature or behavior change.

use super::*;

impl Supervisor {
    /// Send text input to a running Agent Instance's native input channel
    /// (story 4.1, FR-24, spine AD-12) — the v1 interaction surface. For
    /// every adapter that can actually run today (native mock or manifest),
    /// "the native input channel" is the spawned child's OS stdin pipe (both
    /// backends pipe it unconditionally at spawn, Task 1); this needs ZERO
    /// per-kind branching, so one method serves both (AC-A).
    ///
    /// Unlike [`Supervisor::suspend_or_resume`], `send` is NOT itself a state
    /// transition (AD-15's transition table has no `send` entry): no
    /// `next_state` call, no [`TransitionEvent`]. The dispatch order:
    ///
    /// 1. name-resolve (`NotFound` unchanged),
    /// 2. **AC-C**: the instance MUST be [`LifecycleState::Running`] —
    ///    anything else fails with [`EngineError::NotRunning`], checked
    ///    BEFORE the capability read (mirrors "transition gate before any
    ///    side effect"),
    /// 3. **AC-B**: read the effective (current-OS) `Capability::Interaction`
    ///    level — `Unsupported` FAILS FAST with
    ///    [`EngineError::CapabilityUnsupported`] (the already-generic
    ///    machinery, reused verbatim — same shape pause already produces),
    ///    no I/O attempted,
    /// 4. **AC-D**: `Guaranteed` and `BestEffort` take the IDENTICAL action —
    ///    unlike pause/resume there is no OS-conditional difference in
    ///    writing bytes to a pipe, so a declared `best-effort` is purely an
    ///    adapter-author honesty signal, not a different code path. A
    ///    missing handle, or one with no live stdin pipe (an ADOPTED
    ///    instance has no recoverable pipe — see
    ///    [`crate::ports::ProcessBackend::has_stdin`]'s docs), is a HARD
    ///    ERROR ([`EngineError::InteractionUnavailable`]): unlike
    ///    [`Supervisor::signal_backend`]'s "no handle = harmless no-op" (a
    ///    suspend/resume of an already-gone process trivially satisfies its
    ///    own desired end state), there is no equivalent "desired end state"
    ///    for text that was never delivered — a silent success would violate
    ///    FR-24's "honest failure" framing, and this must NEVER be
    ///    misattributed to `CapabilityUnsupported` (the declaration is
    ///    truthful; it is this engine session's reach that is limited).
    ///    **Fix pass addition (review of #79):** a handle whose PRIOR write
    ///    already timed out ([`crate::ports::ProcessBackend::stdin_timed_out`])
    ///    fails fast with [`EngineError::InteractionTimedOut`] here too — a
    ///    cheap, no-I/O check, never a repeat doomed write.
    /// 5. **AC-F**: append exactly one trailing `\n` if `text` doesn't
    ///    already end with one, then write + flush via
    ///    [`crate::ports::ProcessBackend::write_stdin`] — BOUNDED to
    ///    [`crate::ports::STDIN_WRITE_TIMEOUT`] (fix pass, the CRITICAL
    ///    finding: the original unbounded write could freeze the ENTIRE
    ///    engine, since this call runs while the caller already holds the
    ///    single, engine-wide supervisor lock — see `write_stdin`'s docs). A
    ///    timeout maps to [`EngineError::InteractionTimedOut`]; any OTHER
    ///    [`BackendError`] maps to [`EngineError::Backend`] — the SAME
    ///    generic mapping `signal_backend` already uses for pause/resume.
    pub fn send_input(
        &mut self,
        registry: &Registry,
        name: &str,
        text: &str,
    ) -> Result<(), EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = registry.lookup(&name).map_err(registry_to_engine)?;

        // (1) AC-C: send is not a transition, so this is a dedicated
        // pre-flight state check — before any capability read or I/O.
        if instance.state != LifecycleState::Running {
            return Err(EngineError::NotRunning {
                name: name.as_str().to_string(),
                state: instance.state.as_str().to_string(),
            });
        }

        // (2) AC-B: reuse the already-generic capability-unsupported
        // fail-fast machinery verbatim.
        let level = registry
            .effective_support(&name, Capability::Interaction)
            .map_err(registry_to_engine)?;
        let os = OsId::current();
        if level == SupportLevel::Unsupported {
            return Err(EngineError::CapabilityUnsupported {
                name: name.as_str().to_string(),
                capability: Capability::Interaction.as_str().to_string(),
                os: os.as_str().to_string(),
                level: level.as_str().to_string(),
            });
        }

        // (3) AC-D: Guaranteed and BestEffort collapse to the SAME action
        // below (no OS-conditional difference in delivering bytes to a
        // pipe). A missing handle, or one with no live stdin pipe (an
        // adopted instance), is a HARD error — never a silent success.
        let Some(supervised) = self.running.get_mut(&name) else {
            return Err(EngineError::InteractionUnavailable {
                name: name.as_str().to_string(),
                detail: "no live process handle is held in this engine session".to_string(),
            });
        };

        // (3-acp) Story 14-1 (spine AD-19): the acp arm. The connection owns
        // the child's stdin (taken from the handle at start), so the legacy
        // pipe checks below do not apply. A prompt is ONE `session/prompt`
        // request (ONE text ContentBlock) written through the connection's
        // bounded-writer mutex, and this method returns IMMEDIATELY after the
        // bounded write — the turn's chunks + final stopReason land
        // asynchronously in the output log via the reader. A SECOND prompt
        // while a turn is in flight is the surfaced typed refusal (ACP
        // serializes turns per session); the first turn is unaffected.
        if let Some(connection) = supervised.acp.as_ref() {
            if connection.turn_in_flight() {
                return Err(EngineError::AcpTurnInFlight {
                    name: name.as_str().to_string(),
                });
            }
            return match connection.send_prompt(text) {
                Ok(()) => Ok(()),
                Err(crate::acp::PromptError::InFlight) => Err(EngineError::AcpTurnInFlight {
                    name: name.as_str().to_string(),
                }),
                Err(crate::acp::PromptError::TimedOut) => Err(EngineError::InteractionTimedOut {
                    name: name.as_str().to_string(),
                    timeout_secs: crate::ports::STDIN_WRITE_TIMEOUT.as_secs(),
                }),
                Err(err) => Err(EngineError::InteractionUnavailable {
                    name: name.as_str().to_string(),
                    detail: err.to_string(),
                }),
            };
        }

        // Fix pass (CRITICAL finding, review of #79): a cheap, no-I/O check
        // FIRST — a handle whose prior write already exceeded the bounded
        // timeout is PERMANENTLY broken for the rest of this engine session
        // (see `write_stdin`'s docs). Checked before `has_stdin` (which would
        // also read `false` here) so the more precise, honest diagnostic
        // wins: "we had a pipe and it stopped draining" is a materially
        // different fact from "no pipe was ever recoverable", and the CLI's
        // remediation differs (restart to get a fresh channel either way, but
        // the cause is not the same).
        if self.backend.stdin_timed_out(&supervised.handle) {
            return Err(EngineError::InteractionTimedOut {
                name: name.as_str().to_string(),
                timeout_secs: crate::ports::STDIN_WRITE_TIMEOUT.as_secs(),
            });
        }
        if !self.backend.has_stdin(&supervised.handle) {
            return Err(EngineError::InteractionUnavailable {
                name: name.as_str().to_string(),
                detail: "no live stdin pipe is held for this instance in this engine session \
                         (an adopted instance has no recoverable pipe; durable cross-invocation \
                         interaction needs a persistent engine session, planned for Epic 7/v1.x)"
                    .to_string(),
            });
        }

        // (4) AC-F: append exactly one trailing newline if absent, so a
        // line-oriented agent (`BufRead::read_line`) receives a complete
        // line.
        let mut bytes = text.as_bytes().to_vec();
        if !text.ends_with('\n') {
            bytes.push(b'\n');
        }
        // Fix pass (CRITICAL finding): this write is now BOUNDED to
        // `STDIN_WRITE_TIMEOUT` (`write_stdin`'s new contract) rather than
        // the story's original unbounded `write_all` — still runs while
        // `self` (the supervisor) is held under the caller's lock, exactly
        // like `stop`'s existing bounded graceful-window wait; a deliberate,
        // ACCEPTED, BOUNDED tradeoff, not the unbounded-freeze problem this
        // fix closes.
        match self.backend.write_stdin(&mut supervised.handle, &bytes) {
            Ok(()) => Ok(()),
            Err(BackendError::StdinTimedOut { timeout_secs }) => {
                Err(EngineError::InteractionTimedOut {
                    name: name.as_str().to_string(),
                    timeout_secs,
                })
            }
            Err(source) => Err(EngineError::Backend {
                name: name.as_str().to_string(),
                source,
            }),
        }
    }

    /// The current [`RunId`] for a supervised instance (story 3-1), or `None` if
    /// this engine holds no live handle for it (never started this lifetime, or
    /// already stopped/crashed). The Fleet read uses it to scope the current-Run
    /// token totals; a `None` simply means "no active Run" (current-run totals are
    /// zero). Held in memory alongside the process handle for this engine lifetime.
    pub fn current_run_id(&self, name: &InstanceName) -> Option<RunId> {
        self.running.get(name).map(|s| s.run_id.clone())
    }

    /// Read the recorded [`TransitionEvent`]s for an instance from its log
    /// (observation helper for tests / embedders; the AD-14 seed, NOT the 7-2
    /// bus). Returns an empty vec if the log does not exist yet.
    pub fn read_events(
        registry: &Registry,
        name: &str,
    ) -> Result<Vec<TransitionEvent>, EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let path = registry.instance_log_path(&name);
        read_events_from(&path).map_err(|detail| EngineError::Log {
            name: name.as_str().to_string(),
            path: path.to_string_lossy().into_owned(),
            detail,
        })
    }

    /// One-shot full read of every currently-retained ATTRIBUTED output line
    /// for an instance (story 4-2, AC-A/AC-G) — reads the rotated generations
    /// OLDEST-to-newest (`.2`, `.1`, current — skipping any that do not exist
    /// yet), concatenates, and parses each JSON-Lines [`LogLine`] record in
    /// ON-DISK APPEND ORDER (the sole ordering authority; NEVER re-sorted by
    /// `at` — AC-G, since `now_rfc3339`'s whole-second resolution makes
    /// same-second lines common). This reads the NEW, SEPARATE
    /// `logs/output.log[.N]` file (CRITICAL SCOPING #3) — never `agent.log`,
    /// which stays byte-identical and untouched for Epic 3's
    /// `drain_usage_for`.
    ///
    /// DELIBERATE IMPROVEMENT over the `read_events`/`read_breach_events`
    /// precedent above (which never check the registry for the instance's
    /// existence at all — harmless there, since neither is exposed via any
    /// `kt` command): `read_agent_log` is the FIRST CLI-facing consumer of
    /// this shape (`kt agent logs`, Task 6), where silently showing "no
    /// output" for a mistyped name would be genuinely confusing UX
    /// (indistinguishable from "the agent just hasn't said anything yet").
    /// So this DOES check the registry first: a truly UNREGISTERED name
    /// fails [`EngineError::NotFound`] (matching every other CLI-facing
    /// command — `show`/`send`/`pause` all do this); a REGISTERED-but-never-
    /// started instance still falls through to an honest empty vec (mirrors
    /// `read_events_from`'s "missing file → empty" precedent).
    ///
    /// Fix pass (M1, review of #80): ALSO returns the byte-cursor position
    /// (into the CURRENT generation, matching
    /// [`Supervisor::read_agent_log_since`]'s cursor shape exactly) this
    /// read reached — computed from the SAME bytes this call parsed, never a
    /// second, separately-timed read. `kt agent logs --follow` (the sole
    /// production caller) primes its poll loop's cursor from this value
    /// directly, instead of a SEPARATE `read_agent_log_since(name, 0)` call
    /// whose returned lines it used to discard — that discarding call read
    /// up to a slightly LATER point in time than this one-shot dump, so
    /// anything emitted in the gap between the two reads was silently lost
    /// before `--follow` ever started polling. Returning the cursor here
    /// closes that gap: there is only ever ONE read establishing both the
    /// dump and the resume point.
    pub fn read_agent_log(
        registry: &Registry,
        name: &str,
    ) -> Result<(Vec<LogLine>, u64), EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        registry.lookup(&name).map_err(registry_to_engine)?;

        let mut lines = Vec::new();
        // Oldest generation first (LOG_ROTATE_GENERATIONS - 1 down to 1),
        // then the current generation last — append order overall.
        for generation in (1..LOG_ROTATE_GENERATIONS).rev() {
            let path = registry.attributed_output_log_generation_path(&name, generation);
            read_log_lines_from(&path, &mut lines).map_err(|detail| EngineError::Log {
                name: name.as_str().to_string(),
                path: path.to_string_lossy().into_owned(),
                detail,
            })?;
        }
        let current = registry.attributed_output_log_path(&name);
        // Read the CURRENT generation's raw text ONCE so its exact byte
        // length (the cursor) and its parsed lines come from the identical
        // bytes — never a second, later, potentially-inconsistent read.
        let current_text = match std::fs::read_to_string(&current) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(EngineError::Log {
                    name: name.as_str().to_string(),
                    path: current.to_string_lossy().into_owned(),
                    detail: e.to_string(),
                })
            }
        };
        let cursor = current_text.len() as u64;
        parse_log_lines(&current_text, &mut lines).map_err(|detail| EngineError::Log {
            name: name.as_str().to_string(),
            path: current.to_string_lossy().into_owned(),
            detail,
        })?;
        Ok((lines, cursor))
    }

    /// A CURSOR-based follow read for `kt agent logs --follow`'s poll loop
    /// (story 4-2, AC-B/AC-C/AC-H, AD-13). `cursor` is a byte offset into the
    /// CURRENT generation ONLY (mirrors `agent_log_len`/`plan_drain`'s
    /// existing cursor shape) — distinct from `read_agent_log`'s
    /// concatenated multi-generation view, so a caller must not mix cursors
    /// from the two methods. Returns `(new_lines, next_cursor)` — plain
    /// request/response (AD-13), never a `Stream`-typed API (see the story's
    /// Dev Notes on why: this keeps the existing async/blocking pairing with
    /// zero new API shape).
    ///
    /// On a detected SHRINK (the current generation's length is now LESS
    /// than `cursor` — a rotation happened since the last poll), the cursor
    /// snaps to the new length and this returns `(vec![], new_len)` — the
    /// CALLER detects the signal itself by comparing the returned cursor to
    /// the one it just passed in (`next_cursor < cursor`) and prints one
    /// honest notice (Task 6); `read_agent_log` WITHOUT `--follow` always
    /// re-reads everything currently retained, so this never loses data
    /// permanently — only a possible (rare) live-tail gap at the rotation
    /// boundary.
    pub fn read_agent_log_since(
        registry: &Registry,
        name: &str,
        cursor: u64,
    ) -> Result<(Vec<LogLine>, u64), EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        registry.lookup(&name).map_err(registry_to_engine)?;

        let path = registry.attributed_output_log_path(&name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(EngineError::Log {
                    name: name.as_str().to_string(),
                    path: path.to_string_lossy().into_owned(),
                    detail: e.to_string(),
                })
            }
        };
        match plan_follow(&bytes, cursor) {
            FollowPlan::Shrunk { new_cursor } => Ok((Vec::new(), new_cursor)),
            FollowPlan::Consume { range, new_cursor } => {
                let mut lines = Vec::new();
                if !range.is_empty() {
                    parse_log_lines(&String::from_utf8_lossy(&bytes[range]), &mut lines).map_err(
                        |detail| EngineError::Log {
                            name: name.as_str().to_string(),
                            path: path.to_string_lossy().into_owned(),
                            detail,
                        },
                    )?;
                }
                Ok((lines, new_cursor))
            }
        }
    }

    /// Ensure the per-instance log directory exists (AD-12 seed).
    pub(super) fn ensure_log_dir(
        &self,
        registry: &Registry,
        name: &InstanceName,
    ) -> Result<(), EngineError> {
        let dir = registry.instance_log_dir(name);
        std::fs::create_dir_all(&dir).map_err(|e| EngineError::Log {
            name: name.as_str().to_string(),
            path: dir.to_string_lossy().into_owned(),
            detail: e.to_string(),
        })
    }

    // ---- Self-reported usage ingestion → the ONE ledger-commit choke point ----
    //      (story 3-1, spine AD-6/AD-7/AD-12)

    /// The current byte length of an instance's agent-output log, or 0 if it does
    /// not exist yet. Used to set the ingestion cursor at a Run's start so a new
    /// Run never re-reads a prior Run's already-captured lines.
    pub(super) fn agent_log_len(&self, registry: &Registry, name: &InstanceName) -> u64 {
        std::fs::metadata(registry.agent_output_log_path(name))
            .map(|m| m.len())
            .unwrap_or(0)
    }
}
