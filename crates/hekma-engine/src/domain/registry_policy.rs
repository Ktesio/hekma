//! The Registry's Policy surface (story 21-3, spine AD-25 / AD-7).
//!
//! * the cached policy set (amendment 3) and the resolved-limit reads every
//!   enforcement and surface call goes through;
//! * Policy create/change/delete as writer jobs — every change one `Policy`
//!   event, the policy-cache swap in the same job's post-commit hook, and
//!   (FR-60 / PRD §13 Q11's interim) a re-resolution of every matched
//!   non-ended Run in the same batch: lowering a ceiling below a Run's
//!   usage breaches AT ONCE, raising one re-arms the latch and clears the
//!   matching `block` fence reasons (per reason, amendment 2);
//! * the FR-21 upgrade finish: the pending list the v15 migration
//!   committed, the idempotent `config.toml` writes under their locks, and
//!   the closing transaction (markers + exactly one
//!   `breach-default-upgraded` event + the list cleared);
//! * the initial policy sweep at open: every open Run resolves once with
//!   fresh caller-side inputs, so the stored resolutions and the published
//!   snapshot start true.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::budget::BreachAction;
use crate::domain::bus::EngineEvent;
use crate::domain::config::{self, ConfigLayer, SourceLayer};
use crate::domain::cost::Rate;
use crate::domain::name::InstanceName;
use crate::domain::outbox::OUTBOX_MAX_BYTES;
use crate::domain::pipeline::{self, CommittedBreach, EvalInput};
use crate::domain::runs::ops::{self, CommittedRunEvent, RunChanges};
use crate::domain::runs::policy::{
    self, PolicyCache, PolicyEvent, PolicyEventKind, PolicyLimits, PolicyRecord, ResolveInputs,
    BreachDefaultUpgradedEvent, UpgradedInstance, DEFAULT_POLICY_NAME, POLICY_EVENT_SCHEMA_VERSION,
};
use crate::domain::runs::{ControlDepth, PolicyResolution, RunError, RunRecord, Selector};
use crate::domain::usage::RunId;
use crate::domain::Registry;
use crate::metering::WireFence;
use crate::ports::{PolicyRecords, RunRecords, RunStore, StoreError};
use crate::store::BreachDefaultSource;
use crate::time::now_rfc3339;

/// A create/change request: limits to set, fields to clear, and Rate-table
/// edits. Merged over the existing row on a change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PolicyChange {
    /// The Policy's name.
    pub name: String,
    /// Its Selector (required and non-empty unless `default`).
    pub selector: Option<Selector>,
    /// Limits to set (a set field wins over the existing row).
    pub set: PolicyLimits,
    /// Limit fields to clear (`tokens-per-run`, `tokens-cumulative`,
    /// `dollars-per-run`, `dollars-cumulative`, `breach-action`,
    /// `scent-check`, `rate`).
    pub clear: Vec<String>,
    /// Rate-table edits (`default` only): a model to price, or `None` to
    /// remove the model's entry.
    pub rates: BTreeMap<String, Option<Rate>>,
}

/// What a committed Policy change reports (FR-60: the fallback count).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PolicyOutcome {
    /// The Policy after the change (`None` on a delete).
    pub record: Option<PolicyRecord>,
    /// How many non-ended Runs the (old, on a delete) Selector matches.
    pub matched: usize,
    /// How many matched Runs fall back to `warn` at their depth.
    pub fallbacks: usize,
}

/// The live per-instance context the policy-change job's money half needs
/// (built by the caller — engine or supervisor — which holds the locks):
/// the instance's metering wire fence, and its metering source for the
/// breach records.
#[derive(Clone, Default)]
pub(crate) struct InstanceContext {
    /// The instance's live wire fence (an engine-observed instance's
    /// listener fence; `None` when it has no listener).
    pub fence: Option<WireFence>,
    /// The instance's metering source (the breach records name it).
    pub metering_source: String,
}

impl Registry {
    /// The cached policy set (amendment 3): one `Arc` clone, no SQL.
    pub(crate) fn policy_cache(&self) -> &Arc<PolicyCache> {
        &self.policies
    }

    /// Drain the events the open produced (the initial sweep's Run events,
    /// the upgrade event), for the engine to publish on the bus once it
    /// exists. They are already durable in the outbox.
    pub(crate) fn take_open_events(&mut self) -> Vec<EngineEvent> {
        std::mem::take(&mut *self.open_events.lock().expect("open-events mutex poisoned"))
    }

    /// Note an event to publish at open (already durable through the
    /// outbox).
    pub(crate) fn note_open_event(&self, event: EngineEvent) {
        self.open_events
            .lock()
            .expect("open-events mutex poisoned")
            .push(event);
    }

    /// Load the policy cache from the store (at writer open, after the
    /// migration seeded the default Policy).
    pub(crate) fn load_policy_cache(&self) {
        match self.store.snapshot(|store| store.policies()) {
            Ok(policies) => self.policies.publish(policies),
            Err(err) => self.push_open_note(format!(
                "the Policies could not be read into the cached set ({err}); limits resolve \
                 fail closed to pause until a Policy change reloads them"
            )),
        }
    }

    fn push_open_note(&self, note: String) {
        self.open_notes
            .lock()
            .expect("open-notes mutex poisoned")
            .push(note);
    }

    /// The caller-resolved instance facts (explicit config limits + the
    /// model key), reading the instance's effective config. An unreadable
    /// or unknown instance resolves empty (no explicit limits).
    pub(crate) fn resolve_inputs_for(&self, instance: &str) -> ResolveInputs {
        InstanceName::new(instance)
            .ok()
            .and_then(|name| {
                self.effective_config(&name, ConfigLayer::empty())
                    .ok()
                    .map(|effective| config::explicit_limits(&effective))
            })
            .unwrap_or_default()
    }

    /// The resolved limits for one instance and Run (story 21-3): the one
    /// pure resolver over the Run's facts, the instance's explicit config,
    /// the kept-on-pause marker and the cached policy set. `run` may be
    /// `None` (an instance-shaped read: the Fleet and status surfaces) or
    /// have no row (resolves with no Run limits). This is the LIVE
    /// enforcement read (`eval_input`) and the surface read — one function,
    /// never re-implemented. A failed config or store read fails CLOSED
    /// (AD-7: never the new default by accident).
    pub(crate) fn resolved_limits(
        &self,
        name: &InstanceName,
        run: Option<&RunId>,
    ) -> PolicyResolution {
        let facts = run.and_then(|run_id| {
            self.store
                .snapshot(|store| store.run_resolution_facts(run_id))
                .ok()
                .flatten()
        });
        let kept_instance = facts
            .as_ref()
            .map(|(record, _, _, _)| record.instance.clone())
            .unwrap_or_else(|| Some(name.as_str().to_string()));
        let kept = kept_instance
            .as_deref()
            .map(|instance| {
                self.store
                    .snapshot(|store| store.breach_default_kept(instance))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let inputs = self
            .effective_config(name, ConfigLayer::empty())
            .map(|effective| config::explicit_limits(&effective))
            .unwrap_or_else(|_| ResolveInputs {
                malformed_action: Some("the configuration could not be read".to_string()),
                ..ResolveInputs::default()
            });
        let (depth, wire_path, run_limits) = match &facts {
            Some((record, _, limits, _)) => (
                record.control_depth,
                record.wire_path,
                limits.clone(),
            ),
            None => (ControlDepth::Supervised, true, Default::default()),
        };
        policy::resolve(&policy::ResolutionInput {
            depth,
            wire_path,
            bound_labels: Vec::new(),
            run_limits,
            inputs,
            kept_on_pause: kept,
            policies: self.policies.current().as_ref().clone(),
        })
    }

    /// Every Policy, ordered by name (the read-only connection).
    pub(crate) fn policies(&self) -> Result<Vec<PolicyRecord>, StoreError> {
        self.store.snapshot(|store| store.policies())
    }

    /// One Policy by name.
    pub(crate) fn policy(&self, name: &str) -> Result<Option<PolicyRecord>, StoreError> {
        self.store.snapshot(move |store| store.policy(name))
    }

    /// Validate a create/change (the engine's refusals, before any write):
    /// the name grammar, `default`'s reserved shape, the required
    /// non-empty Selector on named Policies, `off`-only Scent-Check values
    /// until 22.3, and the Rate table's ownership.
    fn validate_change(existing: Option<&PolicyRecord>, change: &PolicyChange) -> Result<(), RunError> {
        let invalid = |field: &str, reason: String| RunError::Invalid {
            field: field.to_string(),
            reason,
        };
        let mut chars = change.name.chars();
        let shape_ok = chars.next().is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
            && change.name.len() <= 63
            && change
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !shape_ok {
            return Err(invalid(
                "name",
                "a Policy name is 1-63 characters of lowercase letters, digits, `-` and `_`, \
                 starting with a letter or digit"
                    .to_string(),
            ));
        }
        let is_default = change.name == DEFAULT_POLICY_NAME;
        let selector = change.selector.as_ref().or_else(|| {
            existing
                .filter(|record| record.name == change.name)
                .and_then(|record| record.selector.as_ref())
        });
        if is_default {
            if change.selector.is_some() {
                return Err(invalid(
                    "selector",
                    "the default Policy matches every Run — it cannot carry a Selector"
                        .to_string(),
                ));
            }
        } else if selector.is_none() {
            return Err(invalid(
                "selector",
                "a Policy names its Runs with a Selector (for example team=core,env=prod)"
                    .to_string(),
            ));
        }
        if let Some(mode) = change.set.scent_check {
            if mode != policy::ScentCheckMode::Off {
                return Err(invalid(
                    "scent-check",
                    "Scent-Check Modes land with their own story — only `off` is accepted for \
                     now; `shadow` and `enforce` do nothing until then"
                        .to_string(),
                ));
            }
        }
        if !change.rates.is_empty() && !is_default {
            return Err(invalid(
                "rates",
                "the model-keyed Rate table belongs to the default Policy; a named Policy \
                 carries one flat rate"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Plan a create/change (the dry run): validate, merge, and report the
    /// matched set and the fallback count. Writes nothing.
    pub(crate) fn plan_policy_change(
        &self,
        existing: Option<&PolicyRecord>,
        change: &PolicyChange,
    ) -> Result<PolicyOutcome, RunError> {
        Self::validate_change(existing, change)?;
        let record = merge(existing, change);
        let (matched, fallbacks) = self.matched_fallbacks(&record)?;
        Ok(PolicyOutcome {
            record: Some(record),
            matched,
            fallbacks,
        })
    }

    /// The matched-set and fallback counts for `record` (reads only). The
    /// `default` Policy matches every non-ended Run.
    fn matched_fallbacks(&self, record: &PolicyRecord) -> Result<(usize, usize), RunError> {
        let policies = self.policies.current();
        let runs = self.matched_runs(record)?;
        let mut fallbacks = 0;
        for run in &runs {
            let resolution = self.resolve_record(run, &policies);
            if resolution.fallback.is_some() {
                fallbacks += 1;
            }
        }
        Ok((runs.len(), fallbacks))
    }

    /// The non-ended Runs a (post-change) record matches, capped by the
    /// Selector bound.
    fn matched_runs(&self, record: &PolicyRecord) -> Result<Vec<RunRecord>, RunError> {
        if record.is_default() {
            return self.store.snapshot(|store| store.open_runs()).map_err(RunError::Store);
        }
        let selector = record.selector.clone().expect("validated");
        self.store
            .snapshot(move |store| {
                store.select_runs(&selector, crate::domain::runs::MAX_SELECTED_RUNS)
            })
            .map_err(RunError::Store)
    }

    /// Resolve one Run record against a policy set (the shared pure call,
    /// read-side).
    fn resolve_record(
        &self,
        record: &RunRecord,
        policies: &[PolicyRecord],
    ) -> PolicyResolution {
        let facts = self.store.snapshot(|store| {
            Ok((
                store.run_labels(&record.run_id)?,
                store.run_limits(&record.run_id)?,
            ))
        });
        let (labels, run_limits) = facts.unwrap_or_default();
        let inputs = record
            .instance
            .as_deref()
            .map(|instance| self.resolve_inputs_for(instance))
            .unwrap_or_default();
        let kept = record
            .instance
            .as_deref()
            .map(|name| {
                self.store
                    .snapshot(|store| store.breach_default_kept(name))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        policy::resolve(&policy::ResolutionInput {
            depth: record.control_depth,
            wire_path: record.wire_path,
            bound_labels: labels
                .iter()
                .filter(|label| label.is_bound())
                .map(|label| (label.key.clone(), label.value.clone()))
                .collect(),
            run_limits,
            inputs,
            kept_on_pause: kept,
            policies: policies.to_vec(),
        })
    }

    /// Commit a Policy create or change (one writer job): write the row,
    /// re-resolve every matched non-ended Run (follow-the-latest), breach
    /// at once on ceilings lowered below a Run's usage, re-arm on raised
    /// ones (per-reason fence clears), swap the cached policy set, and
    /// commit one `Policy` event — all in the same transaction. The
    /// returned breaches' process actions are the CALLER's to apply (D2).
    pub(crate) fn apply_policy_change(
        &self,
        existing: Option<PolicyRecord>,
        change: PolicyChange,
        contexts: HashMap<String, InstanceContext>,
    ) -> Result<PolicyApplied, RunError> {
        let plan = self.plan_policy_change(existing.as_ref(), &change)?;
        let record = plan.record.clone().expect("planned");
        let kind = if existing.is_some() {
            PolicyEventKind::Changed
        } else {
            PolicyEventKind::Created
        };
        let prior_limits = existing.as_ref().map(|prior| prior.limits.clone());
        let matched_selector = record.selector.clone();
        let publisher = Arc::clone(&self.run_snapshot);
        let cache = Arc::clone(&self.policies);

        let result = self.writer.call(move |ctx| {
            let store = ctx.store();
            let mut body = || -> Result<_, RunError> {
                store.put_policy(&record).map_err(RunError::Store)?;
                let (outcome, run_events, breaches) = re_resolve_matched(
                    store,
                    ctx,
                    &publisher,
                    matched_selector.as_ref(),
                    &contexts,
                )?;
                let at = now_rfc3339();
                let event = PolicyEvent {
                    schema_version: POLICY_EVENT_SCHEMA_VERSION,
                    kind,
                    name: record.name.clone(),
                    selector: record.selector.clone(),
                    limits: Some(record.limits.clone()),
                    prior_limits,
                    rates: record.rates.clone(),
                    matched: outcome.matched,
                    fallbacks: outcome.fallbacks,
                    at,
                };
                let append = store
                    .append_outbox(&EngineEvent::Policy(event.clone()), OUTBOX_MAX_BYTES)
                    .map_err(RunError::Store)?;
                let _ = append;
                Ok((
                    PolicyApplied {
                        outcome: PolicyOutcome {
                            record: Some(record.clone()),
                            matched: outcome.matched,
                            fallbacks: outcome.fallbacks,
                        },
                        events: vec![(event, append)],
                        breaches,
                        run_events: Vec::new(),
                    },
                    run_events,
                ))
            };
            match body() {
                Ok((value, changes)) => {
                    let events = match ops::commit(ctx, &publisher, changes) {
                        Ok(events) => events,
                        Err(err) => return Err(err),
                    };
                    // Amendment 3: the cached set swaps in the same writer
                    // job — the post-commit hook runs before the ack.
                    let next = match store.policies() {
                        Ok(next) => next,
                        Err(err) => return Err(err),
                    };
                    ctx.after_commit(move || cache.publish(next));
                    Ok(Ok((value, events)))
                }
                Err(RunError::Store(err)) => Err(err),
                Err(refusal) => Ok(Err(refusal)),
            }
        });
        match result {
            Ok(Ok((applied, run_events))) => Ok(PolicyApplied {
                outcome: applied.outcome,
                events: applied.events,
                breaches: applied.breaches,
                run_events,
            }),
            Ok(Err(refusal)) => Err(refusal),
            Err(err) => Err(RunError::Store(err)),
        }
    }

    /// Delete a Policy (one writer job; `default` is refused) and
    /// re-resolve its matched Runs under the loosened set.
    pub(crate) fn apply_policy_delete(
        &self,
        existing: PolicyRecord,
        contexts: HashMap<String, InstanceContext>,
    ) -> Result<PolicyApplied, RunError> {
        if existing.is_default() {
            return Err(RunError::Invalid {
                field: "name".to_string(),
                reason: "the default Policy cannot be deleted — change it instead (it is the \
                         fallback every Run resolves through)"
                    .to_string(),
            });
        }
        let matched_selector = existing.selector.clone();
        let publisher = Arc::clone(&self.run_snapshot);
        let cache = Arc::clone(&self.policies);
        let name = existing.name.clone();
        let prior = existing.clone();
        let result = self.writer.call(move |ctx| {
            let store = ctx.store();
            let mut body = || -> Result<_, RunError> {
                store.delete_policy(&name).map_err(RunError::Store)?;
                // The matched set is the DELETED selector's (the Runs it
                // selected now resolve looser).
                let (outcome, run_events, breaches) = re_resolve_matched(
                    store,
                    ctx,
                    &publisher,
                    matched_selector.as_ref(),
                    &contexts,
                )?;
                let event = PolicyEvent {
                    schema_version: POLICY_EVENT_SCHEMA_VERSION,
                    kind: PolicyEventKind::Deleted,
                    name: name.clone(),
                    selector: prior.selector.clone(),
                    limits: None,
                    prior_limits: Some(prior.limits.clone()),
                    rates: BTreeMap::new(),
                    matched: outcome.matched,
                    fallbacks: outcome.fallbacks,
                    at: now_rfc3339(),
                };
                let append = store
                    .append_outbox(&EngineEvent::Policy(event.clone()), OUTBOX_MAX_BYTES)
                    .map_err(RunError::Store)?;
                Ok((
                    PolicyApplied {
                        outcome: PolicyOutcome {
                            record: None,
                            matched: outcome.matched,
                            fallbacks: outcome.fallbacks,
                        },
                        events: vec![(event, append)],
                        breaches,
                        run_events: Vec::new(),
                    },
                    run_events,
                ))
            };
            match body() {
                Ok((value, changes)) => {
                    let events = match ops::commit(ctx, &publisher, changes) {
                        Ok(events) => events,
                        Err(err) => return Err(err),
                    };
                    let next = match store.policies() {
                        Ok(next) => next,
                        Err(err) => return Err(err),
                    };
                    ctx.after_commit(move || cache.publish(next));
                    Ok(Ok((value, events)))
                }
                Err(RunError::Store(err)) => Err(err),
                Err(refusal) => Ok(Err(refusal)),
            }
        });
        match result {
            Ok(Ok((applied, run_events))) => Ok(PolicyApplied {
                outcome: applied.outcome,
                events: applied.events,
                breaches: applied.breaches,
                run_events,
            }),
            Ok(Err(refusal)) => Err(refusal),
            Err(err) => Err(RunError::Store(err)),
        }
    }

    // -----------------------------------------------------------------------
    // The FR-21 upgrade finish (AD-7's crash order, stages 2 and 3).
    // -----------------------------------------------------------------------

    /// Finish the breach-default upgrade when the v15 migration left a
    /// pending list (a fresh open resumes here; the list itself is the
    /// fail-closed marker until the closing transaction). Stage 2: write
    /// `budget.breach_action = "pause"` into each pending instance's
    /// `config.toml`, idempotently, under its write lock — skipping (and
    /// keeping the row) when the key appeared meanwhile. Stage 3: one
    /// writer job marks every row done and emits exactly ONE
    /// [`BreachDefaultUpgradedEvent`]. The event rides the open events to
    /// the bus; it is durable in the outbox either way.
    pub(crate) fn finish_breach_default_upgrade(&mut self) {
        let pending = match self.store.snapshot(|store| store.breach_default_pending()) {
            Ok(pending) => pending,
            Err(err) => {
                self.push_open_note(format!(
                    "the kept-on-pause upgrade list could not be read ({err}); the kept \
                     instances stay on pause through their markers and the next open retries"
                ));
                return;
            }
        };
        if pending.is_empty() {
            return;
        }
        let mut written: Vec<(String, bool)> = Vec::new();
        for name in &pending {
            let Ok(instance) = InstanceName::new(name) else {
                continue;
            };
            let config_written = match self.upgrade_write_pause(&instance) {
                Ok(written) => written,
                Err(detail) => {
                    // Fail closed: the marker row keeps the instance on
                    // `pause`, surfaced as an open note (never the new
                    // default).
                    self.push_open_note(format!(
                        "{}: the kept-on-pause upgrade could not write its config ({detail}); \
                         the instance stays on pause through its upgrade marker",
                        instance.as_str(),
                    ));
                    false
                }
            };
            written.push((name.clone(), config_written));
        }
        let at = now_rfc3339();
        let at_for_event = at.clone();
        let instances: Vec<UpgradedInstance> = written
            .iter()
            .map(|(name, config_written)| UpgradedInstance {
                name: name.clone(),
                config_written: *config_written,
            })
            .collect();
        let event = BreachDefaultUpgradedEvent {
            schema_version: POLICY_EVENT_SCHEMA_VERSION,
            instances: instances.clone(),
            at: at.clone(),
        };
        let rows = written.clone();
        let result = self.writer.call(move |ctx| {
            let store = ctx.store();
            store.finish_breach_default(&rows, &at)?;
            store
                .append_outbox(&EngineEvent::BreachDefaultUpgraded(event), OUTBOX_MAX_BYTES)
        });
        match result {
            Ok(_) => {
                self.note_open_event(EngineEvent::BreachDefaultUpgraded(
                    BreachDefaultUpgradedEvent {
                        schema_version: POLICY_EVENT_SCHEMA_VERSION,
                        instances,
                        at: at_for_event,
                    },
                ));
            }
            Err(err) => self.push_open_note(format!(
                "the kept-on-pause upgrade could not finish ({err}); the pending list stays \
                 and the next open retries it"
            )),
        }
    }

    /// Stage 2's one idempotent write: `pause` at the instance layer, under
    /// the config write lock, skipped when the key is already present (an
    /// operator write that raced wins; its explicit value outranks the
    /// marker, and the config-write path clears the row). `Ok(false)` =
    /// kept through the marker (missing or unwritable config).
    fn upgrade_write_pause(&self, name: &InstanceName) -> Result<bool, String> {
        let lock_path = self.paths.instance_config_lock(name);
        let _lock = Registry::acquire_config_write_lock(&lock_path)
            .map_err(|detail| format!("the config lock could not be taken: {detail}"))?;
        let path = self.paths.instance_config(name);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(format!("the config could not be read: {err}")),
        };
        let layer = ConfigLayer::parse(SourceLayer::Instance, &path.to_string_lossy(), &text)
            .map_err(|err| err.to_string())?;
        if config::resolve([
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            layer.clone(),
            ConfigLayer::empty(),
        ])
        .value(crate::domain::config::BUDGET_BREACH_ACTION_KEY)
        .is_some()
        {
            // An explicit key appeared (an operator write raced): it wins.
            return Ok(true);
        }
        let mut table = layer.as_table().clone();
        crate::domain::registry::set_dotted(
            &mut table,
            crate::domain::config::BUDGET_BREACH_ACTION_KEY,
            toml::Value::String(BreachAction::Pause.as_str().to_string()),
        )
        .map_err(|err| err.to_string())?;
        let serialized =
            toml::to_string_pretty(&table).map_err(|err| err.to_string())?;
        crate::paths::write_atomically(&path, serialized.as_bytes())
            .map_err(|err| err.to_string())?;
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // The initial policy sweep (open).
    // -----------------------------------------------------------------------

    /// Resolve every open Run once at open, with fresh caller-side inputs:
    /// the stored resolutions and the published snapshot start true (a
    /// Policy created while this engine was off, a hand-edited config, a
    /// depth change offline — all caught here). Fails soft: a failed
    /// resolution is an open note, and the next change re-resolves.
    pub(crate) fn initial_policy_sweep(&mut self) {
        let runs = match self.store.snapshot(|store| store.open_runs()) {
            Ok(runs) => runs,
            Err(err) => {
                self.push_open_note(format!(
                    "the open Runs could not be resolved against the Policies ({err}); they \
                     re-resolve at their next change"
                ));
                return;
            }
        };
        if runs.is_empty() {
            return;
        }
        let mut inputs_by_instance: HashMap<String, ResolveInputs> = HashMap::new();
        for run in &runs {
            if let Some(instance) = &run.instance {
                inputs_by_instance
                    .entry(instance.clone())
                    .or_insert_with(|| self.resolve_inputs_for(instance));
            }
        }
        let publisher = Arc::clone(&self.run_snapshot);
        let result = self.writer.call(move |ctx| {
            let store = ctx.store();
            let mut changes = RunChanges::default();
            let at = now_rfc3339();
            for run in &runs {
                let inputs = run
                    .instance
                    .as_deref()
                    .and_then(|instance| inputs_by_instance.get(instance))
                    .cloned();
                let resolved =
                    policy::resolve_run(store, &run.run_id, inputs, &at)?;
                changes.extend(resolved);
            }
            let events = ops::commit(ctx, &publisher, changes)?;
            Ok::<_, StoreError>(events)
        });
        match result {
            Ok(committed) => {
                for (event, _append) in committed {
                    self.note_open_event(EngineEvent::Run(event));
                }
            }
            Err(err) => self.push_open_note(format!(
                "the open Runs could not be resolved against the Policies ({err}); they \
                 re-resolve at their next change"
            )),
        }
    }
}

/// What one committed Policy write produced.
#[derive(Default)]
pub(crate) struct PolicyApplied {
    /// The reported outcome (matched + fallbacks + the record).
    pub outcome: PolicyOutcome,
    /// The Policy event + its outbox append (publish on the bus).
    pub events: Vec<(PolicyEvent, crate::ports::OutboxAppend)>,
    /// Breaches the change fired at once; the process actions are the
    /// caller's to apply (D2).
    pub breaches: Vec<CommittedBreach>,
    /// Run events the re-resolutions committed (publish on the bus).
    pub run_events: Vec<CommittedRunEvent>,
}

/// The re-resolution pass shared by create, change and delete: every
/// non-ended Run matching `selector` (every non-ended Run when `None` =
/// the default Policy) resolves under the store's current policy set, its
/// `run_policy` row and Anomalies update, and each process-owning Run gets
/// the breach-at-once / re-arm pass.
fn re_resolve_matched(
    store: &crate::store::SqliteStore,
    ctx: &mut crate::store::WriterCtx<'_>,
    publisher: &Arc<crate::domain::runs::snapshot::SnapshotPublisher>,
    selector: Option<&Selector>,
    contexts: &HashMap<String, InstanceContext>,
) -> Result<(PolicyOutcome, RunChanges, Vec<CommittedBreach>), RunError> {
    let matched: Vec<RunRecord> = match selector {
        None => store.open_runs().map_err(RunError::Store)?,
        Some(selector) => store
            .select_runs(selector, crate::domain::runs::MAX_SELECTED_RUNS)
            .map_err(RunError::Store)?,
    };
    let policies: Vec<PolicyRecord> = store.policies().map_err(RunError::Store)?;
    let mut fallbacks = 0;
    let mut changes = RunChanges::default();
    let mut breaches = Vec::new();
    let at = now_rfc3339();
    for run in &matched {
        let inputs = run
            .instance
            .as_deref()
            .map(|instance| {
                // The Run's stored inputs: a Policy change cannot change
                // the instance's explicit limits, and the config-write path
                // re-resolves with fresh ones.
                store
                    .run_policy(&run.run_id)
                    .ok()
                    .flatten()
                    .map(|row| row.inputs)
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let kept = run
            .instance
            .as_deref()
            .map(|instance| store.breach_default_kept(instance))
            .transpose()
            .map_err(RunError::Store)?
            .unwrap_or(false);
        let labels = store.run_labels(&run.run_id).map_err(RunError::Store)?;
        let run_limits = store.run_limits(&run.run_id).map_err(RunError::Store)?;
        let resolution = policy::resolve(&policy::ResolutionInput {
            depth: run.control_depth,
            wire_path: run.wire_path,
            bound_labels: labels
                .iter()
                .filter(|label| label.is_bound())
                .map(|label| (label.key.clone(), label.value.clone()))
                .collect(),
            run_limits,
            inputs: inputs.clone(),
            kept_on_pause: kept,
            policies: policies.clone(),
        });
        if resolution.fallback.is_some() {
            fallbacks += 1;
        }
        let resolved = policy::resolve_and_store(store, run, &resolution, &inputs, &at)?;
        changes.extend(resolved);

        // The money half, on the process-owning Run only (child arms come
        // from run_limits, which a Policy change does not touch).
        if run.process_owner && !run.is_ended() {
            if let Some(instance) = &run.instance {
                if let Ok(name) = InstanceName::new(instance) {
                    let context = contexts.get(instance).cloned().unwrap_or_default();
                    re_arm(store, &name, run, &resolution, &context, ctx, publisher, &mut breaches)?;
                }
            }
        }
    }
    Ok((
        PolicyOutcome {
            record: None,
            matched: matched.len(),
            fallbacks,
        },
        changes,
        breaches,
    ))
}

/// The breach-at-once and re-arm pass over one process-owning Run.
///
/// * RAISED above the Run's subtree usage: the latch rows for the re-armed
///   `(dimension, scope)` clear, the `breach` Anomaly clears when no latch
///   remains, and the matching `block` fence reason clears PER REASON
///   (amendment 2: only the reasons whose new ceiling now sits above the
///   totals; a fence holding any other latched `block` reason stays up).
/// * THEN lowered-to-at-or-below: `pipeline::evaluate` commits the breach
///   NOW (breach row, latch, outbox, `breach` Anomaly; the decision's
///   process action returns to the caller to apply).
fn re_arm(
    store: &crate::store::SqliteStore,
    instance: &InstanceName,
    run: &RunRecord,
    resolution: &PolicyResolution,
    context: &InstanceContext,
    ctx: &mut crate::store::WriterCtx<'_>,
    publisher: &Arc<crate::domain::runs::snapshot::SnapshotPublisher>,
    breaches: &mut Vec<CommittedBreach>,
) -> Result<(), RunError> {
    let usage = store
        .run_rollup(instance, &run.run_id)
        .map_err(RunError::Store)?;
    let tokens_now = usage.totals.total_tokens();
    let dollars_now = usage.cost.get() as u64;

    // ---- the re-arm half (raise above the totals).
    let mut re_armed: Vec<crate::domain::LatchKey> = Vec::new();
    for (key, ceiling, total_now) in [
        (
            crate::domain::LatchKey::new(
                crate::domain::BreachDimension::Tokens,
                crate::domain::BreachScope::PerRun,
            ),
            resolution.tokens.per_run,
            tokens_now,
        ),
        (
            crate::domain::LatchKey::new(
                crate::domain::BreachDimension::Tokens,
                crate::domain::BreachScope::Cumulative,
            ),
            resolution.tokens.cumulative,
            tokens_now,
        ),
        (
            crate::domain::LatchKey::new(
                crate::domain::BreachDimension::Dollars,
                crate::domain::BreachScope::PerRun,
            ),
            resolution.dollars.per_run.map(|micros| micros.get() as u64),
            dollars_now,
        ),
        (
            crate::domain::LatchKey::new(
                crate::domain::BreachDimension::Dollars,
                crate::domain::BreachScope::Cumulative,
            ),
            resolution
                .dollars
                .cumulative
                .map(|micros| micros.get() as u64),
            dollars_now,
        ),
    ] {
        if let Some(ceiling) = ceiling {
            if ceiling > total_now {
                re_armed.push(key);
            }
        }
    }
    let mut any_latch_left = false;
    if !re_armed.is_empty() {
        for (key, action) in store.latches(&run.run_id).map_err(RunError::Store)? {
            if re_armed.contains(&key) {
                store
                    .unlatch(&run.run_id, key, action)
                    .map_err(RunError::Store)?;
                // Per-reason fence clear: only the raised reasons clear; the
                // fence stays up while any other latched `block` reason
                // holds (amendment 2).
                if action == BreachAction::Block {
                    if let Some(fence) = &context.fence {
                        let fence = fence.clone();
                        ctx.after_commit(move || fence.clear(key));
                    }
                }
            } else {
                any_latch_left = true;
            }
        }
        if !any_latch_left {
            ops::clear_breach_anomaly(store, &run.run_id, &now_rfc3339())
                .map_err(RunError::Store)?;
        }
    }

    // ---- the breach-at-once half (lowered to at-or-below the usage):
    // the same pipeline evaluation a usage commit runs, over the NEW arms.
    let armed = resolution.tokens.is_set()
        || (resolution.rate.is_some() && resolution.dollars.is_set());
    if armed {
        let input = EvalInput {
            name: instance.clone(),
            run_id: run.run_id.clone(),
            metering_source: context.metering_source.clone(),
            token_budget: resolution.tokens,
            token_action: resolution.effective_action,
            rate: resolution.rate,
            cost_cap: resolution.dollars,
            cost_action: resolution.effective_action,
            supervised: true,
            fence: context.fence.clone(),
            runs: Some(Arc::clone(publisher)),
            #[cfg(test)]
            totals_fault: false,
        };
        let evaluation = pipeline::evaluate(ctx, &input).map_err(RunError::Store)?;
        breaches.extend(evaluation.breaches);
    }
    Ok(())
}

/// Merge a change over an existing row (the change's `set` fields win;
/// `clear` removes; `rates` edits the named models).
fn merge(existing: Option<&PolicyRecord>, change: &PolicyChange) -> PolicyRecord {
    let mut record = existing.cloned().unwrap_or_else(|| PolicyRecord {
        name: change.name.clone(),
        selector: None,
        limits: PolicyLimits::default(),
        rates: BTreeMap::new(),
    });
    if let Some(selector) = &change.selector {
        record.selector = Some(selector.clone());
    }
    let limits = &mut record.limits;
    if change.set.tokens_per_run.is_some() {
        limits.tokens_per_run = change.set.tokens_per_run;
    }
    if change.set.tokens_cumulative.is_some() {
        limits.tokens_cumulative = change.set.tokens_cumulative;
    }
    if change.set.dollars_per_run_micros.is_some() {
        limits.dollars_per_run_micros = change.set.dollars_per_run_micros;
    }
    if change.set.dollars_cumulative_micros.is_some() {
        limits.dollars_cumulative_micros = change.set.dollars_cumulative_micros;
    }
    if change.set.breach_action.is_some() {
        limits.breach_action = change.set.breach_action;
    }
    if change.set.scent_check.is_some() {
        limits.scent_check = change.set.scent_check;
    }
    if change.set.rate.is_some() {
        limits.rate = change.set.rate;
    }
    for field in &change.clear {
        match field.as_str() {
            "tokens-per-run" => limits.tokens_per_run = None,
            "tokens-cumulative" => limits.tokens_cumulative = None,
            "dollars-per-run" => limits.dollars_per_run_micros = None,
            "dollars-cumulative" => limits.dollars_cumulative_micros = None,
            "breach-action" => limits.breach_action = None,
            "scent-check" => limits.scent_check = None,
            "rate" => limits.rate = None,
            _ => {}
        }
    }
    for (model, rate) in &change.rates {
        match rate {
            Some(rate) => {
                record.rates.insert(model.clone(), *rate);
            }
            None => {
                record.rates.remove(model);
            }
        }
    }
    record
}

/// The FR-21 upgrade's pending-list source (amendment 1): the REAL
/// persisted layers — the embedded engine-default const, the kind-default
/// layer (adapter-supplied, in-memory) and the instance `config.toml` —
/// resolved exactly as runtime resolves them. An unreadable layer counts
/// as carrying the key (fail closed; the runtime read degrades to `pause`
/// the same way).
pub(crate) struct RegistryBreachDefault;

impl BreachDefaultSource for RegistryBreachDefault {
    fn resolve(&self, kind: &str, home: &std::path::Path) -> ResolveInputs {
        let engine =
            crate::domain::registry::engine_defaults_layer().unwrap_or_else(|_| ConfigLayer::empty());
        let kind_layer = crate::domain::registry::kind_defaults_layer(kind);
        let config_path = home.join(crate::paths::INSTANCE_CONFIG_FILE);
        let instance = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|text| {
                ConfigLayer::parse(
                    crate::domain::config::SourceLayer::Instance,
                    &config_path.to_string_lossy(),
                    &text,
                )
                .ok()
            })
            .unwrap_or_else(ConfigLayer::empty);
        let effective = config::resolve([
            engine,
            kind_layer,
            instance,
            ConfigLayer::empty(),
        ]);
        config::explicit_limits(&effective)
    }
}
