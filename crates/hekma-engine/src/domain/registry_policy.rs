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

use crate::domain::budget::{BreachAction, TokenBudget};
use crate::domain::bus::EngineEvent;
use crate::domain::config::{self, ConfigLayer, SourceLayer};
use crate::domain::cost::{CostCap, Rate};
use crate::domain::name::InstanceName;
use crate::domain::outbox::OUTBOX_MAX_BYTES;
use crate::domain::pipeline::{self, CommittedBreach, EvalInput};
use crate::domain::runs::ops::{self, CommittedRunEvent, RunChanges};
use crate::domain::runs::policy::{
    self, BreachDefaultUpgradedEvent, LimitSource, LimitSources, PolicyEvent, PolicyEventKind,
    PolicyLimits, PolicyRecord, ResolveInputs, ScentCheckMode, UpgradedInstance,
    DEFAULT_POLICY_NAME, POLICY_EVENT_SCHEMA_VERSION,
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
pub struct PolicyChange {
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
pub struct PolicyOutcome {
    /// The Policy after the change (`None` on a delete).
    pub record: Option<PolicyRecord>,
    /// How many non-ended Runs the (old, on a delete) Selector matches.
    pub matched: usize,
    /// How many matched Runs fall back to `warn` at their depth.
    pub fallbacks: usize,
    /// Runs the change could not re-resolve (their instance's configuration
    /// could not be read), each named with the reason. Those Runs keep
    /// their previous resolution; enforcement still resolves live at every
    /// usage event.
    pub notes: Vec<String>,
}

/// The live per-instance context the policy-change job's money half needs
/// (built by the caller — engine or supervisor — which holds the locks):
/// the instance's metering wire fence, its metering source for the breach
/// records, and its fresh explicit inputs. Whether the engine supervises
/// the instance is the CALLER's own fact when it applies the returned
/// process actions (D2) — an unsupervised instance's breaches still
/// record and latch here.
#[derive(Clone, Default)]
pub(crate) struct InstanceContext {
    /// The instance's live wire fence (an engine-observed instance's
    /// listener fence; `None` when it has no listener).
    pub fence: Option<WireFence>,
    /// The instance's metering source (the breach records name it).
    pub metering_source: String,
    /// The instance's explicit limits and model, read fresh from its
    /// configuration by the caller; `Err` names why they could not be read
    /// (the Run is then skipped, never resolved as if nothing were set).
    pub inputs: Option<Result<ResolveInputs, String>>,
}

impl Registry {
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
    /// migration seeded the default Policy). A failed read — or a set
    /// without its `default` row — DEGRADES the cache: every resolution
    /// through it fails closed to `pause` with a note until a later load
    /// succeeds (AD-7; the pre-fix empty cache silently resolved the
    /// seeded `warn`, the 21-3 review round 1 finding).
    pub(crate) fn load_policy_cache(&self) {
        match self.store.snapshot(|store| store.policies()) {
            Ok(policies) => {
                if policies.iter().any(|policy| policy.is_default()) {
                    self.policies.publish(policies);
                } else {
                    self.policies.degrade(
                        "the default Policy row is missing from the stored set".to_string(),
                    );
                    self.push_open_note(
                        "the stored Policies carry no `default` row, so every limit resolves \
                         fail-closed to pause until one is restored (a Policy change reloads \
                         the set)"
                            .to_string(),
                    );
                }
            }
            Err(err) => {
                self.policies
                    .degrade(format!("the Policies could not be read ({err})"));
                self.push_open_note(format!(
                    "the Policies could not be read into the cached set ({err}); limits resolve \
                     fail closed to pause until a Policy change reloads them"
                ))
            }
        }
    }

    /// Re-read the policy set when the database moved underneath this
    /// process (story 21-3, the cross-process staleness check): SQLite's
    /// `data_version` on the read connection changes when ANY OTHER
    /// connection or process commits — this engine's own changes swap the
    /// cache in their own post-commit hook — so one pragma read per tick
    /// (never per event) picks up a second engine's Policy change within a
    /// tick. Best-effort: a failed check leaves the cached set standing
    /// (the next tick retries); a failed reload degrades the cache
    /// (fail-closed resolution) exactly like the open load.
    pub(crate) fn refresh_policy_cache(&self) {
        let version = match self.store.data_version() {
            Ok(version) => version,
            Err(_) => return,
        };
        if !self.policies.stale_at(version) {
            return;
        }
        self.load_policy_cache();
        self.policies.stamp(version);
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
    pub(crate) fn try_resolve_inputs_for(&self, instance: &str) -> Result<ResolveInputs, String> {
        let name = InstanceName::new(instance).map_err(|err| err.to_string())?;
        self.effective_config(&name, ConfigLayer::empty())
            .map(|effective| config::explicit_limits(&effective))
            .map_err(|err| err.to_string())
    }

    /// The infallible read: an unreadable or unknown instance resolves
    /// empty. Never use it where a missing explicit limit could let a
    /// Policy act (use [`Self::try_resolve_inputs_for`]).
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

    /// The caller-side instance facts for a Run CREATION or ADOPTION
    /// (story 21-3): fresh from the configuration; an unreadable
    /// configuration fail-closes the ACTION to `pause` (carried as the
    /// malformed note) — the Run is never resolved as if the instance had
    /// set nothing (the creation paths' variant of the money-rule fail
    /// closed, AD-7).
    pub(crate) fn resolve_inputs_fail_closed(&self, name: &InstanceName) -> ResolveInputs {
        match self.try_resolve_inputs_for(name.as_str()) {
            Ok(inputs) => inputs,
            Err(reason) => ResolveInputs {
                malformed_action: Some(reason),
                ..ResolveInputs::default()
            },
        }
    }

    /// The resolved limits for one instance and Run (story 21-3): the one
    /// pure resolver over the Run's facts, the instance's explicit config,
    /// the kept-on-pause marker and the cached policy set. `run` may be
    /// `None` (an instance-shaped read: the Fleet and status surfaces) or
    /// have no row (resolves with no Run limits). This is the LIVE
    /// enforcement read (`eval_input`) and the surface read — one function,
    /// never re-implemented. A failed config, store or cache read fails
    /// CLOSED (AD-7: never the new default by accident) — the arms stay
    /// unset, the action pauses and the fail-closed note names the reason.
    /// The Run's BOUND labels ride the facts read (a named Policy selects
    /// through them — without them no named Policy ever applied, the 21-3
    /// review round 1 finding).
    pub(crate) fn resolved_limits(
        &self,
        name: &InstanceName,
        run: Option<&RunId>,
    ) -> PolicyResolution {
        let facts = match run {
            None => None,
            Some(run_id) => match self
                .store
                .snapshot(|store| store.run_resolution_facts(run_id))
            {
                Ok(facts) => facts,
                Err(err) => {
                    return fail_closed_resolution(format!(
                        "this Run's resolution facts could not be read ({err}), so its limits \
                         fail closed to pause until the read succeeds"
                    ))
                }
            },
        };
        let kept_instance = facts
            .as_ref()
            .map(|facts| facts.record.instance.clone())
            .unwrap_or_else(|| Some(name.as_str().to_string()));
        let kept = match kept_instance.as_deref() {
            Some(instance) => {
                match self
                    .store
                    .snapshot(|store| store.breach_default_kept(instance))
                {
                    Ok(kept) => kept,
                    // A failed kept-marker read keeps `pause` (fail closed):
                    // the marker is the FR-21 promise's carrier.
                    Err(err) => {
                        return fail_closed_resolution(format!(
                            "the kept-on-pause marker could not be read ({err}), so the breach \
                             action fails closed to pause until the read succeeds"
                        ))
                    }
                }
            }
            None => false,
        };
        let inputs = self
            .effective_config(name, ConfigLayer::empty())
            .map(|effective| config::explicit_limits(&effective))
            .unwrap_or_else(|_| ResolveInputs {
                malformed_action: Some("the configuration could not be read".to_string()),
                ..ResolveInputs::default()
            });
        let (depth, wire_path, run_limits, bound_labels) = match &facts {
            Some(facts) => (
                facts.record.control_depth,
                facts.record.wire_path,
                facts.limits,
                facts
                    .labels
                    .iter()
                    .filter(|label| label.is_bound())
                    .map(|label| (label.key.clone(), label.value.clone()))
                    .collect(),
            ),
            None => (
                ControlDepth::Supervised,
                true,
                Default::default(),
                Vec::new(),
            ),
        };
        let policies = match self.policies.current() {
            Ok(policies) => policies,
            Err(unavailable) => {
                return fail_closed_resolution(format!(
                    "the Policies could not be read into the cached set ({unavailable}), so the \
                     limits fail closed to pause until a Policy change reloads them"
                ))
            }
        };
        if !policies.iter().any(|policy| policy.is_default()) {
            // The `default` row is the fallback layer for every limit; a
            // set without it (a corrupt row dropped in a wholesale-failure
            // read is caught above, but a seeded-empty set is not) must not
            // silently resolve the seeded `warn`.
            return fail_closed_resolution(
                "the default Policy row is missing from the cached set, so the limits fail \
                 closed to pause until it is restored"
                    .to_string(),
            );
        }
        policy::resolve(&policy::ResolutionInput {
            depth,
            wire_path,
            bound_labels,
            run_limits,
            inputs,
            caller_limits: PolicyLimits::default(),
            kept_on_pause: kept,
            policies: policies.as_ref().clone(),
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
    fn validate_change(
        existing: Option<&PolicyRecord>,
        change: &PolicyChange,
    ) -> Result<(), RunError> {
        let invalid = |field: &str, reason: String| RunError::Invalid {
            field: field.to_string(),
            reason,
        };
        let mut chars = change.name.chars();
        let shape_ok = chars
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
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
                return Err(RunError::PolicyDefault {
                    what: "cannot carry a Selector",
                });
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
                return Err(RunError::ScentUnavailable {
                    mode: mode.as_str().to_string(),
                });
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
        // The default Policy prices through its model-keyed table
        // (`--rate-for` / the PATCH `rates` part); a FLAT rate on it would
        // be stored and shown but never apply — refuse it (point at the
        // table) instead of accepting a silent no-op.
        if is_default && change.set.rate.is_some() {
            return Err(invalid(
                "rate",
                "the default Policy prices through its model-keyed Rate table (edit it with \
                 --rate-for <model>, or the rates part of a change) — a flat rate belongs to \
                 a named Policy"
                    .to_string(),
            ));
        }
        // An unknown `clear` name is a refusal, not a silent no-op: a typo
        // would otherwise report success while clearing nothing.
        for field in &change.clear {
            if !matches!(
                field.as_str(),
                "tokens-per-run"
                    | "tokens-cumulative"
                    | "dollars-per-run"
                    | "dollars-cumulative"
                    | "breach-action"
                    | "scent-check"
                    | "rate"
            ) {
                return Err(invalid(
                    "clear",
                    format!(
                        "`{field}` is not a limit field a change can clear (tokens-per-run, \
                         tokens-cumulative, dollars-per-run, dollars-cumulative, \
                         breach-action, scent-check, rate)"
                    ),
                ));
            }
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
            notes: Vec::new(),
        })
    }

    /// The matched-set and fallback counts for `record` (reads only). The
    /// `default` Policy matches every non-ended Run. The fallback count
    /// resolves against the PLANNED set (the cache with `record` merged
    /// in), so the dry run reports what the real change would produce —
    /// the pre-change cache under-counted (the 21-3 review round 1
    /// finding).
    fn matched_fallbacks(&self, record: &PolicyRecord) -> Result<(usize, usize), RunError> {
        let policies = self.planned_policy_set(record)?;
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

    /// The cached policy set with `record` merged in (a create pushes it,
    /// a change replaces its row): the set the change will produce.
    fn planned_policy_set(&self, record: &PolicyRecord) -> Result<Vec<PolicyRecord>, RunError> {
        let mut policies = self
            .policies
            .current()
            .map_err(|unavailable| RunError::Store(StoreError::Backend(unavailable.to_string())))?
            .as_ref()
            .clone();
        match policies
            .iter()
            .position(|policy| policy.name == record.name)
        {
            Some(at) => policies[at] = record.clone(),
            None => policies.push(record.clone()),
        }
        policies.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(policies)
    }

    /// The non-ended Runs a (post-change) record matches, capped by the
    /// Selector bound.
    fn matched_runs(&self, record: &PolicyRecord) -> Result<Vec<RunRecord>, RunError> {
        if record.is_default() {
            return self
                .store
                .snapshot(|store| store.open_runs())
                .map_err(RunError::Store);
        }
        let selector = record.selector.clone().expect("validated");
        self.store
            .snapshot(move |store| policy_matched_runs(store, &selector))
            .map_err(RunError::Store)
    }

    /// Resolve one Run record against a policy set (the shared pure call,
    /// read-side).
    fn resolve_record(&self, record: &RunRecord, policies: &[PolicyRecord]) -> PolicyResolution {
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
            caller_limits: PolicyLimits::default(),
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
        let prior_selector = existing.as_ref().and_then(|prior| prior.selector.clone());
        let matched_selector = record.selector.clone();
        let publisher = Arc::clone(&self.run_snapshot);
        let cache = Arc::clone(&self.policies);

        let result = self.writer.call(move |ctx| {
            let store = ctx.store();
            let body = || -> Result<_, RunError> {
                store.put_policy(&record).map_err(RunError::Store)?;
                // The re-resolve set is the UNION of the old and new
                // Selectors: Runs only the OLD Selector matched loosen too.
                let selectors: Vec<Selector> = prior_selector
                    .iter()
                    .chain(matched_selector.iter())
                    .cloned()
                    .collect();
                let set = if record.is_default() {
                    SelectorSet::EveryRun
                } else {
                    SelectorSet::Matching(&selectors)
                };
                let (outcome, run_events, breaches) =
                    re_resolve_matched(store, ctx, set, &contexts)?;
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
                            notes: outcome.notes.clone(),
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
            return Err(RunError::PolicyDefault {
                what: "cannot be deleted — change it instead",
            });
        }
        let matched_selector = existing.selector.clone();
        let publisher = Arc::clone(&self.run_snapshot);
        let cache = Arc::clone(&self.policies);
        let name = existing.name.clone();
        let prior = existing.clone();
        let result = self.writer.call(move |ctx| {
            let store = ctx.store();
            let body = || -> Result<_, RunError> {
                store.delete_policy(&name).map_err(RunError::Store)?;
                // The matched set is the DELETED selector's (the Runs it
                // selected now resolve looser).
                let selectors: Vec<Selector> = matched_selector.into_iter().collect();
                let (outcome, run_events, breaches) =
                    re_resolve_matched(store, ctx, SelectorSet::Matching(&selectors), &contexts)?;
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
                            notes: outcome.notes.clone(),
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
            store.append_outbox(&EngineEvent::BreachDefaultUpgraded(event), OUTBOX_MAX_BYTES)
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
        let serialized = toml::to_string_pretty(&table).map_err(|err| err.to_string())?;
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
                let resolved = policy::resolve_run(store, &run.run_id, inputs, &at)?;
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

/// Which Runs a Policy change re-resolves: every non-ended Run (the
/// `default` Policy) or the union of the named Selectors (a change passes
/// old AND new so only-old matches loosen too; a delete passes the
/// deleted Selector's).
enum SelectorSet<'a> {
    EveryRun,
    Matching(&'a [Selector]),
}

/// What one committed Policy write produced.
#[derive(Debug, Default)]
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
/// non-ended Run matching one of `selectors` (every non-ended Run when
/// `None` = the `default` Policy) resolves under the store's current policy
/// set, its `run_policy` row and Anomalies update, and each Run with an
/// instance gets the breach-at-once / re-arm pass. A CHANGE passes BOTH its
/// old and new Selectors: the Runs only the OLD Selector matched must
/// re-resolve too (they loosen), not just the newly-matched ones (the
/// 21-3 review round 1 finding).
fn re_resolve_matched(
    store: &crate::store::SqliteStore,
    ctx: &mut crate::store::WriterCtx<'_>,
    selectors: SelectorSet<'_>,
    contexts: &HashMap<String, InstanceContext>,
) -> Result<(PolicyOutcome, RunChanges, Vec<CommittedBreach>), RunError> {
    let matched: Vec<RunRecord> = match selectors {
        SelectorSet::EveryRun => store.open_runs().map_err(RunError::Store)?,
        SelectorSet::Matching(selectors) => {
            let mut seen = std::collections::BTreeSet::new();
            let mut matched = Vec::new();
            for selector in selectors {
                for run in policy_matched_runs(store, selector).map_err(RunError::Store)? {
                    if seen.insert(run.run_id.clone()) {
                        matched.push(run);
                    }
                }
            }
            matched
        }
    };
    let mut notes = Vec::new();
    let policies: Vec<PolicyRecord> = store.policies().map_err(RunError::Store)?;
    let mut fallbacks = 0;
    let mut changes = RunChanges::default();
    let mut breaches = Vec::new();
    let at = now_rfc3339();
    for run in &matched {
        // The instance's explicit limits (money code, AD-7: fail closed).
        // Fresh from the configuration when the caller read them; else the
        // Run's stored inputs. A Run whose inputs cannot be known is
        // SKIPPED with a note — resolving it as "nothing set" would let a
        // Policy fill (and breach on) a limit the instance set explicitly.
        let inputs = match run.instance.as_deref() {
            None => ResolveInputs::default(),
            Some(instance) => match contexts.get(instance).and_then(|c| c.inputs.clone()) {
                Some(Ok(inputs)) => inputs,
                Some(Err(reason)) => {
                    notes.push(format!(
                        "Run `{}` (agent `{instance}`) was not re-resolved: its configuration \
                         could not be read ({reason})",
                        run.run_id.as_str()
                    ));
                    continue;
                }
                None => match store.run_policy(&run.run_id).map_err(RunError::Store)? {
                    Some(row) => row.inputs,
                    None => {
                        notes.push(format!(
                            "Run `{}` (agent `{instance}`) was not re-resolved: it has no \
                             stored resolution and its agent is not supervised here",
                            run.run_id.as_str()
                        ));
                        continue;
                    }
                },
            },
        };
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
            caller_limits: PolicyLimits::default(),
            kept_on_pause: kept,
            policies: policies.clone(),
        });
        if resolution.fallback.is_some() {
            fallbacks += 1;
        }
        let resolved = policy::resolve_and_store(store, run, &resolution, &inputs, &at)?;
        changes.extend(resolved);

        // The money half, on every matched non-ended Run with an instance:
        // the process owner for its own arms, a child for its OWN level's
        // arms (each level's arms come from its own resolution — 21-2's
        // `decide_chain` stays consistent).
        if !run.is_ended() {
            if let Some(instance) = &run.instance {
                if let Ok(name) = InstanceName::new(instance) {
                    let context = contexts.get(instance).cloned().unwrap_or_default();
                    re_arm(
                        store,
                        &name,
                        run,
                        &resolution,
                        &context,
                        ctx,
                        &mut breaches,
                        &mut changes,
                        &mut notes,
                    )?;
                }
            }
        }
    }
    Ok((
        PolicyOutcome {
            record: None,
            matched: matched.len(),
            fallbacks,
            notes,
        },
        changes,
        breaches,
    ))
}

/// The re-arm pass over one matched Run.
///
/// * RAISED above the totals (or UNSET — a cleared or deleted ceiling is
///   limit-free, so its latch always re-arms): the latch rows for the
///   re-armed `(dimension, scope)` clear, the `breach` Anomaly clears when
///   no latch remains (with its `anomalies-changed` event and snapshot
///   entry), and the matching `block` fence reason clears PER REASON
///   (amendment 2: only the reasons whose new ceiling now sits above the
///   totals; a fence holding any other latched `block` reason stays up).
///   Per-run ceilings compare against the RUN's totals; cumulative
///   ceilings against the INSTANCE's lifetime totals (a multi-Run instance
///   over its cumulative cap must not unlatch on a per-Run comparison).
/// * THEN lowered-to-at-or-below: `pipeline::evaluate` commits the breach
///   NOW (breach row, latch, outbox, `breach` Anomaly; the decision's
///   process action returns to the caller to apply).
///
/// The evaluation's Run changes fold into `changes` (the caller's ONE
/// commit publishes the post-breach entries last — the pre-breach entry
/// must not overwrite them), and its degraded reads surface as notes.
#[allow(clippy::too_many_arguments)]
fn re_arm(
    store: &crate::store::SqliteStore,
    instance: &InstanceName,
    run: &RunRecord,
    resolution: &PolicyResolution,
    context: &InstanceContext,
    ctx: &mut crate::store::WriterCtx<'_>,
    breaches: &mut Vec<CommittedBreach>,
    changes: &mut RunChanges,
    notes: &mut Vec<String>,
) -> Result<(), RunError> {
    let usage = store
        .run_rollup(instance, &run.run_id)
        .map_err(RunError::Store)?;
    let tokens_now = usage.totals.total_tokens();
    let dollars_now = usage.cost.get() as u64;
    // The CUMULATIVE scope compares against the instance's LIFETIME totals
    // (every Run), not this Run's — the same read the pipeline's cumulative
    // arm uses (`instance_rollup`).
    let instance_usage = store.instance_rollup(instance).map_err(RunError::Store)?;
    let instance_tokens = instance_usage.totals.total_tokens();
    let instance_dollars = instance_usage.cost.get() as u64;

    // ---- the re-arm half (raise above the totals, or unset).
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
            instance_tokens,
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
            instance_dollars,
        ),
    ] {
        // A ceiling that became UNSET re-arms too: `--clear` and delete
        // loosen, and a latched breach under a ceiling that no longer
        // exists would hold the Run (and a `block` fence) forever.
        if ceiling.is_none_or(|ceiling| ceiling > total_now) {
            re_armed.push(key);
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
            changes.extend(
                ops::clear_breach_anomaly(store, &run.run_id, &now_rfc3339())
                    .map_err(RunError::Store)?,
            );
        }
    }

    // ---- the breach-at-once half (lowered to at-or-below the usage):
    // the same pipeline evaluation a usage commit runs, over the NEW arms.
    // The process level's arms are the instance's (the process-owning
    // Run's resolution — the matched Run's own when it IS the owner; the
    // owner's STORED resolution when the match is a child, whose own arms
    // ride its own level inside the evaluation).
    let owner_arms = if run.process_owner {
        resolution.clone()
    } else {
        let owner_row = match store.open_process_run(instance).map_err(RunError::Store)? {
            Some(owner) => owner,
            None => {
                notes.push(format!(
                    "Run `{}` (agent `{}`) was not breach-checked: its agent has no open                      process-owning Run to arm the process level",
                    run.run_id.as_str(),
                    instance.as_str(),
                ));
                return Ok(());
            }
        };
        match store
            .run_policy(&owner_row.run_id)
            .map_err(RunError::Store)?
        {
            Some(row) => row.resolution,
            None => {
                notes.push(format!(
                    "Run `{}` (agent `{}`) was not breach-checked: its agent's process-owning                      Run has no stored resolution to arm the process level",
                    run.run_id.as_str(),
                    instance.as_str(),
                ));
                return Ok(());
            }
        }
    };
    let armed = owner_arms.tokens.is_set()
        || (owner_arms.rate.is_some() && owner_arms.dollars.is_set())
        || resolution.tokens.per_run.is_some()
        || (resolution.rate.is_some() && resolution.dollars.per_run.is_some());
    if armed {
        let input = EvalInput {
            name: instance.clone(),
            run_id: run.run_id.clone(),
            // A missing context (the instance not supervised here) is the
            // honest `unknown`, never a fabricated empty source.
            metering_source: if context.metering_source.is_empty() {
                "unknown".to_string()
            } else {
                context.metering_source.clone()
            },
            token_budget: owner_arms.tokens,
            token_action: owner_arms.effective_action,
            rate: owner_arms.rate,
            cost_cap: owner_arms.dollars,
            cost_action: owner_arms.effective_action,
            supervised: true,
            fence: context.fence.clone(),
            runs: None,
            #[cfg(test)]
            totals_fault: false,
        };
        let mut evaluation = pipeline::evaluate(ctx, &input).map_err(RunError::Store)?;
        breaches.extend(std::mem::take(&mut evaluation.breaches));
        notes.extend(std::mem::take(&mut evaluation.degraded_reads));
        changes.extend(evaluation.take_run_changes());
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
        let engine = crate::domain::registry::engine_defaults_layer()
            .unwrap_or_else(|_| ConfigLayer::empty());
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
        let effective = config::resolve([engine, kind_layer, instance, ConfigLayer::empty()]);
        config::explicit_limits(&effective)
    }
}

/// The all-unset `LimitSources` (a fail-closed resolution's sources).
fn unset_sources() -> LimitSources {
    LimitSources {
        tokens_per_run: LimitSource::Unset,
        tokens_cumulative: LimitSource::Unset,
        dollars_per_run: LimitSource::Unset,
        dollars_cumulative: LimitSource::Unset,
        breach_action: LimitSource::Unset,
        scent_check: LimitSource::Unset,
        rate: LimitSource::Unset,
    }
}

/// A resolution that fails CLOSED (AD-7): no arms, `pause`, and the note
/// that names why — what `Registry::resolved_limits` answers when a read
/// the resolution depends on failed (money code never resolves the
/// default by accident).
fn fail_closed_resolution(note: String) -> PolicyResolution {
    PolicyResolution {
        tokens: TokenBudget::none(),
        dollars: CostCap::none(),
        action: BreachAction::Pause,
        effective_action: BreachAction::Pause,
        scent_check: ScentCheckMode::Off,
        rate: None,
        sources: unset_sources(),
        fallback: None,
        unpriced: None,
        fail_closed: Some(note),
    }
}

/// The non-ended Runs a Policy Selector matches: every term against BOUND
/// labels only (PRD FR-60 — a Caller label never selects a Policy), with no
/// cap, so a Policy change reaches every Run it governs.
fn policy_matched_runs(
    store: &crate::store::SqliteStore,
    selector: &Selector,
) -> Result<Vec<RunRecord>, crate::ports::StoreError> {
    use crate::ports::RunRecords;
    let candidates = store.select_runs(selector, usize::MAX)?;
    let mut matched = Vec::with_capacity(candidates.len());
    for run in candidates {
        let labels = store.run_labels(&run.run_id)?;
        if selector.matches(&labels, crate::domain::runs::SelectorScope::BoundOnly) {
            matched.push(run);
        }
    }
    Ok(matched)
}
