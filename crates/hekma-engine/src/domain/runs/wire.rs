//! Wire Runs created from the shared Metering Proxy's request path (story
//! 23-1, spine AD-26 "Run creation from the request path").
//!
//! A first call for a new identity sends [`ensure_wire_run`] to the writer
//! and awaits its commit for at most the writer's ack bound. The job inserts
//! the Run, its labels and its `run_identities` row (UNIQUE, so concurrent
//! first calls converge on one Run), resolves Policies, notes `created` and
//! publishes. Identities:
//!
//! * the unattributed Run — `proxy` / `unattributed`;
//! * a labelled binding's Run — `binding` / `<binding>`;
//! * a split Run beneath a labelled binding — `binding:<binding>` / `<hint>`;
//! * a hint child beneath a Run — `run:<parent id>` / `<hint>`.

use super::ops::{self, RunChanges};
use super::{
    labels, ControlDepth, Label, LabelOrigin, RunId, RunRecord, RunStatus,
    MAX_CALLER_OPEN_CHILDREN, MAX_RUN_NESTING,
};
use crate::ports::{NewRun, RunRecords, StoreError};
use crate::store::SqliteStore;

/// The longest external id a hint may carry (visible ASCII).
pub const MAX_EXTERNAL_ID_CHARS: usize = 128;

/// The most labels one hint may carry.
pub const MAX_HINT_LABELS: usize = 16;

/// What kind of wire Run to ensure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WireIdentity {
    /// The single unattributed Run.
    Unattributed,
    /// A labelled binding's own Run, carrying its labels.
    Binding {
        binding: String,
        labels: Vec<(String, String)>,
    },
    /// A split Run beneath a labelled binding, carrying its labels.
    Split {
        binding: String,
        external: String,
        labels: Vec<(String, String)>,
    },
    /// A hint child beneath `parent`, with Caller labels.
    Child {
        parent: RunId,
        external: String,
        caller_labels: Vec<(String, String)>,
    },
}

impl WireIdentity {
    /// The `(namespace, external id)` key in `run_identities`.
    pub(crate) fn key(&self) -> (String, String) {
        match self {
            WireIdentity::Unattributed => (
                super::bindings::UNATTRIBUTED_IDENTITY.0.to_string(),
                super::bindings::UNATTRIBUTED_IDENTITY.1.to_string(),
            ),
            WireIdentity::Binding { binding, .. } => (
                super::bindings::BINDING_NAMESPACE.to_string(),
                binding.clone(),
            ),
            WireIdentity::Split {
                binding, external, ..
            } => (super::bindings::split_namespace(binding), external.clone()),
            WireIdentity::Child {
                parent, external, ..
            } => (format!("run:{}", parent.as_str()), external.clone()),
        }
    }
}

/// What [`ensure_wire_run`] answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ensured {
    /// The Run (existing or created now).
    Run(RunId),
    /// The breadth cap is reached: the call stays on its parent (or the
    /// binding's own Run for a split).
    Capped,
    /// The parent is gone (ended): nothing was created.
    ParentGone,
}

/// Whether a hint's external id is acceptable: 1..=128 visible ASCII.
pub(crate) fn valid_external_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_EXTERNAL_ID_CHARS
        && raw.bytes().all(|b| b.is_ascii_graphic())
}

/// Parse an `x-hekma-labels` value: comma-separated percent-encoded
/// `k=v` items, at most [`MAX_HINT_LABELS`], each validated like any label.
/// `None` when any item is malformed (the hint's labels are then dropped).
pub(crate) fn parse_hint_labels(raw: &str) -> Option<Vec<(String, String)>> {
    let mut labels: Vec<(String, String)> = Vec::new();
    for item in raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let (key, value) = item.split_once('=')?;
        let key = crate::domain::credential::percent_decode(key.trim());
        let value = crate::domain::credential::percent_decode(value.trim());
        labels::validate_label_write(&key, &value).ok()?;
        if labels.iter().any(|(k, _)| *k == key) {
            return None;
        }
        labels.push((key, value));
        if labels.len() > MAX_HINT_LABELS {
            return None;
        }
    }
    labels.sort();
    Some(labels)
}

/// The writer job: return the open Run for `identity`, or create it.
pub(crate) fn ensure_wire_run(
    store: &SqliteStore,
    identity: &WireIdentity,
    run_id: RunId,
    at: &str,
) -> Result<(Ensured, RunChanges), StoreError> {
    let mut changes = RunChanges::default();
    let (namespace, external) = identity.key();
    if let Some(existing) = store.identity_run(&namespace, &external)? {
        match store.run_record(&existing)? {
            Some(record) if !record.is_ended() => return Ok((Ensured::Run(existing), changes)),
            // An ended Run keeps its identity row; a new Run takes the
            // identity (a binding re-created under the same name).
            _ => store.release_identity(&namespace, &external)?,
        }
    }
    let (parent, instance, held, labels) = match identity {
        WireIdentity::Unattributed => (None, None, false, Vec::new()),
        WireIdentity::Binding { labels, .. } => (None, None, false, bound(labels)),
        WireIdentity::Split { labels, .. } => {
            if store.open_identities_in(&namespace)? >= MAX_CALLER_OPEN_CHILDREN {
                return Ok((Ensured::Capped, changes));
            }
            (None, None, false, bound(labels))
        }
        WireIdentity::Child {
            parent,
            caller_labels,
            ..
        } => {
            let Some(parent) = store.run_record(parent)? else {
                return Ok((Ensured::ParentGone, changes));
            };
            if parent.is_ended() {
                return Ok((Ensured::ParentGone, changes));
            }
            if store.open_children(&parent.run_id)? >= MAX_CALLER_OPEN_CHILDREN
                || store.run_chain(&parent.run_id)?.len() >= MAX_RUN_NESTING
            {
                return Ok((Ensured::Capped, changes));
            }
            let caller: Vec<Label> = caller_labels
                .iter()
                .map(|(k, v)| Label::caller(k.clone(), v.clone()))
                .collect();
            // The Caller guard (FR-49) — vacuous for a fresh Run.
            if labels::check_caller_labels(&run_id, &[], &caller).is_err() {
                return Ok((Ensured::Capped, changes));
            }
            let held = parent.status == RunStatus::Held;
            (Some(parent.run_id), parent.instance, held, caller)
        }
    };
    let record = RunRecord {
        run_id: run_id.clone(),
        control_depth: ControlDepth::Wire,
        wire_path: true,
        status: if held {
            RunStatus::Held
        } else {
            RunStatus::Active
        },
        anomalies: Vec::new(),
        parent_run_id: parent,
        instance,
        process_owner: false,
        end_cause: None,
        created_at: at.to_string(),
        updated_at: at.to_string(),
        ended_at: None,
    };
    store.insert_run(&NewRun {
        record: record.clone(),
        labels,
        limits: super::RunLimits::default(),
        identity: Some((namespace, external)),
    })?;
    ops::note_created(
        store,
        &mut changes,
        &record,
        held.then(|| "parent-paused".to_string()),
    )?;
    changes.extend(super::policy::resolve_run(store, &run_id, None, at)?);
    // The created note ran before the resolution; the freshest entry (its
    // resolution and identity) publishes last.
    if let Some(entry) = ops::refreshed_entry(store, &run_id)? {
        changes.entries.push(entry);
    }
    Ok((Ensured::Run(run_id), changes))
}

fn bound(labels: &[(String, String)]) -> Vec<Label> {
    let mut out: Vec<Label> = labels
        .iter()
        .map(|(k, v)| Label::bound_by(k.clone(), v.clone(), LabelOrigin::Binding))
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_ids_and_hint_labels_are_bounded_and_validated() {
        assert!(valid_external_id("job-7"));
        assert!(!valid_external_id(""));
        assert!(!valid_external_id("a b"));
        assert!(!valid_external_id(&"x".repeat(129)));
        assert!(valid_external_id(&"x".repeat(128)));
        assert_eq!(
            parse_hint_labels("team=core, step=a%20b"),
            None,
            "a space in a value is not a valid label value"
        );
        assert_eq!(
            parse_hint_labels("step=two,team=core"),
            Some(vec![
                ("step".to_string(), "two".to_string()),
                ("team".to_string(), "core".to_string())
            ])
        );
        assert_eq!(parse_hint_labels(""), Some(Vec::new()));
        assert_eq!(parse_hint_labels("team"), None);
        assert_eq!(parse_hint_labels("a=1,a=2"), None);
        let many: Vec<String> = (0..17).map(|i| format!("k{i}=v")).collect();
        assert_eq!(parse_hint_labels(&many.join(",")), None);
        let sixteen: Vec<String> = (0..16).map(|i| format!("k{i}=v")).collect();
        assert_eq!(parse_hint_labels(&sixteen.join(",")).unwrap().len(), 16);
    }

    #[test]
    fn identity_keys_name_their_namespace() {
        assert_eq!(
            WireIdentity::Unattributed.key(),
            ("proxy".to_string(), "unattributed".to_string())
        );
        assert_eq!(
            WireIdentity::Binding {
                binding: "openai".into(),
                labels: vec![]
            }
            .key(),
            ("binding".to_string(), "openai".to_string())
        );
        assert_eq!(
            WireIdentity::Split {
                binding: "openai".into(),
                external: "job".into(),
                labels: vec![]
            }
            .key(),
            ("binding:openai".to_string(), "job".to_string())
        );
        assert_eq!(
            WireIdentity::Child {
                parent: RunId::from_wire("run_P"),
                external: "job-7".into(),
                caller_labels: vec![]
            }
            .key(),
            ("run:run_P".to_string(), "job-7".to_string())
        );
    }
}
