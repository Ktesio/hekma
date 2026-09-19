//! usage family — split out of the supervisor monolith by story 13-3
//! (epic-13; boundary ratified 2026-09-19, study: Option B). Pure move:
//! additional `impl Supervisor` block, no signature or behavior change.

use super::*;

impl Supervisor {
    /// Drain self-reported usage from EVERY currently-running instance (the reaper
    /// cadence). Best-effort per instance — one instance's drain failure never
    /// blocks another's or crash detection.
    ///
    /// This is the MID-RUN cadence: the process is (believed) still alive, so a
    /// half-written final line is left for the next pass ([`DrainMode::MidRun`]).
    pub(super) fn drain_usage_all(&mut self, registry: &Registry) {
        let names: Vec<InstanceName> = self.running.keys().cloned().collect();
        for name in names {
            self.drain_usage_for(registry, &name, DrainMode::MidRun);
        }
    }

    /// Drain the NEWLY-captured tail of one instance's agent-output log, ingesting
    /// each well-formed usage sentinel line through the commit choke point
    /// ([`Supervisor::ingest_usage`]), and advance the read cursor.
    ///
    /// Reads from the per-instance cursor to the file's end (only the bytes written
    /// since the last drain), parses usage lines via the self-reported
    /// [`UsageSource`](crate::ports::UsageSource), and records each. A read error
    /// (log gone / unreadable) is a best-effort skip — the DB is the source of
    /// truth, and the next pass retries. Malformed usage lines are skipped inside
    /// the parser (a diagnostic, never fatal — AD-12).
    ///
    /// **AI-41 (billing honesty): the cursor only advances past DURABLE bytes.**
    /// Every parsed event is ingested FIRST; the cursor moves past the consumed
    /// block only when every event committed (or was a recognized duplicate
    /// replay). A store error parks the cursor where it was — the failed event is
    /// retried on the next drain, and already-committed neighbors in the same
    /// block re-drift safely into the DB dedup key (`DuplicateReplay`, never a
    /// double-count). The pre-fix advance-then-ingest order silently dropped any
    /// event whose INSERT failed.
    ///
    /// The `mode` decides how the TAIL is treated (story 3-1 under-count fix, H1):
    /// * [`DrainMode::MidRun`] — the process may still be mid-`writeln!`, so only
    ///   bytes UP TO the last newline are consumed; a partial trailing line waits
    ///   for the next drain (it lands whole then).
    /// * [`DrainMode::Terminal`] — the process is DEAD (drain-on-stop / drain-on-
    ///   reap); no more bytes will ever append, so a final usage line flushed
    ///   WITHOUT a trailing newline is consumed to end-of-log rather than stranded
    ///   (which the next Run's cursor would skip past → a permanent under-count).
    ///
    /// Log-shrink guard (M2): if the file is shorter than the cursor (a truncate /
    /// rotation — nothing in-tree does this yet; Epic 4 owns rotation), we do NOT
    /// re-read from 0 under the same live `run_id` (that would re-ingest already-
    /// counted lines → a double-count, an INFLATED bill). We instead treat it as an
    /// anomaly: advance the cursor to the new length and ingest nothing this pass.
    /// Proper rotation handling is deferred to Epic 4.
    pub(super) fn drain_usage_for(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        mode: DrainMode,
    ) {
        // Only running/adopted instances have a cursor + metering context.
        let (cursor, run_id, metering_source) = match self.running.get(name) {
            Some(s) => (s.usage_cursor, s.run_id.clone(), s.metering_source.clone()),
            None => return,
        };
        let path = registry.agent_output_log_path(name);
        // AI-63 (billing-critical stall fix): read ONLY the tail written since the
        // last drain (`[cursor, len)`) — NEVER the whole never-rotated `agent.log`.
        // `read_usage_tail` returns the SAME bytes the old whole-file read's
        // `bytes[cursor..]` slice held (and catches the M2 shrink, where
        // `len - cursor` would underflow), so the BILLING decision below —
        // `plan_drain` on those exact tail bytes — is byte-identical to before.
        // See `read_usage_tail`'s docs for the full equivalence + snapshot proof.
        let tail = match read_usage_tail(&path, cursor) {
            // Read error: best-effort skip, cursor untouched, retry next pass —
            // identical to the old `let Ok(bytes) = std::fs::read(..) else { return }`.
            UsageTail::Unavailable => return,
            // M2 shrink guard: snap the cursor to the file's new (shorter) length
            // and ingest nothing — identical to the old `DrainPlan::Shrunk { .. }`
            // arm (never re-read from 0 under the same live `run_id` → no
            // double-count → no inflated bill).
            UsageTail::Shrunk { new_cursor } => {
                if let Some(s) = self.running.get_mut(name) {
                    s.usage_cursor = new_cursor;
                }
                return;
            }
            UsageTail::Tail { bytes } => bytes,
        };
        // `tail` == the old code's `bytes[cursor..]`. Feeding it to `plan_drain`
        // with a 0 base makes the SAME `(bytes, cursor, mode)` decision on the same
        // bytes, just in tail-relative coordinates: the returned `range` slices
        // `tail` directly (0-based), and the returned count is added to `cursor` to
        // recover the ABSOLUTE cursor. The MidRun (up-to-last-newline) and Terminal
        // (whole tail, incl. a newline-less final line — H1) rules are computed
        // purely from these bytes, so BOTH are preserved unchanged.
        match plan_drain(&tail, 0, mode) {
            DrainPlan::Consume {
                range,
                new_cursor: consumed,
            } => {
                let block = String::from_utf8_lossy(&tail[range]);
                let parsed = self.usage_source.drain(&block);
                // AI-41 (billing honesty): ingest FIRST; advance the cursor past
                // the consumed block ONLY when every parsed event committed (or
                // was a recognized duplicate replay). The pre-fix code advanced
                // the cursor BEFORE ingesting, so a store error silently DROPPED
                // the event(s) behind it — usage that never reached the ledger
                // and was never retried. Now a store error PARKS the cursor
                // where it was: the failed event is retried on the next drain,
                // and any already-committed neighbor in the same block re-drifts
                // safely into the DB dedup key (`DuplicateReplay` — never a
                // double-count; see the design note on AI-41's dedup safety).
                let mut committed_count = 0usize;
                for usage in &parsed {
                    if self
                        .ingest_usage(registry, name, &run_id, &metering_source, usage, mode)
                        .is_ok()
                    {
                        committed_count += 1;
                    } else {
                        // Park at the FIRST store error — later events in this
                        // block are not even attempted, so they cannot leapfrog
                        // the failed one.
                        break;
                    }
                }
                // `cursor + consumed` equals the old whole-file path's
                // `new_cursor` (which was `cursor + consumed`) exactly —
                // reached only when every event behind the advance is durable.
                if committed_count == parsed.len() {
                    if let Some(s) = self.running.get_mut(name) {
                        s.usage_cursor = cursor + consumed;
                        s.usage_park_attempts = None;
                    }
                } else if mode == DrainMode::Terminal {
                    // AI-41 (loop 1): on the TERMINAL drain there IS no next
                    // pass — the handle is being removed right after, and with
                    // it the cursor and the Run context. The park-and-retry
                    // claim must not silently fail exactly where loss is
                    // likeliest: say the batch is lost, out loud.
                    let lost = parsed.len() - committed_count;
                    let loss = format!(
                        "{}: {lost} usage event(s) in the final drained block could not be \
                         committed to the Usage Ledger and were NOT counted — this is the \
                         terminal drain (the process is dead or the handle is being removed), \
                         so the batch cannot be retried and is lost",
                        name.as_str(),
                    );
                    self.emit_diagnostic(&loss);
                } else {
                    // AI-41 (loop 2): BOUND the park. A permanently failing
                    // event (a row poisoned beyond what the dedup key covers)
                    // would otherwise wedge the cursor at this offset forever —
                    // silently stranding every later usage event for the Run.
                    // After USAGE_PARK_MAX_ATTEMPTS failed passes AT THE SAME
                    // offset, skip the block with a loud diagnostic: billing
                    // honesty cuts both ways — announce the loss, don't strand
                    // the ledger.
                    let attempts = match self.running.get(name).and_then(|s| s.usage_park_attempts)
                    {
                        Some((parked_cursor, n)) if parked_cursor == cursor => n + 1,
                        _ => 1,
                    };
                    if attempts >= USAGE_PARK_MAX_ATTEMPTS {
                        let lost = parsed.len() - committed_count;
                        if let Some(s) = self.running.get_mut(name) {
                            s.usage_cursor = cursor + consumed;
                            s.usage_park_attempts = None;
                        }
                        let skip = format!(
                            "{}: {lost} usage event(s) at byte offset {cursor} failed to \
                             commit on {attempts} consecutive drains and are SKIPPED (not \
                             counted) — the cursor moves past them so the rest of the Run's \
                             usage keeps counting; investigate the Usage Ledger store",
                            name.as_str(),
                        );
                        self.emit_diagnostic(&skip);
                    } else if let Some(s) = self.running.get_mut(name) {
                        s.usage_park_attempts = Some((cursor, attempts));
                    }
                }
            }
            // Nothing to consume this pass (an empty tail, or a MidRun tail with no
            // newline yet) — leave the cursor where it is, exactly as before.
            DrainPlan::Nothing => {}
            // Unreachable by construction: `plan_drain` returns `Shrunk` only when
            // its `cursor` argument exceeds the slice length, and the base here is
            // 0 (`0 > len` is impossible). The REAL shrink is handled above in
            // `read_usage_tail`, where the file length is known WITHOUT a whole-file
            // read. Leave the cursor untouched (nothing was ingested, so no
            // miscount) — a billing path must never panic.
            DrainPlan::Shrunk { .. } => {}
        }
    }

    /// Drain ENGINE-OBSERVED usage from EVERY currently-running observed instance
    /// (story 3-4 — the reaper cadence, parallel to [`Self::drain_usage_all`]).
    /// Best-effort per instance — one instance's drain never blocks another's or
    /// crash detection. A `self-reported` instance (no observed listener) is a
    /// no-op here (it rides the log-tail drain instead).
    pub(super) fn drain_observed_all(&mut self, registry: &Registry) {
        let names: Vec<InstanceName> = self.running.keys().cloned().collect();
        for name in names {
            self.drain_observed_for(registry, &name, DrainMode::MidRun);
        }
    }

    /// Drain one instance's OBSERVED usage queue (the counts the loopback listener
    /// parsed out of the agent's model traffic) into the SAME [`Self::ingest_usage`]
    /// choke point (story 3-4), minting the per-Run `sequence` for each.
    ///
    /// The listener task PUSHES each parsed `(input, output)` pair; this reaper pass
    /// DRAINS the queue (event-driven, NOT the log-tail path — observed usage does
    /// NOT ride the agent-output log, AD-12 contrast), the [`ObservedUsageSource`]
    /// mints the engine-side `sequence` (the agent supplies none), and each becomes
    /// a `ParsedUsage` fed to `ingest_usage` under the instance's CURRENT Run id +
    /// `engine-observed` source. NO new ledger writer, NO new enforcement path — the
    /// SAME choke point stamps + records + enforces (so 3-2 budgets + 3-3 caps apply
    /// unchanged). A `self-reported` instance (no `observed_source`/`observed_listener`)
    /// is a no-op.
    ///
    /// **Story 12-4 — the AI-41 treatment for the observed channel (durability
    /// under store failure):** a commit failure PARKS the un-committed minted
    /// events (in [`Supervised::observed_park`]) and RETRIES the EXACT same
    /// `ParsedUsage` values on the next pass — never re-minted, so the dedup
    /// keys are stable and an already-committed neighbor re-drifts safely into
    /// the DB dedup (a `DuplicateReplay`, never a double-count). The retry is
    /// BOUNDED: after [`USAGE_PARK_MAX_ATTEMPTS`] consecutive failed passes at
    /// the SAME front event, that event is SKIPPED with a loud diagnostic and
    /// the events behind it keep counting (surfaced-not-silent: an announced
    /// loss, never a silent drop, never a wedged drain). In the TERMINAL mode
    /// (stop / crash-reap) there IS no next pass — the loss is announced
    /// explicitly and the parked buffer dies with the instance. The mode is the
    /// [`DrainMode`] analog of the self-reported drain's.
    pub(super) fn drain_observed_for(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        mode: DrainMode,
    ) {
        // Read the Run context; park-retry first, then drain the queue, all under
        // the instance's held state (a short critical section). Ingest happens
        // OUTSIDE the borrow so `ingest_usage` can take `&mut self`.
        let (run_id, metering_source, events) = match self.running.get(name) {
            Some(s) => {
                // Only an observed instance has both a listener (its queue) + a source
                // (the sequence minter). A self-reported instance skips (no-op).
                let (Some(listener), Some(source)) =
                    (s.observed_listener.as_ref(), s.observed_source.as_ref())
                else {
                    return;
                };
                // 12-4: retry any PARKED events FIRST, preserving order — the exact
                // same `ParsedUsage` values as the failed pass (never re-minted).
                let mut events = s
                    .observed_park
                    .as_ref()
                    .map(|(pending, _)| pending.events.clone())
                    .unwrap_or_default();
                // Then drain the NEW queue entries and mint their per-Run sequences.
                // A poisoned queue lock is a best-effort skip of the DRAIN — any
                // parked events still retry below, and the queued pairs stay queued
                // for the next pass (never silently lost).
                let drained: Vec<(u64, u64)> = match listener.queue().lock() {
                    Ok(mut q) => q.drain(..).collect(),
                    Err(_) => Vec::new(),
                };
                for (input, output) in drained {
                    events.push(source.mint(input, output));
                }
                if events.is_empty() {
                    return;
                }
                (s.run_id.clone(), s.metering_source.clone(), events)
            }
            None => return,
        };
        // Ingest each observed event through the SAME single choke point (stamps the
        // Run id + `engine-observed` source + timestamp, records, and enforces).
        // Story 12-4: break on the FIRST store error — later events cannot leapfrog
        // the failed one (billing honesty: the ledger sequence stays order-faithful).
        let mut committed_count = 0usize;
        for usage in &events {
            if self
                .ingest_usage(registry, name, &run_id, &metering_source, usage, mode)
                .is_ok()
            {
                committed_count += 1;
            } else {
                break;
            }
        }
        if committed_count == events.len() {
            // Every event durable (or a recognized duplicate replay): the park
            // clears.
            if let Some(s) = self.running.get_mut(name) {
                s.observed_park = None;
            }
        } else if mode == DrainMode::Terminal {
            // 12-4, the terminal arm: there IS no next pass — the handle (and any
            // parked buffer) is dropped right after. Announce the loss explicitly,
            // never a silent drop and never a false retry claim, and drop the park
            // (no phantom retry state survives the instance). (The local is
            // deliberately NOT named `loss`: the embed-clean audit pins
            // `emit_diagnostic(&loss)` to the ONE AI-41 self-reported site, and a
            // duplicate marker would read as a route regression.)
            let lost = events.len() - committed_count;
            let observed_loss = format!(
                "{}: {lost} observed usage event(s) could not be committed to the Usage \
                 Ledger and are LOST — this is the terminal drain (the process is dead or \
                 the handle is being removed, and any parked buffer is dropped with it), \
                 so they cannot be retried",
                name.as_str(),
            );
            self.emit_diagnostic(&observed_loss);
            if let Some(s) = self.running.get_mut(name) {
                s.observed_park = None;
            }
        } else {
            // 12-4, the bounded MIDRUN park: park the un-committed tail (including
            // the failed front) and count the attempt against the SAME front event.
            // The 12-4 AMENDMENT cap runs FIRST — a store outage keeps minting
            // events every tick, so the tail itself must be bounded (overflow
            // drops the OLDEST with a loud diagnostic; see the helper).
            let remaining: Vec<ParsedUsage> =
                self.cap_observed_park_buffer(name, events[committed_count..].to_vec());
            if remaining.is_empty() {
                // The cap dropped EVERYTHING (a backlog larger than the whole
                // buffer, loudly announced by the helper): no park state
                // survives this pass — never a phantom one-front buffer.
                if let Some(s) = self.running.get_mut(name) {
                    s.observed_park = None;
                }
                return;
            }
            let front_sequence = remaining[0].sequence;
            let attempts = match self
                .running
                .get(name)
                .and_then(|s| s.observed_park.as_ref())
            {
                Some((pending, n)) if pending.front_sequence == front_sequence => n + 1,
                // A different front event resets the streak (the cursor-analog).
                _ => 1,
            };
            if attempts >= USAGE_PARK_MAX_ATTEMPTS {
                // The front event is poisoned beyond the dedup key's reach: SKIP it
                // LOUDLY and keep counting the rest (an announced loss — billing
                // honesty cuts both ways — never a wedged drain). The events BEHIND
                // the skipped front park again with a fresh streak (their front
                // changed), so they retry on the next pass.
                let rest: Vec<ParsedUsage> = remaining[1..].to_vec();
                let skip = format!(
                    "{}: an observed usage event (sequence {front_sequence}) failed to \
                     commit on {attempts} consecutive drains and is SKIPPED (not \
                     counted) — the rest of the Run's observed usage keeps counting; \
                     investigate the Usage Ledger store",
                    name.as_str(),
                );
                self.emit_diagnostic(&skip);
                if let Some(s) = self.running.get_mut(name) {
                    let next_front = rest.first().map(|next| next.sequence);
                    s.observed_park = next_front.map(|front_sequence| {
                        (
                            ObservedPending {
                                front_sequence,
                                events: rest.clone(),
                            },
                            1,
                        )
                    });
                }
            } else if let Some(s) = self.running.get_mut(name) {
                s.observed_park = Some((
                    ObservedPending {
                        front_sequence,
                        events: remaining,
                    },
                    attempts,
                ));
            }
        }
    }

    /// Story 12-4 AMENDMENT (review loop 1): bound the observed pending-park
    /// buffer to [`OBSERVED_PARK_MAX_EVENTS`]. Returns the events to park; when
    /// the tail exceeds the cap, the OLDEST events are dropped (the ledger's
    /// order-faithful sequence discipline cuts both ways: the newest events are
    /// the ones still reachable for retry) and the loss is announced LOUDLY —
    /// the count, the dropped sequence range, the why, and the remediation —
    /// never a silent truncation, never unbounded engine memory during a store
    /// outage.
    fn cap_observed_park_buffer(
        &mut self,
        name: &InstanceName,
        mut events: Vec<ParsedUsage>,
    ) -> Vec<ParsedUsage> {
        let overflow = events.len().saturating_sub(OBSERVED_PARK_MAX_EVENTS);
        if overflow > 0 {
            let dropped = events.drain(..overflow).collect::<Vec<_>>();
            let first = dropped.first().map(|e| e.sequence).unwrap_or(0);
            let last = dropped.last().map(|e| e.sequence).unwrap_or(0);
            let overflow_note = format!(
                "{}: {overflow} parked observed usage event(s) (sequences {first}..{last}) \
                 were DROPPED and are NOT counted — the pending buffer is capped at \
                 {OBSERVED_PARK_MAX_EVENTS} events while the Usage Ledger store keeps \
                 failing, so the oldest un-committed events had to go; investigate the \
                 store outage",
                name.as_str(),
            );
            self.emit_diagnostic(&overflow_note);
        }
        events
    }

    /// THE ledger-commit choke point (story 3-1, spine AD-7) — the SOLE writer of
    /// the `usage_events` table.
    ///
    /// Constructs the full [`UsageEvent`] from the agent-supplied [`ParsedUsage`]
    /// plus the engine-stamped fields (the current Run id, the instance name, the
    /// metering source, and the commit timestamp), then records it in its OWN
    /// transaction via `record_usage_event` (AD-6: one transaction per event). A
    /// re-delivered batch is classified [`RecordOutcome::DuplicateReplay`] by the
    /// DB `UNIQUE` index and is a no-op (AC-A no-double-count). On a fresh insert it
    /// builds the AD-14 [`UsageUpdateEvent`] (the wire shape frozen in 3-1;
    /// delivered on the event bus since story 7-2).
    ///
    /// **AI-41 — a store error is REPORTED, never silently dropped:** the method
    /// returns the failure so the self-reported drain can PARK its cursor
    /// and retry the event on the next pass (the observed drain reports + skips —
    /// it has no cursor). A diagnostic is emitted through the AD-12 sink either
    /// way, so dropped usage is always VISIBLE. Usage ingestion still never
    /// crashes the supervisor or a lifecycle op (the ledger is advisory to the
    /// RUN, not gating it) — honest reporting, not a panic.
    ///
    /// **12-4 AMENDMENT (review loop 1) — the diagnostic is MODE-aware:** the
    /// `mode` the CALLER is draining under decides what the failure text may
    /// honestly claim. A `MidRun` caller WILL re-attempt, so the text says the
    /// event "is parked for retry on the next drain". A `Terminal` caller
    /// (stop / crash-reap) has NO next pass — its text says the event is LOST
    /// and will NOT be retried — because a terminal diagnostic that promised a
    /// retry would contradict the adjacent LOST notice and lie to the operator
    /// (surfaced-not-silent means the surface must also be TRUE).
    ///
    /// **The AD-7 single-writer invariant lives here:** no other code path may call
    /// `record_usage_event`.
    ///
    /// **The AD-7 ENFORCEMENT stage lives here too (story 3-2):** IMMEDIATELY after
    /// a fresh `Inserted` commit — in the SAME synchronous path, before returning —
    /// this method reads the CURRENT resolved [`TokenBudget`] + [`BreachAction`]
    /// (a LIVE config read, so a budget changed while `running` applies on the very
    /// next event — AC-B), reads the just-committed per-run + cumulative token
    /// totals (3-1's `usage_totals`/`run_totals`), and calls the pure
    /// [`BudgetEvaluator`]. On a [`BreachDecision::Breached`] it RECORDS the breach
    /// event FIRST/independently ([`Self::record_breach`]) — so a best-effort/
    /// unsupported/failed pause never loses the breach record (FR-21 "always
    /// recorded regardless of action") — and THEN executes the action via Epic-1's
    /// lifecycle (`pause`/`stop`/`warn`). This is the SOLE enforcement site (the
    /// AD-7 companion to the single-writer invariant). A [`RecordOutcome::DuplicateReplay`]
    /// is NOT evaluated (nothing new was committed → no new breach can occur).
    /// Ingestion + enforcement stay best-effort to the RUN: a store/lifecycle error
    /// is a diagnostic, NEVER a supervisor crash (3-1's rule extended to enforcement).
    fn ingest_usage(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        metering_source: &str,
        parsed: &ParsedUsage,
        mode: DrainMode,
    ) -> Result<Option<UsageUpdateEvent>, crate::domain::error::RegistryError> {
        let event = assemble_usage_event(
            parsed,
            name.as_str(),
            run_id.clone(),
            metering_source,
            now_rfc3339(),
        );
        // Story 3-3 — NO-RETROACTIVE-REPRICING: resolve the EFFECTIVE Rate at COMMIT
        // (a live config read) and PERSIST it onto this row, so historical dollars
        // keep the Rate in force when consumed. A later Rate change re-prices FUTURE
        // events only (each row is priced at its own stored Rate on read). A degraded
        // config read / absent-or-half Rate → `None` (the row contributes $0; AC-B).
        let rate = registry
            .effective_config(name, ConfigLayer::empty())
            .ok()
            .and_then(|eff| config::resolve_cost(&eff).0);
        match registry.record_usage_event(&event, rate) {
            // A fresh row: build the AD-14 usage-update wire struct (frozen in 3-1;
            // published on the event bus since 7-2), THEN run the AD-7 enforcement
            // stage on the just-committed totals — synchronously, in this same
            // commit path.
            Ok(RecordOutcome::Inserted) => {
                let update = UsageUpdateEvent::new(event);
                // Story 7-2: the ledger row COMMITTED — publish onto the event
                // bus BEFORE the enforcement stage runs, preserving commit
                // order (the usage row precedes any breach/transition the
                // enforcement commits, so the bus shows exactly the durable
                // sequence: usage → token breach → pause → dollar breach).
                // Ordering obligation: this runs under the supervisor lock
                // (see the `domain::bus` caller-enforced invariant).
                self.publish(EngineEvent::UsageUpdate(update.clone()));
                self.enforce_budget(registry, name, run_id, metering_source);
                Ok(Some(update))
            }
            // A recognized replay — no double-count, no event emitted (nothing new
            // was committed). This is the AC-A guarantee in action; the evaluator is
            // NOT run (AC5 — no new total, no new breach).
            Ok(RecordOutcome::DuplicateReplay) => Ok(None),
            // AI-41: a store error must never SILENTLY drop the event. Report it
            // (diagnostic + Err) so the drains can park and retry the exact same
            // event on the next pass — the self-reported drain (its byte cursor)
            // since AI-41, and the OBSERVED drain (its pending buffer) since story
            // 12-4. Never a supervisor crash.
            Err(err) => {
                // Caller-factual (AI-41 loop 1; MODE-aware per the 12-4
                // AMENDMENT): a MIDRUN drain will really retry, so the text
                // says the event is parked for the next pass and not counted
                // until that commit succeeds (it does not claim the retry can
                // never fail — the bound skips loudly). A TERMINAL drain has
                // NO next pass, so its text must never claim "parked for
                // retry" (that would contradict the adjacent LOST notice) —
                // it names the loss and the why instead.
                let failure = if mode == DrainMode::Terminal {
                    format!(
                        "{}: a usage event could not be committed to the Usage Ledger: \
                         {err} — this is the terminal drain (the process is dead or the \
                         handle is being removed), so the event is LOST and will NOT be \
                         retried",
                        name.as_str(),
                    )
                } else {
                    format!(
                        "{}: a usage event could not be committed to the Usage Ledger: {err} — \
                         it is parked for retry on the next drain and is NOT counted until \
                         that commit succeeds",
                        name.as_str(),
                    )
                };
                self.emit_diagnostic(&failure);
                Err(err)
            }
        }
    }

    /// The AD-7 ENFORCEMENT stage (story 3-2 tokens + story 3-3 dollars), run INSIDE
    /// [`Self::ingest_usage`] right after a fresh commit — the SOLE place a budget or
    /// Cost Cap is evaluated + a Breach Action fired.
    ///
    /// Reads the CURRENT resolved budget/Rate/cap + action (live, AC-B), reads the
    /// committed per-run + cumulative totals, evaluates purely (TOKENS then DOLLARS,
    /// in the SAME choke point), and on a breach records the event FIRST then
    /// executes the action. Every step is best-effort to the RUN: a failed config
    /// read / totals read / lifecycle op is a diagnostic, never a crash (AD-12). A
    /// no-budget + no-cap instance evaluates to `WithinBudget` for both — so the
    /// common path is a cheap config read + two pure comparisons and nothing else.
    ///
    /// **STORY 3-3 — the DOLLAR evaluation folds in HERE (AD-7, no new path):** after
    /// the token evaluation, IF a [`Rate`](super::cost::Rate) is present AND the
    /// [`CostCap`](super::cost::CostCap) `is_set()`, derive the per-run + cumulative
    /// COST (each row priced at its own persisted Rate — no retro-repricing) and run
    /// the pure [`CostEvaluator`]. NO Rate ⇒ dollar enforcement is SKIPPED entirely
    /// (AC-B inert — a `CostCap` with no Rate cannot be enforced). Both dimensions
    /// reuse the SAME record-first-then-act path + the SAME per-Run latch, keyed by
    /// `(dimension, scope)` so a token breach and a dollar breach of the same scope
    /// each fire ONCE per Run (both can fire on the same event; the action is
    /// identical).
    ///
    /// **Idempotence — at most one breach per (dimension, scope) per Run:** this runs
    /// on EVERY committed usage event, so once a total crosses a ceiling every
    /// subsequent event would re-evaluate to the SAME breach. The per-Run breach
    /// LATCH ([`Supervised::breached_scopes`], keyed by `(dimension, scope)`)
    /// short-circuits BOTH the [`Self::record_breach`] and the action for an
    /// already-fired pair; the latch resets when a new Run starts.
    pub(super) fn enforce_budget(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        metering_source: &str,
    ) {
        // (1) LIVE config read (AC-B "changes apply immediately"): resolve the
        // CURRENT effective config ONCE, for BOTH the token budget and the dollar
        // Rate/cap. A malformed on-disk layer degrades to "no budget / no Rate"
        // (best-effort — never a crash mid-ingestion).
        let Ok(effective) = registry.effective_config(name, ConfigLayer::empty()) else {
            return;
        };
        let (budget, token_action) = config::resolve_token_budget(&effective);
        let (rate, cost_cap, cost_action) = config::resolve_cost(&effective);

        // Whether each dimension is ARMED: a token budget is armed when a ceiling is
        // set; the dollar cap is armed ONLY when BOTH a Rate is present AND a cap
        // scope is set (AC-B: a cap with no Rate is inert). If NEITHER is armed, skip
        // the totals reads entirely (the common un-governed path).
        let token_armed = budget.is_set();
        let dollar_armed = rate.is_some() && cost_cap.is_set();
        if !token_armed && !dollar_armed {
            return;
        }

        // (2) TOKEN dimension (story 3-2): the just-committed token totals + the pure
        // evaluator. Reuses the record-first-then-act helper with the tokens dimension.
        if token_armed {
            let run_total = registry
                .run_usage_totals(name, run_id)
                .map(|t| t.total_tokens())
                .unwrap_or(0);
            let cumulative_total = registry
                .usage_totals(name)
                .map(|t| t.total_tokens())
                .unwrap_or(0);
            let decision =
                BudgetEvaluator::evaluate(run_total, cumulative_total, &budget, token_action);
            if let BreachDecision::Breached {
                scope,
                action,
                limit,
                observed,
            } = decision
            {
                let cause = TransitionCause::budget_exceeded(scope, limit, observed);
                self.apply_breach(
                    registry,
                    name,
                    run_id,
                    BreachDimension::Tokens,
                    scope,
                    action,
                    cause,
                    metering_source,
                    // The token breach event carries token counts (no dollar fields).
                    |registry, sup, run_id, scope, action, src| {
                        sup.record_token_breach(
                            registry, name, run_id, scope, limit, observed, action, src,
                        );
                    },
                );
            }
        }

        // (3) DOLLAR dimension (story 3-3): derive the per-run + cumulative COST from
        // the ledger (each row priced at its own persisted Rate — no retro-repricing),
        // then the pure CostEvaluator. Only when a Rate is present AND the cap is set
        // (AC-B inert otherwise). v1 the estimate label is always `estimated`.
        if dollar_armed {
            let run_cost = registry
                .run_cost_totals(name, run_id)
                .unwrap_or(Micros::ZERO);
            let cumulative_cost = registry.cost_totals(name).unwrap_or(Micros::ZERO);
            let decision =
                CostEvaluator::evaluate(run_cost, cumulative_cost, &cost_cap, cost_action);
            if let BreachDecision::Breached {
                scope,
                action,
                limit,
                observed,
            } = decision
            {
                let label = EstimateLabel::Estimated;
                let cause = TransitionCause::cost_cap_exceeded(
                    scope,
                    Micros(limit as i64),
                    Micros(observed as i64),
                    label,
                );
                self.apply_breach(
                    registry,
                    name,
                    run_id,
                    BreachDimension::Dollars,
                    scope,
                    action,
                    cause,
                    metering_source,
                    // The dollar breach event carries integer micros + the label.
                    |registry, sup, run_id, scope, action, src| {
                        sup.record_cost_breach(
                            registry,
                            name,
                            run_id,
                            scope,
                            Micros(limit as i64),
                            Micros(observed as i64),
                            label,
                            action,
                            src,
                        );
                    },
                );
            }
        }
    }

    /// Apply ONE breach decision for a given `dimension` (story 3-3 shared path):
    /// consult the per-Run `(dimension, scope)` latch, and if this pair has NOT yet
    /// fired this Run, RECORD the breach (via `record`, the dimension-specific event
    /// writer) FIRST/INDEPENDENTLY and THEN execute the action via Epic-1's lifecycle
    /// (AD-15 — a REASON, not a new edge). A pair already latched short-circuits both.
    /// A missing `Supervised` (not currently supervised — a race with stop) declines
    /// to enforce. All best-effort: a lifecycle error is a diagnostic, never a crash.
    #[allow(clippy::too_many_arguments)]
    fn apply_breach(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        dimension: BreachDimension,
        scope: BreachScope,
        action: BreachAction,
        cause: TransitionCause,
        metering_source: &str,
        record: impl FnOnce(&Registry, &Self, &RunId, BreachScope, BreachAction, &str),
    ) {
        // IDEMPOTENCE LATCH (story 3-2/3-3): fire at most once per (dimension, scope)
        // per Run. Insert the pair; if it was already present, short-circuit.
        match self.running.get_mut(name) {
            Some(supervised) => {
                if !supervised.breached_scopes.insert((dimension, scope)) {
                    return;
                }
            }
            None => return,
        }
        // RECORD THE BREACH FIRST (AC7/AC10 / FR-21 "always recorded regardless of
        // action"), BEFORE the lifecycle side-effect, so a best-effort/unsupported/
        // failing pause never loses the record.
        record(registry, self, run_id, scope, action, metering_source);
        // EXECUTE THE ACTION via Epic-1's EXISTING lifecycle. The breach is already
        // recorded; a lifecycle error here is a best-effort diagnostic, never a crash.
        match action {
            BreachAction::Warn => {
                // No lifecycle transition — the breach event is the whole guardrail.
            }
            BreachAction::Pause => {
                self.enforce_pause(registry, name, cause);
            }
            BreachAction::Stop => {
                self.enforce_stop(registry, name, cause);
            }
        }
    }

    /// Execute a `pause` Breach Action honestly (story 3-2 AC6, honoring story
    /// 1-5's Capability Declaration). Drives `running → paused` and STAMPS the
    /// resulting transition with the [`TransitionCause::BudgetExceeded`] cause (so
    /// the lifecycle log explains WHY), via [`Self::pause`]. A best-effort pause
    /// still transitions (1-5) and the breach is already recorded; an UNSUPPORTED
    /// pause fails fast in [`Self::pause`] — we do NOT fake a pause and do NOT
    /// silently escalate to stop (AC6), we surface the honest diagnostic on the
    /// engine log (the breach event already captured the fact). All best-effort:
    /// never a supervisor crash.
    fn enforce_pause(&mut self, registry: &Registry, name: &InstanceName, cause: TransitionCause) {
        match self.pause_with_cause(registry, name, cause) {
            Ok(_) => {}
            Err(e) => {
                // Honest surface (AD-12): pause could not be honored (unsupported /
                // not running / backend hiccup). The breach is ALREADY recorded; log
                // and move on — no fake pause, no escalation.
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!("budget breach pause could not be honored: {e}"),
                );
            }
        }
    }

    /// Execute a `stop` Breach Action (story 3-2). Drives `running → stopping →
    /// stopped` (story 1-4) and, before that, records the [`TransitionCause::BudgetExceeded`]
    /// as the WHY marker on the `running → stopping` edge (the stop path itself
    /// records the graceful/forced escalation on the terminal edge). Best-effort:
    /// a stop error is logged, never a crash (the breach is already recorded).
    fn enforce_stop(&mut self, registry: &Registry, name: &InstanceName, cause: TransitionCause) {
        match self.stop_with_cause(registry, name, cause) {
            Ok(_) => {}
            Err(e) => {
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!("budget breach stop could not be honored: {e}"),
                );
            }
        }
    }

    /// Record a TOKEN [`BudgetBreachEvent`] (story 3-2, AC7) — the token-dimension
    /// event writer passed to [`Self::apply_breach`]. Builds the token breach struct
    /// (token `limit`/`observed`, no dollar fields) and persists it via
    /// [`Self::persist_breach_event`].
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_token_breach(
        &self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        scope: BreachScope,
        limit: u64,
        observed: u64,
        action: BreachAction,
        metering_source: &str,
    ) {
        let event = BudgetBreachEvent::new(
            name.as_str(),
            run_id.as_str(),
            scope,
            limit,
            observed,
            action,
            metering_source,
            now_rfc3339(),
        );
        self.persist_breach_event(registry, name, &event);
    }

    /// Record a DOLLAR [`BudgetBreachEvent`] (story 3-3, AC10) — the dollar-dimension
    /// event writer passed to [`Self::apply_breach`]. Builds the dollar breach struct
    /// (integer-micro `dollar_limit`/`dollar_observed` + the [`EstimateLabel`]) and
    /// persists it via [`Self::persist_breach_event`]. NO `$` string, NO `f64` — the
    /// wire carries integer micros + the label (AD-14).
    #[allow(clippy::too_many_arguments)]
    fn record_cost_breach(
        &self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        scope: BreachScope,
        limit_micros: Micros,
        observed_micros: Micros,
        label: EstimateLabel,
        action: BreachAction,
        metering_source: &str,
    ) {
        let event = BudgetBreachEvent::new_cost(
            name.as_str(),
            run_id.as_str(),
            scope,
            limit_micros,
            observed_micros,
            label,
            action,
            metering_source,
            now_rfc3339(),
        );
        self.persist_breach_event(registry, name, &event);
    }

    /// Persist a built [`BudgetBreachEvent`] to the durable per-instance breach log
    /// (story 3-2 shared path, AC7 / FR-21 "always recorded regardless of action").
    /// Recorded for EVERY action (including `warn`) and BEFORE the lifecycle
    /// side-effect, so the breach is never lost.
    ///
    /// Non-fatal but NOT swallowed: this is the PRIMARY durable record of the breach
    /// (FR-21), so a write failure (disk full / IO / perms) must not vanish silently
    /// while the action still fires — that would lose the mandated record with no
    /// diagnostic. We keep enforcement acting (the write failure is NOT made fatal),
    /// but SURFACE the error on the engine-log stderr breadcrumb (mirroring how
    /// `enforce_pause`/`enforce_stop` log their best-effort diagnostics), so a lost
    /// breach record is visible to an operator. Both the dir-create and the append
    /// failure are surfaced.
    fn persist_breach_event(
        &self,
        registry: &Registry,
        name: &InstanceName,
        event: &BudgetBreachEvent,
    ) {
        let path = registry.instance_breach_log_path(name);
        // Ensure the log dir exists (a never-transitioned instance may lack it) —
        // non-fatal, mirroring `ensure_log_dir`, but a failure is surfaced (below) if
        // it then makes the append fail.
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!(
                        "could not create the breach-log directory {}: {e} — the breach \
                         record may be lost",
                        parent.display()
                    ),
                );
            }
        }
        // Surface (do NOT swallow) an append failure: the breach record is the FR-21
        // mandated durable artifact; a lost record with no diagnostic is the bug. Log
        // and move on — enforcement still acts.
        match append_breach_event(&path, event) {
            Ok(()) => {
                // Story 7-2: the append COMMITTED — publish onto the event bus
                // (after the durable record exists, never before; a failed
                // append publishes nothing). Ordering obligation: this runs
                // under the supervisor lock (see the `domain::bus`
                // caller-enforced invariant).
                self.publish(EngineEvent::BudgetBreach(event.clone()));
            }
            Err(e) => {
                self.log_enforcement_diagnostic(
                    registry,
                    name,
                    &format!(
                        "could not record the budget breach event to {}: {e} — the mandated \
                         breach record was NOT written",
                        path.display()
                    ),
                );
            }
        }
    }

    /// Surface one enforcement diagnostic through the engine's diagnostic
    /// emission (AD-12: enforcement diagnostics ride the engine log / stderr —
    /// or the host's story-10-2 sink when one is installed — NEVER `kt`
    /// stdout, NEVER a crash). Used when a breach action (pause/stop) could
    /// not be honored — the breach itself is already durably recorded in the
    /// breach log, so this is only an operator breadcrumb, not the record of
    /// the breach. `registry` is unused (the diagnostic is not persisted to a
    /// strict-parse log to avoid corrupting the transition-event reader) but
    /// kept for signature symmetry with the other enforcement helpers.
    fn log_enforcement_diagnostic(&self, _registry: &Registry, name: &InstanceName, detail: &str) {
        self.emit_diagnostic(&format!("{}: {detail}", name.as_str()));
    }

    /// Read back the recorded [`BudgetBreachEvent`]s for an instance from its
    /// breach log (observation helper for tests / embedders — the AD-14 seed, NOT
    /// the 7-2 bus). Empty vec if none recorded yet.
    pub fn read_breach_events(
        registry: &Registry,
        name: &str,
    ) -> Result<Vec<BudgetBreachEvent>, EngineError> {
        let name = InstanceName::new(name).map_err(|reason| EngineError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let path = registry.instance_breach_log_path(&name);
        read_breach_events_from(&path).map_err(|detail| EngineError::Log {
            name: name.as_str().to_string(),
            path: path.to_string_lossy().into_owned(),
            detail,
        })
    }
}
