//! The label grammar and the instance `labels` config key (story 21-2, D3).
//!
//! Bound labels for a supervised Run come from the instance `labels` config
//! key: `labels.<key> = "<value>"` in any layer, merged per key (the config
//! resolver's per-leaf merge, higher layer wins). The invocation override
//! layer never binds a label (AD-25: an invocation override cannot bind
//! labels), so [`bound_labels_from`] reads only leaves supplied by the
//! engine-default, kind-default and instance layers.

use super::Label;
use crate::domain::config::{EffectiveConfig, SourceLayer};

/// The config namespace bound labels live under.
pub(crate) const LABELS_PREFIX: &str = "labels.";

/// The longest label value accepted.
const VALUE_MAX: usize = 128;

/// Validate a label key: a lowercase letter, then up to 62 of lowercase
/// letters, digits, `_`, `.` and `-` (`^[a-z][a-z0-9_.-]{0,62}$`).
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
}
