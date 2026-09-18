//! The [`SecretResolver`] port + its v1 resolvers (spine AD-10, story 2-4,
//! FR-14/NFR-6).
//!
//! A `secret:NAME` config reference (classified by
//! [`crate::domain::is_secret_ref`]) is RESOLVED at start into a
//! [`SecretString`](crate::domain::SecretString) through this hexagonal PORT
//! (like [`StateStore`](crate::ports::StateStore) /
//! [`ProcessBackend`](crate::ports::ProcessBackend)). v1 ships two resolvers,
//! composed in a recorded ORDER (Assumption 4 — env FIRST, then the file):
//!
//! * [`EnvSecretResolver`] — `secret:OPENAI_KEY` resolves to
//!   `std::env::var("OPENAI_KEY")` (the operator's ad-hoc override).
//! * [`FileSecretResolver`] — the engine secrets file (mode 0600) at
//!   [`EnginePaths::secrets_file`](crate::paths::EnginePaths::secrets_file), a
//!   TOML `NAME = "value"` table (the durable store), permission-checked before
//!   read.
//!
//! [`CompositeSecretResolver`] tries them in order; a `secret:NAME` resolved by
//! NEITHER is a typed [`SecretError::Unresolved`] naming `NAME` + the resolvers
//! tried, NEVER echoing a value. At start this rejects the start (no half-launch —
//! the supervisor maps it to a typed `EngineError` before the `starting`
//! transition).
//!
//! OS-keychain stays a DEFERRED resolver behind this SAME port (AD-10) — v1 builds
//! NONE of it.
//!
//! ## The OS-agnostic boundary (AD-4)
//!
//! The resolvers here are OS-AGNOSTIC. The 0600 permission INSPECTION is inherently
//! OS-specific (Unix mode bits vs Windows ACLs), so it lives in
//! [`crate::backends`] (the sole allowlisted `#[cfg]` home) as
//! [`crate::backends::check_secrets_file_permissions`]; the file resolver CALLS it
//! and never branches on OS itself.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::domain::SecretString;

/// The secret-resolution port (spine AD-10). Given a `NAME` (the lookup key after
/// the `secret:` prefix), a resolver either HOLDS the secret, does not, or fails
/// hard trying:
///
/// * `Ok(Some(secret))` — this resolver resolved `NAME` to a cleartext secret.
/// * `Ok(None)` — this resolver does not hold `NAME` (the composite tries the
///   next).
/// * `Err(_)` — this resolver TRIED and failed HARD (e.g. an ill-permissioned or
///   unreadable secrets file); the composite surfaces it (a hard failure is not
///   "absent", so it is not silently skipped).
///
/// The recorded signature (Assumption 3). Implementors are OS-agnostic; the
/// per-OS permission check is a [`crate::backends`] call, not part of the trait.
pub trait SecretResolver {
    /// Resolve `NAME` (the key after `secret:`), per the contract above.
    fn resolve(&self, name: &str) -> Result<Option<SecretString>, SecretError>;
}

/// Why a `secret:NAME` reference could not be resolved (story 2-4). `thiserror`
/// (never `miette` — `kt` wraps it), mirroring
/// [`StoreError`](crate::ports::StoreError). CRITICAL: NO variant EVER carries a
/// resolved secret VALUE — only the `NAME`, the resolvers tried, the file path, and
/// an I/O/permission detail (NFR-6). The engine maps this into a start-rejecting
/// `EngineError::Secret` before the `starting` transition.
#[derive(Debug, Error)]
pub enum SecretError {
    /// The `secret:NAME` reference resolved in NONE of the composed resolvers.
    /// Names `NAME` + the resolvers tried so the operator can fix it (set the env
    /// var, or add `NAME` to the 0600 secrets file), WITHOUT echoing any value.
    #[error(
        "secret '{name}' could not be resolved (tried: {tried}); set the environment variable {name}, or add {name} to the engine secrets file (mode 0600)"
    )]
    Unresolved {
        /// The unresolved secret NAME (the lookup key, NOT a value).
        name: String,
        /// A human list of the resolvers tried (e.g. "environment, secrets file").
        tried: String,
    },

    /// The engine secrets file exists but is NOT owner-only (mode `0600`) — a
    /// group/other-accessible secrets file defeats the guarantee, so the resolver
    /// REFUSES to read it (AC6). Names the path + a `chmod 600` remediation. The
    /// permission INSPECTION lives in [`crate::backends`] (AD-4); this variant is
    /// its typed refusal.
    #[error(
        "the engine secrets file at '{path}' is not owner-only ({detail}); fix it with: chmod 600 '{path}'"
    )]
    FilePermissions {
        /// The secrets file path.
        path: String,
        /// What specifically is wrong (e.g. "mode 0644 grants group/other access").
        detail: String,
    },

    /// The engine secrets file exists and passed the permission check but could
    /// not be read or parsed as TOML. Names the path + detail (NEVER a value). A
    /// missing file is NOT this error — it is simply "this resolver does not hold
    /// it" (`Ok(None)`).
    #[error("the engine secrets file at '{path}' could not be read/parsed: {detail}")]
    FileUnreadable {
        /// The secrets file path.
        path: String,
        /// The underlying I/O or parse detail (no secret value).
        detail: String,
    },
}

/// The PROCESS-ENV resolver (AD-10 v1): `NAME` → `std::env::var(NAME)`. The
/// operator's ad-hoc override; tried FIRST by the composite (Assumption 4). An
/// unset var is `Ok(None)` (not held here — the file may hold it); a present var
/// is `Ok(Some(SecretString))`. A var set to non-UTF-8 is treated as absent
/// (`std::env::var` errors → `Ok(None)`), so the file can still resolve it.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvSecretResolver;

impl EnvSecretResolver {
    /// Construct the env resolver.
    pub fn new() -> Self {
        EnvSecretResolver
    }
}

impl SecretResolver for EnvSecretResolver {
    fn resolve(&self, name: &str) -> Result<Option<SecretString>, SecretError> {
        // `std::env::var` errors on both "not present" and "not unicode"; both mean
        // "the env does not hold a usable value for NAME" → Ok(None), letting the
        // file resolver try. It NEVER errors hard (env access is infallible here).
        match std::env::var(name) {
            Ok(value) => Ok(Some(SecretString::new(value))),
            Err(_) => Ok(None),
        }
    }
}

/// The 0600 SECRETS-FILE resolver (AD-10 v1): reads `NAME = "value"` from the TOML
/// secrets file at [`EnginePaths::secrets_file`](crate::paths::EnginePaths::secrets_file).
/// The durable store; tried AFTER env (Assumption 4). A MISSING file is `Ok(None)`
/// (not an error — env may resolve every reference). A PRESENT file is
/// permission-checked FIRST ([`crate::backends::check_secrets_file_permissions`],
/// AC6) — a group/other-accessible file is a hard [`SecretError::FilePermissions`]
/// — then parsed; a lookup hit is `Ok(Some)`, a miss `Ok(None)`.
#[derive(Clone, Debug)]
pub struct FileSecretResolver {
    path: PathBuf,
}

impl FileSecretResolver {
    /// Construct a file resolver reading the secrets file at `path` (the engine
    /// passes [`EnginePaths::secrets_file`](crate::paths::EnginePaths::secrets_file)).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileSecretResolver { path: path.into() }
    }

    /// Read + parse the secrets file into a TOML table, or `None` if the file is
    /// absent. Permission-checked before read (AC6). A read/parse failure of a
    /// PRESENT file is a hard error (never treated as "absent").
    fn load_table(&self) -> Result<Option<toml::Table>, SecretError> {
        // A missing file is "not held here" — env may resolve everything, and a
        // secrets file is optional. Probe existence first so a missing file never
        // trips the permission check.
        if !self.path.exists() {
            return Ok(None);
        }
        // The file exists: REFUSE a non-owner-only file (AC6). The OS-specific
        // inspection lives in backends (AD-4); this call is OS-agnostic.
        crate::backends::check_secrets_file_permissions(&self.path)?;
        let text =
            std::fs::read_to_string(&self.path).map_err(|e| SecretError::FileUnreadable {
                path: self.path.to_string_lossy().into_owned(),
                detail: e.to_string(),
            })?;
        let table = text
            .parse::<toml::Table>()
            .map_err(|e| SecretError::FileUnreadable {
                path: self.path.to_string_lossy().into_owned(),
                detail: e.to_string(),
            })?;
        Ok(Some(table))
    }
}

impl SecretResolver for FileSecretResolver {
    fn resolve(&self, name: &str) -> Result<Option<SecretString>, SecretError> {
        let Some(table) = self.load_table()? else {
            return Ok(None);
        };
        // Look up NAME. Only a TOML STRING value is a usable secret; a non-string
        // entry (a table/int/bool under NAME) is treated as absent here — the
        // secrets file format is `NAME = "value"`, so a non-string is a malformed
        // entry the resolver does not honor (and never echoes).
        match table.get(name) {
            Some(toml::Value::String(value)) => Ok(Some(SecretString::new(value.clone()))),
            _ => Ok(None),
        }
    }
}

/// The COMPOSITE resolver (AD-10): tries an ordered list of resolvers and returns
/// the FIRST hit. A hard error from any resolver (e.g. an ill-permissioned file)
/// short-circuits and surfaces — it is not "absent". If EVERY resolver returns
/// `Ok(None)`, the reference is [`SecretError::Unresolved`] naming `NAME` + the
/// resolvers tried. The recorded v1 order (Assumption 4) is env → file, built by
/// [`CompositeSecretResolver::env_then_file`].
pub struct CompositeSecretResolver {
    resolvers: Vec<(&'static str, Box<dyn SecretResolver>)>,
}

impl CompositeSecretResolver {
    /// The recorded v1 composition (Assumption 4): process ENV first (the ad-hoc
    /// override), then the 0600 secrets FILE at `secrets_path` (the durable store).
    pub fn env_then_file(secrets_path: impl Into<PathBuf>) -> Self {
        CompositeSecretResolver {
            resolvers: vec![
                ("environment", Box::new(EnvSecretResolver::new())),
                (
                    "secrets file",
                    Box::new(FileSecretResolver::new(secrets_path)),
                ),
            ],
        }
    }

    /// Construct from an explicit ordered list of `(label, resolver)` pairs (test
    /// injection / future compositions). The `label` names the resolver in the
    /// `Unresolved` diagnostic.
    pub fn new(resolvers: Vec<(&'static str, Box<dyn SecretResolver>)>) -> Self {
        CompositeSecretResolver { resolvers }
    }

    /// Resolve `NAME`, or a typed [`SecretError::Unresolved`] if NO resolver holds
    /// it. A hard error from any resolver short-circuits. This is the entry point
    /// the supervisor calls at start for each secret-classified leaf.
    pub fn require(&self, name: &str) -> Result<SecretString, SecretError> {
        if let Some(secret) = self.resolve(name)? {
            return Ok(secret);
        }
        let tried = self
            .resolvers
            .iter()
            .map(|(label, _)| *label)
            .collect::<Vec<_>>()
            .join(", ");
        Err(SecretError::Unresolved {
            name: name.to_string(),
            tried,
        })
    }
}

impl SecretResolver for CompositeSecretResolver {
    fn resolve(&self, name: &str) -> Result<Option<SecretString>, SecretError> {
        for (_, resolver) in &self.resolvers {
            // A hard error short-circuits (not "absent"). A None tries the next.
            if let Some(secret) = resolver.resolve(name)? {
                return Ok(Some(secret));
            }
        }
        Ok(None)
    }
}

/// Whether a secrets file's mode is owner-only per the Unix rule (`mode & 0o077 ==
/// 0`, story 2-4 AC6). Kept here as a pure, OS-AGNOSTIC helper the Unix backend
/// calls (the backend owns the mode-bit READ; this owns the RULE), so the rule is
/// unit-testable without a real file and is not duplicated. `mode` is the raw
/// Unix permission bits.
pub fn mode_is_owner_only(mode: u32) -> bool {
    mode & 0o077 == 0
}

/// Build the [`SecretError::FilePermissions`] refusal for a non-owner-only Unix
/// secrets file (story 2-4 AC6). Shared by the Unix backend so the message +
/// remediation are consistent. `mode` is the offending raw permission bits.
pub fn file_permissions_error(path: &Path, mode: u32) -> SecretError {
    SecretError::FilePermissions {
        path: path.to_string_lossy().into_owned(),
        detail: format!("mode {:#o} grants group/other access", mode & 0o777),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The process environment is global; serialize the env-touching tests so they
    // do not race each other's set/remove (mirrors the paths.rs env-var discipline).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A resolver that always returns a fixed outcome, for composite tests.
    struct Fixed(Result<Option<&'static str>, ()>);
    impl SecretResolver for Fixed {
        fn resolve(&self, _name: &str) -> Result<Option<SecretString>, SecretError> {
            match &self.0 {
                Ok(Some(v)) => Ok(Some(SecretString::new(*v))),
                Ok(None) => Ok(None),
                Err(()) => Err(SecretError::FileUnreadable {
                    path: "<fixed>".to_string(),
                    detail: "boom".to_string(),
                }),
            }
        }
    }

    #[test]
    fn env_resolver_reads_a_set_var_and_misses_an_unset_one() {
        let _guard = ENV_LOCK.lock().unwrap();
        let key = "KTESIO_TEST_SECRET_ENV_HIT";
        let prev = std::env::var_os(key);
        std::env::set_var(key, "env-cleartext");
        let resolver = EnvSecretResolver::new();
        let hit = resolver.resolve(key).unwrap();
        assert_eq!(hit.unwrap().expose_secret(), "env-cleartext");
        // An unset var is a miss (Ok(None)), not an error.
        std::env::remove_var(key);
        assert!(resolver.resolve(key).unwrap().is_none());
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn file_resolver_reads_a_named_secret_and_misses_absent_ones() {
        // A present, owner-only secrets file resolves a NAME to its string value;
        // an absent NAME is Ok(None). (Permission-check passes on a freshly written
        // temp file on Unix; on Windows the check is a documented skip.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "OPENAI_KEY = \"file-cleartext\"\n").unwrap();
        set_owner_only(&path);
        let resolver = FileSecretResolver::new(&path);
        assert_eq!(
            resolver
                .resolve("OPENAI_KEY")
                .unwrap()
                .unwrap()
                .expose_secret(),
            "file-cleartext"
        );
        assert!(resolver.resolve("ABSENT").unwrap().is_none());
    }

    #[test]
    fn file_resolver_missing_file_is_a_miss_not_an_error() {
        // A missing secrets file is Ok(None) (env may resolve everything), never an
        // error, and never trips the permission check.
        let dir = tempfile::tempdir().unwrap();
        let resolver = FileSecretResolver::new(dir.path().join("does-not-exist.toml"));
        assert!(resolver.resolve("ANY").unwrap().is_none());
    }

    #[test]
    fn file_resolver_non_string_entry_is_treated_as_absent() {
        // The format is NAME = "value"; a non-string entry (a table/int) is not a
        // usable secret → Ok(None) (and never echoed).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "NUM = 42\n[TAB]\nx = \"y\"\n").unwrap();
        set_owner_only(&path);
        let resolver = FileSecretResolver::new(&path);
        assert!(resolver.resolve("NUM").unwrap().is_none());
        assert!(resolver.resolve("TAB").unwrap().is_none());
    }

    #[test]
    fn file_resolver_unreadable_present_path_is_a_hard_error() {
        // A path that EXISTS and passes the (owner-only) permission check but cannot
        // be read as a file — a DIRECTORY at the secrets-file path — is a hard
        // FileUnreadable error (read_to_string on a dir fails), never treated as
        // "absent" and never a panic. Exercises the read-failure branch.
        let dir = tempfile::tempdir().unwrap();
        let secrets_dir = dir.path().join("secrets.toml");
        std::fs::create_dir(&secrets_dir).unwrap();
        set_owner_only(&secrets_dir); // a 0700 dir is owner-only → passes the check
        let resolver = FileSecretResolver::new(&secrets_dir);
        let err = resolver.resolve("ANY").unwrap_err();
        assert!(
            matches!(err, SecretError::FileUnreadable { .. }),
            "an unreadable present path must be a hard error, got {err:?}"
        );
    }

    #[test]
    fn file_resolver_malformed_toml_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "not = = valid toml").unwrap();
        set_owner_only(&path);
        let resolver = FileSecretResolver::new(&path);
        let err = resolver.resolve("ANY").unwrap_err();
        assert!(
            matches!(err, SecretError::FileUnreadable { .. }),
            "got {err:?}"
        );
        // The error names the path, never a value.
        assert!(err.to_string().contains("secrets.toml"), "{err}");
    }

    #[test]
    fn composite_env_beats_file_then_file_when_env_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "SHARED = \"from-file\"\nONLYFILE = \"file-only\"\n").unwrap();
        set_owner_only(&path);

        let key_shared = "SHARED";
        let prev = std::env::var_os(key_shared);
        std::env::set_var(key_shared, "from-env");

        let composite = CompositeSecretResolver::env_then_file(&path);
        // Env HIT wins over the file for the same NAME (env is tried first).
        assert_eq!(
            composite.require("SHARED").unwrap().expose_secret(),
            "from-env"
        );
        // When env is absent, the file resolves it.
        std::env::remove_var(key_shared);
        assert_eq!(
            composite.require("ONLYFILE").unwrap().expose_secret(),
            "file-only"
        );

        match prev {
            Some(v) => std::env::set_var(key_shared, v),
            None => std::env::remove_var(key_shared),
        }
    }

    #[test]
    fn composite_unresolved_names_the_name_and_resolvers_tried_without_a_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // No secrets file at all; env unset → unresolved.
        let key = "KTESIO_TEST_DEFINITELY_UNSET_SECRET";
        std::env::remove_var(key);
        let composite = CompositeSecretResolver::env_then_file(dir.path().join("secrets.toml"));
        let err = composite.require(key).unwrap_err();
        match &err {
            SecretError::Unresolved { name, tried } => {
                assert_eq!(name, key);
                assert!(tried.contains("environment"), "{tried}");
                assert!(tried.contains("secrets file"), "{tried}");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
        // The message names NAME + resolvers + a remediation, never a value.
        let msg = err.to_string();
        assert!(msg.contains(key), "{msg}");
        assert!(
            msg.contains("chmod 600") || msg.contains("secrets file"),
            "{msg}"
        );
    }

    #[test]
    fn composite_hard_error_short_circuits_and_is_not_absent() {
        // A resolver that errors HARD short-circuits `require` — it is surfaced,
        // NOT skipped as "absent" (so an ill-permissioned file is never masked by a
        // later miss).
        let composite = CompositeSecretResolver::new(vec![
            ("first", Box::new(Fixed(Err(())))),
            ("second", Box::new(Fixed(Ok(Some("late"))))),
        ]);
        let err = composite.require("X").unwrap_err();
        assert!(
            matches!(err, SecretError::FileUnreadable { .. }),
            "a hard error must surface, got {err:?}"
        );
    }

    #[test]
    fn mode_is_owner_only_rule() {
        // The Unix 0600 rule: no group/other bit set.
        assert!(mode_is_owner_only(0o600));
        assert!(mode_is_owner_only(0o400));
        assert!(!mode_is_owner_only(0o644));
        assert!(!mode_is_owner_only(0o640));
        assert!(!mode_is_owner_only(0o601));
    }

    #[test]
    fn file_permissions_error_names_path_and_mode_no_value() {
        let err = file_permissions_error(Path::new("/x/secrets.toml"), 0o644);
        let msg = err.to_string();
        assert!(msg.contains("/x/secrets.toml"), "{msg}");
        assert!(msg.contains("chmod 600"), "{msg}");
    }

    /// Set a temp secrets file to owner-only (0600) on Unix so the permission check
    /// passes; a no-op on Windows (where the check is a documented skip). Uses a
    /// RUNTIME `OsId::current()` gate rather than an OS compile attribute, so this
    /// test module stays free of the OS conditionals the grep gate confines to
    /// `backends/`. Delegates to a `chmod` subprocess (portable across the supported
    /// Unix hosts) instead of importing a Unix-only permissions API.
    fn set_owner_only(path: &Path) {
        if hekma_adapter_api::OsId::current() != hekma_adapter_api::OsId::Windows {
            let _ = std::process::Command::new("chmod")
                .arg("600")
                .arg(path)
                .status();
        }
    }
}
