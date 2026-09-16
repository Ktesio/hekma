//! The `adapter.toml` manifest schema + validation (spine AD-3 — OWNED HERE).
//!
//! The manifest-adapter schema's serde types and [`Manifest::validate`] live
//! **only** in this crate, versioned under the same Adapter Contract semver
//! ([`crate::CONTRACT_VERSION`]). The engine parses `adapter.toml` exclusively
//! through [`Manifest::from_toml_str`] and defines no schema of its own.
//!
//! ## Mandatory sections (the AC2 seed)
//!
//! A manifest is valid only if it declares, and [`Manifest::validate`] names the
//! first one that is missing/empty/invalid:
//!
//! * `contract_version` — the Adapter Contract version the manifest targets.
//! * `[adapter]` identity — `kind` (and optional `name`).
//! * `[lifecycle]` — at least a `start` op template (`exec`, optional `args`/`env`).
//! * `[capabilities]` — a non-empty per-OS Capability Declaration (AD-4).
//! * `[metering]` — a viable Metering Source (AD-7; the FR-19 hard line, AC4).
//!
//! `#[serde(deny_unknown_fields)]` (the repo's manifest style) additionally
//! rejects unknown keys, catching typos.
//!
//! ## Not executed this story
//!
//! The lifecycle templates are stored, never run. The manifest executor / process
//! launch is story 1-4.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::capability::CapabilityDeclaration;
use crate::config::ConfigMapping;
use crate::metering::MeteringSource;
use crate::STRICT_SEMVER_REQUIREMENT;

/// The parsed `adapter.toml` (spine AD-3 manifest schema).
///
/// Fields are `Option` where a section is mandatory-by-validation rather than
/// mandatory-by-deserialization, so a missing section yields a precise,
/// section-naming [`ManifestError`] from [`Manifest::validate`] instead of an
/// opaque serde error. Unknown keys are rejected (`deny_unknown_fields`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The Adapter Contract version this manifest targets (e.g. `"1.0.0"`).
    ///
    /// A STRICT `X.Y.Z` semver version (AI-6, resolved at the 6-6 freeze): no
    /// `v` prefix and no partial versions (`1`, `1.0`) — those fail
    /// [`Manifest::validate`] naming the field. A prerelease/build suffix
    /// (`1.0.0-rc.1+build.5`) parses as semver and negotiates by MAJOR only
    /// (see `crate::negotiate_contract_version`); the versioning policy at
    /// `docs/adapter-contract.md#versioning` states the full stance.
    pub contract_version: Option<String>,
    /// Adapter identity (`kind`, optional `name`).
    pub adapter: Option<AdapterIdentity>,
    /// Lifecycle op templates (`start`/`stop`/`pause`/`resume`).
    pub lifecycle: Option<Lifecycle>,
    /// Per-OS Capability Declaration (AD-4). Reuses the contract type directly.
    pub capabilities: Option<CapabilityDeclaration>,
    /// Metering configuration (AD-7).
    pub metering: Option<Metering>,
    /// Interaction wiring (channel). Optional this story (defaults documented).
    pub interaction: Option<Interaction>,
    /// The unified→native config MAPPING (story 2-2, FR-12). OPTIONAL: an adapter
    /// that maps no unified keys omits it entirely (an empty mapping is valid and
    /// common). Each `[config.<key>]` sub-table declares one native mechanism
    /// (`env`/`flag`/`file`). Validated by [`Manifest::validate`] when present.
    pub config: Option<ConfigMapping>,
}

/// The `[adapter]` identity block.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterIdentity {
    /// The adapter kind (stored on the Agent Instance; e.g. `"hermes"`).
    pub kind: String,
    /// An optional human-friendly name.
    pub name: Option<String>,
}

/// The `[lifecycle]` block: one [`OpTemplate`] per lifecycle op.
///
/// `start` is the minimum this story requires; the others are optional seeds
/// that 1-4's executor will consume. `[ASSUMPTION]` on requiring only `start`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lifecycle {
    /// The start op template (required by [`Manifest::validate`]).
    pub start: Option<OpTemplate>,
    /// The stop op template.
    pub stop: Option<OpTemplate>,
    /// The pause op template.
    pub pause: Option<OpTemplate>,
    /// The resume op template.
    pub resume: Option<OpTemplate>,
}

/// A single lifecycle op's exec/args/env template (carried, not executed).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpTemplate {
    /// The executable to run (a path or program name).
    pub exec: String,
    /// Positional arguments (defaults to empty).
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment overrides (defaults to empty).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// The `[metering]` block.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metering {
    /// The declared Metering Source. An unknown value fails deserialization,
    /// which [`Manifest::from_toml_str`] surfaces as a section-naming error.
    pub source: MeteringSource,
}

/// The declared interaction channel kind (spine AD-12, story 4.1; vocabulary
/// extended at the 6-6 freeze via CP-6.5-a option (i)).
///
/// A CLOSED set: the manifest's `[interaction]` `channel` is validated at
/// PARSE time (an unrecognized value is REJECTED, mirroring
/// [`crate::MeteringSource`]'s closed-enum handling), rather than merely
/// carried as a free-form string. The serde wire form is kebab-case so an
/// `adapter.toml` `[interaction]` section reads naturally:
///
/// ```toml
/// [interaction]
/// channel = "stdio"
/// ```
///
/// Two variants as of contract v1 (story 6-6, CP-6.5-a ratified option (i)):
///
/// * [`InteractionChannelKind::Stdio`] — the spawned child's OS stdin pipe.
/// * [`InteractionChannelKind::Http`] — an HTTP-native interaction surface
///   (e.g. an agent exposing `POST /session/:id/message` over a loopback
///   server). Declaring it is DOCUMENTARY vocabulary: it names the adapter's
///   real transport so a manifest is honest about where interaction happens,
///   and it lets an HTTP-native agent declare `interaction` supported instead
///   of being forced into an unregisterable all-unsupported declaration.
///   v1 ships NO engine-side HTTP delivery: the engine never branches on the
///   declared channel — the AD-12 stdin pipe stays unconditional, and a
///   `send_input` on an Http-declared adapter still writes the child's stdin
///   (failing fast typed when unsupported). A real HTTP `send_input`
///   implementation is a post-v1 change (R1's deferred TCK leg), and it would
///   arrive under the published policy at `docs/adapter-contract.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InteractionChannelKind {
    /// The spawned child's OS stdin pipe — the channel the engine actually
    /// delivers through today.
    Stdio,
    /// An HTTP-native interaction surface (CP-6.5-a option (i), v1
    /// vocabulary). Documentary in v1: the engine does not branch on it, and
    /// no engine-side HTTP send implementation exists yet.
    Http,
}

impl InteractionChannelKind {
    /// The kebab-case wire name, matching the serde form and manifest value.
    pub fn as_str(&self) -> &'static str {
        match self {
            InteractionChannelKind::Stdio => "stdio",
            InteractionChannelKind::Http => "http",
        }
    }
}

impl std::fmt::Display for InteractionChannelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `[interaction]` block (channel wiring).
///
/// OPTIONAL (unchanged): an adapter that omits `[interaction]` entirely still
/// gets the AD-12 default — the engine unconditionally pipes stdin for every
/// spawned process (story 4.1 Task 1), regardless of what (or whether) this
/// section says. Declaring the section only firms up, documentarily, WHICH
/// channel the adapter author expects; the engine does not branch on it (the
/// AD-12 stdin pipe is unconditional for every spawn).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interaction {
    /// The interaction channel. A closed enum (story 4.1) — an unrecognized
    /// value is rejected at PARSE time rather than silently accepted.
    pub channel: InteractionChannelKind,
}

/// Why a manifest is invalid (spine AD-3; `thiserror`, never `miette`).
///
/// Every message NAMES the failing section or the invalid value (AC2), so `kt`
/// can render a diagnostic quoting it and the engine can map it to a
/// `RegistryError` variant.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The TOML failed to parse (syntax error, or a section held the wrong
    /// type / an unknown key). Carries the parser's message.
    #[error("adapter.toml is not valid TOML: {detail}")]
    Toml {
        /// The underlying parser message.
        detail: String,
    },

    /// A mandatory section (or field) was missing or empty. Names it.
    #[error("adapter.toml is missing the required {section}")]
    MissingSection {
        /// A human phrase naming the section, e.g. "`[metering]` section" or
        /// "`contract_version` field".
        section: String,
    },

    /// A present field held a syntactically invalid value (e.g. a
    /// `contract_version` that is not semver, or a `kind` that breaks the
    /// adapter-kind charset). Names the field and why it is invalid.
    #[error("adapter.toml has an invalid {field}: {detail}")]
    InvalidField {
        /// A human phrase naming the field, e.g. "`contract_version`" or
        /// "`kind` field in the `[adapter]` section".
        field: String,
        /// Why the value is invalid.
        detail: String,
    },
}

impl ManifestError {
    /// Convenience constructor naming a missing section.
    fn missing(section: impl Into<String>) -> Self {
        ManifestError::MissingSection {
            section: section.into(),
        }
    }
}

impl Manifest {
    /// Parse an `adapter.toml` document into a [`Manifest`].
    ///
    /// Only parses (syntax + shape + unknown-key rejection). Presence of the
    /// mandatory sections is enforced separately by [`Manifest::validate`], so
    /// callers get a precise section-naming error rather than an opaque serde
    /// message. A bad `source`/`support-level`/OS value fails here (wrong type
    /// for the field) and is reported as a TOML error naming the position.
    pub fn from_toml_str(input: &str) -> Result<Self, ManifestError> {
        toml::from_str(input).map_err(|e| {
            let message = e.message().to_string();
            // Two enums in the schema reject an out-of-range value at PARSE
            // time: the `[metering]` source and (story 4.1) the
            // `[interaction]` channel. `toml` reports each as "unknown
            // variant `x`, expected ..." without naming the section; attribute
            // it to the right section so the diagnostic names it (AC2/AC4),
            // matching every other rejection.
            //
            // M3 fix (review of #79): disambiguate using the message's
            // "expected ..." clause — the FAILING FIELD's own valid values —
            // never by whether the OFFENDING value happens to ALSO be a
            // valid variant name of the OTHER enum. The original logic
            // string-matched the whole message against both enums' variant
            // names, so `[interaction] channel = "self-reported"` (an
            // INVALID channel — the only valid one is "stdio" — that also
            // happens to be a valid MeteringSource variant NAME) produced
            // the message `unknown variant \`self-reported\`, expected
            // \`stdio\`` and was misattributed to `[metering]`, because
            // "self-reported" appears in the message (as the REJECTED
            // value), even though `[metering]`'s own valid values
            // ("self-reported" / "engine-observed") are never what's
            // "expected" here. Splitting on "expected" and checking only
            // the tail (the field's own valid-values clause) makes this
            // robust regardless of what the offending value happens to spell.
            let expected_clause = message
                .split_once("expected")
                .map(|(_, rest)| rest)
                .unwrap_or("");
            let detail = if !message.starts_with("unknown variant") {
                message
            } else if expected_clause.contains("self-reported")
                || expected_clause.contains("engine-observed")
            {
                format!("the `[metering]` section has an invalid `source`: {message}")
            } else if expected_clause.contains("stdio") {
                format!("the `[interaction]` section has an invalid `channel`: {message}")
            } else {
                message
            };
            ManifestError::Toml { detail }
        })
    }

    /// Enforce the mandatory sections (AC2) and a viable Metering Source (AC4).
    ///
    /// Returns the first failure, its message NAMING the missing/invalid
    /// section. Order is stable (contract version → adapter identity → lifecycle
    /// → capabilities → metering) so diagnostics are deterministic.
    pub fn validate(&self) -> Result<(), ManifestError> {
        // contract_version present, non-empty, AND a real semver version (the
        // negotiation in `crate::negotiate_contract_version` builds on it). The
        // parse is STRICT `X.Y.Z` (AI-6, resolved at the 6-6 freeze): no `v`
        // prefix, no partial versions — the error documents the requirement
        // because the field doc alone is not what an operator sees when a load
        // fails. The error names the field.
        match &self.contract_version {
            Some(v) if !v.trim().is_empty() => {
                if semver::Version::parse(v.trim()).is_err() {
                    return Err(ManifestError::InvalidField {
                        field: "`contract_version`".to_string(),
                        // The requirement clause interpolates the SHARED
                        // `STRICT_SEMVER_REQUIREMENT` const — the same single
                        // source `ContractVersionError::Unparseable` quotes —
                        // so the two identical rejections cannot drift apart.
                        detail: format!(
                            "'{v}' is not a valid semver version ({STRICT_SEMVER_REQUIREMENT})"
                        ),
                    });
                }
            }
            _ => return Err(ManifestError::missing("`contract_version` field")),
        }

        // [adapter] identity with a non-empty kind that obeys the adapter-kind
        // charset rule (`^[a-z0-9][a-z0-9_-]*$`). A kind with whitespace,
        // newlines, or other punctuation would later corrupt DB/CLI tables and
        // the 1-4 launch, so it is rejected here naming the field.
        match &self.adapter {
            Some(a) if !a.kind.trim().is_empty() => {
                if !is_valid_adapter_kind(&a.kind) {
                    return Err(ManifestError::InvalidField {
                        field: "`kind` field in the `[adapter]` section".to_string(),
                        detail: format!(
                            "'{}' must match ^[a-z0-9][a-z0-9_-]*$ (lowercase letters, digits, \
                             '_' or '-', not starting with '_' or '-')",
                            a.kind
                        ),
                    });
                }
            }
            Some(_) => {
                return Err(ManifestError::missing(
                    "`kind` field in the `[adapter]` section",
                ))
            }
            None => return Err(ManifestError::missing("`[adapter]` section")),
        }

        // [lifecycle] with at least a start op (the minimum this story requires).
        match &self.lifecycle {
            Some(l) if l.start.is_some() => {}
            Some(_) => {
                return Err(ManifestError::missing(
                    "`start` op in the `[lifecycle]` section",
                ))
            }
            None => return Err(ManifestError::missing("`[lifecycle]` section")),
        }

        // [capabilities] declares real support (AD-4). Not merely non-empty: a
        // capability key with an empty per-OS map, or one whose every entry is
        // `unsupported`, promises support nowhere and is rejected — an adapter
        // that supports nothing is not viable.
        match &self.capabilities {
            Some(c) if c.has_any_support() => {}
            _ => {
                return Err(ManifestError::missing(
                    "`[capabilities]` section (it declares no supported capabilities)",
                ))
            }
        }

        // [metering] with a viable source (AC4 / FR-19 hard line). The source
        // type only holds viable kinds, so its mere presence proves viability;
        // an invalid value would already have failed to parse.
        if self.metering.is_none() {
            return Err(ManifestError::missing("`[metering]` section"));
        }

        // [config] mapping (story 2-2, FR-12), IF present. Absent is valid (an
        // adapter that maps no unified keys — the common case). A present-but-
        // malformed rule (an empty native token, or a `file.path` that is
        // absolute / escapes the Agent Home) is an InvalidField naming the
        // `[config.<key>]` sub-section. A rule that names an unknown/ambiguous
        // native mechanism never reaches here — `#[serde(untagged)]` fails to
        // match a variant at PARSE time (surfaced as a section error by
        // `from_toml_str`), like every other typed field.
        if let Some(mapping) = &self.config {
            if let Err((key, detail)) = mapping.validate() {
                return Err(ManifestError::InvalidField {
                    field: format!("`[config.{key}]` mapping in the `[config]` section"),
                    detail,
                });
            }
        }

        Ok(())
    }

    /// The declared Capability Declaration (present only after [`Self::validate`]).
    ///
    /// Returns a reference to the parsed declaration, or an empty one if absent
    /// (validation rejects that case, so a validated manifest always has a real
    /// declaration here).
    pub fn capability_declaration(&self) -> CapabilityDeclaration {
        self.capabilities.clone().unwrap_or_default()
    }

    /// The declared Metering Source (present only after [`Self::validate`]).
    ///
    /// Returns `None` if absent (validation rejects that case).
    pub fn metering_source(&self) -> Option<MeteringSource> {
        self.metering.as_ref().map(|m| m.source)
    }

    /// The adapter kind declared in `[adapter]` (present after validation).
    pub fn adapter_kind(&self) -> Option<&str> {
        self.adapter.as_ref().map(|a| a.kind.as_str())
    }

    /// The declared unified→native config [`ConfigMapping`] (story 2-2, FR-12).
    ///
    /// Returns the parsed `[config]` mapping, or an EMPTY mapping when the section
    /// is absent — so the engine's start seam treats "no `[config]`" as "maps no
    /// unified keys" (a no-op) uniformly with a native adapter that declares no
    /// mapping. Mirrors [`Self::capability_declaration`]'s empty-when-absent shape.
    pub fn config_mapping(&self) -> ConfigMapping {
        self.config.clone().unwrap_or_default()
    }
}

/// The adapter-kind charset rule: `^[a-z0-9][a-z0-9_-]*$`.
///
/// A `kind` is stored on the Agent Instance, rendered in CLI tables, and (in
/// 1-4) used to select a launch target, so it must be a safe token: lowercase
/// letters, digits, `_` or `-`, not starting with `_`/`-`, and never containing
/// whitespace, newlines, or other punctuation. This mirrors the `InstanceName`
/// rule (spine Consistency Conventions), which native builtin kinds already
/// satisfy (`mock`, `hermes`).
pub(crate) fn is_valid_adapter_kind(kind: &str) -> bool {
    let mut chars = kind.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, SupportLevel};
    use crate::os::OsId;

    /// A complete, valid manifest covering every section.
    const VALID: &str = r#"
contract_version = "1.0.0"

[adapter]
kind = "demo"
name = "Demo Agent"

[lifecycle.start]
exec = "demo-agent"
args = ["--serve"]
env = { DEMO_MODE = "1" }

[lifecycle.stop]
exec = "demo-agent"
args = ["--shutdown"]

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[capabilities.interaction]
linux = "guaranteed"

[metering]
source = "self-reported"

[interaction]
channel = "stdio"
"#;

    #[test]
    fn happy_path_parses_and_validates() {
        let m = Manifest::from_toml_str(VALID).expect("parse");
        m.validate().expect("validate");

        assert_eq!(m.adapter_kind(), Some("demo"));
        assert_eq!(m.metering_source(), Some(MeteringSource::SelfReported));

        let decl = m.capability_declaration();
        assert_eq!(decl.len(), 2);
        assert_eq!(
            decl.support(Capability::Pause, OsId::Windows),
            SupportLevel::BestEffort
        );

        // Lifecycle templates are carried (not executed).
        let lc = m.lifecycle.as_ref().unwrap();
        let start = lc.start.as_ref().unwrap();
        assert_eq!(start.exec, "demo-agent");
        assert_eq!(start.args, vec!["--serve".to_string()]);
        assert_eq!(start.env.get("DEMO_MODE").map(String::as_str), Some("1"));
        assert!(lc.stop.is_some());
        assert_eq!(
            m.interaction.as_ref().unwrap().channel,
            InteractionChannelKind::Stdio
        );
    }

    #[test]
    fn missing_contract_version_names_the_field() {
        let toml = VALID.replace("contract_version = \"1.0.0\"\n", "");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::MissingSection { section } if section.contains("contract_version")),
            "got {err}"
        );
    }

    #[test]
    fn empty_contract_version_is_rejected() {
        let toml = VALID.replace("contract_version = \"1.0.0\"", "contract_version = \"\"");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("contract_version"), "{err}");
    }

    #[test]
    fn non_semver_contract_version_is_rejected_naming_the_field() {
        // F5: a present, non-empty but non-semver contract_version is rejected
        // (parsed with semver::Version::parse), naming the field.
        let toml = VALID.replace(
            "contract_version = \"1.0.0\"",
            "contract_version = \"banana\"",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. } if field.contains("contract_version")),
            "got {err}"
        );
        assert!(err.to_string().contains("banana"), "got {err}");
    }

    #[test]
    fn contract_version_strict_parse_edges_are_rejected_with_the_requirement_documented() {
        // AI-6, resolved at the 6-6 freeze: the parse stays STRICT `X.Y.Z` —
        // `1` and `1.0` are not versions, `v1.0.0` is not the field's grammar.
        // Each rejection's detail STATES the requirement (an operator reading
        // the diagnostic learns the rule without opening the docs).
        for bad in ["1", "1.0", "v1.0.0"] {
            let toml = VALID.replace(
                "contract_version = \"1.0.0\"",
                &format!("contract_version = \"{bad}\""),
            );
            let m = Manifest::from_toml_str(&toml).expect("parse");
            let err = m.validate().unwrap_err();
            assert!(
                matches!(&err, ManifestError::InvalidField { field, .. } if field.contains("contract_version")),
                "{bad:?}: got {err}"
            );
            let text = err.to_string();
            assert!(text.contains(bad), "{bad:?}: {text}");
            assert!(
                text.contains(crate::STRICT_SEMVER_REQUIREMENT),
                "{bad:?}: the diagnostic must quote the SHARED strict-parse requirement \
                 verbatim (drift check — the same const backs                  `ContractVersionError::Unparseable`): {text}"
            );
        }
    }

    #[test]
    fn contract_version_prerelease_and_build_metadata_parse_and_validate() {
        // AI-6's documented stance: `semver::Version::parse` accepts
        // prerelease/build-metadata suffixes, and negotiation compares majors
        // only — so a same-major prerelease manifest is a VALID manifest here
        // (the ENGINE's registration negotiation decides compatibility; see
        // `crate::negotiate_contract_version`'s tests).
        for good in ["1.0.0-rc.1", "1.2.3+build.7", "1.0.0-beta.2+meta"] {
            let toml = VALID.replace(
                "contract_version = \"1.0.0\"",
                &format!("contract_version = \"{good}\""),
            );
            let m = Manifest::from_toml_str(&toml).expect("parse");
            m.validate()
                .unwrap_or_else(|e| panic!("{good} must validate: {e}"));
        }
    }

    #[test]
    fn invalid_adapter_kind_charset_is_rejected_naming_the_field() {
        // F6: a kind with whitespace/tab/newline/uppercase is rejected by the
        // adapter-kind charset rule, naming the field. (An empty kind is caught
        // separately by the missing-field arm — see empty_adapter_kind_*.)
        for bad in [
            "a\tb",
            "a b",
            "a\nb",
            "Demo",
            "_leading",
            "-leading",
            "has.dot",
            "camelCase",
        ] {
            let toml = VALID.replace("kind = \"demo\"", &format!("kind = {bad:?}"));
            let m =
                Manifest::from_toml_str(&toml).unwrap_or_else(|e| panic!("parse for {bad:?}: {e}"));
            let err = m.validate().unwrap_err();
            assert!(
                matches!(&err, ManifestError::InvalidField { field, .. } if field.contains("kind")),
                "kind={bad:?} got {err}"
            );
        }
    }

    #[test]
    fn valid_adapter_kinds_pass_the_charset() {
        // Native builtin kinds and typical manifest kinds obey the rule.
        for good in ["mock", "hermes", "demo-manifest", "a", "a1_b-2", "0abc"] {
            assert!(
                super::is_valid_adapter_kind(good),
                "expected {good:?} to be a valid adapter kind"
            );
        }
        for bad in ["", "A", "_x", "-x", "a b", "a\tb", "a.b", "naïve"] {
            assert!(
                !super::is_valid_adapter_kind(bad),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn missing_adapter_section_names_it() {
        let toml = VALID.replace("[adapter]\nkind = \"demo\"\nname = \"Demo Agent\"\n", "");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("[adapter]"), "{err}");
    }

    #[test]
    fn empty_adapter_kind_names_the_field() {
        let toml = VALID.replace("kind = \"demo\"", "kind = \"\"");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("kind"), "{err}");
        assert!(err.to_string().contains("[adapter]"), "{err}");
    }

    #[test]
    fn missing_lifecycle_section_names_it() {
        // Remove both lifecycle op tables.
        let toml = VALID
            .replace(
                "[lifecycle.start]\nexec = \"demo-agent\"\nargs = [\"--serve\"]\nenv = { DEMO_MODE = \"1\" }\n\n",
                "",
            )
            .replace(
                "[lifecycle.stop]\nexec = \"demo-agent\"\nargs = [\"--shutdown\"]\n\n",
                "",
            );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("[lifecycle]"), "got {err}");
    }

    #[test]
    fn lifecycle_without_start_names_the_start_op() {
        // Keep [lifecycle.stop] but drop start.
        let toml = VALID.replace(
            "[lifecycle.start]\nexec = \"demo-agent\"\nargs = [\"--serve\"]\nenv = { DEMO_MODE = \"1\" }\n\n",
            "",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("start"), "got {err}");
        assert!(err.to_string().contains("[lifecycle]"), "got {err}");
    }

    #[test]
    fn missing_capabilities_names_the_section() {
        let toml = VALID
            .replace(
                "[capabilities.pause]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"best-effort\"\n\n",
                "",
            )
            .replace("[capabilities.interaction]\nlinux = \"guaranteed\"\n\n", "");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("[capabilities]"), "got {err}");
    }

    #[test]
    fn all_unsupported_capabilities_are_rejected() {
        // F1: a [capabilities] section that declares keys but every entry is
        // `unsupported` promises support nowhere — rejected naming the section
        // (parallels an empty declaration).
        let toml = VALID
            .replace(
                "[capabilities.pause]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"best-effort\"",
                "[capabilities.pause]\nlinux = \"unsupported\"\nmacos = \"unsupported\"\nwindows = \"unsupported\"",
            )
            .replace(
                "[capabilities.interaction]\nlinux = \"guaranteed\"",
                "[capabilities.interaction]\nlinux = \"unsupported\"",
            );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        // Parsed fine (the keys exist) but validation rejects: no real support.
        assert!(!m.capability_declaration().is_empty());
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("[capabilities]"), "got {err}");
    }

    #[test]
    fn missing_metering_names_the_section() {
        let toml = VALID.replace("[metering]\nsource = \"self-reported\"\n", "");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::MissingSection { section } if section.contains("[metering]")),
            "got {err}"
        );
    }

    #[test]
    fn invalid_metering_source_fails_to_parse_naming_metering() {
        // AC4: an invalid/"none" metering source is not a viable kind. Because
        // MeteringSource has no `none` variant, this fails at PARSE. The parser's
        // raw "unknown variant" message does not name the section, so
        // `from_toml_str` attributes it to `[metering]` (the sole enum field),
        // keeping the diagnostic section-naming like every other rejection.
        let toml = VALID.replace("source = \"self-reported\"", "source = \"none\"");
        let err = Manifest::from_toml_str(&toml).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
        // The diagnostic now names the section AND surfaces the offending value.
        assert!(err.to_string().contains("[metering]"), "got {err}");
        assert!(err.to_string().contains("none"), "got {err}");
        assert!(err.to_string().contains("source"), "got {err}");
    }

    #[test]
    fn invalid_interaction_channel_fails_to_parse_naming_interaction() {
        // Story 4.1 AC-E: `[interaction].channel` is a closed enum (stdio and
        // http since the 6-6 freeze), so an unrecognized value fails at PARSE,
        // not merely accepted-and-ignored. Mirrors
        // `invalid_metering_source_fails_to_parse_naming_metering`: the raw
        // serde message does not name the section, so `from_toml_str`
        // attributes it to `[interaction]`.
        let toml = VALID.replace("channel = \"stdio\"", "channel = \"carrier-pigeon\"");
        let err = Manifest::from_toml_str(&toml).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
        assert!(err.to_string().contains("[interaction]"), "got {err}");
        assert!(err.to_string().contains("carrier-pigeon"), "got {err}");
        assert!(err.to_string().contains("channel"), "got {err}");
    }

    #[test]
    fn http_interaction_channel_is_v1_vocabulary_and_round_trips() {
        // CP-6.5-a option (i), ratified at the 6-6 freeze: `http` is a legal
        // `[interaction].channel` value — the additive vocabulary an
        // HTTP-native agent (e.g. opencode) declares instead of being forced
        // into an unregisterable all-unsupported interaction declaration. The
        // engine does not branch on it (the stdin pipe stays unconditional);
        // this vocabulary is documentary, and both variants' `as_str`/
        // `Display` stay in lockstep.
        let toml = VALID.replace("channel = \"stdio\"", "channel = \"http\"");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("validate");
        assert_eq!(
            m.interaction.as_ref().unwrap().channel,
            InteractionChannelKind::Http
        );
        assert_eq!(InteractionChannelKind::Http.as_str(), "http");
        assert_eq!(InteractionChannelKind::Stdio.as_str(), "stdio");
        assert_eq!(
            InteractionChannelKind::Http.to_string(),
            InteractionChannelKind::Http.as_str()
        );
    }

    #[test]
    fn invalid_interaction_channel_that_is_a_valid_metering_source_name_still_names_interaction() {
        // M3 fix (review of #79): `[interaction].channel = "self-reported"`
        // is an INVALID channel value (the only valid one is "stdio") that
        // ALSO happens to be a valid `MeteringSource` variant NAME. The
        // original section-naming logic disambiguated by matching the
        // OFFENDING VALUE against both enums' known variants, so this
        // specific value was misattributed to `[metering]` even though the
        // field that actually failed to parse is `[interaction].channel` —
        // `[metering].source` was never touched by this edit. The fix
        // disambiguates using the message's "expected ..." clause (the
        // FAILING FIELD's own valid values) instead of the rejected value.
        let toml = VALID.replace("channel = \"stdio\"", "channel = \"self-reported\"");
        let err = Manifest::from_toml_str(&toml).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
        assert!(
            err.to_string().contains("[interaction]"),
            "must name [interaction] (the field that actually failed to parse), got {err}"
        );
        assert!(
            !err.to_string().contains("[metering]"),
            "must NOT be misattributed to [metering] merely because the offending value \
             is also a valid MeteringSource variant name, got {err}"
        );
        assert!(err.to_string().contains("self-reported"), "got {err}");
        assert!(err.to_string().contains("channel"), "got {err}");
    }

    #[test]
    fn invalid_metering_source_that_is_a_valid_interaction_channel_name_still_names_metering() {
        // The mirror direction of the M3 fix: `[metering].source = "stdio"`
        // is an INVALID source (only "self-reported"/"engine-observed" are
        // valid) that ALSO happens to be a valid
        // `InteractionChannelKind` variant name. Must still name
        // `[metering]`, never `[interaction]` — confirms the "expected ..."
        // clause disambiguation is symmetric, not merely patched for the
        // one direction M3 called out.
        let toml = VALID.replace("source = \"self-reported\"", "source = \"stdio\"");
        let err = Manifest::from_toml_str(&toml).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
        assert!(err.to_string().contains("[metering]"), "got {err}");
        assert!(
            !err.to_string().contains("[interaction]"),
            "must NOT be misattributed to [interaction], got {err}"
        );
        assert!(err.to_string().contains("stdio"), "got {err}");
        assert!(err.to_string().contains("source"), "got {err}");
    }

    #[test]
    fn unknown_variant_error_from_an_unrelated_enum_is_passed_through_unattributed() {
        // Coverage-closing (fix pass, review of #79): `from_toml_str`'s
        // section-naming only recognizes TWO specific "unknown variant"
        // shapes (`[metering].source` / `[interaction].channel`). A THIRD
        // enum in the schema — `SupportLevel` (`[capabilities.*]`'s
        // guaranteed/best-effort/unsupported) — can ALSO produce an "unknown
        // variant" message (e.g. a typo'd support level), and its valid
        // values overlap with NEITHER metering's nor interaction's. This
        // must fall through to the raw, UNMODIFIED message (never crash,
        // never be misattributed to either section) — the fallback branch
        // the M3 restructuring's `if/else if/else if/else` chain still needs
        // reachable and correct.
        let toml = VALID.replace(
            "linux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"best-effort\"",
            "linux = \"sometimes\"\nmacos = \"guaranteed\"\nwindows = \"best-effort\"",
        );
        let err = Manifest::from_toml_str(&toml).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
        assert!(
            !err.to_string().contains("[metering]"),
            "an unrelated enum's bad value must not be misattributed to [metering], got {err}"
        );
        assert!(
            !err.to_string().contains("[interaction]"),
            "an unrelated enum's bad value must not be misattributed to [interaction], got {err}"
        );
        assert!(err.to_string().contains("sometimes"), "got {err}");
    }

    #[test]
    fn absent_interaction_section_still_validates_defaulting_to_stdio_at_the_engine_layer() {
        // AC-E: `[interaction]` stays OPTIONAL — omitting it entirely is still
        // valid (the engine's unconditional stdin pipe, Task 1, IS the
        // default; this type only firms up the value when the section is
        // present).
        let toml = VALID.replace("[interaction]\nchannel = \"stdio\"\n", "");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("validate");
        assert!(m.interaction.is_none());
    }

    #[test]
    fn interaction_channel_kind_as_str_and_display_agree() {
        // Mirrors MeteringSource's `serde_round_trips_both_variants` sanity
        // check: the seed-enum `as_str()`/`Display` pair (currently one
        // variant) stays in lockstep.
        assert_eq!(InteractionChannelKind::Stdio.as_str(), "stdio");
        assert_eq!(
            InteractionChannelKind::Stdio.to_string(),
            InteractionChannelKind::Stdio.as_str()
        );
    }

    #[test]
    fn malformed_toml_is_reported_as_toml_error() {
        let err = Manifest::from_toml_str("this is not = = toml").unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let toml = format!("{VALID}\nbogus_key = true\n");
        let err = Manifest::from_toml_str(&toml).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
        assert!(err.to_string().contains("bogus_key"), "got {err}");
    }

    #[test]
    fn unknown_nested_key_is_rejected() {
        let toml = VALID.replace(
            "[metering]\nsource = \"self-reported\"",
            "[metering]\nsource = \"self-reported\"\nextra = 1",
        );
        let err = Manifest::from_toml_str(&toml).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
    }

    #[test]
    fn accessors_default_when_sections_absent() {
        // A near-empty manifest: accessors return safe defaults/None without
        // panicking (validation is what rejects it).
        let m = Manifest::from_toml_str("contract_version = \"1.0.0\"").expect("parse");
        assert!(m.capability_declaration().is_empty());
        assert_eq!(m.metering_source(), None);
        assert_eq!(m.adapter_kind(), None);
        // Story 2-2: an absent `[config]` yields an EMPTY mapping (a no-op), not
        // an error.
        assert!(m.config_mapping().is_empty());
        assert!(m.validate().is_err());
    }

    // ---- Story 2-2: the optional `[config]` mapping section (FR-12) ----

    #[test]
    fn absent_config_section_validates_with_an_empty_mapping() {
        // The common case: a valid manifest with NO `[config]` section is valid,
        // and `config_mapping()` returns an empty mapping (delivers nothing).
        let m = Manifest::from_toml_str(VALID).expect("parse");
        m.validate().expect("validate");
        assert!(m.config.is_none());
        assert!(m.config_mapping().is_empty());
    }

    #[test]
    fn config_section_with_each_target_parses_validates_and_reads_back() {
        // AC3: a manifest can declare, per documented unified key, its native
        // target (env / flag / file). All three parse, validate, and read back
        // through the accessor.
        let toml = format!(
            "{VALID}\n\
             [config.model]\nenv = \"MODEL\"\n\n\
             [config.temperature]\nflag = \"--temp\"\n\n\
             [config.seed]\nfile = {{ path = \"config/agent.toml\", key = \"llm.seed\" }}\n"
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("validate");

        let mapping = m.config_mapping();
        assert_eq!(mapping.len(), 3);
        assert_eq!(mapping.target("model").unwrap().env_var(), Some("MODEL"));
        assert_eq!(
            mapping
                .target("temperature")
                .unwrap()
                .render_flag_args("0.7"),
            Some(["--temp".to_string(), "0.7".to_string()])
        );
        let placement = mapping.target("seed").unwrap().file_placement().unwrap();
        assert_eq!(placement.path, "config/agent.toml");
        assert_eq!(placement.key, "llm.seed");
    }

    #[test]
    fn malformed_config_rule_is_rejected_naming_the_config_subsection() {
        // A present-but-malformed rule (an empty env var name) is an InvalidField
        // naming the `[config.<key>]` sub-section.
        let toml = format!("{VALID}\n[config.model]\nenv = \"\"\n");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. } if field.contains("[config.model]")),
            "got {err}"
        );
    }

    #[test]
    fn config_file_path_escaping_the_home_is_rejected() {
        // A `file.path` that escapes the Agent Home is rejected (the engine is the
        // sole writer inside the home — AD-6).
        let toml = format!(
            "{VALID}\n[config.model]\nfile = {{ path = \"../escape.toml\", key = \"k\" }}\n"
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("[config.model]"), "got {err}");
        assert!(err.to_string().contains("RELATIVE"), "got {err}");
    }

    #[test]
    fn config_rule_with_no_native_mechanism_fails_to_parse_deny_unknown() {
        // A `[config.<key>]` sub-table naming no known mechanism fails at PARSE
        // (untagged variant match), surfaced as a Toml error — like every other
        // typo the manifest's deny_unknown_fields style catches.
        let toml = format!("{VALID}\n[config.model]\nbogus = \"x\"\n");
        let err = Manifest::from_toml_str(&toml).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
    }
}
