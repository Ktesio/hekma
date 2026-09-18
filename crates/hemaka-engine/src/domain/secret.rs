//! The [`SecretString`] newtype (spine AD-10, story 2-4, FR-14/NFR-6) — the
//! "safe by construction" core.
//!
//! A resolved secret's cleartext lives ONLY inside a [`SecretString`]. Its
//! `Display` AND `Debug` both emit a fixed [`REDACTED`] token and NEVER the inner
//! value, so a secret that rides in a `launch.env` / `launch.args` cannot leak
//! through a `{:?}` on a launch spec, an event `detail`, or a log line — the type
//! is the STRUCTURAL guard behind the no-leak matrix (AC-B / AC10). The cleartext
//! is reachable ONLY through the deliberately greppable [`SecretString::expose_secret`]
//! accessor (the `secrecy`-crate convention), so a reviewer / CI can audit every
//! cleartext access by grepping one name.
//!
//! ## Deliberate design constraints (recorded, Assumption 2)
//!
//! * **Hand-rolled, no new dependency.** The type is a dozen lines and the
//!   guarantee is unit-testable directly, so no `secrecy` / `zeroize` crate is
//!   adopted (a crate would add supply-surface for little gain). Recorded.
//! * **NOT `#[derive(Serialize)]`.** Deriving `serde` on a struct that embeds one
//!   would silently leak the cleartext (the "skipped/masked in serialization"
//!   clause of AD-10). Any serialization of a secret is a DELIBERATE masked encode
//!   (the config `display()` mask), never a derive. `SecretString` carries no
//!   serde impl at all — it is a transient, resolved-at-start value the engine
//!   hands to the adapter and drops; it is never persisted.
//! * **No zeroize-on-drop in v1.** Flagged out-of-scope (Assumption 2) — not free
//!   without a dependency, and not required by FR-14/NFR-6.
//!
//! ## Where the cleartext goes (display vs delivery diverge — AC9)
//!
//! The adapter delivery path ([`crate::adapter::apply_config_mapping`]) calls
//! [`SecretString::expose_secret`] to place the REAL key into the agent's native
//! env/flag/file; every display/log/serialize path instead meets the redacting
//! `Display`/`Debug` (or the config `display()` mask). The same leaf therefore
//! shows a mask in `config get` / the snapshot / logs while delivering cleartext
//! to the adapter's private native config — `SecretString` is what keeps the two
//! honest.

/// The fixed token [`SecretString`]'s `Display` and `Debug` emit in place of the
/// cleartext (story 2-4, AC7). A reviewer seeing `[REDACTED]` in any log / debug
/// dump knows a secret was present but not exposed. Kept distinct in spelling from
/// the config-layer [`crate::domain::config::SECRET_MASK`] (`secret:****`), which
/// masks the `config get` / snapshot value; both communicate "redacted".
pub const REDACTED: &str = "[REDACTED]";

/// A resolved secret's cleartext, wrapped so it cannot leak by accident (AD-10).
///
/// Construct with [`SecretString::new`]; read the cleartext ONLY with the explicit
/// [`SecretString::expose_secret`]. `Display`/`Debug` redact (see [`REDACTED`]).
/// It is NOT `Clone` by default is fine — but resolution hands out exactly one per
/// leaf and the mapping consumes it by reference, so no clone is needed. Deliberately
/// NOT `Serialize` (see the module docs).
#[derive(PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a resolved cleartext secret. The ONLY constructor — resolvers build
    /// one from the env var value / the secrets-file entry.
    pub fn new(cleartext: impl Into<String>) -> Self {
        SecretString(cleartext.into())
    }

    /// Expose the cleartext — the SOLE way to read the inner value (AC7/AC9).
    ///
    /// Deliberately greppable (the `secrecy`-crate convention) so every cleartext
    /// access is auditable. Called at the FINAL placement into the adapter's native
    /// mechanism ([`crate::adapter::apply_config_mapping`]) and by the `--reveal`
    /// read path; nothing else should call it.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

/// Redacting `Display`: emits [`REDACTED`], NEVER the cleartext (AC7). This is why
/// interpolating a secret into a user-facing string cannot leak it.
impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Redacting `Debug`: emits [`REDACTED`], NEVER the cleartext (AC7). This is the
/// structural guard that keeps a secret in `launch.env`/`launch.args` from leaking
/// through a `{:?}` on a launch spec or an event `detail` (AC-B/AC10). A struct
/// embedding a `SecretString` and deriving `Debug` inherits this redaction.
impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_debug_redact_never_the_cleartext() {
        // AC7: both Display and Debug emit the fixed token, NEVER the inner value.
        let s = SecretString::new("s3cr3t-sentinel");
        assert_eq!(format!("{s}"), REDACTED);
        assert_eq!(format!("{s:?}"), REDACTED);
        assert!(!format!("{s}").contains("s3cr3t-sentinel"));
        assert!(!format!("{s:?}").contains("s3cr3t-sentinel"));
    }

    #[test]
    fn expose_secret_returns_the_cleartext() {
        // The SOLE cleartext accessor returns the real value (for adapter delivery
        // + --reveal).
        let s = SecretString::new("s3cr3t-sentinel");
        assert_eq!(s.expose_secret(), "s3cr3t-sentinel");
    }

    #[test]
    fn a_struct_embedding_a_secret_does_not_leak_via_debug() {
        // AC-B type-level guard: a struct that HOLDS a SecretString and derives
        // Debug inherits the redaction — the sentinel never appears in `{:?}`. This
        // is the structural reason a secret in launch.env cannot leak through a
        // Debug on the surrounding launch spec / event.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Launch {
            exec: String,
            token: SecretString,
        }
        let launch = Launch {
            exec: "the-agent".to_string(),
            token: SecretString::new("s3cr3t-sentinel"),
        };
        let dumped = format!("{launch:?}");
        assert!(dumped.contains("the-agent"), "non-secret fields still show");
        assert!(
            !dumped.contains("s3cr3t-sentinel"),
            "the embedded secret must be redacted in a derived Debug: {dumped}"
        );
        assert!(
            dumped.contains(REDACTED),
            "the redaction token appears: {dumped}"
        );
    }

    #[test]
    fn secret_string_equality_is_by_cleartext() {
        // Eq compares the inner cleartext (useful for tests / dedup); it does NOT
        // weaken redaction (Display/Debug still mask).
        assert_eq!(SecretString::new("a"), SecretString::new("a".to_string()));
        assert_ne!(SecretString::new("a"), SecretString::new("b"));
    }
}
