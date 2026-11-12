//! The Supervisor's Run-registry calls (story 21-2, AD-25).
//!
//! Run writes are store-writer jobs ([`crate::domain::runs::ops`]); the
//! Supervisor only drives them where they need its view (the hosting
//! process, its metering source) and publishes the committed Run events on
//! the bus — every caller holds the supervisor lock, so bus order stays
//! commit order (the `domain::bus` invariant).

use super::Supervisor;
use crate::domain::config::ConfigLayer;
use crate::domain::name::InstanceName;
use crate::domain::registry::Registry;
use crate::domain::runs::{
    self, ops, ChildRunRequest, Label, LabelChange, LabelOutcome, RunError, RunView,
};
use crate::domain::usage::RunId;

impl Supervisor {
    /// The bound labels an instance's configuration gives its supervised
    /// Runs (story 21-2, D3): the `labels.*` keys, resolved with an empty
    /// invocation layer (an override binds nothing). A malformed leaf is
    /// named on the diagnostic surface, never silently dropped (AI-18); an
    /// unreadable config binds nothing and says so.
    pub(super) fn bound_labels(&self, registry: &Registry, name: &InstanceName) -> Vec<Label> {
        match registry.effective_config(name, ConfigLayer::empty()) {
            Ok(effective) => {
                let (labels, notes) = runs::labels::bound_labels_from(&effective);
                for note in notes {
                    self.emit_diagnostic(&format!("{}: {note}", name.as_str()));
                }
                labels
            }
            Err(err) => {
                self.emit_diagnostic(&format!(
                    "{}: the instance configuration could not be read ({err}), so its Run \
                     carries no bound labels until the next start or adoption",
                    name.as_str(),
                ));
                Vec::new()
            }
        }
    }

    /// Keep an adopted instance's Run in step with this engine's view (story
    /// 21-2): the Run is kept (created if the record predates Runs), its wire
    /// path re-derived from the active metering source (a change is a
    /// `depth-changed` event) and its bound labels re-synced. A failed write
    /// is a diagnostic — adoption never fails on Run bookkeeping.
    pub(super) fn sync_adopted_run(
        &self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        metering_source: &str,
    ) {
        let run = ops::SupervisedRun {
            instance: name.clone(),
            run_id: run_id.clone(),
            wire_path: metering_source == "engine-observed",
            labels: self.bound_labels(registry, name),
            fresh: false,
            // Story 21-3: the adoption resolves with fresh caller-side
            // inputs; an unreadable configuration fail-closes the action to
            // `pause` in the resolution (never resolved as if unset).
            inputs: Some(registry.resolve_inputs_fail_closed(name)),
        };
        match registry.ensure_supervised_run(run) {
            Ok(events) => self.publish_run_events(events),
            Err(err) => self.emit_diagnostic(&format!(
                "{}: the adopted Run `{run_id}` could not be brought up to date ({err}); its \
                 depth and labels refresh at the next start or adoption",
                name.as_str(),
            )),
        }
    }

    /// End the open process-owning Runs whose instance holds no process
    /// (story 21-2): a planned relaunch that never ran — its engine exited
    /// during the backoff — leaves the Run `active` with the `failed`
    /// Anomaly. Run at engine open, after adoption. A relaunch that does run
    /// later reopens the Run. A failed write is a diagnostic.
    pub(crate) fn end_runs_without_a_process(&self, registry: &Registry) {
        let result = registry.run_job(|store| {
            let changes = ops::end_runs_without_a_process(store, &crate::time::now_rfc3339())?;
            Ok(((), changes))
        });
        match result {
            Ok(((), events)) => self.publish_run_events(events),
            Err(err) => self.emit_diagnostic(&format!(
                "the Runs of agents with no process could not be ended ({err}); they stay \
                 listed as active until their next start"
            )),
        }
    }

    /// Re-sync the open Run's bound labels after a `labels.*` config write
    /// (story 21-2): one `labels-changed` event when the set changed. An
    /// instance with no open Run has nothing to sync (the next start binds
    /// the new labels).
    pub(crate) fn sync_run_labels(&self, registry: &Registry, name: &InstanceName) {
        let labels = self.bound_labels(registry, name);
        let instance = name.clone();
        let result = registry.run_job(move |store| {
            use crate::ports::RunRecords;
            let Some(open) = store.open_process_run(&instance)? else {
                return Ok(((), ops::RunChanges::default()));
            };
            let changes = ops::sync_bound_labels(
                store,
                &open.run_id,
                &labels,
                None,
                &crate::time::now_rfc3339(),
            )?;
            Ok(((), changes))
        });
        match result {
            Ok(((), events)) => self.publish_run_events(events),
            Err(err) => self.emit_diagnostic(&format!(
                "{}: the configuration was written, but the open Run's bound labels could not \
                 be updated ({err}); they refresh at the next start or adoption",
                name.as_str(),
            )),
        }
    }

    /// Re-resolve the open process-owning Run's Policies after a
    /// `budget.*` or `cost.rate.*` config write (story 21-3), then run the
    /// SAME re-arm pass a Policy change and `hekma budget` run (story 21-6,
    /// the live un-fence's config-write trigger): one writer job with FRESH
    /// caller-side inputs (the write just changed them), so the stored
    /// resolution, its Anomalies and the published snapshot match the
    /// config the operator sees; a ceiling raised above the totals re-arms
    /// its latch and lifts the matching `block` fence reasons per reason,
    /// and one lowered to at or below the usage breaches at once (its
    /// process action applied here). An instance with no open Run resolves
    /// nothing (the next start resolves at creation).
    pub(crate) fn sync_run_policy(&mut self, registry: &Registry, name: &InstanceName) {
        // The fail-closed read (round 3, L6): an unreadable configuration
        // is carried as the malformed note (action pause), never resolved
        // as if the instance had set nothing.
        let inputs = registry.resolve_inputs_fail_closed(name);
        let context = self.instance_context(registry, name);
        match registry.rearm_after_config_write(name, inputs, context) {
            Ok(applied) => {
                self.publish_and_act(
                    registry,
                    applied.breaches,
                    Vec::new(),
                    applied.run_events,
                    &applied.notes,
                    "a configuration write",
                );
            }
            Err(err) => self.emit_diagnostic(&format!(
                "{}: the configuration was written, but the open Run's Policies could not be \
                 re-resolved ({err}); they re-resolve at the next change or start",
                name.as_str(),
            )),
        }
    }

    /// One instance's live context for the re-arm pass (story 21-6): its
    /// wire fence and metering source when this engine supervises it, and
    /// its fresh explicit inputs.
    pub(crate) fn instance_context(
        &self,
        registry: &Registry,
        name: &InstanceName,
    ) -> crate::domain::registry_policy::InstanceContext {
        match self.running.get(name) {
            Some(supervised) => crate::domain::registry_policy::InstanceContext {
                fence: supervised
                    .observed_listener
                    .as_ref()
                    .map(crate::metering::ObservedListener::fence),
                metering_source: supervised.metering_source.clone(),
                supervised: true,
                inputs: Some(registry.try_resolve_inputs_for(name.as_str())),
            },
            None => crate::domain::registry_policy::InstanceContext {
                fence: None,
                metering_source: String::new(),
                supervised: false,
                inputs: Some(registry.try_resolve_inputs_for(name.as_str())),
            },
        }
    }

    /// Set Run-level limits as `actor` (story 21-6, the budget job behind
    /// `hekma budget set` and `PATCH /v0/runs/{id}/limits`): one writer job
    /// per Run, each answered on its own — a refusal on one Run never stops
    /// the others. A committed change re-arms (raise) or breaches at once
    /// (lower) through the shared re-arm pass; the breaches' process
    /// actions are executed here (D2). `dry_run` plans each Run and writes
    /// nothing. A malformed change is refused before any Run is read.
    pub(crate) fn set_run_limits(
        &mut self,
        registry: &Registry,
        run_ids: &[RunId],
        change: &crate::domain::RunLimitChange,
        actor: crate::domain::LimitActor,
        dry_run: bool,
    ) -> Result<crate::domain::RunLimitsResults, RunError> {
        change.validate()?;
        let mut results = Vec::with_capacity(run_ids.len());
        for run_id in run_ids {
            let context = match registry.run_record(run_id) {
                Ok(Some(record)) => record
                    .instance
                    .as_deref()
                    .and_then(|instance| InstanceName::new(instance).ok())
                    .map(|name| self.instance_context(registry, &name)),
                Ok(None) => None,
                Err(err) => {
                    results.push((run_id.clone(), Err(err)));
                    continue;
                }
            };
            if dry_run {
                let planned = registry.plan_run_limits(run_id, change, actor, context.as_ref());
                results.push((run_id.clone(), planned));
                continue;
            }
            match registry.apply_run_limits(run_id, change, actor, context) {
                Ok(applied) => {
                    self.publish_and_act(
                        registry,
                        applied.breaches,
                        Vec::new(),
                        applied.run_events,
                        &applied.notes,
                        "a Run limit change",
                    );
                    let outcome = applied.outcome.expect("the budget job reports its Run");
                    results.push((run_id.clone(), Ok(outcome)));
                }
                Err(err) => results.push((run_id.clone(), Err(err))),
            }
        }
        Ok(results)
    }

    /// The live per-instance context a Policy change's money half needs
    /// (story 21-3): each supervised instance's wire fence (when it has a
    /// metering listener), its metering source and its fresh explicit
    /// inputs.
    fn policy_contexts(
        &self,
        registry: &Registry,
    ) -> std::collections::HashMap<String, crate::domain::registry_policy::InstanceContext> {
        let mut contexts: std::collections::HashMap<
            String,
            crate::domain::registry_policy::InstanceContext,
        > = self
            .running
            .iter()
            .map(|(name, supervised)| {
                (
                    name.as_str().to_string(),
                    crate::domain::registry_policy::InstanceContext {
                        fence: supervised
                            .observed_listener
                            .as_ref()
                            .map(crate::metering::ObservedListener::fence),
                        metering_source: supervised.metering_source.clone(),
                        supervised: true,
                        inputs: Some(registry.try_resolve_inputs_for(name.as_str())),
                    },
                )
            })
            .collect();
        // Every OTHER instance with an open Run gets fresh inputs too (its
        // stored resolution must not run one config read behind), with no
        // fence and `supervised: false` — its money half never runs here
        // (the supervising engine owns the latch), but its re-resolve is
        // as true as any other's.
        if let Ok(open_runs) = registry.open_runs() {
            for record in open_runs {
                if let Some(instance) = record.instance.as_deref() {
                    if contexts.contains_key(instance) {
                        continue;
                    }
                    contexts.insert(
                        instance.to_string(),
                        crate::domain::registry_policy::InstanceContext {
                            fence: None,
                            metering_source: String::new(),
                            supervised: false,
                            inputs: Some(registry.try_resolve_inputs_for(instance)),
                        },
                    );
                }
            }
        }
        contexts
    }

    /// Create or change a Policy (story 21-3, PRD FR-60): one writer job
    /// re-resolves every matched open Run, fires a breach at once where a
    /// lowered ceiling is at or below usage and re-arms where a raised one
    /// is above it. The committed events are published here and the
    /// breaches' process actions executed (D2: the caller applies them).
    /// `dry_run` plans the change, reporting the matched and fallback
    /// counts, and writes nothing.
    pub(crate) fn change_policy(
        &mut self,
        registry: &Registry,
        change: crate::domain::PolicyChange,
        dry_run: bool,
    ) -> Result<crate::domain::PolicyOutcome, RunError> {
        let existing = match registry.policy(&change.name) {
            Ok(existing) => existing,
            // A CORRUPT `default` row cannot be deleted (the name is
            // reserved) and cannot be read: the change falls back to the
            // KNOWN SEED as the prior — `change default` is the one write
            // that can restore the row and heal the cache (any other
            // corrupt row is deleted by name and recreated).
            Err(crate::ports::StoreError::CorruptRow { .. })
                if change.name == crate::domain::DEFAULT_POLICY_NAME =>
            {
                Some(crate::domain::PolicyRecord::default_policy())
            }
            Err(other) => return Err(RunError::Store(other)),
        };
        if dry_run {
            return registry.plan_policy_change(existing.as_ref(), &change);
        }
        let applied =
            registry.apply_policy_change(existing, change, self.policy_contexts(registry))?;
        Ok(self.finish_policy_applied(registry, applied))
    }

    /// Delete a Policy (story 21-3): its matched Runs re-resolve under the
    /// remaining set. The `default` Policy is refused. `dry_run` reports the
    /// Runs the delete would re-resolve and writes nothing. A CORRUPT row —
    /// one whose stored Selector no longer parses, which fails every
    /// decoded read — is still deletable by name (the remediation; the job
    /// re-resolves every non-ended Run because the corrupt scope is
    /// unknowable) with a note saying so.
    pub(crate) fn delete_policy(
        &mut self,
        registry: &Registry,
        name: &str,
        dry_run: bool,
    ) -> Result<crate::domain::PolicyOutcome, RunError> {
        if name == crate::domain::DEFAULT_POLICY_NAME {
            return Err(RunError::PolicyDefault {
                what: "cannot be deleted — change it instead",
            });
        }
        let read = match registry.policy(name) {
            Ok(Some(existing)) => Some(existing),
            Ok(None) => {
                return Err(RunError::PolicyNotFound {
                    name: name.to_string(),
                })
            }
            // The row exists but cannot be DECODED: fall through to the
            // by-name delete — the row must stay removable (a corrupt
            // Selector once wedged every surface out of the store).
            Err(crate::ports::StoreError::CorruptRow { .. }) => None,
            Err(other) => return Err(RunError::Store(other)),
        };
        if read.is_none() {
            // The corrupt path: confirm the row is really there.
            match registry.policy_selector_raw(name) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(RunError::PolicyNotFound {
                        name: name.to_string(),
                    })
                }
                Err(other) => return Err(RunError::Store(other)),
            }
        }
        if dry_run {
            if let Some(existing) = &read {
                let plan = registry.plan_policy_delete(existing)?;
                return Ok(crate::domain::PolicyOutcome {
                    record: None,
                    ..plan
                });
            }
            return Ok(crate::domain::PolicyOutcome {
                record: None,
                notes: vec![format!(
                    "the Policy `{name}` is stored in an unreadable shape; deleting it \
                     re-resolves every non-ended Run (its scope cannot be read)"
                )],
                ..Default::default()
            });
        }
        let raw_selector = match registry.policy_selector_raw(name) {
            Ok(raw) => raw.flatten(),
            Err(other) => return Err(RunError::Store(other)),
        };
        let applied = registry.apply_policy_delete(
            name,
            read,
            raw_selector,
            self.policy_contexts(registry),
        )?;
        Ok(self.finish_policy_applied(registry, applied))
    }

    /// Publish what a committed Policy write produced, in COMMIT order,
    /// and execute its breaches' process actions ([`Self::publish_and_act`]).
    fn finish_policy_applied(
        &mut self,
        registry: &Registry,
        applied: crate::domain::registry_policy::PolicyApplied,
    ) -> crate::domain::PolicyOutcome {
        self.publish_and_act(
            registry,
            applied.breaches,
            applied.events,
            applied.run_events,
            &applied.outcome.notes,
            "a Policy change",
        );
        applied.outcome
    }

    /// Publish what a committed money job produced (a Policy write, a
    /// Run-level limit change, a config-write re-arm) in COMMIT order —
    /// the breaches the job fired (their outbox rows were appended first,
    /// inside the re-arm pass), then its Policy events, then the Run events
    /// (the bus invariant is bus order == outbox order) — and execute each
    /// breach's process action through the lifecycle AFTER the publishes
    /// (the breaches are durable; a failing pause never loses the record,
    /// and the lifecycle's own transition events then land last, as they
    /// do on the outbox). `what` names the cause in the diagnostics.
    pub(crate) fn publish_and_act(
        &mut self,
        registry: &Registry,
        breaches: Vec<crate::domain::pipeline::CommittedBreach>,
        policy_events: Vec<(crate::domain::PolicyEvent, crate::ports::OutboxAppend)>,
        run_events: Vec<ops::CommittedRunEvent>,
        notes: &[String],
        what: &str,
    ) {
        let mut actions: Vec<(InstanceName, Vec<crate::domain::pipeline::CommittedBreach>)> =
            Vec::new();
        for breach in breaches {
            let instance = breach.event.instance.clone();
            let name = match InstanceName::new(&instance) {
                Ok(name) => name,
                Err(err) => {
                    // The breach is durable and can be published, but its
                    // instance name is not even parseable — say so, never
                    // a silent drop.
                    self.note_outbox_append(&breach.outbox);
                    self.publish(crate::domain::EngineEvent::BudgetBreach(
                        breach.event.clone(),
                    ));
                    self.emit_diagnostic(&format!(
                        "{what} fired a breach on `{instance}`, which is not a valid agent name \
                         ({err}); the breach is recorded but its action was not executed"
                    ));
                    continue;
                }
            };
            self.mirror_breach(registry, &name, &breach.event);
            self.note_outbox_append(&breach.outbox);
            self.publish(crate::domain::EngineEvent::BudgetBreach(
                breach.event.clone(),
            ));
            match actions.iter_mut().find(|(at, _)| *at == name) {
                Some((_, list)) => list.push(breach),
                None => actions.push((name, vec![breach])),
            }
        }
        for (event, outbox) in policy_events {
            self.note_outbox_append(&outbox);
            self.publish(crate::domain::EngineEvent::Policy(Box::new(event)));
        }
        self.publish_run_events(run_events);
        // Then the process actions (only for instances this engine
        // supervises; D2: the caller applies them).
        for (name, breaches) in actions {
            if !self.running.contains_key(&name) {
                // Not ours to act on (a foreign engine supervises the
                // agent, or the process is gone): the breach stays
                // recorded and published, and ONE diagnostic says so —
                // never a failed pause per breach.
                self.emit_diagnostic(&format!(
                    "{}: {what} fired {} breach(es) on a Run of an agent this engine does not \
                     supervise; the breaches are recorded, and the supervising engine applies \
                     the action",
                    name.as_str(),
                    breaches.len(),
                ));
                continue;
            }
            self.execute_breach_actions(registry, &name, breaches);
        }
        for note in notes {
            self.emit_diagnostic(note);
        }
    }

    /// Execute already-published breaches' process actions (the action
    /// half of [`Self::apply_evaluation`] without its publishing — the
    /// publish half ran in commit order above). Only a process-level
    /// breach acts; each executes its action in commit order.
    fn execute_breach_actions(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        breaches: Vec<crate::domain::pipeline::CommittedBreach>,
    ) {
        for committed in breaches
            .into_iter()
            .filter(|committed| committed.process_level)
        {
            let decision = committed.decision;
            if decision.action == crate::domain::BreachAction::Block {
                self.note_block(registry, name, decision.dimension, decision.scope);
                continue;
            }
            let cause = match decision.dimension {
                crate::domain::BreachDimension::Tokens => {
                    crate::domain::TransitionCause::budget_exceeded(
                        decision.scope,
                        decision.limit,
                        decision.observed,
                    )
                }
                // v1 the estimate label is always `estimated`.
                crate::domain::BreachDimension::Dollars => {
                    crate::domain::TransitionCause::cost_cap_exceeded(
                        decision.scope,
                        crate::domain::Micros(decision.limit as i64),
                        crate::domain::Micros(decision.observed as i64),
                        crate::domain::EstimateLabel::Estimated,
                    )
                }
            };
            match decision.action {
                // No lifecycle transition — the breach event is the whole guardrail.
                crate::domain::BreachAction::Warn | crate::domain::BreachAction::Block => {}
                crate::domain::BreachAction::Pause => self.enforce_pause(registry, name, cause),
                crate::domain::BreachAction::Stop => self.enforce_stop(registry, name, cause),
            }
        }
    }

    /// Open a child Run (FR-43, FR-49) and publish its `created` event.
    /// The child resolves against the Policies in the same job (story
    /// 21-3): its parent instance's facts are read fresh here, outside the
    /// job (D2), failing closed to `pause` when the configuration cannot
    /// be read.
    pub(crate) fn open_child_run(
        &self,
        registry: &Registry,
        request: ChildRunRequest,
    ) -> Result<RunView, RunError> {
        let run_id = RunId::mint();
        // The parent instance's facts, read outside the job (D2): the
        // child resolves at creation (story 21-3).
        let parent_instance = match &request.parent {
            crate::domain::RunParent::Instance(instance) => Some(instance.clone()),
            crate::domain::RunParent::Run(id) => {
                registry.run_record(id)?.and_then(|record| record.instance)
            }
        };
        let inputs = parent_instance
            .as_deref()
            .and_then(|instance| InstanceName::new(instance).ok())
            .map(|name| registry.resolve_inputs_fail_closed(&name));
        let ((opened, _), events) = registry.run_job(move |store| {
            let (id, changes) =
                ops::open_child(store, &request, run_id, inputs, &crate::time::now_rfc3339())?;
            Ok(((id, ()), changes))
        })?;
        self.publish_run_events(events);
        registry.run_view(&opened)
    }

    /// Close a child Run and publish its `ended` event.
    pub(crate) fn close_run(
        &self,
        registry: &Registry,
        run_id: &RunId,
    ) -> Result<RunView, RunError> {
        let id = run_id.clone();
        let ((), events) = registry.run_job(move |store| {
            let changes = ops::close(store, &id, &crate::time::now_rfc3339())?;
            Ok(((), changes))
        })?;
        self.publish_run_events(events);
        registry.run_view(run_id)
    }

    /// The label verb (story 21-9): bind and unbind labels on one Run as the
    /// Operator, as one writer job, and publish its `labels-changed` event.
    /// A process-owning Run's configuration labels are resolved first (here,
    /// outside the job), so removing a verb label restores a configured value
    /// it shadowed in the same change.
    pub(crate) fn label_run(
        &self,
        registry: &Registry,
        run_id: &RunId,
        change: LabelChange,
        dry_run: bool,
    ) -> Result<LabelOutcome, RunError> {
        change.validate()?;
        let record = registry
            .run_record(run_id)?
            .ok_or_else(|| RunError::NotFound {
                run_id: run_id.as_str().to_string(),
            })?;
        // Only a removal can restore a configured value, so only a removal
        // needs the configuration — and must not plan without it: an
        // unreadable configuration refuses the removal instead of dropping
        // the key it would have restored.
        let config = match record.instance.as_deref().map(InstanceName::new) {
            Some(Ok(name)) if record.process_owner && !change.remove.is_empty() => {
                let effective = registry
                    .effective_config(&name, ConfigLayer::empty())
                    .map_err(|err| RunError::Invalid {
                        field: "config".to_string(),
                        reason: format!(
                            "the configuration of `{}` could not be read ({err}), so removing a \
                             label cannot restore the value it binds — fix the configuration \
                             and run the command again",
                            name.as_str()
                        ),
                    })?;
                runs::labels::bound_labels_from(&effective).0
            }
            _ => Vec::new(),
        };
        if dry_run {
            return registry.plan_run_labels(run_id, &change, &config);
        }
        let id = run_id.clone();
        let (outcome, events) = registry.run_job(move |store| {
            ops::label(
                store,
                &id,
                &change,
                &config,
                false,
                &crate::time::now_rfc3339(),
            )
        })?;
        self.publish_run_events(events);
        Ok(outcome)
    }

    /// Commit one usage event attributed to any Run of a supervised instance
    /// — a child Run included — through the full AD-7 pipeline (ancestor
    /// ceilings, process actions). The seam the shared proxy's attributed
    /// calls use once they exist (story 23.1); exercised by tests until then.
    #[cfg(test)]
    pub(crate) fn commit_run_usage(
        &mut self,
        registry: &Registry,
        name: &InstanceName,
        run_id: &RunId,
        parsed: &crate::ports::ParsedUsage,
    ) -> Result<Option<crate::domain::usage::UsageUpdateEvent>, crate::domain::error::RegistryError>
    {
        let metering_source = self
            .running
            .get(name)
            .map(|supervised| supervised.metering_source.clone())
            .unwrap_or_else(|| "self-reported".to_string());
        self.ingest_usage(
            registry,
            name,
            run_id,
            &metering_source,
            parsed,
            super::DrainMode::MidRun,
        )
    }
}
