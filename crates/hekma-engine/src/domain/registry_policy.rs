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
    self, BreachDefaultUpgradedEvent, PolicyEvent, PolicyEventKind, PolicyLimits, PolicyRecord,
    ResolveInputs, UpgradedInstance, DEFAULT_POLICY_NAME, POLICY_EVENT_SCHEMA_VERSION,
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
    /// Whether THIS engine supervises the instance right now: only a
    /// supervised instance's money half runs (the breach-at-once latch
    /// must be executable by the engine that records it); an unsupervised
    /// instance still gets FRESH inputs for its stored resolution.
    pub supervised: bool,
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
    /// without its `default` row — DEGRADES the cache while KEEPING the
    /// last good set for enforcement: every resolution then carries the
    /// arms that set could resolve with the action forced to `pause`
    /// (AD-7; the pre-fix empty cache silently resolved the seeded
    /// `warn`, and an armless fail-close turned enforcement off — the
    /// 21-3 review rounds). Returns the degradation note when the cache
    /// is degraded (the OPEN path surfaces it as an open note; the tick
    /// path hands it to the supervisor's diagnostic surface).
    pub(crate) fn load_policy_cache(&self) -> Option<String> {
        match self.store.snapshot(|store| store.policies()) {
            Ok(policies) => {
                if policies.iter().any(|policy| policy.is_default()) {
                    self.policies.publish(policies);
                    None
                } else {
                    self.policies.degrade(
                        "the default Policy row is missing from the stored set".to_string(),
                    );
                    Some(
                        "the stored Policies carry no `default` row, so every limit resolves \
                         fail-closed to pause (the last good set still arms its ceilings) \
                         until one is restored"
                            .to_string(),
                    )
                }
            }
            Err(err) => {
                self.policies
                    .degrade(format!("the Policies could not be read ({err})"));
                Some(format!(
                    "the Policies could not be read into the cached set ({err}); limits resolve \
                     fail closed to pause (the last good set still arms its ceilings) until the \
                     read succeeds"
                ))
            }
        }
    }

    /// Re-read the policy set when the policy table moved underneath this
    /// process (story 21-3, the cross-process staleness check): the
    /// table's fingerprint (every row's name and update time) changes on
    /// ANY committed Policy change — this engine's own writer's (a
    /// different connection, so its commits move the read-only
    /// connection's counters too; harmless, one redundant reload) or
    /// another engine's — so one tiny aggregate read per tick (never per
    /// event) picks a second engine's change up within a tick. A failed
    /// reload degrades the cache exactly like a failed open load BUT is
    /// retried on the next tick (the fingerprint is stamped only after a
    /// SUCCESSFUL load — a transient read error once stuck the whole
    /// fleet at forced `pause` until a restart, the 21-3 review round 3
    /// finding M2); the returned note is the caller's to surface (the
    /// supervisor emits it ONCE per continuous degraded state, re-armed
    /// on recovery).
    pub(crate) fn refresh_policy_cache(&self) -> PolicyRefresh {
        let fingerprint = match self.store.policies_fingerprint() {
            Ok(fingerprint) => fingerprint,
            Err(err) => {
                // The check itself failed: treat it as a reload failure
                // (degrade, keep the last good set) rather than silently
                // standing still. The unstamped fingerprint makes the next
                // tick retry.
                self.policies
                    .degrade(format!("the policy table could not be read ({err})"));
                return PolicyRefresh {
                    reloaded: false,
                    degraded: Some(format!(
                        "the policy table could not be read ({err}); limits resolve fail closed \
                         to pause (the last good set still arms its ceilings) until the read \
                         succeeds"
                    )),
                };
            }
        };
        // A healthy, unchanged table costs nothing further. A DEGRADED
        // cache always retries: whatever failed (a transient read error)
        // may have healed without moving the fingerprint, and one failed
        // load must never hold the fleet at forced `pause` (the 21-3
        // review round 3 finding M2).
        if !self.policies.stale_at(&fingerprint) && self.policies.current().is_ok() {
            return PolicyRefresh::default();
        }
        match self.load_policy_cache() {
            None => {
                // A successful load: stamp the baseline so an unchanged
                // table costs nothing, and tell the caller the set moved
                // (its latched breaches and fences need a re-check).
                self.policies.stamp(fingerprint);
                PolicyRefresh {
                    reloaded: true,
                    degraded: None,
                }
            }
            Some(note) => PolicyRefresh {
                reloaded: false,
                degraded: Some(note),
            },
        }
    }

    /// Whether the policy cache is currently degraded (the supervisor's
    /// once-per-state diagnostic re-arms on a healthy tick).
    pub(crate) fn policy_cache_degraded(&self) -> bool {
        self.policies.current().is_err()
    }

    pub(crate) fn push_open_note(&self, note: String) {
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
    /// `None` (an instance-shaped read: the Fleet and status surfaces —
    /// then the open process-owning Run's facts are used when one exists,
    /// else the labels the instance's configuration BINDS, so a fresh
    /// start's fence and the Fleet see the Policies the next Run will
    /// carry) or have no row (same fallback). This is the LIVE enforcement
    /// read (`eval_input`) and the surface read — one function, never
    /// re-implemented.
    ///
    /// **Fail closed WITHOUT disarming (AD-7; 21-3 review round 2):** a
    /// failed facts, kept-marker or policy-cache read never returns an
    /// armless resolution — that once turned every budget in the fleet
    /// off, explicit instance ceilings included. Instead the resolution
    /// keeps every arm it CAN still resolve (the explicit Run limits, the
    /// explicit instance config, the Rate, and the LAST GOOD policy set
    /// when the cache is degraded), forces the action to `pause`, and
    /// carries a fail-closed note naming the reason.
    pub(crate) fn resolved_limits(
        &self,
        name: &InstanceName,
        run: Option<&RunId>,
    ) -> PolicyResolution {
        let mut fail_closed = None;
        // The Run facts: the named Run's, else the instance's open
        // process-owning Run's, else none (a fresh start before its Run
        // row exists).
        let mut facts = None;
        if let Some(run_id) = run {
            match self
                .store
                .snapshot(|store| store.run_resolution_facts(run_id))
            {
                Ok(Some(read)) => facts = Some(read),
                Ok(None) => {}
                Err(err) => {
                    fail_closed = Some(format!(
                        "this Run's resolution facts could not be read ({err}), so its limits \
                         fail closed to pause until the read succeeds"
                    ))
                }
            }
        }
        if facts.is_none() && fail_closed.is_none() {
            match self.store.snapshot(|store| store.open_process_run(name)) {
                Ok(Some(owner)) => match self
                    .store
                    .snapshot(|store| store.run_resolution_facts(&owner.run_id))
                {
                    Ok(Some(read)) => facts = Some(read),
                    Ok(None) => {}
                    Err(err) => {
                        fail_closed = Some(format!(
                            "this Run's resolution facts could not be read ({err}), so its \
                             limits fail closed to pause until the read succeeds"
                        ))
                    }
                },
                Ok(None) => {}
                Err(err) => {
                    fail_closed = Some(format!(
                        "the open Run could not be read ({err}), so the limits fail closed to \
                         pause until the read succeeds"
                    ))
                }
            }
        }
        let kept_instance = facts
            .as_ref()
            .map(|facts| facts.record.instance.clone())
            .unwrap_or_else(|| Some(name.as_str().to_string()));
        let mut kept = false;
        if let Some(instance) = kept_instance.as_deref() {
            match self
                .store
                .snapshot(|store| store.breach_default_kept(instance))
            {
                Ok(read) => kept = read,
                // A failed kept-marker read keeps `pause` (fail closed):
                // the marker is the FR-21 promise's carrier.
                Err(err) => {
                    fail_closed = Some(format!(
                        "the kept-on-pause marker could not be read ({err}), so the breach \
                         action fails closed to pause until the read succeeds"
                    ))
                }
            }
        }
        let (inputs, config_labels) = match self.effective_config(name, ConfigLayer::empty()) {
            Ok(effective) => {
                let labels = crate::domain::runs::labels::bound_labels_from(&effective).0;
                (config::explicit_limits(&effective), labels)
            }
            Err(_) => (
                ResolveInputs {
                    malformed_action: Some("the configuration could not be read".to_string()),
                    ..ResolveInputs::default()
                },
                Vec::new(),
            ),
        };
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
                config_labels
                    .iter()
                    .filter(|label| label.is_bound())
                    .map(|label| (label.key.clone(), label.value.clone()))
                    .collect(),
            ),
        };
        let (policies, cache_note) = self.policies.current_or_last_good();
        if let Some(reason) = cache_note {
            fail_closed = Some(format!(
                "the Policies could not be read into the cached set ({reason}), so the limits \
                 fail closed to pause — the last good set still arms every ceiling it could — \
                 until a Policy change reloads them"
            ));
        } else if !policies.iter().any(|policy| policy.is_default()) {
            // The `default` row is the fallback layer for every limit; a
            // set without it must not silently resolve the seeded `warn`.
            fail_closed = Some(
                "the default Policy row is missing from the cached set, so the limits fail \
                 closed to pause until it is restored"
                    .to_string(),
            );
        }
        let mut resolution = policy::resolve(&policy::ResolutionInput {
            depth,
            wire_path,
            bound_labels,
            run_limits,
            inputs,
            caller_limits: PolicyLimits::default(),
            kept_on_pause: kept,
            policies: policies.as_ref().clone(),
        });
        if let Some(note) = fail_closed {
            // Fail closed WITHOUT disarming and WITHOUT loosening: every
            // arm the reads could still resolve stays enforced, and the
            // action only TIGHTENS — `pause` at minimum, but an already
            // tighter explicit `stop` or `block` stays (an explicit
            // `block` keeps its Run-start fence through a degraded read;
            // the round-3 L2 fix replaced an unconditional pause that
            // quietly downgraded it). The source label stays honest.
            let tighten = |action: BreachAction| {
                if crate::domain::runs::policy::action_tightness(action)
                    < crate::domain::runs::policy::action_tightness(BreachAction::Pause)
                {
                    BreachAction::Pause
                } else {
                    action
                }
            };
            resolution.action = tighten(resolution.action);
            resolution.effective_action = tighten(resolution.effective_action);
            resolution.fail_closed = Some(note);
        }
        resolution
    }

    /// Every Policy, ordered by name (the read-only connection).
    pub(crate) fn policies(&self) -> Result<Vec<PolicyRecord>, StoreError> {
        self.store.snapshot(|store| store.policies())
    }

    /// One Policy by name.
    pub(crate) fn policy(&self, name: &str) -> Result<Option<PolicyRecord>, StoreError> {
        self.store.snapshot(move |store| store.policy(name))
    }

    /// The RAW stored Selector text of one Policy, undecoded (`None` when
    /// no row has the name) — the corrupt-row delete path's read.
    pub(crate) fn policy_selector_raw(
        &self,
        name: &str,
    ) -> Result<Option<Option<String>>, StoreError> {
        self.store
            .snapshot(move |store| store.policy_selector_raw(name))
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
        // A Rate with a $0 direction is refused on every surface: a
        // half-priced or free model would understate cost and pass every
        // Cost Cap (the instance rule — both directions or no Rate —
        // applied to the Policy writes; the API body makes a missing
        // direction a 422, the engine makes a zero one a refusal).
        let rate_shape = |rate: &Rate| {
            rate.input_micros_per_1m != 0
                && rate.output_micros_per_1m != 0
                // A $0 CACHED direction prices cached tokens at nothing
                // (understating cost the same way a $0 direction does);
                // unset keeps the conservative input-rate default.
                && rate.cached_micros_per_1m != Some(0)
        };
        if let Some(rate) = change.set.rate {
            if !rate_shape(&rate) {
                return Err(invalid(
                    "rate",
                    "a Rate needs both directions priced above zero (and a cached \
                     direction, when set, above zero too) — a $0 direction understates cost \
                     and passes every Cost Cap"
                        .to_string(),
                ));
            }
        }
        for (model, rate) in &change.rates {
            if let Some(rate) = rate {
                if !rate_shape(rate) {
                    return Err(invalid(
                        "rates",
                        format!(
                            "the Rate for model `{model}` needs both directions priced above \
                             zero (and a cached direction, when set, above zero too) — a $0 \
                             direction understates cost and passes every Cost Cap"
                        ),
                    ));
                }
            }
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
    /// matched set and the fallback count. Writes nothing. The counts are
    /// the REAL change's: when the Selector MOVES, the matched set is old
    /// ∪ new (the Runs only the old Selector matched loosen too — the
    /// committed change re-resolves exactly those).
    pub(crate) fn plan_policy_change(
        &self,
        existing: Option<&PolicyRecord>,
        change: &PolicyChange,
    ) -> Result<PolicyOutcome, RunError> {
        Self::validate_change(existing, change)?;
        let record = merge(existing, change);
        let prior_selector = existing
            .filter(|prior| prior.name == record.name)
            .and_then(|prior| prior.selector.clone());
        let (matched, fallbacks) = self.matched_fallbacks(&record, prior_selector.as_ref())?;
        Ok(PolicyOutcome {
            record: Some(record),
            matched,
            fallbacks,
            notes: Vec::new(),
        })
    }

    /// Plan a DELETE (the dry run): the Runs the deleted Selector matches
    /// under the set WITHOUT the Policy (the loosened set the real delete
    /// resolves against), so the fallback count answers what the delete
    /// leaves behind — not what the still-present Policy produces.
    pub(crate) fn plan_policy_delete(
        &self,
        existing: &PolicyRecord,
    ) -> Result<PolicyOutcome, RunError> {
        let (set, _degraded) = self.policies.current_or_last_good();
        let mut policies = set.as_ref().clone();
        policies.retain(|policy| policy.name != existing.name);
        let runs = self.matched_runs(existing)?;
        let mut fallbacks = 0;
        for run in &runs {
            let resolution = self.resolve_record(run, &policies);
            if resolution.fallback.is_some() {
                fallbacks += 1;
            }
        }
        Ok(PolicyOutcome {
            record: None,
            matched: runs.len(),
            fallbacks,
            notes: Vec::new(),
        })
    }

    /// The matched-set and fallback counts for `record` (reads only). The
    /// `default` Policy matches every non-ended Run; a Selector CHANGE
    /// matches old ∪ new (`prior_selector`, the real change's set). The
    /// fallback count resolves against the PLANNED set (the cache with
    /// `record` merged in), so the dry run reports what the real change
    /// would produce — the pre-change cache under-counted (the 21-3
    /// review round 1 finding).
    fn matched_fallbacks(
        &self,
        record: &PolicyRecord,
        prior_selector: Option<&Selector>,
    ) -> Result<(usize, usize), RunError> {
        let policies = self.planned_policy_set(record)?;
        let mut runs = self.matched_runs(record)?;
        if let Some(prior) = prior_selector {
            if Some(prior) != record.selector.as_ref() {
                let prior = prior.clone();
                for run in self
                    .store
                    .snapshot(move |store| policy_matched_runs(store, &prior))
                    .map_err(RunError::Store)?
                {
                    if !runs.iter().any(|other| other.run_id == run.run_id) {
                        runs.push(run);
                    }
                }
            }
        }
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
    /// Plans against the LAST GOOD set while the cache is degraded (a
    /// degraded cache once refused every create and change, so one
    /// transient read error locked the Operator out of the very writes
    /// that heal it — `create default` above all; the 21-3 review round 3
    /// finding M2). The change's own commit publishes a freshly-read set,
    /// which clears the degradation.
    fn planned_policy_set(&self, record: &PolicyRecord) -> Result<Vec<PolicyRecord>, RunError> {
        let (set, _degraded) = self.policies.current_or_last_good();
        let mut policies = set.as_ref().clone();
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
                // The existence check runs INSIDE the job (round 3, L9):
                // the caller's read is only a plan. A CREATE over a name
                // another engine created meanwhile is a typed
                // `policy-exists` (not a silent last-write-wins), and a
                // CHANGE over a row deleted meanwhile is `not-found`. A
                // change re-merges over the FRESH row, so a concurrent
                // edit to another field is kept rather than overwritten.
                let fresh = store.policy(&change.name).map_err(RunError::Store)?;
                let record = match (&existing, fresh) {
                    (None, Some(_)) => {
                        return Err(RunError::PolicyExists {
                            name: change.name.clone(),
                        })
                    }
                    (Some(_), None) => {
                        return Err(RunError::PolicyNotFound {
                            name: change.name.clone(),
                        })
                    }
                    (Some(fresh), Some(_)) => merge(Some(fresh), &change),
                    (None, None) => record.clone(),
                };
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
    /// re-resolve its matched Runs under the loosened set. `prior` is the
    /// decoded row when it could be read (its limits ride the event);
    /// `raw_selector` is the RAW stored Selector text, so a CORRUPT row —
    /// one whose Selector no longer parses, which fails every decoded
    /// read — is still deletable by name (the remediation; money code
    /// never wedges the operator out of the loop). When the raw Selector
    /// cannot be parsed the matched set is EVERY non-ended Run (honest:
    /// the row's scope is unknowable, so everything re-resolves) with a
    /// note naming it.
    pub(crate) fn apply_policy_delete(
        &self,
        name: &str,
        prior: Option<PolicyRecord>,
        raw_selector: Option<String>,
        contexts: HashMap<String, InstanceContext>,
    ) -> Result<PolicyApplied, RunError> {
        if name == DEFAULT_POLICY_NAME {
            return Err(RunError::PolicyDefault {
                what: "cannot be deleted — change it instead",
            });
        }
        // The matched set: the decoded prior's Selector when there is one,
        // else the raw text; unparseable raw text re-resolves everything.
        let matched_selector = prior
            .as_ref()
            .and_then(|prior| prior.selector.clone())
            .or_else(|| {
                raw_selector
                    .as_deref()
                    .and_then(|raw| Selector::parse(raw).ok())
            });
        let publisher = Arc::clone(&self.run_snapshot);
        let cache = Arc::clone(&self.policies);
        let name = name.to_string();
        // The event's Selector: the decoded prior's when there is one; a
        // corrupt row's raw text cannot become a Selector, so the event
        // carries none and the note below names the scope honestly.
        let event_selector = prior.as_ref().and_then(|prior| prior.selector.clone());
        let raw_as_text = raw_selector
            .as_deref()
            .map(|raw| format!("`{raw}`"))
            .unwrap_or_else(|| "NULL".to_string());
        let unreadable_scope = prior.is_none() && matched_selector.is_none();
        let result = self.writer.call(move |ctx| {
            let store = ctx.store();
            let body = || -> Result<_, RunError> {
                // Delete FIRST: a corrupt row must be gone before the
                // policy-set read inside the re-resolution runs.
                store.delete_policy(&name).map_err(RunError::Store)?;
                let (outcome, run_events, breaches) = match &matched_selector {
                    // The matched set is the DELETED selector's (the Runs
                    // it selected now resolve looser).
                    Some(selector) => {
                        let selectors = vec![selector.clone()];
                        re_resolve_matched(
                            store,
                            ctx,
                            SelectorSet::Matching(&selectors),
                            &contexts,
                        )?
                    }
                    None => re_resolve_matched(store, ctx, SelectorSet::EveryRun, &contexts)?,
                };
                let mut outcome = outcome;
                if unreadable_scope {
                    outcome.notes.push(format!(
                        "the deleted Policy `{name}` had an unreadable Selector (stored as \
                         {raw_as_text}), so EVERY non-ended Run re-resolved — its scope could \
                         not be read",
                    ));
                }
                let event = PolicyEvent {
                    schema_version: POLICY_EVENT_SCHEMA_VERSION,
                    kind: PolicyEventKind::Deleted,
                    name: name.clone(),
                    selector: event_selector,
                    limits: None,
                    prior_limits: prior.as_ref().map(|prior| prior.limits.clone()),
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
                // An undecodable name can never be written or resolved:
                // mark it DONE with `config_written = false` (the open
                // note names it) so it does not wedge the pending list —
                // every later open once emitted an empty event for it.
                written.push((name.clone(), false));
                self.push_open_note(format!(
                    "the kept-on-pause upgrade list carries `{name}`, which is not a valid \
                     agent name; its row is closed unwritten and its instance resolves fail \
                     closed (no config write was possible)"
                ));
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
        // Stage 3, cross-open idempotent: the job claims only rows still
        // pending and the EVENT fires for exactly the rows THIS job
        // finished — two engines that both read the same pending list
        // emit exactly one event between them, and a job that finished
        // nothing emits none.
        let at = now_rfc3339();
        let rows = written.clone();
        let result = self.writer.call(move |ctx| {
            let store = ctx.store();
            let finished = store.finish_breach_default(&rows, &at)?;
            if finished.is_empty() {
                return Ok(None);
            }
            let event = BreachDefaultUpgradedEvent {
                schema_version: POLICY_EVENT_SCHEMA_VERSION,
                instances: rows
                    .iter()
                    .filter(|(name, _)| finished.contains(name))
                    .map(|(name, config_written)| UpgradedInstance {
                        name: name.clone(),
                        config_written: *config_written,
                    })
                    .collect(),
                at: at.clone(),
            };
            let append = store.append_outbox(
                &EngineEvent::BreachDefaultUpgraded(event.clone()),
                OUTBOX_MAX_BYTES,
            )?;
            Ok(Some((event, append)))
        });
        match result {
            Ok(Some((event, _append))) => {
                self.note_open_event(EngineEvent::BreachDefaultUpgraded(event));
            }
            Ok(None) => {
                // Another open finished the list first (the concurrent
                // case): nothing to emit, and the pending read on the
                // next open is empty.
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
                // The fail-closed read (round 3, L6): an unreadable config
                // carries its reason as the malformed note (action pause)
                // instead of silently resolving as if nothing were set —
                // the stored row feeds child levels and change-job arms.
                inputs_by_instance
                    .entry(instance.clone())
                    .or_insert_with(|| {
                        InstanceName::new(instance)
                            .map(|name| self.resolve_inputs_fail_closed(&name))
                            .unwrap_or_default()
                    });
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

/// What one reaper-tick policy refresh found (story 21-3): whether the
/// set RELOADED (the caller re-checks its latched breaches and fences —
/// another engine's change may have unlatched a Run or lowered a ceiling
/// this engine supervises) and the degradation note when the cache is
/// degraded (the last good set stays armed with the action forced to
/// `pause`; a failed load retries on the next tick).
#[derive(Debug, Default)]
pub(crate) struct PolicyRefresh {
    pub reloaded: bool,
    pub degraded: Option<String>,
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

        // The money half. The RE-ARM half (a raise, clear or delete above
        // the totals: unlatch, clear the `breach` Anomaly) runs for EVERY
        // matched instance — it is a shared-DB write any engine may make,
        // and skipping it once left a raised ceiling latched forever, so
        // the next crossing never fired and spend ran unbounded (the 21-3
        // review round 3 finding M1). Only the BREACH-AT-ONCE half (and
        // the in-memory fence clear) needs this engine to supervise the
        // instance: the latch must be executable by the engine that
        // records it. A change by an engine that does NOT supervise the
        // agent leaves the lowering to the supervising engine's next
        // evaluation (its tick reload re-checks), with a note.
        if !run.is_ended() {
            if let Some(instance) = &run.instance {
                let supervised = contexts
                    .get(instance)
                    .is_some_and(|context| context.supervised);
                if !supervised {
                    notes.push(format!(
                        "Run `{}` (agent `{instance}`) was not breach-checked here: this engine \
                         does not supervise the agent. A raise or clear still re-arms its latch \
                         (done here); a lowering waits for the supervising engine's next \
                         evaluation",
                        run.run_id.as_str(),
                    ));
                }
                let context = contexts.get(instance).cloned().unwrap_or_default();
                if let Ok(name) = InstanceName::new(instance) {
                    re_arm(
                        store,
                        &name,
                        run,
                        &resolution,
                        &context,
                        supervised,
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
    supervised: bool,
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
    // A CHILD level enforces only its Run- and Policy-sourced PER-RUN arms
    // (the instance's ceilings arm the process level alone — see
    // `pipeline::levels_for`), so a child's latch compares against the
    // ceiling its OWN level would enforce today. An instance-sourced
    // ceiling that the child level never arms must re-arm the latch it
    // once held (a Policy armed the child at 50, latched at 60, and the
    // Policy's deletion left an instance-sourced 40: the latch once stuck
    // forever, because `40 > 60` is false — the 21-3 review round 2
    // finding N14).
    let level_sourced = |source: &crate::domain::runs::LimitSource| {
        !matches!(source, crate::domain::runs::LimitSource::Instance)
    };
    let token_per_run = if run.process_owner {
        resolution.tokens.per_run
    } else {
        level_sourced(&resolution.sources.tokens_per_run)
            .then_some(resolution.tokens.per_run)
            .flatten()
    };
    let dollar_per_run = if run.process_owner {
        resolution.dollars.per_run.map(|micros| micros.get() as u64)
    } else {
        level_sourced(&resolution.sources.dollars_per_run)
            .then(|| resolution.dollars.per_run.map(|micros| micros.get() as u64))
            .flatten()
    };
    let mut re_armed: Vec<crate::domain::LatchKey> = Vec::new();
    for (key, ceiling, total_now) in [
        (
            crate::domain::LatchKey::new(
                crate::domain::BreachDimension::Tokens,
                crate::domain::BreachScope::PerRun,
            ),
            token_per_run,
            tokens_now,
        ),
        (
            crate::domain::LatchKey::new(
                crate::domain::BreachDimension::Tokens,
                crate::domain::BreachScope::Cumulative,
            ),
            if run.process_owner {
                resolution.tokens.cumulative
            } else {
                None
            },
            instance_tokens,
        ),
        (
            crate::domain::LatchKey::new(
                crate::domain::BreachDimension::Dollars,
                crate::domain::BreachScope::PerRun,
            ),
            dollar_per_run,
            dollars_now,
        ),
        (
            crate::domain::LatchKey::new(
                crate::domain::BreachDimension::Dollars,
                crate::domain::BreachScope::Cumulative,
            ),
            if run.process_owner {
                resolution
                    .dollars
                    .cumulative
                    .map(|micros| micros.get() as u64)
            } else {
                None
            },
            instance_dollars,
        ),
    ] {
        // A ceiling that became UNSET (or one this level no longer
        // enforces) re-arms too: `--clear` and delete loosen, and a
        // latched breach under a ceiling that no longer exists would hold
        // the Run (and a `block` fence) forever.
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

    // ---- the breach-at-once half (lowered to at-or-below the usage),
    // ONLY on an instance this engine supervises (the latch must be
    // executable by the engine that records it):
    // the same pipeline evaluation a usage commit runs, over the NEW arms.
    // The process level's arms are the instance's (the process-owning
    // Run's resolution — the matched Run's own when it IS the owner; the
    // owner's STORED resolution when the match is a child, whose own arms
    // ride its own level inside the evaluation).
    if !supervised {
        return Ok(());
    }
    let owner_arms = if run.process_owner {
        resolution.clone()
    } else {
        let owner_row = match store.open_process_run(instance).map_err(RunError::Store)? {
            Some(owner) => owner,
            None => {
                notes.push(format!(
                    "Run `{}` (agent `{}`) was not breach-checked: its agent has no open \
                     process-owning Run to arm the process level",
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
                    "Run `{}` (agent `{}`) was not breach-checked: its agent's process-owning \
                     Run has no stored resolution to arm the process level",
                    run.run_id.as_str(),
                    instance.as_str(),
                ));
                return Ok(());
            }
        }
    };
    let armed = supervised
        && (owner_arms.tokens.is_set()
            || (owner_arms.rate.is_some() && owner_arms.dollars.is_set())
            || resolution.tokens.per_run.is_some()
            || (resolution.rate.is_some() && resolution.dollars.per_run.is_some()));
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
