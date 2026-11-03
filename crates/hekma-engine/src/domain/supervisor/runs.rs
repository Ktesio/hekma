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
        let config = match record.instance.as_deref().map(InstanceName::new) {
            Some(Ok(name)) if record.process_owner => self.bound_labels(registry, &name),
            _ => Vec::new(),
        };
        let id = run_id.clone();
        let (outcome, events) = registry.run_job(move |store| {
            ops::label(
                store,
                &id,
                &change,
                &config,
                dry_run,
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
