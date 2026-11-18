//! A Run's telemetry context (story 24-5, AD-14's amendment): what the
//! `otlp-metrics v2` exposition groups by — the Run's Control Depth, the
//! Policies that supplied its limits, and its labels — read in the same
//! transaction that commits a usage row or a breach, and stamped onto the
//! event as additive fields. The exporter reads it from the event, never
//! from a join of its own (which would go blind for Runs alive before it
//! started).

use super::policy::{LimitSource, LimitSources};
use super::{ControlDepth, RunId};
use crate::domain::budget::BreachScope;
use crate::domain::event::BreachDimension;
use crate::ports::{PolicyRecords, RunRecords, StoreError};

/// One Run's context at commit time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunContext {
    pub control_depth: Option<ControlDepth>,
    /// Every Policy that supplied a limit, sorted, deduplicated.
    pub policies: Vec<String>,
    /// The Run's labels, `(key, value)` sorted by key.
    pub labels: Vec<(String, String)>,
    /// The stored resolution's per-limit sources, when resolved.
    pub sources: Option<LimitSources>,
}

impl RunContext {
    /// Read `run_id`'s context; a Run with no row reads as empty (the
    /// event then carries no context, as an older event does).
    pub(crate) fn read<S: RunRecords + PolicyRecords>(
        store: &S,
        run_id: &RunId,
    ) -> Result<Self, StoreError> {
        let Some(record) = store.run_record(run_id)? else {
            return Ok(RunContext::default());
        };
        let mut labels: Vec<(String, String)> = store
            .run_labels(run_id)?
            .into_iter()
            .map(|label| (label.key, label.value))
            .collect();
        labels.sort();
        let sources = store
            .run_policy(run_id)?
            .map(|stored| stored.resolution.sources);
        let mut policies = sources
            .as_ref()
            .map(LimitSources::policy_names)
            .unwrap_or_default();
        policies.sort();
        policies.dedup();
        Ok(RunContext {
            control_depth: Some(record.control_depth),
            policies,
            labels,
            sources,
        })
    }

    /// What supplied the limit a breach on `dimension`/`scope` crossed: the
    /// Policy's name, `default`, or `explicit` (a limit set on the Run, its
    /// Agent Instance, or by a Caller) — never a fabricated Policy.
    pub fn supplier(&self, dimension: BreachDimension, scope: BreachScope) -> String {
        let Some(sources) = &self.sources else {
            return "explicit".to_string();
        };
        let source = match (dimension, scope) {
            (BreachDimension::Tokens, BreachScope::PerRun) => &sources.tokens_per_run,
            (BreachDimension::Tokens, BreachScope::Cumulative) => &sources.tokens_cumulative,
            (BreachDimension::Dollars, BreachScope::PerRun) => &sources.dollars_per_run,
            (BreachDimension::Dollars, BreachScope::Cumulative) => &sources.dollars_cumulative,
        };
        match source {
            LimitSource::Policy(name) => name.clone(),
            LimitSource::Default => "default".to_string(),
            LimitSource::Run | LimitSource::Instance | LimitSource::Caller | LimitSource::Unset => {
                "explicit".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_breach_names_the_policy_that_supplied_its_limit_or_explicit_or_default() {
        let sources = LimitSources {
            tokens_per_run: LimitSource::Policy("prod".into()),
            tokens_cumulative: LimitSource::Default,
            dollars_per_run: LimitSource::Run,
            dollars_cumulative: LimitSource::Caller,
            breach_action: LimitSource::Default,
            scent_check: LimitSource::Unset,
            rate: LimitSource::Instance,
        };
        let context = RunContext {
            sources: Some(sources),
            ..RunContext::default()
        };
        assert_eq!(
            context.supplier(BreachDimension::Tokens, BreachScope::PerRun),
            "prod"
        );
        assert_eq!(
            context.supplier(BreachDimension::Tokens, BreachScope::Cumulative),
            "default"
        );
        assert_eq!(
            context.supplier(BreachDimension::Dollars, BreachScope::PerRun),
            "explicit"
        );
        assert_eq!(
            context.supplier(BreachDimension::Dollars, BreachScope::Cumulative),
            "explicit"
        );
        assert_eq!(
            RunContext::default().supplier(BreachDimension::Tokens, BreachScope::PerRun),
            "explicit"
        );
    }
}
