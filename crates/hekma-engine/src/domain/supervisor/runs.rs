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
            let changes =
                ops::sync_bound_labels(store, &open.run_id, &labels, &crate::time::now_rfc3339())?;
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
    /// `budget.*` or `cost.rate.*` config write (story 21-3): one writer
    /// job with FRESH caller-side inputs (the write just changed them), so
    /// the stored resolution, its Anomalies and the published snapshot
    /// match the config the operator sees. An instance with no open Run
    /// resolves nothing (the next start resolves at creation).
    pub(crate) fn sync_run_policy(&self, registry: &Registry, name: &InstanceName) {
        let inputs = registry.resolve_inputs_for(name.as_str());
        let instance = name.clone();
        let result = registry.run_job(move |store| {
            use crate::ports::RunRecords;
            let Some(open) = store.open_process_run(&instance)? else {
                return Ok(((), crate::domain::runs::ops::RunChanges::default()));
            };
            let changes = crate::domain::runs::policy::resolve_run(
                store,
                &open.run_id,
                Some(inputs),
                &crate::time::now_rfc3339(),
            )?;
            Ok(((), changes))
        });
        match result {
            Ok(((), events)) => {
                self.publish_run_events(events);
            }
            Err(err) => self.emit_diagnostic(&format!(
                "{}: the configuration was written, but the open Run's Policies could not be \
                 re-resolved ({err}); they re-resolve at the next change or start",
                name.as_str(),
            )),
        }
    }

    /// The live per-instance context a Policy change's money half needs
    /// (story 21-3): each supervised instance's wire fence (when it has a
    /// metering listener) and its metering source.
    fn policy_contexts(
        &self,
    ) -> std::collections::HashMap<String, crate::domain::registry_policy::InstanceContext> {
        self.running
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
                    },
                )
            })
            .collect()
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
        let existing = registry.policy(&change.name).map_err(RunError::Store)?;
        if dry_run {
            return registry.plan_policy_change(existing.as_ref(), &change);
        }
        let applied = registry.apply_policy_change(existing, change, self.policy_contexts())?;
        Ok(self.finish_policy_applied(registry, applied))
    }

    /// Delete a Policy (story 21-3): its matched Runs re-resolve under the
    /// remaining set. The `default` Policy is refused. `dry_run` reports the
    /// Runs the delete would re-resolve and writes nothing.
    pub(crate) fn delete_policy(
        &mut self,
        registry: &Registry,
        name: &str,
        dry_run: bool,
    ) -> Result<crate::domain::PolicyOutcome, RunError> {
        let existing = registry
            .policy(name)
            .map_err(RunError::Store)?
            .ok_or_else(|| RunError::PolicyNotFound {
                name: name.to_string(),
            })?;
        if dry_run {
            if existing.is_default() {
                return Err(RunError::Invalid {
                    field: "name".to_string(),
                    reason: "the default Policy cannot be deleted — change it instead (it is the \
                             fallback every Run resolves through)"
                        .to_string(),
                });
            }
            let plan = registry.plan_policy_change(
                Some(&existing),
                &crate::domain::PolicyChange {
                    name: existing.name.clone(),
                    ..Default::default()
                },
            )?;
            return Ok(crate::domain::PolicyOutcome {
                record: None,
                ..plan
            });
        }
        let applied = registry.apply_policy_delete(existing, self.policy_contexts())?;
        Ok(self.finish_policy_applied(registry, applied))
    }

    /// Publish what a committed Policy write produced, in commit order (the
    /// Policy event, the Runs it re-resolved, then the breaches it fired),
    /// and execute each breach's process action through the lifecycle.
    fn finish_policy_applied(
        &mut self,
        registry: &Registry,
        applied: crate::domain::registry_policy::PolicyApplied,
    ) -> crate::domain::PolicyOutcome {
        for (event, outbox) in applied.events {
            self.note_outbox_append(&outbox);
            self.publish(crate::domain::EngineEvent::Policy(event));
        }
        self.publish_run_events(applied.run_events);
        let mut by_instance: std::collections::BTreeMap<
            String,
            Vec<crate::domain::pipeline::CommittedBreach>,
        > = std::collections::BTreeMap::new();
        for breach in applied.breaches {
            by_instance
                .entry(breach.event.instance.clone())
                .or_default()
                .push(breach);
        }
        for (instance, breaches) in by_instance {
            match InstanceName::new(&instance) {
                Ok(name) => self.apply_evaluation(
                    registry,
                    &name,
                    crate::domain::pipeline::Evaluation {
                        breaches,
                        ..Default::default()
                    },
                ),
                Err(err) => self.emit_diagnostic(&format!(
                    "a Policy change fired a breach on `{instance}`, which is not a valid \
                     instance name ({err}); the breach is recorded but its action was not \
                     executed"
                )),
            }
        }
        applied.outcome
    }

    /// Open a child Run (FR-43, FR-49) and publish its `created` event.
    pub(crate) fn open_child_run(
        &self,
        registry: &Registry,
        request: ChildRunRequest,
    ) -> Result<RunView, RunError> {
        let run_id = RunId::mint();
        let ((opened, _), events) = registry.run_job(move |store| {
            let (id, changes) =
                ops::open_child(store, &request, run_id, &crate::time::now_rfc3339())?;
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
