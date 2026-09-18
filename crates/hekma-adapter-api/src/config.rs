//! The unified→native config MAPPING model (spine AD-3/AD-9, FR-12) — story 2-2.
//!
//! Story 2-1 owns the unified layered-config MODEL (the resolver, the four
//! layers, `KNOWN_KEYS`, the `agent.*` pass-through). This module owns the
//! ADAPTER-DECLARED MAPPING that turns a resolved unified key into the agent's
//! NATIVE mechanism at start: a config FILE, an ENV var, or a CLI FLAG (FR-12's
//! three verbatim mechanisms). The types live HERE (only here) because the
//! Adapter Contract owns every adapter-facing schema (AD-3) — the engine consumes
//! the parsed form and defines no schema of its own.
//!
//! ## Two kinds, one shape (AD-3)
//!
//! * A **manifest** adapter declares its mapping in an optional `[config]` section
//!   of `adapter.toml` (deserialized into [`ConfigMapping`], validated by
//!   [`ConfigMapping::validate`] alongside the other manifest sections).
//! * A **native** adapter (the builtin `mock`, later `hermes`) declares the SAME
//!   [`ConfigMapping`] shape in code, via the
//!   [`AgentAdapter::config_mapping`](crate::AgentAdapter::config_mapping) trait
//!   accessor (default: empty).
//!
//! Both yield the identical [`ConfigMapping`] the engine's start path applies —
//! the "two kinds, one trait" invariant. This module is PURE: it defines the
//! rule shape and how each target RENDERS a value; APPLYING the mapping (mutating
//! the launch env/args, or writing a file into the Agent Home) is the engine's
//! start seam, not this crate's job.
//!
//! ## Explicitly OUT of scope (later Epic-2 stories)
//!
//! * Provenance rendering + the persisted `EffectiveConfig` snapshot — **2-3**.
//! * SECRETS (`secret:` resolution, `SecretString`, masking) — **2-4**. A
//!   `secret:` value is ORDINARY opaque text here, rendered verbatim into the
//!   native mechanism; this module builds NONE of the secret machinery.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The three native delivery mechanisms a unified config key can map to (FR-12).
///
/// Exactly one variant per mapped key. Deserialized from a manifest's
/// `[config.<key>]` sub-table (exactly one of `env`/`flag`/`file` — enforced by
/// `#[serde(untagged)]` + [`ConfigMapping::validate`]) OR constructed in code by a
/// native adapter. `#[serde(deny_unknown_fields)]` on the struct variants catches
/// typos (the repo's manifest style); the `untagged` representation means a
/// sub-table with none of the three (or more than one) native shape simply fails
/// to match a variant, surfacing as a section-naming validation error.
///
/// PURE rendering (no I/O): each variant knows how a value BECOMES its native
/// form. The engine's start seam consumes [`ConfigTarget::render_flag_args`] /
/// [`ConfigTarget::env_var`] / [`ConfigTarget::file_placement`] to place the value;
/// this crate never touches the filesystem or a process.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ConfigTarget {
    /// Deliver as an ENVIRONMENT variable (placed in the spawned process's
    /// environment — the engine writes it into `StartLaunch.env`).
    Env {
        /// The environment variable name (e.g. `MODEL`).
        env: String,
    },
    /// Deliver as a CLI FLAG (appended to the launch arguments — the engine pushes
    /// it onto `StartLaunch.args`). Rendered as TWO arguments — the flag then the
    /// value (`--model` `gpt-4`) — the unambiguous, shell-free form (no `=`
    /// splitting, no quoting rules). `[ASSUMPTION]` recorded (Decision 1): the
    /// separate-token form over `--model=<v>`; a future story can add a joined
    /// variant additively if an agent needs it.
    Flag {
        /// The flag token (e.g. `--model`).
        flag: String,
    },
    /// Deliver into a native config FILE the engine RENDERS into the Agent Home
    /// (the engine is the sole writer — path authority, AD-6/AD-9). The file is a
    /// TOML document at `path` (relative to the Agent Home) with the value set at
    /// the dotted native `key`.
    File(FileTarget),
}

/// A FILE mapping target: a native TOML config file rendered into the Agent Home.
///
/// `path` is a RELATIVE path inside the Agent Home (the engine joins it onto the
/// home and is the sole writer — AD-6). `key` is the dotted native key the value
/// is written at inside that file (so an agent that reads `model = "..."` under a
/// `[llm]` table gets `key = "llm.model"`). Kept a struct (not inline fields on
/// the enum variant) so `#[serde(deny_unknown_fields)]` can reject typos in the
/// `file = { ... }` inline table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileTarget {
    /// The file target wrapper (an inline table `file = { path = "...", key =
    /// "..." }`).
    pub file: FilePlacement,
}

/// The `path` + native `key` inside a [`FileTarget`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePlacement {
    /// The config file's path RELATIVE to the Agent Home (the engine joins +
    /// writes it; a leading `/` or a `..` escape is rejected by
    /// [`ConfigMapping::validate`] so the engine never writes outside the home).
    pub path: String,
    /// The dotted native key to set the value at inside the rendered file.
    pub key: String,
}

impl ConfigTarget {
    /// A native-`env`-var target (code-declared convenience for native adapters).
    pub fn env(var: impl Into<String>) -> Self {
        ConfigTarget::Env { env: var.into() }
    }

    /// A native-`flag` target (code-declared convenience for native adapters).
    pub fn flag(flag: impl Into<String>) -> Self {
        ConfigTarget::Flag { flag: flag.into() }
    }

    /// A native-`file` target (code-declared convenience for native adapters).
    pub fn file(path: impl Into<String>, key: impl Into<String>) -> Self {
        ConfigTarget::File(FileTarget {
            file: FilePlacement {
                path: path.into(),
                key: key.into(),
            },
        })
    }

    /// The env var name, if this is an [`ConfigTarget::Env`] target.
    pub fn env_var(&self) -> Option<&str> {
        match self {
            ConfigTarget::Env { env } => Some(env),
            _ => None,
        }
    }

    /// The two flag arguments (`[flag, value]`) for a [`ConfigTarget::Flag`]
    /// target, or `None` for the other kinds. The separate-token form (Decision 1)
    /// — the engine appends both to `StartLaunch.args` in order.
    pub fn render_flag_args(&self, value: &str) -> Option<[String; 2]> {
        match self {
            ConfigTarget::Flag { flag } => Some([flag.clone(), value.to_string()]),
            _ => None,
        }
    }

    /// The [`FilePlacement`] (path + native key), if this is a
    /// [`ConfigTarget::File`] target. The engine renders the value into that file
    /// inside the Agent Home.
    pub fn file_placement(&self) -> Option<&FilePlacement> {
        match self {
            ConfigTarget::File(target) => Some(&target.file),
            _ => None,
        }
    }

    /// A stable label for the target KIND (diagnostics / tests).
    pub fn kind_str(&self) -> &'static str {
        match self {
            ConfigTarget::Env { .. } => "env",
            ConfigTarget::Flag { .. } => "flag",
            ConfigTarget::File(_) => "file",
        }
    }
}

/// The adapter-declared config MAPPING (FR-12): documented unified key →
/// [`ConfigTarget`] native mechanism.
///
/// A [`BTreeMap`] keyed by the DOTTED unified key (`model`, …) for deterministic
/// iteration (same order on every machine — the "same inputs → same launch"
/// property the start seam relies on). Deserialized from a manifest `[config]`
/// section (each `[config.<key>]` sub-table is one [`ConfigTarget`]) or built in
/// code by a native adapter. An EMPTY mapping is valid and normal — most simple
/// agents map no unified keys (Decision 6: an unmapped key is a silent no-op).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct ConfigMapping {
    rules: BTreeMap<String, ConfigTarget>,
}

impl ConfigMapping {
    /// An empty mapping (the "maps no unified keys" case — valid, common).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) a rule for `key`, returning `self` (builder style, for
    /// native adapters declaring their mapping in code — mirrors
    /// [`CapabilityDeclaration::with`](crate::CapabilityDeclaration::with)).
    pub fn with(mut self, key: impl Into<String>, target: ConfigTarget) -> Self {
        self.rules.insert(key.into(), target);
        self
    }

    /// The [`ConfigTarget`] declared for `key`, if any. `None` means the adapter
    /// declares no rule for that unified key — the start seam delivers it NOWHERE
    /// (a no-op, Decision 6).
    pub fn target(&self, key: &str) -> Option<&ConfigTarget> {
        self.rules.get(key)
    }

    /// Iterate the rules (dotted unified key → target), sorted by key.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ConfigTarget)> {
        self.rules.iter()
    }

    /// Whether the mapping declares no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The number of declared rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Validate a `[config]` mapping (story 2-2, AC3) — used by
    /// [`Manifest::validate`](crate::Manifest::validate) when the section is
    /// present. Returns the first offending `(key, reason)`:
    ///
    /// * an EMPTY unified key, or an empty native `env`/`flag`/`file.key` token,
    ///   is rejected (a malformed rule);
    /// * a FILE `path` that is absolute or escapes the Agent Home (contains a `..`
    ///   segment, or starts with `/`) is rejected — the engine is the sole writer
    ///   inside the home and must never be steered outside it (AD-6).
    ///
    /// A mapping that names an unknown NATIVE mechanism (neither env/flag/file, or
    /// more than one) never reaches here: `#[serde(untagged)]` fails to match a
    /// variant at PARSE time, which the manifest loader surfaces as a section
    /// error. This validates the SEMANTIC rules parsing cannot.
    pub fn validate(&self) -> Result<(), (String, String)> {
        for (key, target) in &self.rules {
            if key.trim().is_empty() {
                return Err((key.clone(), "the unified config key is empty".to_string()));
            }
            match target {
                ConfigTarget::Env { env } => {
                    if env.trim().is_empty() {
                        return Err((key.clone(), "the `env` var name is empty".to_string()));
                    }
                }
                ConfigTarget::Flag { flag } => {
                    if flag.trim().is_empty() {
                        return Err((key.clone(), "the `flag` is empty".to_string()));
                    }
                }
                ConfigTarget::File(target) => {
                    let placement = &target.file;
                    if placement.path.trim().is_empty() {
                        return Err((key.clone(), "the `file.path` is empty".to_string()));
                    }
                    if placement.key.trim().is_empty() {
                        return Err((key.clone(), "the `file.key` is empty".to_string()));
                    }
                    if !is_safe_relative_path(&placement.path) {
                        return Err((
                            key.clone(),
                            format!(
                                "the `file.path` '{}' must be RELATIVE to the Agent Home (no \
                                 leading '/', no '..' segment)",
                                placement.path
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Whether `path` is a safe, NORMALIZED RELATIVE path inside the Agent Home, so
/// the engine (the sole writer) can never be steered outside the home — AD-6. The
/// check is path-agnostic `std` (a `\` is treated as a separator on ANY host,
/// which is the stricter choice), so no OS-conditional compilation is needed and
/// the OS-cfg gate stays green. Rejected as unsafe:
///
/// * an EMPTY path;
/// * a Unix-absolute path (leading `/` or `\`);
/// * a WINDOWS drive-letter or UNC-ish prefix — a first segment like `C:` /
///   `C:foo` (drive-relative) or a leading `\\` UNC (already caught by the
///   leading-`\` rule): a `<letter>:` first segment is rejected explicitly, since
///   `C:\x` and even the drive-relative `C:x` escape the home;
/// * any `..` segment (a parent-directory traversal);
/// * any EMPTY segment (`a//b`, a trailing or leading separator) — a malformed
///   path that would render ambiguously;
/// * any bare `.` segment (`.`, `./a`, `a/./b`) — a no-op current-dir segment
///   that indicates an un-normalized path (a leading-DOT filename like `.agents`
///   is fine — only a segment that is EXACTLY `.` is rejected).
fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return false;
    }
    let mut segments = path.split(['/', '\\']);
    // Reject a Windows drive-letter FIRST segment (`C:` or `C:foo`): a single
    // ASCII letter followed by `:` names a drive, which escapes the home whether
    // drive-absolute (`C:\x`) or drive-relative (`C:x`).
    if let Some(first) = path.split(['/', '\\']).next() {
        let mut chars = first.chars();
        if matches!(chars.next(), Some(c) if c.is_ascii_alphabetic()) && chars.next() == Some(':') {
            return false;
        }
    }
    // Every segment must be a plain name: non-empty, and neither `.` nor `..`.
    !segments.any(|segment| segment.is_empty() || segment == "." || segment == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ConfigTarget rendering (each of the three targets) ----

    #[test]
    fn env_target_renders_the_var_name() {
        let t = ConfigTarget::env("MODEL");
        assert_eq!(t.env_var(), Some("MODEL"));
        assert_eq!(t.render_flag_args("gpt-4"), None);
        assert_eq!(t.file_placement(), None);
        assert_eq!(t.kind_str(), "env");
    }

    #[test]
    fn flag_target_renders_two_args() {
        let t = ConfigTarget::flag("--model");
        assert_eq!(
            t.render_flag_args("gpt-4"),
            Some(["--model".to_string(), "gpt-4".to_string()])
        );
        assert_eq!(t.env_var(), None);
        assert_eq!(t.kind_str(), "flag");
    }

    #[test]
    fn file_target_renders_path_and_key() {
        let t = ConfigTarget::file("config/agent.toml", "llm.model");
        let placement = t.file_placement().unwrap();
        assert_eq!(placement.path, "config/agent.toml");
        assert_eq!(placement.key, "llm.model");
        assert_eq!(t.env_var(), None);
        assert_eq!(t.render_flag_args("x"), None);
        assert_eq!(t.kind_str(), "file");
    }

    // ---- ConfigMapping builder + accessors ----

    #[test]
    fn mapping_builder_and_accessors() {
        let m = ConfigMapping::new()
            .with("model", ConfigTarget::env("MODEL"))
            .with("agent.foo", ConfigTarget::flag("--foo"));
        assert_eq!(m.len(), 2);
        assert!(!m.is_empty());
        assert_eq!(m.target("model").unwrap().env_var(), Some("MODEL"));
        assert_eq!(
            m.target("agent.foo").unwrap().render_flag_args("bar"),
            Some(["--foo".to_string(), "bar".to_string()])
        );
        assert!(m.target("nope").is_none());

        // Deterministic (sorted) iteration.
        let keys: Vec<&String> = m.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["agent.foo", "model"]);
    }

    #[test]
    fn empty_mapping_is_valid_and_default() {
        let m = ConfigMapping::default();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert!(m.validate().is_ok());
        assert!(m.target("model").is_none());
    }

    // ---- Deserialization from the manifest `[config]` shape (round-trip) ----

    #[test]
    fn deserializes_each_target_kind_from_toml() {
        // The `[config]` section shape: per-key sub-tables, exactly one native
        // mechanism each.
        let toml = r#"
[model]
env = "MODEL"

[temperature]
flag = "--temp"

[seed]
file = { path = "config/agent.toml", key = "llm.seed" }
"#;
        let mapping: ConfigMapping = toml::from_str(toml).expect("parse mapping");
        assert_eq!(mapping.len(), 3);
        assert_eq!(
            mapping.target("model").unwrap(),
            &ConfigTarget::env("MODEL")
        );
        assert_eq!(
            mapping.target("temperature").unwrap(),
            &ConfigTarget::flag("--temp")
        );
        assert_eq!(
            mapping.target("seed").unwrap(),
            &ConfigTarget::file("config/agent.toml", "llm.seed")
        );
        assert!(mapping.validate().is_ok());
    }

    #[test]
    fn a_rule_with_no_native_mechanism_fails_to_parse() {
        // A sub-table with none of env/flag/file matches no untagged variant.
        let toml = r#"
[model]
bogus = "x"
"#;
        assert!(toml::from_str::<ConfigMapping>(toml).is_err());
    }

    #[test]
    fn a_file_rule_with_unknown_field_is_rejected() {
        // deny_unknown_fields on FilePlacement catches a typo'd file sub-field.
        let toml = r#"
[model]
file = { path = "a.toml", key = "k", extra = 1 }
"#;
        assert!(toml::from_str::<ConfigMapping>(toml).is_err());
    }

    // ---- validate(): semantic rules parsing cannot catch ----

    #[test]
    fn validate_rejects_an_empty_unified_key() {
        // A rule keyed by an empty (or whitespace) unified key is malformed.
        let m = ConfigMapping::new().with("  ", ConfigTarget::env("MODEL"));
        let (key, reason) = m.validate().unwrap_err();
        assert_eq!(key, "  ");
        assert!(reason.contains("unified config key is empty"), "{reason}");
    }

    #[test]
    fn validate_rejects_an_empty_env_var() {
        let m = ConfigMapping::new().with("model", ConfigTarget::env(""));
        let (key, reason) = m.validate().unwrap_err();
        assert_eq!(key, "model");
        assert!(reason.contains("env"), "{reason}");
    }

    #[test]
    fn validate_rejects_an_empty_flag() {
        let m = ConfigMapping::new().with("model", ConfigTarget::flag("   "));
        let (key, reason) = m.validate().unwrap_err();
        assert_eq!(key, "model");
        assert!(reason.contains("flag"), "{reason}");
    }

    #[test]
    fn validate_rejects_an_empty_file_path_or_key() {
        let m = ConfigMapping::new().with("model", ConfigTarget::file("", "k"));
        assert!(m.validate().unwrap_err().1.contains("file.path"));
        let m = ConfigMapping::new().with("model", ConfigTarget::file("a.toml", ""));
        assert!(m.validate().unwrap_err().1.contains("file.key"));
    }

    #[test]
    fn validate_rejects_an_absolute_or_escaping_file_path() {
        // Absolute (Unix + Windows drive), `..`-escaping, empty-segment, and bare
        // `.`-segment paths are all rejected so the engine never writes outside the
        // Agent Home.
        for bad in [
            "/etc/passwd",
            "\\windows\\system32",
            "../escape.toml",
            "a/../../b",
            "C:\\windows\\x",
            "C:x",
            "a//b",
            "a/./b",
            "./a",
            ".",
        ] {
            let m = ConfigMapping::new().with("model", ConfigTarget::file(bad, "k"));
            let (_, reason) = m.validate().unwrap_err();
            assert!(reason.contains("RELATIVE"), "path {bad:?}: {reason}");
        }
        // Normal nested relative paths (including leading-dot dir names) are fine.
        for good in ["config/agent.toml", ".agents/skills/x", "config/model.txt"] {
            let m = ConfigMapping::new().with("model", ConfigTarget::file(good, "k"));
            assert!(m.validate().is_ok(), "path {good:?} should be safe");
        }
    }

    #[test]
    fn is_safe_relative_path_matrix() {
        // Safe: plain relative paths, including a leading-DOT filename segment
        // (`.agents` is a dotfile name, NOT a bare `.` current-dir segment).
        assert!(is_safe_relative_path("agent.toml"));
        assert!(is_safe_relative_path("config/agent.toml"));
        assert!(is_safe_relative_path("a/b/c.toml"));
        assert!(is_safe_relative_path(".agents/skills/x"));
        assert!(is_safe_relative_path("config/model.txt"));
        assert!(is_safe_relative_path(".hidden"));
        // Unsafe: empty, Unix-absolute, Windows drive (absolute + drive-relative),
        // UNC, `..` traversal, empty segments, and bare `.` segments.
        assert!(!is_safe_relative_path(""));
        assert!(!is_safe_relative_path("/abs.toml"));
        assert!(!is_safe_relative_path("\\abs.toml"));
        assert!(!is_safe_relative_path("\\\\server\\share")); // UNC
        assert!(!is_safe_relative_path("C:\\windows\\x")); // drive-absolute
        assert!(!is_safe_relative_path("C:x")); // drive-relative
        assert!(!is_safe_relative_path("z:/x")); // any ASCII-letter drive
        assert!(!is_safe_relative_path("../up.toml"));
        assert!(!is_safe_relative_path("a/../b.toml"));
        assert!(!is_safe_relative_path("a//b")); // empty segment
        assert!(!is_safe_relative_path("a/")); // trailing separator → empty segment
        assert!(!is_safe_relative_path(".")); // bare current-dir
        assert!(!is_safe_relative_path("./a")); // leading current-dir segment
        assert!(!is_safe_relative_path("a/./b")); // interior current-dir segment
    }
}
