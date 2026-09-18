//! [`InstanceName`] — the Fleet-unique Agent Instance identifier newtype.
//!
//! The naming rule is a spine convention (`^[a-z0-9][a-z0-9_-]*$`, unique per
//! Fleet). Validation happens once at construction so that every layer that
//! holds an [`InstanceName`] can trust it: the store keys rows by it, the path
//! authority builds the Agent Home directory from it, and `kt` displays it.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A validated Agent Instance name.
///
/// Guaranteed to match `^[a-z0-9][a-z0-9_-]*$`: it starts with a lowercase
/// ASCII letter or digit, and the remainder is lowercase letters, digits,
/// underscores, or hyphens. Constructing one is the ONLY way to obtain the
/// type, so downstream code never re-validates.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstanceName(String);

/// Maximum length of an [`InstanceName`], in characters.
///
/// Bounds the name so an over-long value is rejected early with a clear rule
/// rather than failing late and opaquely at `create_dir_all` (many filesystems
/// cap a single path component at 255 bytes; 128 leaves ample headroom for the
/// surrounding Agent Home path). `[ASSUMPTION]` on the exact cap.
pub const MAX_NAME_LEN: usize = 128;

/// Why a candidate string is not a valid [`InstanceName`].
///
/// Carried inside the domain error so `kt` can render a precise remediation
/// hint naming the exact rule that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    /// The string was empty.
    Empty,
    /// The first character was not `[a-z0-9]`.
    BadFirstChar,
    /// A later character was outside `[a-z0-9_-]`.
    BadChar,
    /// The string exceeded [`MAX_NAME_LEN`] characters.
    TooLong,
}

impl NameError {
    /// Human-readable statement of the rule that failed (used in diagnostics).
    pub fn rule(&self) -> &'static str {
        match self {
            NameError::Empty => "name must not be empty",
            NameError::BadFirstChar => {
                "name must start with a lowercase letter or digit ([a-z0-9])"
            }
            NameError::BadChar => {
                "name may contain only lowercase letters, digits, '_' or '-' ([a-z0-9_-])"
            }
            NameError::TooLong => "name must be at most 128 characters",
        }
    }
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.rule())
    }
}

impl std::error::Error for NameError {}

impl InstanceName {
    /// Validate `raw` and construct an [`InstanceName`].
    ///
    /// The rule is applied by hand (no `regex` dependency in the engine — the
    /// pattern is trivial): the first byte must be `[a-z0-9]`, and every later
    /// byte must be `[a-z0-9_-]`.
    pub fn new(raw: impl Into<String>) -> Result<Self, NameError> {
        let raw = raw.into();
        // Bound the length first so an over-long name fails with a clear rule
        // here, not opaquely at create_dir_all later (F9).
        if raw.chars().count() > MAX_NAME_LEN {
            return Err(NameError::TooLong);
        }
        let mut chars = raw.chars();
        match chars.next() {
            None => return Err(NameError::Empty),
            Some(first) if !is_head(first) => return Err(NameError::BadFirstChar),
            Some(_) => {}
        }
        for ch in chars {
            if !is_tail(ch) {
                return Err(NameError::BadChar);
            }
        }
        Ok(InstanceName(raw))
    }

    /// The validated name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// First-character rule: lowercase ASCII letter or ASCII digit.
fn is_head(ch: char) -> bool {
    ch.is_ascii_lowercase() || ch.is_ascii_digit()
}

/// Tail-character rule: head set plus `_` and `-`.
fn is_tail(ch: char) -> bool {
    is_head(ch) || ch == '_' || ch == '-'
}

impl fmt::Display for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<InstanceName> for String {
    fn from(name: InstanceName) -> Self {
        name.0
    }
}

impl TryFrom<String> for InstanceName {
    type Error = NameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        InstanceName::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for good in ["a", "0", "demo", "agent-1", "web_worker", "a-b_c-9", "z0"] {
            let name = InstanceName::new(good).unwrap_or_else(|e| panic!("{good}: {e}"));
            assert_eq!(name.as_str(), good);
            assert_eq!(name.to_string(), good);
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(InstanceName::new(""), Err(NameError::Empty));
    }

    #[test]
    fn rejects_bad_first_char() {
        for bad in ["-x", "_x", "Ab", "9x-OK-but-cap", ".a", " a"] {
            // Only names whose FIRST char is invalid land here.
            if bad == "9x-OK-but-cap" {
                // starts with '9' (valid head) but has uppercase later.
                assert_eq!(InstanceName::new(bad), Err(NameError::BadChar), "{bad}");
            } else {
                assert_eq!(
                    InstanceName::new(bad),
                    Err(NameError::BadFirstChar),
                    "{bad}"
                );
            }
        }
    }

    #[test]
    fn rejects_bad_tail_char() {
        for bad in ["aB", "a b", "a.b", "a/b", "a!", "agent#1", "a\u{00e9}"] {
            assert_eq!(InstanceName::new(bad), Err(NameError::BadChar), "{bad}");
        }
    }

    #[test]
    fn accepts_name_at_max_length_and_rejects_over() {
        // F9: exactly MAX_NAME_LEN chars is accepted; one more is TooLong.
        let at_max = "a".repeat(MAX_NAME_LEN);
        assert_eq!(InstanceName::new(&at_max).unwrap().as_str(), at_max);

        let over = "a".repeat(MAX_NAME_LEN + 1);
        assert_eq!(InstanceName::new(over), Err(NameError::TooLong));
    }

    #[test]
    fn too_long_is_checked_before_char_rules() {
        // An over-long name that ALSO contains an invalid char reports TooLong
        // (the length gate runs first), giving a stable, actionable message.
        let over_and_bad = format!("{}!", "a".repeat(MAX_NAME_LEN));
        assert_eq!(InstanceName::new(over_and_bad), Err(NameError::TooLong));
    }

    #[test]
    fn error_rule_text_is_specific() {
        assert!(NameError::Empty.rule().contains("empty"));
        assert!(NameError::BadFirstChar.rule().contains("start"));
        assert!(NameError::BadChar.rule().contains("only"));
        assert!(NameError::TooLong.rule().contains("128"));
        // Display forwards to rule().
        assert_eq!(NameError::Empty.to_string(), NameError::Empty.rule());
    }

    #[test]
    fn serde_uses_validated_string() {
        let name = InstanceName::new("demo-1").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"demo-1\"");
        let back: InstanceName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, name);
        // Deserializing an invalid name fails through TryFrom.
        assert!(serde_json::from_str::<InstanceName>("\"Bad\"").is_err());
    }
}
