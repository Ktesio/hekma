//! Selectors (story 21-9, PRD FR-50): a label expression that names a set of
//! Runs.
//!
//! A Selector is `key=value[,key=value…]`; every term must match (AND). Keys
//! and values follow the label grammar, and a key may appear once. Listings
//! and verbs match bound and Caller labels alike ([`SelectorScope::Any`],
//! FR-49); Policies will match bound labels only ([`SelectorScope::BoundOnly`],
//! FR-60). The store resolves a Selector over the non-ended Runs with one
//! indexed read, capped at [`MAX_SELECTED_RUNS`].

use std::fmt;
use std::str::FromStr;

use super::{validate_label_key, validate_label_value, Label};

/// The most Runs one Selector may act on; a larger match is refused
/// (`too-many-runs`) rather than truncated.
pub const MAX_SELECTED_RUNS: usize = 1_000;

/// Which labels a Selector term may match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectorScope {
    /// Bound and Caller labels (listings and verbs).
    Any,
    /// Bound labels only (Policies).
    BoundOnly,
}

/// A parsed Selector: its `key=value` terms, in the order given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    terms: Vec<(String, String)>,
}

impl Selector {
    /// Parse `key=value[,key=value…]`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("a Selector cannot be empty (for example team=core,env=prod)".to_string());
        }
        let mut terms: Vec<(String, String)> = Vec::new();
        for term in raw.split(',') {
            let Some((key, value)) = term.split_once('=') else {
                return Err(format!(
                    "Selector term `{term}` must be key=value (for example team=core)"
                ));
            };
            validate_label_key(key)?;
            validate_label_value(value)?;
            if terms.iter().any(|(seen, _)| seen == key) {
                return Err(format!("Selector names label key `{key}` twice"));
            }
            terms.push((key.to_string(), value.to_string()));
        }
        Ok(Self { terms })
    }

    /// The `key=value` terms, in the order given.
    pub fn terms(&self) -> &[(String, String)] {
        &self.terms
    }

    /// Whether a Run carrying `labels` matches every term within `scope`.
    pub fn matches(&self, labels: &[Label], scope: SelectorScope) -> bool {
        self.terms.iter().all(|(key, value)| {
            labels.iter().any(|label| {
                &label.key == key
                    && &label.value == value
                    && (scope == SelectorScope::Any || label.is_bound())
            })
        })
    }
}

impl FromStr for Selector {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rendered: Vec<String> = self
            .terms
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        f.write_str(&rendered.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selector_parses_and_renders_its_terms() {
        let selector: Selector = "team=core,env=prod".parse().unwrap();
        assert_eq!(
            selector.terms(),
            &[
                ("team".to_string(), "core".to_string()),
                ("env".to_string(), "prod".to_string())
            ]
        );
        assert_eq!(selector.to_string(), "team=core,env=prod");
        for bad in [
            "",
            "team",
            "team=core,",
            "Team=core",
            "team=has space",
            "team=core,team=x",
            "team=secret:X",
            " team=core",
        ] {
            assert!(Selector::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn every_term_must_match_and_policies_see_bound_labels_only() {
        let selector = Selector::parse("team=core,env=prod").unwrap();
        let labels = vec![Label::verb("team", "core"), Label::caller("env", "prod")];
        assert!(selector.matches(&labels, SelectorScope::Any));
        assert!(!selector.matches(&labels, SelectorScope::BoundOnly));
        assert!(!selector.matches(&labels[..1], SelectorScope::Any));
        let wrong = vec![Label::bound("team", "core"), Label::bound("env", "dev")];
        assert!(!selector.matches(&wrong, SelectorScope::Any));
        let bound = vec![Label::bound("team", "core"), Label::bound("env", "prod")];
        assert!(selector.matches(&bound, SelectorScope::BoundOnly));
    }
}
