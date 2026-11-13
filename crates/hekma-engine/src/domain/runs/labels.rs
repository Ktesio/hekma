//! The label grammar and the instance `labels` config key (story 21-2, D3).
//!
//! Bound labels for a supervised Run come from the instance `labels` config
//! key: `labels.<key> = "<value>"` in any layer, merged per key (the config
//! resolver's per-leaf merge, higher layer wins). The invocation override
//! layer never binds a label (AD-25: an invocation override cannot bind
//! labels), so [`bound_labels_from`] reads only leaves supplied by the
//! engine-default, kind-default and instance layers.
//!
//! Story 21-9 adds the label verb's pure planners: [`plan_label_change`]
//! (the Operator's set/remove on one Run) and [`plan_config_sync`] (a
//! configuration re-sync), both returning the Run's next full label set, and
//! [`check_caller_labels`], the guard every Caller-label write calls. Per key
//! the precedence is `label-verb` > `config` > Caller.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Label, LabelOrigin, RunError};
use crate::domain::config::{EffectiveConfig, SourceLayer};
use crate::domain::usage::RunId;

/// The config namespace bound labels live under.
pub(crate) const LABELS_PREFIX: &str = "labels.";

/// The longest label value accepted.
const VALUE_MAX: usize = 128;

/// Validate a label key: 1 to 63 characters, a lowercase letter first, then
/// lowercase letters, digits, `_`, `.` or `-`.
pub fn validate_label_key(key: &str) -> Result<(), String> {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        Some(_) => {
            return Err(format!(
                "label key `{key}` must start with a lowercase letter"
            ))
        }
        None => return Err("a label key cannot be empty".to_string()),
    }
    if key.len() > 63 {
        return Err(format!("label key `{key}` is longer than 63 characters"));
    }
    if let Some(bad) = chars
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-')))
    {
        return Err(format!(
            "label key `{key}` contains `{bad}` (allowed: a-z, 0-9, `_`, `.`, `-`)"
        ));
    }
    Ok(())
}

/// Validate a label value: 1 to 128 of ASCII letters, digits and
/// `_ . : / @ + -`, never a `secret:` reference (a label is shown on every
/// surface and exported, so it can never carry a secret).
pub fn validate_label_value(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("a label value cannot be empty".to_string());
    }
    if value.len() > VALUE_MAX {
        return Err(format!("label value is longer than {VALUE_MAX} characters"));
    }
    if value.starts_with(crate::domain::config::SECRET_PREFIX) {
        return Err("a label value cannot be a `secret:` reference".to_string());
    }
    if let Some(bad) = value.chars().find(|c| {
        !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '/' | '@' | '+' | '-'))
    }) {
        return Err(format!(
            "label value `{value}` contains `{bad}` (allowed: letters, digits, `_ . : / @ + -`)"
        ));
    }
    Ok(())
}

/// Validate a `labels.<key>` config write: the key segment and the value.
pub(crate) fn validate_label_write(key: &str, value: &str) -> Result<(), String> {
    let label_key = key.strip_prefix(LABELS_PREFIX).unwrap_or(key);
    validate_label_key(label_key)?;
    // The raw value: it is stored verbatim, and the resolver validates the
    // same bytes (a padded value would bind nothing).
    validate_label_value(value)
}

/// The bound labels an instance's effective config gives its supervised
/// Runs, sorted by key. Callers resolve with an empty invocation layer; as a
/// guard, any leaf the invocation override layer supplied is skipped; a leaf that is not a valid string label (a hand-edited file) is
/// skipped and named in the returned notes, never silently (AI-18).
pub(crate) fn bound_labels_from(effective: &EffectiveConfig) -> (Vec<Label>, Vec<String>) {
    let mut labels = Vec::new();
    let mut notes = Vec::new();
    for (key, resolved) in effective.iter() {
        let Some(label_key) = key.strip_prefix(LABELS_PREFIX) else {
            continue;
        };
        if resolved.source == SourceLayer::InvocationOverride {
            continue;
        }
        let Some(value) = resolved.value.as_str() else {
            notes.push(format!(
                "config key `{key}` is not a string, so it binds no label"
            ));
            continue;
        };
        if let Err(reason) =
            validate_label_key(label_key).and_then(|()| validate_label_value(value))
        {
            notes.push(format!("config key `{key}` binds no label: {reason}"));
            continue;
        }
        labels.push(Label::bound(label_key, value));
    }
    labels.sort();
    (labels, notes)
}

/// One label-verb request on a Run (story 21-9): keys to bind (origin
/// `label-verb`) and keys to unbind. All-or-nothing per Run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelChange {
    /// `key = value` pairs to bind.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Keys to unbind.
    #[serde(default)]
    pub remove: Vec<String>,
}

impl LabelChange {
    /// A change that binds `pairs`.
    pub fn set<K: Into<String>, V: Into<String>>(pairs: impl IntoIterator<Item = (K, V)>) -> Self {
        Self {
            set: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            remove: Vec::new(),
        }
    }

    /// A change that unbinds `keys`.
    pub fn remove<K: Into<String>>(keys: impl IntoIterator<Item = K>) -> Self {
        Self {
            set: BTreeMap::new(),
            remove: keys.into_iter().map(Into::into).collect(),
        }
    }

    /// Validate the request: something to change, every key and value in
    /// the label grammar, no key removed twice, and no key both set and
    /// removed. A refusal is [`RunError::Invalid`].
    pub fn validate(&self) -> Result<(), RunError> {
        let invalid = |reason: String| RunError::Invalid {
            field: "labels".to_string(),
            reason,
        };
        if self.set.is_empty() && self.remove.is_empty() {
            return Err(invalid(
                "the change is empty — name at least one label to set or remove".to_string(),
            ));
        }
        for (key, value) in &self.set {
            validate_label_key(key)
                .and_then(|()| validate_label_value(value))
                .map_err(invalid)?;
        }
        // A set, not a pre-sized Vec: nothing is allocated from the
        // request's own length, and a duplicate is found in O(log n).
        let mut seen = std::collections::BTreeSet::new();
        for key in &self.remove {
            validate_label_key(key).map_err(invalid)?;
            if !seen.insert(key) {
                return Err(invalid(format!("label key `{key}` is removed twice")));
            }
            if self.set.contains_key(key) {
                return Err(invalid(format!(
                    "label key `{key}` is both set and removed"
                )));
            }
        }
        Ok(())
    }
}

/// What a label-verb request did (or, on a dry run, would do) to one Run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelOutcome {
    /// The Run.
    pub run_id: RunId,
    /// Its labels after the change (or as planned), bound and Caller,
    /// sorted by key.
    pub labels: Vec<Label>,
    /// Its labels before, bound and Caller — so a replaced Caller value is
    /// named.
    pub prior_labels: Vec<Label>,
    /// Whether the label set changed (or would change).
    pub changed: bool,
    /// Whether this was a dry run (nothing written).
    pub dry_run: bool,
}

/// Why [`plan_label_change`] refused (the caller names the Run and
/// instance when it maps this to a [`RunError`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LabelRefusal {
    /// The key is bound only by the instance configuration.
    FromConfig(String),
    /// The key is a Caller's.
    NotBound(String),
}

/// Plan one label-verb request on a Run whose labels are `current`: the
/// Run's next full label set, sorted by key. `config` is what the instance
/// configuration binds a process-owning Run today (empty for any other Run):
/// removing a verb label a config value shadowed brings that value back
/// (origin `config`). Removing a `config` label or a Caller label is refused;
/// removing an absent key changes nothing. `change` must be valid
/// ([`LabelChange::validate`]).
pub(crate) fn plan_label_change(
    current: &[Label],
    change: &LabelChange,
    config: &[Label],
) -> Result<Vec<Label>, LabelRefusal> {
    let mut next: BTreeMap<String, Label> = current
        .iter()
        .map(|label| (label.key.clone(), label.clone()))
        .collect();
    for key in &change.remove {
        let Some(existing) = next.get(key) else {
            continue;
        };
        match existing.bound_origin() {
            None => return Err(LabelRefusal::NotBound(key.clone())),
            // A binding's label is changed through the binding (story 23-1).
            Some(LabelOrigin::Config | LabelOrigin::Binding) => {
                return Err(LabelRefusal::FromConfig(key.clone()))
            }
            Some(LabelOrigin::LabelVerb) => {
                next.remove(key);
                if let Some(configured) = config.iter().find(|label| &label.key == key) {
                    next.insert(
                        key.clone(),
                        Label::bound(key.clone(), configured.value.clone()),
                    );
                }
            }
        }
    }
    for (key, value) in &change.set {
        next.insert(key.clone(), Label::verb(key.clone(), value.clone()));
    }
    Ok(next.into_values().collect())
}

/// Plan a binding re-sync (story 23-1) of a binding Run whose labels are
/// `current` against its binding's labels now: every `binding` row is
/// replaced by `binding`, a `label-verb` key is never touched (the verb
/// wins), and a binding key replaces a Caller row (bound wins). Returns the
/// next full label set, sorted by key.
pub(crate) fn plan_binding_sync(current: &[Label], binding: &[(String, String)]) -> Vec<Label> {
    let mut next: BTreeMap<String, Label> = current
        .iter()
        .filter(|label| label.bound_origin() != Some(LabelOrigin::Binding))
        .map(|label| (label.key.clone(), label.clone()))
        .collect();
    for (key, value) in binding {
        let verb_owned = next
            .get(key)
            .is_some_and(|existing| existing.bound_origin() == Some(LabelOrigin::LabelVerb));
        if !verb_owned {
            next.insert(
                key.clone(),
                Label::bound_by(key.clone(), value.clone(), LabelOrigin::Binding),
            );
        }
    }
    next.into_values().collect()
}

/// Plan a configuration re-sync of a Run whose labels are `current` against
/// the bound labels its instance configuration gives it now: every `config`
/// row is replaced by `config`, a `label-verb` key is never touched (the verb
/// wins), and a configured key replaces a Caller row (bound wins). Returns
/// the next full label set, sorted by key.
pub(crate) fn plan_config_sync(current: &[Label], config: &[Label]) -> Vec<Label> {
    let mut next: BTreeMap<String, Label> = current
        .iter()
        .filter(|label| label.bound_origin() != Some(LabelOrigin::Config))
        .map(|label| (label.key.clone(), label.clone()))
        .collect();
    for label in config {
        let verb_owned = next
            .get(&label.key)
            .is_some_and(|existing| existing.bound_origin() == Some(LabelOrigin::LabelVerb));
        if !verb_owned {
            next.insert(
                label.key.clone(),
                Label::bound(label.key.clone(), label.value.clone()),
            );
        }
    }
    next.into_values().collect()
}

/// The Caller guard (FR-49: a Caller cannot set a key that is already
/// bound): refuse when any of `caller`'s keys is bound among `existing`.
/// Every Caller-label write calls it — a child open today (vacuous for a
/// fresh Run), the announce and identity-header paths when they land.
pub(crate) fn check_caller_labels(
    run_id: &RunId,
    existing: &[Label],
    caller: &[Label],
) -> Result<(), RunError> {
    for label in caller {
        if existing
            .iter()
            .any(|bound| bound.is_bound() && bound.key == label.key)
        {
            return Err(RunError::LabelBound {
                run_id: run_id.as_str().to_string(),
                key: label.key.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::{resolve, ConfigLayer};

    fn layer(text: &str) -> ConfigLayer {
        ConfigLayer::parse(SourceLayer::Instance, "<test>", text).unwrap()
    }

    #[test]
    fn keys_follow_the_grammar() {
        for good in ["team", "a", "cost-center", "app.tier_2", &"k".repeat(63)] {
            assert!(validate_label_key(good).is_ok(), "{good}");
        }
        for bad in ["", "Team", "1team", "team!", "te am", &"k".repeat(64)] {
            assert!(validate_label_key(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn values_follow_the_grammar_and_never_carry_a_secret() {
        for good in ["payments", "v1.2", "eu-west-1", "a@b", "x/y:z+1"] {
            assert!(validate_label_value(good).is_ok(), "{good}");
        }
        for bad in ["", "has space", "secret:KEY", "é", &"v".repeat(129)] {
            assert!(validate_label_value(bad).is_err(), "{bad}");
        }
        assert!(validate_label_write("labels.team", "payments").is_ok());
        assert!(validate_label_write("labels.Team", "payments").is_err());
        assert!(validate_label_write("labels.team", "secret:X").is_err());
        assert!(validate_label_write("labels.team", " payments").is_err());
    }

    #[test]
    fn labels_merge_per_key_and_the_invocation_layer_binds_nothing() {
        let kind = ConfigLayer::parse(
            SourceLayer::KindDefault,
            "<kind>",
            "[labels]\ntier = \"gold\"\nteam = \"core\"\n",
        )
        .unwrap();
        let instance = layer("[labels]\nteam = \"payments\"\n");
        let invocation = ConfigLayer::parse(
            SourceLayer::InvocationOverride,
            "<cli>",
            "[labels]\nteam = \"override\"\nextra = \"x\"\n",
        )
        .unwrap();
        // The engine resolves labels with an EMPTY invocation layer, so the
        // instance value wins per key and the kind sibling survives.
        let effective = resolve([
            ConfigLayer::empty(),
            kind.clone(),
            instance.clone(),
            ConfigLayer::empty(),
        ]);
        let (labels, notes) = bound_labels_from(&effective);
        assert_eq!(
            labels,
            vec![
                Label::bound("team", "payments"),
                Label::bound("tier", "gold")
            ]
        );
        assert!(notes.is_empty(), "{notes:?}");
        // The guard: a resolution that did include an override binds none of
        // its leaves, so an override can never change what a Policy matches.
        let effective = resolve([ConfigLayer::empty(), kind, instance, invocation]);
        let (labels, _notes) = bound_labels_from(&effective);
        assert_eq!(labels, vec![Label::bound("tier", "gold")]);
    }

    #[test]
    fn a_malformed_label_leaf_is_named_not_silently_dropped() {
        let effective = resolve([
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            layer("[labels]\nteam = 3\nBad = \"x\"\nok = \"yes\"\n"),
            ConfigLayer::empty(),
        ]);
        let (labels, notes) = bound_labels_from(&effective);
        assert_eq!(labels, vec![Label::bound("ok", "yes")]);
        assert_eq!(notes.len(), 2, "{notes:?}");
    }

    fn change(set: &[(&str, &str)], remove: &[&str]) -> LabelChange {
        LabelChange {
            set: set
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            remove: remove.iter().map(|k| k.to_string()).collect(),
        }
    }

    #[test]
    fn a_change_is_validated_before_any_plan() {
        assert!(change(&[("team", "core")], &["env"]).validate().is_ok());
        for bad in [
            change(&[], &[]),
            change(&[("Team", "core")], &[]),
            change(&[("team", "has space")], &[]),
            change(&[("team", "secret:X")], &[]),
            change(&[], &["Bad"]),
            change(&[], &["env", "env"]),
            change(&[("team", "core")], &["team"]),
        ] {
            assert!(
                matches!(bad.validate(), Err(RunError::Invalid { .. })),
                "{bad:?}"
            );
        }
        assert_eq!(
            LabelChange::set([("a", "b")])
                .set
                .get("a")
                .map(String::as_str),
            Some("b")
        );
        assert_eq!(LabelChange::remove(["a"]).remove, vec!["a".to_string()]);
    }

    #[test]
    fn the_verb_wins_over_config_and_caller_and_removal_restores_config() {
        let config = vec![Label::bound("team", "payments")];
        let current = vec![Label::caller("task", "x"), Label::bound("team", "payments")];
        // Set over a config key and a Caller key: both become verb labels.
        let next = plan_label_change(
            &current,
            &change(&[("team", "core"), ("task", "y")], &[]),
            &config,
        )
        .unwrap();
        assert_eq!(
            next,
            vec![Label::verb("task", "y"), Label::verb("team", "core")]
        );
        // Removing the verb label brings the shadowed config value back.
        let restored = plan_label_change(&next, &change(&[], &["team"]), &config).unwrap();
        assert_eq!(
            restored,
            vec![Label::verb("task", "y"), Label::bound("team", "payments")]
        );
        // With no config value the key simply goes.
        let gone = plan_label_change(&next, &change(&[], &["task"]), &config).unwrap();
        assert_eq!(gone, vec![Label::verb("team", "core")]);
        // An absent key is unchanged.
        assert_eq!(
            plan_label_change(&current, &change(&[], &["nope"]), &config).unwrap(),
            current
        );
    }

    #[test]
    fn removing_a_config_or_caller_label_is_refused() {
        let current = vec![Label::bound("team", "payments"), Label::caller("task", "x")];
        assert_eq!(
            plan_label_change(&current, &change(&[], &["team"]), &[]),
            Err(LabelRefusal::FromConfig("team".to_string()))
        );
        assert_eq!(
            plan_label_change(&current, &change(&[], &["task"]), &[]),
            Err(LabelRefusal::NotBound("task".to_string()))
        );
        // A bound row with no recorded origin is a config row.
        let legacy = vec![Label {
            origin: None,
            ..Label::bound("team", "payments")
        }];
        assert_eq!(
            plan_label_change(&legacy, &change(&[], &["team"]), &[]),
            Err(LabelRefusal::FromConfig("team".to_string()))
        );
    }

    #[test]
    fn a_config_sync_replaces_only_config_rows_and_never_a_verb_key() {
        let current = vec![
            Label::bound("env", "dev"),
            Label::caller("task", "x"),
            Label::verb("team", "core"),
        ];
        let next = plan_config_sync(
            &current,
            &[
                Label::bound("team", "payments"),
                Label::bound("task", "cfg"),
                Label::bound("tier", "gold"),
            ],
        );
        assert_eq!(
            next,
            vec![
                // `env` left the configuration; the Caller `task` lost to the
                // configured key; the verb's `team` stayed.
                Label::bound("task", "cfg"),
                Label::verb("team", "core"),
                Label::bound("tier", "gold"),
            ]
        );
        // The same configuration again changes nothing.
        let configured = [
            Label::bound("team", "payments"),
            Label::bound("task", "cfg"),
            Label::bound("tier", "gold"),
        ];
        assert_eq!(plan_config_sync(&next, &configured), next);
    }

    #[test]
    fn a_caller_label_on_a_bound_key_is_refused() {
        let run = RunId::from_wire("run_X");
        let existing = vec![Label::bound("team", "payments"), Label::caller("task", "x")];
        assert!(check_caller_labels(&run, &existing, &[Label::caller("task", "y")]).is_ok());
        assert!(check_caller_labels(&run, &[], &[Label::caller("team", "y")]).is_ok());
        let refused =
            check_caller_labels(&run, &existing, &[Label::caller("team", "core")]).unwrap_err();
        assert_eq!(refused.code(), "label-bound");
        assert!(refused.to_string().contains("team"), "{refused}");
    }
}
