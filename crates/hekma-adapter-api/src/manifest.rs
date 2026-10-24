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
//! ## Optional sections
//!
//! * `[interaction]` — the documentary interaction channel (story 4.1).
//! * `[config]` — the unified→native config mapping (story 2-2, FR-12).
//! * `[readiness]` — ONE declared readiness probe gating the engine's
//!   `starting → running` transition (added at contract `1.0.0 → 1.1.0`):
//!   `http-get { url, expected_status }`, `file_fresh { path, max_age_ms }`,
//!   or `log_contains { line }`, each with a `timeout_ms` in `1..=600_000`.
//!   An engine built at 1.0.0 rejects the section for free
//!   (`deny_unknown_fields`) — the versioning policy's older-same-major rule.
//! * `[liveness]` — ONE declared liveness check the engine's reaper evaluates
//!   while the instance is `running` (joins the SAME contract `1.1.0` minor —
//!   both sections ship in one release, so an engine that knows `[readiness]`
//!   but not `[liveness]` never exists): `[liveness.heartbeat_file]
//!   { path, max_age_ms }` or `[liveness.http_get]
//!   { url, expected_status, max_age_ms }`. The unifying semantic: the
//!   declared signal failing CONTINUOUSLY for `max_age_ms` fails the
//!   instance.
//!
//! `#[serde(deny_unknown_fields)]` (the repo's manifest style) additionally
//! rejects unknown keys, catching typos.
//!
//! ## Not executed this story
//!
//! The lifecycle templates are stored, never run. The manifest executor / process
//! launch is story 1-4.

use std::collections::BTreeMap;
use std::net::IpAddr;

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
    /// The declared readiness probe (added at contract `1.0.0 → 1.1.0`).
    /// OPTIONAL: a manifest without it starts exactly as before. Validated by
    /// [`Manifest::validate`] when present — a v1.1.0 engine names the section's
    /// first invalid field; a v1.0.0 engine (whose schema has no `readiness`
    /// field at all) rejects the section at PARSE time via
    /// `deny_unknown_fields`, exactly as the versioning policy states.
    pub readiness: Option<Readiness>,
    /// The declared liveness check (joins the contract `1.1.0` minor). OPTIONAL:
    /// a manifest without it supervises exactly as before (process liveness
    /// only). Validated by [`Manifest::validate`] when present — a v1.1.0
    /// engine names the section's first invalid field; a v1.0.0 engine (whose
    /// schema has no `liveness` field at all) rejects the section at PARSE
    /// time via `deny_unknown_fields`, exactly as the versioning policy
    /// states. (Moot in practice: both 1.1.0 sections ship in one release, so
    /// the intermediate engine never exists — the pin stays as the policy's
    /// free guarantee.)
    pub liveness: Option<Liveness>,
    /// The declared exit-code classes (joins the contract `1.1.0` minor).
    /// OPTIONAL: a manifest without it classifies nothing — every unrequested
    /// exit is a crash exactly as before (byte-identical). Validated by
    /// [`Manifest::validate`] when present — a v1.1.0 engine names the
    /// section's first invalid field; a v1.0.0 engine (whose schema has no
    /// `exit_codes` field at all) rejects the section at PARSE time via
    /// `deny_unknown_fields`, exactly as the versioning policy states.
    pub exit_codes: Option<ExitCodes>,
    /// The declared drain wait (story 18-4, joins the contract `1.1.0` minor —
    /// the story's ONLY new schema; the `[lifecycle.pause]`/`[lifecycle.resume]`
    /// templates it composes with are v1.0.0 seeds now executed). OPTIONAL: a
    /// manifest without it drains with the engine's default posture (a declared
    /// pause command gets the engine's default 30 s bound; nothing declared is a
    /// degenerate direct stop). Validated by [`Manifest::validate`] when present
    /// — a v1.1.0 engine names the section's first invalid field; a v1.0.0
    /// engine (whose schema has no `drain` field at all) rejects the section at
    /// PARSE time via `deny_unknown_fields`, exactly as the versioning policy
    /// states.
    pub drain: Option<Drain>,
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

/// The declared readiness probe kind (added at contract `1.0.0 → 1.1.0`).
///
/// A CLOSED set, mirroring [`InteractionChannelKind`]'s seed-enum pattern:
/// kebab-case on the wire (`http-get` / `file-fresh` / `log-contains`), an
/// unrecognized value fails at PARSE time (an unknown variant), and an
/// `as_str()`/`Display` pair names the wire form. Exactly ONE kind per
/// manifest — the alternatives are alternatives, not a pipeline (the
/// `[readiness]` section's shape — a table — is where a future ordered-gate
/// LIST would grow, without a breaking change).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadinessKind {
    /// An HTTP GET against a URL; the probe passes when the response's status
    /// equals the declared expected status.
    HttpGet,
    /// A file whose modification age is at most the declared maximum — the
    /// agent proves readiness by (re)writing a file.
    FileFresh,
    /// A substring that must appear in the ENGINE-owned agent output log
    /// after this Run's spawn point — the agent proves readiness by printing
    /// a line.
    LogContains,
}

impl ReadinessKind {
    /// The kebab-case wire name, matching the serde form and manifest value.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReadinessKind::HttpGet => "http-get",
            ReadinessKind::FileFresh => "file-fresh",
            ReadinessKind::LogContains => "log-contains",
        }
    }
}

impl std::fmt::Display for ReadinessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `http-get` probe's resolved target: a loopback-first URL and the ONE
/// status that counts as ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpGetProbe {
    /// The probe URL. `http://` ONLY (no TLS, no other schemes): the probe
    /// targets the AGENT's own listener on the same host, so a remote or
    /// HTTPS target is refused at validation — a remote readiness endpoint
    /// is a Host concern, not an adapter-contract one.
    pub url: String,
    /// The status code that counts as ready. ANY response carrying it passes
    /// (connection-refused is a not-yet, retried on the engine's poll cadence —
    /// never an error).
    pub expected_status: u16,
}

/// The `file-fresh` probe's resolved target: a path (relative to the Agent
/// Home when relative) and the maximum modification age that counts as fresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFreshProbe {
    /// The file the agent (re)writes to prove readiness. A relative path
    /// resolves against the instance's Agent Home (the agent's own working
    /// directory); an absolute path is taken verbatim.
    pub path: String,
    /// The maximum modification age, in milliseconds. A file modified within
    /// this window of the check is fresh. SystemTime deltas only — the
    /// per-OS mtime granularity honesty (e.g. 2 s on Windows FAT) lives in
    /// the section's docs, never in the comparison.
    pub max_age_ms: u64,
}

/// The `log-contains` probe's resolved target: the SUBSTRING that must appear
/// in the engine-owned agent output log after this Run's spawn point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogContainsProbe {
    /// The readiness line. A plain SUBSTRING match (the engine is regex-free
    /// by design) over a bounded incremental read starting at the spawn-time
    /// cursor. Under the engine's `capture = "off"` posture no log exists, so
    /// this probe is unsatisfiable there and fails at its timeout — honestly
    /// named, never a fake pass.
    pub line: String,
}

/// The ONE declared readiness probe, RESOLVED (exactly one variant, every
/// field present and legal). Built only by [`Readiness::gate`] after
/// validation, so the engine's runner consumes this enum without re-checking
/// anything: an internally-tagged Rust mirror of the manifest's per-kind
/// fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadinessProbe {
    /// An HTTP GET with an expected status.
    HttpGet(HttpGetProbe),
    /// A file's maximum modification age.
    FileFresh(FileFreshProbe),
    /// A substring in the engine-owned agent log.
    LogContains(LogContainsProbe),
}

/// The declared readiness gate RESOLVED (contract `1.1.0`): the probe plus
/// its OWN budget. The engine's runner waits for [`ReadinessGate::probe`] to
/// pass within [`ReadinessGate::timeout_ms`] on ITS OWN poll cadence (the
/// manifest declares the budget, never the cadence).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessGate {
    /// The probe's whole budget, in milliseconds (`1..=600_000`, enforced by
    /// [`Readiness::gate`]).
    pub timeout_ms: u64,
    /// The one resolved probe.
    pub probe: ReadinessProbe,
}

/// The optional `[readiness]` section (added at contract `1.0.0 → 1.1.0`):
/// exactly ONE declared readiness probe, shaped `{ kind, timeout_ms, <the
/// kind's own fields> }` —
///
/// ```toml
/// [readiness]
/// kind = "http-get"
/// timeout_ms = 5000
/// url = "http://127.0.0.1:8080/health"
/// expected_status = 200
/// ```
///
/// OPTIONAL: a manifest without the section behaves exactly as before (the
/// engine's process-level startup watch alone). Declaring it makes the
/// engine's supervisor gate `starting → running` on the probe: the instance
/// reaches `running` only once the probe passes within its declared
/// `timeout_ms`; a timeout lands it `failed` with the probe NAMED (a
/// terminal failure, never an auto-restart). The section is
/// `deny_unknown_fields` like every section struct, so an engine built at
/// contract 1.0.0 rejects a manifest declaring it (the versioning policy's
/// older-same-major-engine rule, for free).
///
/// Validation is [`Readiness::gate`] (consumed by [`Manifest::validate`]):
/// the `kind`'s own fields are REQUIRED, every OTHER kind's fields are
/// FORBIDDEN (a `file-fresh` manifest carrying `url` names the offender),
/// and the value rules hold: `timeout_ms` in `1..=600_000`; `url` is
/// `http://` targeting a LITERAL loopback address (`127.0.0.0/8` or `::1`,
/// optional port, no hostnames — the probe never consults a resolver) with
/// no `#fragment`; `expected_status` in `100..=599`; `max_age_ms` in
/// `1..=timeout_ms`; `url`/`path`/`line` free of control characters (NUL
/// and the rest of C0/C1).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    /// Which probe this is. A closed enum — an unrecognized value fails at
    /// PARSE time (an unknown variant naming the section).
    pub kind: ReadinessKind,
    /// The probe's whole budget, in milliseconds. `1..=600_000` (ten
    /// minutes): below 1 is not a poll at all, above the cap a start would
    /// hold supervision unboundedly. The ENGINE owns the poll cadence within
    /// this budget — the manifest declares operator intent only.
    pub timeout_ms: u64,
    /// `http-get` only: the probe URL. `http://` only, and the authority
    /// must be a LITERAL loopback address (`127.0.0.0/8` or `::1`, optional
    /// `:port`) — never a hostname, so the probe can never consult a
    /// resolver/DNS.
    pub url: Option<String>,
    /// `http-get` only: the status code that counts as ready
    /// (`100..=599`).
    pub expected_status: Option<u16>,
    /// `file-fresh` only: the readiness file (relative to the Agent Home).
    pub path: Option<String>,
    /// `file-fresh` only: the maximum modification age, in milliseconds
    /// (`1..=timeout_ms`).
    pub max_age_ms: Option<u64>,
    /// `log-contains` only: the readiness line (substring, non-empty, no
    /// control characters).
    pub line: Option<String>,
}

impl Readiness {
    /// Resolve this section into its [`ReadinessGate`] (the probe + its own
    /// budget), enforcing the whole value matrix and naming the first failing
    /// field (the section's validation — run at registration via
    /// [`Manifest::validate`] and re-checked at start like the `[config]`
    /// mapping).
    ///
    /// Rules, in stable diagnostic order:
    /// * `timeout_ms` must be `1..=600_000`;
    /// * the `kind`'s own fields are all required (with per-kind value rules);
    /// * every OTHER kind's fields are forbidden — a stray `url` under
    ///   `file-fresh` is a named `InvalidField`, never silently ignored.
    ///
    /// Per-kind value rules: `http-get` takes an `http://` URL whose
    /// authority is a literal loopback address (`127.0.0.0/8` or `::1`,
    /// optional `:port` — no hostnames, no userinfo, no fragment) and an
    /// `expected_status` in `100..=599`; `file-fresh` takes a
    /// `max_age_ms` in `1..=timeout_ms`; text fields (`url`, `path`,
    /// `line`) carry no control characters (NUL and the rest of C0/C1).
    pub fn gate(&self) -> Result<ReadinessGate, ManifestError> {
        if self.timeout_ms == 0 || self.timeout_ms > 600_000 {
            return Err(ManifestError::InvalidField {
                field: "`timeout_ms` field in the `[readiness]` section".to_string(),
                detail: format!(
                    "{} is out of range — the declared probe budget must be between 1 and \
                     600000 milliseconds",
                    self.timeout_ms
                ),
            });
        }
        // The kind's OWN fields, required + per-kind value rules; the
        // foreign-field sweep runs after, so a manifest that both omits its
        // own field and carries a foreign one names the missing field first.
        let probe = match self.kind {
            ReadinessKind::HttpGet => {
                let url = required_str(
                    self.url.as_deref(),
                    "`url` field in the `[readiness]` section",
                )?;
                if contains_control_char(url) {
                    return Err(control_char_error("`url`", "URL"));
                }
                if !url.starts_with("http://") {
                    return Err(ManifestError::InvalidField {
                        field: "`url` field in the `[readiness]` section".to_string(),
                        detail: format!(
                            "'{url}' is not an http:// URL — the readiness probe targets the \
                             agent's own loopback listener, so only http:// is accepted (no \
                             TLS, no other schemes)"
                        ),
                    });
                }
                // A `#fragment` never reaches the wire (it is client-side),
                // so it is refused here at validation and the probe runner
                // never has to guess what path was meant. Queries (`?`)
                // stay allowed — servers see them.
                if url.contains('#') {
                    return Err(ManifestError::InvalidField {
                        field: "`url` field in the `[readiness]` section".to_string(),
                        detail: format!(
                            "'{url}' carries a `#fragment` — a fragment never reaches the \
                             wire, so the probe would not request the path the URL names; \
                             drop it"
                        ),
                    });
                }
                // The authority (host[:port]) must be present: `http://` alone
                // or `http:///path` has nowhere to connect and would only
                // fail obscurely at probe time.
                // Strict single-strip: `strip_prefix` removes AT MOST one
                // `http://`, so `http://http://...` keeps its inner scheme in
                // the authority and is rejected by the loopback-literal rule
                // below. (`trim_start_matches` would strip every repetition,
                // silently laundering a double-scheme URL into a valid one.)
                let authority = url
                    .strip_prefix("http://")
                    .unwrap_or(url)
                    .split(['/', '?'])
                    .next()
                    .unwrap_or("");
                if authority.is_empty() {
                    return Err(ManifestError::InvalidField {
                        field: "`url` field in the `[readiness]` section".to_string(),
                        detail: format!(
                            "'{url}' carries no host — the probe URL must name the agent's \
                             listener host (e.g. http://127.0.0.1:8080/health)"
                        ),
                    });
                }
                // The loopback-literal posture: the authority is a literal
                // loopback IP, never a name.
                require_loopback_authority(
                    authority,
                    url,
                    "`url` field in the `[readiness]` section",
                    "readiness probe",
                )?;
                let expected_status = self.expected_status.ok_or_else(|| {
                    ManifestError::missing(
                        "`expected_status` field in the `[readiness]` section (the http-get \
                         probe's ready status)",
                    )
                })?;
                if !(100..=599).contains(&expected_status) {
                    return Err(ManifestError::InvalidField {
                        field: "`expected_status` field in the `[readiness]` section".to_string(),
                        detail: format!(
                            "{expected_status} is not an HTTP status code — must be 100..=599 \
                             (1xx informational through 5xx server error)"
                        ),
                    });
                }
                ReadinessProbe::HttpGet(HttpGetProbe {
                    url: url.to_string(),
                    expected_status,
                })
            }
            ReadinessKind::FileFresh => {
                let path = required_str(
                    self.path.as_deref(),
                    "`path` field in the `[readiness]` section",
                )?;
                if contains_control_char(path) {
                    return Err(control_char_error("`path`", "file path"));
                }
                let max_age_ms = self.max_age_ms.ok_or_else(|| {
                    ManifestError::missing(
                        "`max_age_ms` field in the `[readiness]` section (the file-fresh \
                         probe's maximum age)",
                    )
                })?;
                if max_age_ms == 0 {
                    return Err(ManifestError::InvalidField {
                        field: "`max_age_ms` field in the `[readiness]` section".to_string(),
                        detail: "0 is not a freshness window — the file could never count \
                                 as fresh; the minimum is 1 millisecond"
                            .to_string(),
                    });
                }
                if max_age_ms > self.timeout_ms {
                    return Err(ManifestError::InvalidField {
                        field: "`max_age_ms` field in the `[readiness]` section".to_string(),
                        detail: format!(
                            "{max_age_ms} exceeds the probe's own timeout_ms ({}) — a file \
                             cannot be required fresher than the probe is allowed to wait",
                            self.timeout_ms
                        ),
                    });
                }
                ReadinessProbe::FileFresh(FileFreshProbe {
                    path: path.to_string(),
                    max_age_ms,
                })
            }
            ReadinessKind::LogContains => {
                let line = required_str(
                    self.line.as_deref(),
                    "`line` field in the `[readiness]` section",
                )?;
                if contains_control_char(line) {
                    return Err(control_char_error("`line`", "line"));
                }
                ReadinessProbe::LogContains(LogContainsProbe {
                    line: line.to_string(),
                })
            }
        };
        // The FOREIGN-field sweep: fields belonging to a different kind must
        // be absent. A present one is a named error (the typo-protection the
        // house style owes), never a silently-ignored key.
        let foreign: &[(&str, bool)] = match self.kind {
            ReadinessKind::HttpGet => &[
                ("`path`", self.path.is_some()),
                ("`max_age_ms`", self.max_age_ms.is_some()),
                ("`line`", self.line.is_some()),
            ],
            ReadinessKind::FileFresh => &[
                ("`url`", self.url.is_some()),
                ("`expected_status`", self.expected_status.is_some()),
                ("`line`", self.line.is_some()),
            ],
            ReadinessKind::LogContains => &[
                ("`url`", self.url.is_some()),
                ("`expected_status`", self.expected_status.is_some()),
                ("`path`", self.path.is_some()),
                ("`max_age_ms`", self.max_age_ms.is_some()),
            ],
        };
        if let Some((field, _)) = foreign.iter().find(|(_, present)| *present) {
            return Err(ManifestError::InvalidField {
                field: format!("{field} field in the `[readiness]` section"),
                detail: format!(
                    "not a `{}` probe field — the `{}` kind carries only its own fields \
                     ({})",
                    self.kind,
                    self.kind,
                    match self.kind {
                        ReadinessKind::HttpGet => "`url`, `expected_status`",
                        ReadinessKind::FileFresh => "`path`, `max_age_ms`",
                        ReadinessKind::LogContains => "`line`",
                    }
                ),
            });
        }
        Ok(ReadinessGate {
            timeout_ms: self.timeout_ms,
            probe,
        })
    }
}

/// The `[liveness.heartbeat_file]` sub-table: the agent proves liveness by
/// (re)writing a file — the heartbeat — whose modification age must stay
/// within `max_age_ms`. Resolved shape: serde requires both fields (neither
/// is `Option`), so a present sub-table is already type-complete and only the
/// VALUE rules remain for [`Liveness::check`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatFileCheck {
    /// The heartbeat file (relative to the Agent Home when relative). The
    /// agent rewrites/touches it on its own cadence; the engine only READS
    /// the mtime. A missing file is failing — absent ≡ stale (an agent that
    /// never wrote its heartbeat has never proven liveness).
    pub path: String,
    /// The freshness window, in milliseconds (`250..=600_000`): how long the
    /// file's mtime may lag before the heartbeat counts as stopped. This ONE
    /// number is the window for the unifying semantic — the signal failing
    /// CONTINUOUSLY for `max_age_ms` fails the instance.
    pub max_age_ms: u64,
}

/// The `[liveness.http_get]` sub-table: the agent exposes a loopback HTTP
/// endpoint whose declared status proves liveness. The engine polls ONE
/// bounded attempt per reaper tick; the check FAILING CONTINUOUSLY for
/// `max_age_ms` fails the instance (a transient blip inside the window
/// recovers — the k8s `failureThreshold × periodSeconds` precedent).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpGetCheck {
    /// The check URL. `http://` ONLY, targeting a LITERAL loopback address —
    /// the same no-DNS posture as the readiness probe (validation is shared).
    pub url: String,
    /// The status code that counts as ALIVE (`100..=599`). Any other status —
    /// or a connect/read failure — is one failing attempt, retried on the
    /// engine's reaper cadence (never an error that aborts supervision).
    pub expected_status: u16,
    /// The failure window, in milliseconds (`1..=600_000`) — the same
    /// unifying semantic as the heartbeat's.
    pub max_age_ms: u64,
}

/// The ONE declared liveness check, RESOLVED (exactly one variant, every
/// field present and legal). Built only by [`Liveness::check`] after
/// validation, so the engine's runner consumes this enum without re-checking
/// anything — the liveness twin of [`ReadinessProbe`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LivenessCheck {
    /// A heartbeat file's maximum modification age.
    HeartbeatFile(HeartbeatFileCheck),
    /// An HTTP GET with an expected status.
    HttpGet(HttpGetCheck),
}

impl LivenessCheck {
    /// The window (`max_age_ms`) both kinds carry — the unifying semantic's
    /// one number. The engine reads it here instead of matching the variant
    /// again, so a third kind cannot forget to state its window.
    pub fn max_age_ms(&self) -> u64 {
        match self {
            LivenessCheck::HeartbeatFile(h) => h.max_age_ms,
            LivenessCheck::HttpGet(h) => h.max_age_ms,
        }
    }
}

/// The optional `[liveness]` section (joins the contract `1.0.0 → 1.1.0`
/// minor): exactly ONE declared liveness check, shaped as ONE of two
/// sub-tables —
///
/// ```toml
/// [liveness.heartbeat_file]
/// path = "heartbeat.marker"
/// max_age_ms = 5000
/// ```
///
/// or
///
/// ```toml
/// [liveness.http_get]
/// url = "http://127.0.0.1:8080/health"
/// expected_status = 200
/// max_age_ms = 5000
/// ```
///
/// OPTIONAL: a manifest without the section behaves exactly as before (the
/// engine watches PROCESS liveness only). Declaring it makes the engine's
/// reaper evaluate the check every tick while the instance is `running`,
/// under ONE unifying semantic — **the declared signal failing CONTINUOUSLY
/// for `max_age_ms` fails the instance** (`running → failed`, cause
/// `unresponsive`, and the Restart Policy applies exactly as for a crash).
/// A transient failure shorter than the window recovers; a PAUSED instance
/// is never probed (a suspended process cannot heartbeat). The section is
/// `deny_unknown_fields` like every section struct, so an engine built at
/// contract 1.0.0 rejects it at parse time (the policy's older-engine rule,
/// for free) — though the intermediate engine that knows `[readiness]` but
/// not `[liveness]` never exists: both sections ship in the same 1.1.0
/// release.
///
/// Validation is [`Liveness::check`] (consumed by [`Manifest::validate`]):
/// exactly ONE sub-table may be present (both or neither is a named error),
/// and the value rules hold: `max_age_ms` in `250..=600_000`; `path` non-empty
/// and free of control characters; `http_get.url` an `http://` URL targeting
/// a LITERAL loopback address (`127.0.0.0/8` or `::1`, optional port — no
/// hostnames, no DNS, no fragment, the SAME shared rule the readiness probe
/// enforces); `expected_status` in `100..=599`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Liveness {
    /// The heartbeat-file check. Exactly ONE of the two sub-tables may be
    /// present — enforced here at validation (TOML cannot express
    /// "exactly-one-of" structurally), naming the section when violated.
    pub heartbeat_file: Option<HeartbeatFileCheck>,
    /// The HTTP check (mutually exclusive with `heartbeat_file`).
    pub http_get: Option<HttpGetCheck>,
}

impl Liveness {
    /// Resolve this section into its [`LivenessCheck`], enforcing the whole
    /// value matrix and naming the first failing field (the section's
    /// validation — run at registration via [`Manifest::validate`] and
    /// re-checked at start and adoption like the `[config]` mapping and the
    /// `[readiness]` probe).
    ///
    /// Rules, in stable diagnostic order:
    /// * exactly ONE sub-table present (both, or neither, names the section);
    /// * `max_age_ms` in `250..=600_000` (below 250 the window is smaller
    ///   than one reaper tick — ~250 ms — so a window that short could expire
    ///   without the engine ever observing the failure; above the cap a
    ///   wedged agent would hold supervision unboundedly);
    /// * `heartbeat_file.path` non-empty, no control characters;
    /// * `http_get` takes the SAME loopback-literal URL family the
    ///   `[readiness]` probe enforces (one shared helper — the two sections
    ///   cannot drift), plus an `expected_status` in `100..=599`.
    pub fn check(&self) -> Result<LivenessCheck, ManifestError> {
        // Exactly ONE check: both sub-tables is a contradiction (which signal
        // governs?); neither is an empty declaration (declaring `[liveness]`
        // while checking nothing would silently supervise nothing new).
        match (&self.heartbeat_file, &self.http_get) {
            (Some(_), Some(_)) => {
                return Err(ManifestError::InvalidField {
                    field: "`[liveness]` section".to_string(),
                    detail: "declares BOTH checks (`heartbeat_file` and `http_get`) — \
                             exactly ONE liveness check per manifest; drop one sub-table"
                        .to_string(),
                });
            }
            (None, None) => {
                return Err(ManifestError::InvalidField {
                    field: "`[liveness]` section".to_string(),
                    detail: "declares NO check — add exactly ONE of \
                             `[liveness.heartbeat_file]` or `[liveness.http_get]`, or drop \
                             the empty `[liveness]` section"
                        .to_string(),
                });
            }
            _ => {}
        }
        let max_age_ok = |max_age_ms: u64| -> Result<(), ManifestError> {
            if max_age_ms < 250 {
                return Err(ManifestError::InvalidField {
                    field: "`max_age_ms` field in the `[liveness]` section".to_string(),
                    detail: format!(
                        "{max_age_ms} is below the liveness floor — the window must be between \
                         250 and 600000 milliseconds. The reaper polls the declared signal on a \
                         ~250 ms tick, so a window shorter than one tick could expire without \
                         the engine ever observing the failure; 250 ms is the smallest \
                         observable window"
                    ),
                });
            }
            if max_age_ms > 600_000 {
                return Err(ManifestError::InvalidField {
                    field: "`max_age_ms` field in the `[liveness]` section".to_string(),
                    detail: format!(
                        "{max_age_ms} is out of range — the liveness window must be between \
                         250 and 600000 milliseconds"
                    ),
                });
            }
            Ok(())
        };
        if let Some(heartbeat) = &self.heartbeat_file {
            let path = required_str(
                Some(heartbeat.path.as_str()),
                "`path` field in the `[liveness.heartbeat_file]` sub-table",
            )?;
            if contains_control_char(path) {
                return Err(ManifestError::InvalidField {
                    field: "`path` field in the `[liveness.heartbeat_file]` sub-table".to_string(),
                    detail: "the liveness heartbeat path contains a control character (NUL \
                             and the rest of the C0/C1 control bytes are forbidden — they \
                             would corrupt the path)"
                        .to_string(),
                });
            }
            max_age_ok(heartbeat.max_age_ms)?;
            return Ok(LivenessCheck::HeartbeatFile(heartbeat.clone()));
        }
        let http = self
            .http_get
            .as_ref()
            .expect("exactly-one guard above leaves exactly one sub-table present");
        let url = required_str(
            Some(http.url.as_str()),
            "`url` field in the `[liveness.http_get]` sub-table",
        )?;
        if contains_control_char(url) {
            return Err(ManifestError::InvalidField {
                field: "`url` field in the `[liveness.http_get]` sub-table".to_string(),
                detail: "the liveness URL contains a control character (NUL and the rest of \
                         the C0/C1 control bytes are forbidden — a newline in a URL is \
                         header-injection material)"
                    .to_string(),
            });
        }
        if !url.starts_with("http://") {
            return Err(ManifestError::InvalidField {
                field: "`url` field in the `[liveness.http_get]` sub-table".to_string(),
                detail: format!(
                    "'{url}' is not an http:// URL — the liveness check targets the agent's \
                     own loopback listener, so only http:// is accepted (no TLS, no other \
                     schemes)"
                ),
            });
        }
        // The same fragment refusal the readiness probe makes: a `#fragment`
        // never reaches the wire, so the check would not request the path the
        // URL names.
        if url.contains('#') {
            return Err(ManifestError::InvalidField {
                field: "`url` field in the `[liveness.http_get]` sub-table".to_string(),
                detail: format!(
                    "'{url}' carries a `#fragment` — a fragment never reaches the wire, so \
                     the check would not request the path the URL names; drop it"
                ),
            });
        }
        // Strict single-strip, mirroring the `[readiness]` URL rule above:
        // at most one `http://` is removed, so `http://http://...` keeps its
        // inner scheme in the authority and is rejected by the loopback rule.
        let authority = url
            .strip_prefix("http://")
            .unwrap_or(url)
            .split(['/', '?'])
            .next()
            .unwrap_or("");
        if authority.is_empty() {
            return Err(ManifestError::InvalidField {
                field: "`url` field in the `[liveness.http_get]` sub-table".to_string(),
                detail: format!(
                    "'{url}' carries no host — the check URL must name the agent's listener \
                     host (e.g. http://127.0.0.1:8080/health)"
                ),
            });
        }
        // THE SHARED loopback-literal rule (the no-DNS posture): one helper
        // backs both `[readiness]` and `[liveness]`, so the two sections
        // cannot drift apart on which URLs they admit.
        require_loopback_authority(
            authority,
            url,
            "`url` field in the `[liveness.http_get]` sub-table",
            "liveness check",
        )?;
        if !(100..=599).contains(&http.expected_status) {
            return Err(ManifestError::InvalidField {
                field: "`expected_status` field in the `[liveness.http_get]` sub-table".to_string(),
                detail: format!(
                    "{} is not an HTTP status code — must be 100..=599 (1xx informational \
                     through 5xx server error)",
                    http.expected_status
                ),
            });
        }
        max_age_ok(http.max_age_ms)?;
        Ok(LivenessCheck::HttpGet(http.clone()))
    }
}

/// Which declared class an exit code belongs to, RESOLVED (built only by
/// [`ExitCodes::classes`] after validation, so the engine's classifier
/// consumes this without re-checking anything).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitClass {
    /// The agent exited with this code to REQUEST a relaunch: the engine
    /// runs the ordinary restart plan (backoff verbatim) but the
    /// consecutive-failure counter is NOT charged.
    RestartRequested,
    /// The agent finished ON PURPOSE with this code: the instance lands
    /// `stopped` (settled like a requested stop), no restart plan, no
    /// counter action.
    Clean,
}

/// The declared exit-code classes, RESOLVED and validated (every code in the
/// portable `0..=255` domain, the two lists disjoint, no duplicates within a
/// list). Built only by [`ExitCodes::classes`] after validation; the engine
/// classifies an observed exit with [`Self::class_of`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExitCodeClasses {
    /// The codes that request a relaunch (the exit-75 pattern).
    pub restart_requested: Vec<u16>,
    /// The codes that mean a deliberate, clean finish.
    pub clean: Vec<u16>,
}

impl ExitCodeClasses {
    /// Classify one observed exit code against the declared classes.
    /// `None` — the overwhelmingly common answer — means "no class contains
    /// this code": the engine treats the exit exactly as it did before this
    /// section existed (a crash under the Restart Policy).
    pub fn class_of(&self, code: u16) -> Option<ExitClass> {
        // 18-3 review: defense in depth. `ExitCodes::classes` refuses an
        // overlap, but this resolver is `pub` — a hand-built
        // `ExitCodeClasses` (or a future construction path that skips the
        // validation) could carry a code in BOTH lists. Classification must
        // stay DETERMINISTIC and single-answer: an ambiguous code classifies
        // as nothing (a crash), never "whatever the ordered check hit
        // first".
        let in_restart = self.restart_requested.contains(&code);
        let in_clean = self.clean.contains(&code);
        match (in_restart, in_clean) {
            (true, false) => Some(ExitClass::RestartRequested),
            (false, true) => Some(ExitClass::Clean),
            // An overlap (unvalidated construction) or a miss: no class.
            _ => None,
        }
    }
}

/// The optional `[exit_codes]` section (joins the contract `1.1.0` minor):
/// the two ADAPTER-DECLARED exit-code classes —
///
/// ```toml
/// [exit_codes]
/// restart_requested = [75]
/// clean = [0]
/// ```
///
/// Both lists are OPTIONAL (default empty); an absent section — or one
/// declaring nothing — leaves supervision byte-identical to before. The
/// classes classify RUNS, not launches: an exit during `starting` (survival
/// window, readiness probe, ACP handshake) is a launch failure and is never
/// classified, and a signal exit carries no code to classify.
///
/// Semantics (the engine enforces these; this section only declares them):
///
/// * `restart_requested` — the agent asked its supervisor for a relaunch.
///   The exit takes the ordinary crash-path dance (`running → failed` + a
///   restart plan on the standard backoff) but under the
///   `restart-requested` cause, and the consecutive-failure counter is NOT
///   charged. A restart-requested loop therefore NEVER trips the crash-loop
///   terminal — the operator asked — while the backoff still grows to its
///   cap, bounding the rate.
/// * `clean` — the agent finished on purpose. The instance lands `stopped`
///   (settled like a requested stop), no restart plan, no counter action.
///
/// The operator's `RestartPolicy` stays the operator's: `never` outranks
/// `restart_requested` (terminal failure, no plan).
///
/// Validation is [`ExitCodes::classes`] (consumed by [`Manifest::validate`]):
/// every code in `0..=255` (the portable exit-code domain — unix exit
/// status carries the low 8 bits, so a Windows code above 255 could never be
/// observed there; declare the portable form), the two lists DISJOINT (an
/// overlap names both fields), and no duplicates within one list. The
/// section is `deny_unknown_fields` like every section struct, so an engine
/// built at contract 1.0.0 rejects it at parse time (the policy's
/// older-engine rule, for free).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitCodes {
    /// Exit codes with which the agent requests a relaunch (e.g. `[75]` for
    /// the conventional `EX_TEMPFAIL`-style contract restart).
    #[serde(default)]
    pub restart_requested: Vec<u16>,
    /// Exit codes with which the agent finishes deliberately (commonly
    /// `[0]`).
    #[serde(default)]
    pub clean: Vec<u16>,
}

impl ExitCodes {
    /// Resolve this section into its [`ExitCodeClasses`], enforcing the
    /// whole value matrix and naming the first failing field (the section's
    /// validation — run at registration via [`Manifest::validate`] and
    /// re-checked at start and adoption like the `[config]` mapping and the
    /// `[readiness]`/`[liveness]` sections).
    ///
    /// Rules, in stable diagnostic order:
    /// * every code in `restart_requested` and `clean` is `0..=255` (the
    ///   portable exit-code domain);
    /// * no duplicates within ONE list (a repeated code is a declaration
    ///   typo, not a semantic);
    /// * the two lists are DISJOINT — an overlap is refused naming BOTH
    ///   fields, because one exit cannot be both a relaunch request and a
    ///   clean finish.
    pub fn classes(&self) -> Result<ExitCodeClasses, ManifestError> {
        for (field, codes) in [
            ("`restart_requested`", &self.restart_requested),
            ("`clean`", &self.clean),
        ] {
            for code in codes {
                if *code > 255 {
                    return Err(ManifestError::InvalidField {
                        field: format!("{field} list in the `[exit_codes]` section"),
                        detail: format!(
                            "{code} is out of range — exit codes must be 0..=255, the \
                             portable exit-code domain (unix exit status carries the low \
                             8 bits, so a wider code could never be observed there)"
                        ),
                    });
                }
            }
            let mut seen = std::collections::BTreeSet::new();
            for code in codes {
                if !seen.insert(*code) {
                    return Err(ManifestError::InvalidField {
                        field: format!("{field} list in the `[exit_codes]` section"),
                        detail: format!(
                            "{code} appears more than once — declare each code at most \
                             once per list"
                        ),
                    });
                }
            }
        }
        let overlap: Vec<u16> = self
            .restart_requested
            .iter()
            .filter(|c| self.clean.contains(c))
            .copied()
            .collect();
        if !overlap.is_empty() {
            let names = overlap
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ManifestError::InvalidField {
                field: "`[exit_codes]` section".to_string(),
                detail: format!(
                    "code(s) {names} appear in BOTH `restart_requested` and `clean` — a \
                     code can belong to only ONE class (an exit is either a relaunch \
                     request or a clean finish, never both); remove it from one list"
                ),
            });
        }
        Ok(ExitCodeClasses {
            restart_requested: self.restart_requested.clone(),
            clean: self.clean.clone(),
        })
    }
}

/// The resolved `[drain]` section (story 18-4): the bounded-wait values the
/// engine's drain composite consumes. Produced only by [`Drain::spec`] (the
/// same validate-at-registration / re-check-at-use family as
/// [`ReadinessGate`](crate::ReadinessGate) / [`LivenessCheck`](crate::LivenessCheck)
/// / [`ExitCodeClasses`](crate::ExitCodeClasses)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainSpec {
    /// The optional idle signal path, AGENT-HOME-RELATIVE (the agent touches it
    /// when its in-flight work reaches zero; the engine reads its freshness).
    /// `None` = no declared idle signal.
    pub idle_file: Option<String>,
    /// The declared drain bound, in milliseconds (`1..=600_000`).
    pub timeout_ms: u64,
}

/// The `[drain]` section (story 18-4, added at contract `1.1.0`): the bounded
/// wait the engine's DRAIN composite (`stop new work → wait, bounded → stop`)
/// observes after running the declared `[lifecycle.pause]` command.
///
/// Fields:
///
/// * `timeout_ms` (REQUIRED) — the drain's wall-clock bound, `1..=600_000`
///   (the same ten-minute ceiling the `[readiness]`/`[liveness]` windows use).
///   A drain that outlives it still stops (the composite's last step is
///   unconditional); the outcome is recorded honestly (`completed: false`).
/// * `idle_file` (OPTIONAL) — an Agent-Home-RELATIVE path the agent TOUCHES
///   when its in-flight work reaches zero. Freshness, not existence, is the
///   signal (the engine's evaluator applies a small freshness slack — a file
///   touched once at startup must not read idle forever), and an
///   absent-or-stale file reads "not idle yet", never an error.
///
/// The section composes with the `[lifecycle.pause]`/`[lifecycle.resume]` op
/// templates (v1.0.0 schema, EXECUTED since 18-4): the drain runs the declared
/// pause command, waits (this section's bound) for the command's exit-0 AND the
/// idle file's freshness, then stops. A `[lifecycle.pause]` WITHOUT this
/// section still drains — with the engine's default 30 s bound (the verb must
/// work; documented). Validation is [`Drain::spec`], run by
/// [`Manifest::validate`]; the section is `deny_unknown_fields` like every
/// section struct, so an engine built at contract 1.0.0 rejects it at parse
/// time (the policy's older-engine rule, for free).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Drain {
    /// The optional idle signal path (Agent-Home-relative).
    #[serde(default)]
    pub idle_file: Option<String>,
    /// The drain bound in milliseconds (required, `1..=600_000`).
    pub timeout_ms: u64,
}

impl Drain {
    /// Resolve this section into its [`DrainSpec`], enforcing the whole value
    /// matrix and naming the first failing field (the section's validation —
    /// run at registration via [`Manifest::validate`] and re-checked at pause/
    /// resume/drain like the `[readiness]`/`[liveness]`/`[exit_codes]`
    /// sections).
    ///
    /// Rules, in stable diagnostic order:
    /// * `timeout_ms` is `1..=600_000` — `0` cannot bound a wait (it would
    ///   time out before any poll), and anything above the ten-minute ceiling
    ///   is a stalled fleet read, not a drain;
    /// * `idle_file`, when present, is non-empty and free of control
    ///   characters (the C0/C1 sweep — it is used as a filesystem path),
    ///   RELATIVE (an absolute path would read a file outside the Agent Home
    ///   the engine has authority over), and carries no `..` component (the
    ///   same escape refusal the `[config]` file targets enforce — the idle
    ///   signal lives inside the agent's home or nowhere).
    pub fn spec(&self) -> Result<DrainSpec, ManifestError> {
        if !(1..=600_000).contains(&self.timeout_ms) {
            return Err(ManifestError::InvalidField {
                field: "`timeout_ms` field in the `[drain]` section".to_string(),
                detail: format!(
                    "{} is out of range — the drain bound must be 1..=600_000 \
                     milliseconds (zero cannot bound a wait; ten minutes is the \
                     declared ceiling)",
                    self.timeout_ms
                ),
            });
        }
        let idle_file = match &self.idle_file {
            None => None,
            Some(path) => {
                if path.trim().is_empty() {
                    return Err(ManifestError::InvalidField {
                        field: "`idle_file` field in the `[drain]` section".to_string(),
                        detail: "the value is empty".to_string(),
                    });
                }
                if contains_control_char(path) {
                    return Err(ManifestError::InvalidField {
                        field: "`idle_file` field in the `[drain]` section".to_string(),
                        detail: "the drain idle_file path contains a control character \
                                 (NUL and the rest of the C0/C1 control bytes are \
                                 forbidden — they would corrupt the path)"
                            .to_string(),
                    });
                }
                if std::path::Path::new(path).is_absolute() {
                    return Err(ManifestError::InvalidField {
                        field: "`idle_file` field in the `[drain]` section".to_string(),
                        detail: format!(
                            "'{path}' is an ABSOLUTE path — the idle_file is resolved \
                             against the instance's Agent Home and must be relative \
                             (an absolute path would read a file outside the engine's \
                             path authority)"
                        ),
                    });
                }
                if std::path::Path::new(path)
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
                {
                    return Err(ManifestError::InvalidField {
                        field: "`idle_file` field in the `[drain]` section".to_string(),
                        detail: format!(
                            "'{path}' contains a `..` component — the idle_file must \
                             stay INSIDE the Agent Home (the same escape refusal the \
                             `[config]` file targets enforce)"
                        ),
                    });
                }
                Some(path.clone())
            }
        };
        Ok(DrainSpec {
            idle_file,
            timeout_ms: self.timeout_ms,
        })
    }
}

/// Convenience constructor naming a required-but-absent `[readiness]` field
/// (the `missing` phrasing the other section validators use).
fn required_str<'a>(value: Option<&'a str>, field: &'static str) -> Result<&'a str, ManifestError> {
    match value {
        Some(v) if !v.trim().is_empty() => Ok(v),
        Some(_) => Err(ManifestError::InvalidField {
            field: field.to_string(),
            detail: "the value is empty".to_string(),
        }),
        None => Err(ManifestError::missing(field)),
    }
}

/// Whether `s` carries any control character: the C0 set (U+0000..=U+001F —
/// NUL, newline, carriage return, tab, escape) or the C1 set
/// (U+007F..=U+009F). A `[readiness]` text field is matched against captured
/// process output or used as a filesystem path; control bytes would corrupt
/// both (a newline in a probe URL is header-injection material), so they are
/// refused outright at validation.
fn contains_control_char(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\u{0000}'..='\u{001F}' | '\u{007F}'..='\u{009F}'))
}

/// The shared control-character refusal for a `[readiness]` text field.
/// The message keeps the "NUL" wording the older NUL-only pin asserted, now
/// covering the whole C0/C1 sweep.
fn control_char_error(field: &str, what: &str) -> ManifestError {
    ManifestError::InvalidField {
        field: format!("{field} field in the `[readiness]` section"),
        detail: format!(
            "the readiness {what} contains a control character (NUL and the rest of the \
             C0/C1 control bytes are forbidden — they would corrupt the matched output or \
             the path)"
        ),
    }
}

/// Enforce the loopback-probe posture on an http-get URL's authority
/// (`host[:port]`, already split from scheme and path): the host must be a
/// LITERAL loopback address — `127.0.0.0/8` or `::1` — and the optional port
/// must be `1..=65535` (default 80 for `http://`).
///
/// No hostnames (`localhost` included): a name would send the probe through
/// the machine's resolver/DNS, and neither declared poll — the `[readiness]`
/// gate nor the `[liveness]` check — must only ever touch this host's own
/// loopback; a literal parses locally, deterministically. IPv6 takes the
/// bracketed `[addr]:port` form (a bare `::1` is refused — its colons are
/// ambiguous against the port separator); userinfo (`@`) is refused.
///
/// `field` names the offending manifest field in the error (the `[readiness]`
/// and `[liveness.http_get]` sections word it differently); `what` reads as
/// the polling role in the requirement clause ("readiness probe" / "liveness
/// check") so each section's rejection speaks its own name.
fn require_loopback_authority(
    authority: &str,
    url: &str,
    field: &str,
    what: &str,
) -> Result<(), ManifestError> {
    let refuse = |why: String| ManifestError::InvalidField {
        field: field.to_string(),
        detail: format!(
            "{why} — the {what} URL must target a literal loopback address \
                 (e.g. 127.0.0.1 or [::1])"
        ),
    };
    if authority.contains('@') {
        return Err(refuse(format!(
            "'{url}' carries userinfo; an authority is host[:port] only"
        )));
    }
    // Split host from port. Inside `[...]` a `:` belongs to the IPv6
    // literal, so a bracketed authority splits at the closing bracket;
    // unbracketed, the LAST `:` (if any) is the port separator and more
    // than one `:` means a bare IPv6 literal that should have been
    // bracketed.
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        match rest.split_once(']') {
            Some((literal, tail)) => {
                let port = match tail.strip_prefix(':') {
                    Some(p) => Some(p),
                    None if tail.is_empty() => None,
                    None => {
                        return Err(refuse(format!(
                            "'{url}' has trailing characters after the bracketed host"
                        )));
                    }
                };
                (literal, port)
            }
            None => {
                return Err(refuse(format!(
                    "'{url}' opens a `[` bracket it never closes"
                )));
            }
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p)),
            Some(_) => {
                return Err(refuse(format!(
                    "'{url}' is a bare IPv6 literal — bracket it: [::1]"
                )));
            }
            None => (authority, None),
        }
    };
    let addr: IpAddr = host.parse().map_err(|_| {
        refuse(format!(
            "'{url}' names host '{host}', which is not a literal IP address (hostnames — \
             including `localhost` — would consult the machine's resolver)"
        ))
    })?;
    if !addr.is_loopback() {
        return Err(refuse(format!(
            "'{url}' targets {addr}, which is not a loopback address"
        )));
    }
    if let Some(p) = port {
        let valid = p.parse::<u16>().map(|n| n > 0).unwrap_or(false);
        if !valid {
            return Err(refuse(format!(
                "'{url}' carries port '{p}' — the port must be a decimal 1..=65535"
            )));
        }
    }
    Ok(())
}

/// Why a manifest is invalid (spine AD-3; `thiserror`, never `miette`).
///
/// Every message NAMES the failing section or the invalid value (AC2), so `hekma`
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
            } else if expected_clause.contains("http-get")
                || expected_clause.contains("file-fresh")
                || expected_clause.contains("log-contains")
            {
                format!("the `[readiness]` section has an invalid `kind`: {message}")
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

        // The pause/resume PAIR rule (story 18-4): declaring exactly ONE of
        // `[lifecycle.pause]` / `[lifecycle.resume]` is refused at registration,
        // naming BOTH fields. Legal schema (both fields are optional seeds),
        // new SEMANTIC rule: a pause without a resume is an unresumable trap —
        // since 18-4 executes the pair, a cooperatively-paused instance could
        // never be returned to `running` by its own adapter — and every other
        // bad manifest dies at registration, so this one does too.
        if let Some(lifecycle) = &self.lifecycle {
            if lifecycle.pause.is_some() != lifecycle.resume.is_some() {
                let declared = if lifecycle.pause.is_some() {
                    "`pause`"
                } else {
                    "`resume`"
                };
                let missing = if lifecycle.pause.is_some() {
                    "`resume`"
                } else {
                    "`pause`"
                };
                return Err(ManifestError::InvalidField {
                    field: "`[lifecycle]` section".to_string(),
                    detail: format!(
                        "{declared} is declared WITHOUT {missing} — the lifecycle \
                         pause/resume templates form a PAIR (a cooperative pause the \
                         adapter cannot reverse would strand the instance paused \
                         forever); declare both, or neither (an OS-signal pause needs \
                         no templates)"
                    ),
                });
            }
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

        // [readiness] probe (added at contract 1.1.0), IF present. Absent is
        // valid (the pre-1.1.0 behavior — no declared probe — and the common
        // case). A present-but-invalid section names its first failing field
        // (`Readiness::gate` enforces the value matrix: the timeout range,
        // the kind's own required fields, the http:// loopback-literal
        // posture, and the foreign-field sweep).
        if let Some(readiness) = &self.readiness {
            readiness.gate()?;
        }

        // [liveness] check (added at contract 1.1.0 alongside [readiness]), IF
        // present. Absent is valid (the pre-1.1.0 behavior — the engine watches
        // PROCESS liveness only — and the common case). A present-but-invalid
        // section names its first failing field (`Liveness::check` enforces the
        // exactly-one-sub-table guard, the shared window range, the chosen
        // kind's own required fields, and the same http:// loopback-literal
        // posture [readiness] answers to).
        if let Some(liveness) = &self.liveness {
            liveness.check()?;
        }

        // [exit_codes] classes (added at contract 1.1.0 alongside [readiness]
        // and [liveness]), IF present. Absent is valid (the pre-classification
        // behavior — every unrequested exit is a crash — and the common
        // case). A present-but-invalid section names its first failing field
        // (`ExitCodes::classes` enforces the portable 0..=255 code domain, the
        // no-duplicates rule, and the DISJOINT-lists rule that names both
        // fields on an overlap).
        if let Some(exit_codes) = &self.exit_codes {
            exit_codes.classes()?;
        }

        // [drain] section (story 18-4, joins the contract 1.1.0 minor), IF
        // present. Absent is valid (the engine's default drain posture — a
        // declared pause command gets the default 30 s bound; nothing declared
        // is a degenerate direct stop). A present-but-invalid section names its
        // first failing field (`Drain::spec` enforces the 1..=600_000 bound and
        // the idle_file's relative/in-home/control-char rules).
        if let Some(drain) = &self.drain {
            drain.spec()?;
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

    /// The declared readiness [`ReadinessGate`] (added at contract 1.1.0),
    /// present only after [`Self::validate`] (which resolves the section).
    ///
    /// Returns `None` when the manifest declares no `[readiness]` section — the
    /// engine's start seam treats that as "no probe" (the pre-1.1.0 behavior,
    /// byte-identical). The `Result` is defensive depth: a validated manifest's
    /// section already resolved cleanly, but the engine calls THIS accessor, so
    /// it re-states the failure honestly rather than panicking.
    pub fn readiness_probe(&self) -> Result<Option<ReadinessGate>, ManifestError> {
        match &self.readiness {
            Some(section) => section.gate().map(Some),
            None => Ok(None),
        }
    }

    /// The declared liveness [`LivenessCheck`] (added at contract 1.1.0),
    /// present only after [`Self::validate`] (which resolves the section).
    ///
    /// Returns `None` when the manifest declares no `[liveness]` section — the
    /// engine's reaper treats that as "no declared check" (the pre-1.1.0
    /// behavior, byte-identical: process liveness only). The `Result` is
    /// defensive depth, mirroring [`Self::readiness_probe`]: a validated
    /// manifest's section already resolved cleanly, but the engine calls THIS
    /// accessor, so it re-states the failure honestly rather than panicking.
    pub fn liveness_check(&self) -> Result<Option<LivenessCheck>, ManifestError> {
        match &self.liveness {
            Some(section) => section.check().map(Some),
            None => Ok(None),
        }
    }

    /// The declared exit-code [`ExitCodeClasses`] (added at contract 1.1.0),
    /// present only after [`Self::validate`] (which resolves the section).
    ///
    /// Returns `None` when the manifest declares no `[exit_codes]` section —
    /// the engine's crash classifier treats that as "classify nothing" (the
    /// pre-classification behavior, byte-identical: every unrequested exit is
    /// a crash under the Restart Policy). The `Result` is defensive depth,
    /// mirroring [`Self::readiness_probe`] / [`Self::liveness_check`]: a
    /// validated manifest's section already resolved cleanly, but the engine
    /// calls THIS accessor, so it re-states the failure honestly rather than
    /// panicking.
    pub fn exit_codes(&self) -> Result<Option<ExitCodeClasses>, ManifestError> {
        match &self.exit_codes {
            Some(section) => section.classes().map(Some),
            None => Ok(None),
        }
    }

    /// The declared drain [`DrainSpec`] (story 18-4, added at contract 1.1.0),
    /// present only after [`Self::validate`] (which resolves the section).
    ///
    /// Returns `None` when the manifest declares no `[drain]` section — the
    /// engine's drain verb treats that as "no declared wait" (the engine's
    /// default posture: a declared pause command drains under its own default
    /// bound; nothing declared is a degenerate direct stop). The `Result` is
    /// defensive depth, mirroring [`Self::readiness_probe`] /
    /// [`Self::liveness_check`] / [`Self::exit_codes`]: a validated manifest's
    /// section already resolved cleanly, but the engine calls THIS accessor, so
    /// it re-states the failure honestly rather than panicking.
    pub fn drain(&self) -> Result<Option<DrainSpec>, ManifestError> {
        match &self.drain {
            Some(section) => section.spec().map(Some),
            None => Ok(None),
        }
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

    // -- [readiness] (added at contract 1.0.0 → 1.1.0) ----------------------

    /// Append a `[readiness]` section carrying exactly `body` to [`VALID`].
    fn with_readiness(body: &str) -> String {
        format!("{VALID}\n[readiness]\n{body}\n")
    }

    #[test]
    fn contract_v1_0_0_engine_rejects_the_readiness_section() {
        // The v1.0.0-rejection PIN (the versioning policy's
        // older-same-major-engine rule, stated on the release surface): an
        // engine built at contract 1.0.0 has a `Manifest` schema WITHOUT the
        // `readiness` field, and `deny_unknown_fields` refuses the unknown
        // section at PARSE time. Simulated here with the exact pre-1.1.0
        // shape — the same section list, minus `readiness` — against a
        // 1.1.0 manifest that declares the section. The diagnostic must NAME
        // it (`readiness`), so an operator on an older engine learns which
        // section their engine does not speak.
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct V1EngineManifest {
            #[allow(dead_code)]
            contract_version: Option<String>,
            #[allow(dead_code)]
            adapter: Option<AdapterIdentity>,
            #[allow(dead_code)]
            lifecycle: Option<Lifecycle>,
            #[allow(dead_code)]
            capabilities: Option<CapabilityDeclaration>,
            #[allow(dead_code)]
            metering: Option<Metering>,
            #[allow(dead_code)]
            interaction: Option<Interaction>,
            #[allow(dead_code)]
            config: Option<ConfigMapping>,
        }
        let toml = with_readiness("kind = \"log-contains\"\ntimeout_ms = 5000\nline = \"ready\"");
        let err = toml::from_str::<V1EngineManifest>(&toml).unwrap_err();
        assert!(err.to_string().contains("readiness"), "got {err}");
    }

    #[test]
    fn no_readiness_section_is_valid_and_resolves_to_none() {
        // The pre-1.1.0 shape: no section, no probe — the engine's start
        // behavior must stay byte-identical for it.
        let m = Manifest::from_toml_str(VALID).expect("parse");
        m.validate().expect("validate");
        assert!(m.readiness_probe().expect("resolve").is_none());
    }

    #[test]
    fn readiness_http_get_parses_and_resolves() {
        let toml = with_readiness(
            "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \
             \"http://127.0.0.1:8080/health\"\nexpected_status = 200",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("validate");
        assert_eq!(
            m.readiness_probe().expect("resolve"),
            Some(ReadinessGate {
                timeout_ms: 5000,
                probe: ReadinessProbe::HttpGet(HttpGetProbe {
                    url: "http://127.0.0.1:8080/health".to_string(),
                    expected_status: 200,
                }),
            })
        );
    }

    #[test]
    fn readiness_file_fresh_parses_and_resolves() {
        let toml = with_readiness(
            "kind = \"file-fresh\"\ntimeout_ms = 5000\npath = \"ready.marker\"\nmax_age_ms = 250",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("validate");
        assert_eq!(
            m.readiness_probe().expect("resolve"),
            Some(ReadinessGate {
                timeout_ms: 5000,
                probe: ReadinessProbe::FileFresh(FileFreshProbe {
                    path: "ready.marker".to_string(),
                    max_age_ms: 250,
                }),
            })
        );
    }

    #[test]
    fn readiness_log_contains_parses_and_resolves() {
        let toml =
            with_readiness("kind = \"log-contains\"\ntimeout_ms = 5000\nline = \"listening on\"");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("validate");
        assert_eq!(
            m.readiness_probe().expect("resolve"),
            Some(ReadinessGate {
                timeout_ms: 5000,
                probe: ReadinessProbe::LogContains(LogContainsProbe {
                    line: "listening on".to_string(),
                }),
            })
        );
    }

    #[test]
    fn readiness_timeout_bounds_are_enforced() {
        // Below 1 is not a poll at all; above 600_000 a start would hold
        // supervision unboundedly. Both name the field. Both are written as
        // plain TOML integers — TOML allows digit separators, so `600_001`
        // parses fine and must reach VALIDATION (the old comment claiming
        // it was not TOML integer syntax was wrong; the rejection is the
        // range check, not the parser).
        for timeout in ["0", "600_001"] {
            let toml = with_readiness(&format!(
                "kind = \"log-contains\"\ntimeout_ms = {timeout}\nline = \"ready\""
            ));
            let m = Manifest::from_toml_str(&toml).expect("toml must parse");
            let err = m.validate().unwrap_err();
            assert!(
                err.to_string().contains("timeout_ms"),
                "timeout {timeout} must name the field: {err}"
            );
        }
    }

    #[test]
    fn readiness_non_http_url_is_rejected_naming_the_field() {
        // The loopback-probe posture: http:// only — no TLS, no other schemes.
        for url in [
            "https://127.0.0.1:8080/health",
            "tcp://127.0.0.1:8080",
            "127.0.0.1:8080/health",
        ] {
            let toml = with_readiness(&format!(
                "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \"{url}\"\nexpected_status = 200"
            ));
            let m = Manifest::from_toml_str(&toml).expect("parse");
            let err = m.validate().unwrap_err();
            let text = err.to_string();
            assert!(
                text.contains("`url`") && text.contains("readiness"),
                "url {url} must name the field + section: {text}"
            );
        }
    }

    #[test]
    fn readiness_url_without_a_host_is_rejected() {
        let toml = with_readiness(
            "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \"http://\"\nexpected_status = 200",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            err.to_string().contains("no host"),
            "a host-less URL must name why: {err}"
        );
    }

    #[test]
    fn readiness_http_url_must_target_a_literal_loopback_address() {
        // The loopback-literal posture: the authority is PARSED as an IP,
        // never resolved — a name would consult the machine's resolver/DNS,
        // and the gate must only ever touch this host's own loopback. The
        // whole `127.0.0.0/8` counts (not just `.1`), `::1` takes the
        // bracketed form, the port is optional (`http://` default 80) or
        // `1..=65535`.
        for url in [
            "http://127.0.0.1:8080/health",
            "http://127.0.0.1/",
            "http://127.0.0.2:9/x",
            "http://[::1]:9000/x",
            "http://[::1]/",
        ] {
            let toml = with_readiness(&format!(
                "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \
                 \"{url}\"\nexpected_status = 200"
            ));
            Manifest::from_toml_str(&toml)
                .unwrap_or_else(|e| panic!("{url} must parse: {e}"))
                .validate()
                .unwrap_or_else(|e| panic!("{url} must be accepted: {e}"));
        }

        for url in [
            "http://example.com/health",  // a hostname — DNS, never
            "http://localhost:8080/",     // `localhost` included
            "http://192.168.1.5/",        // a literal NON-loopback address
            "http://127.0.0.1:8080@x/",   // userinfo smuggled into the authority
            "http://user@127.0.0.1/",     // classic userinfo form
            "http://127.0.0.1:0/",        // port 0 is not a port
            "http://127.0.0.1:65536/",    // above the u16 port range
            "http://[::2]/",              // IPv6 but not loopback
            "http://[::ffff:127.0.0.1]/", // v4-mapped is not `::1`
        ] {
            let toml = with_readiness(&format!(
                "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \
                 \"{url}\"\nexpected_status = 200"
            ));
            let err = Manifest::from_toml_str(&toml)
                .expect("parse")
                .validate()
                .unwrap_err();
            assert!(
                err.to_string().contains("literal loopback address"),
                "{url} must name the loopback posture: {err}"
            );
        }
    }

    #[test]
    fn readiness_url_with_a_fragment_is_rejected_and_a_query_is_allowed() {
        // A `#fragment` never reaches the wire (it is client-side), so the
        // probe would not request the path the URL names — rejected here at
        // validation, before the runner ever sees it.
        let toml = with_readiness(
            "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \
             \"http://127.0.0.1:8080/health#frag\"\nexpected_status = 200",
        );
        let err = Manifest::from_toml_str(&toml)
            .expect("parse")
            .validate()
            .unwrap_err();
        assert!(
            err.to_string().contains("fragment"),
            "the fragment must be named: {err}"
        );

        // A query (`?`) DOES reach the wire — allowed.
        let toml = with_readiness(
            "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \
             \"http://127.0.0.1:8080/health?probe=1\"\nexpected_status = 200",
        );
        Manifest::from_toml_str(&toml)
            .expect("parse")
            .validate()
            .expect("a query is part of the request path");
    }

    #[test]
    fn readiness_expected_status_bounds_are_enforced() {
        // A status is 100..=599: below 100 is not an HTTP status, 600+ is
        // not a code any conforming server may send.
        for status in [0u16, 99, 600, 1000] {
            let toml = with_readiness(&format!(
                "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \
                 \"http://127.0.0.1:8080/health\"\nexpected_status = {status}"
            ));
            let err = Manifest::from_toml_str(&toml)
                .expect("parse")
                .validate()
                .unwrap_err();
            assert!(
                err.to_string().contains("expected_status"),
                "status {status} must name the field: {err}"
            );
        }
        // The edges hold: 100 (informational) and 599 (max server error).
        for status in [100u16, 599] {
            let toml = with_readiness(&format!(
                "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \
                 \"http://127.0.0.1:8080/health\"\nexpected_status = {status}"
            ));
            Manifest::from_toml_str(&toml)
                .expect("parse")
                .validate()
                .unwrap_or_else(|e| panic!("status {status} must be accepted: {e}"));
        }
    }

    #[test]
    fn readiness_zero_max_age_is_rejected() {
        // A 0 ms freshness window can never be satisfied — the file would
        // always be at least "now" old — so the probe would spin its whole
        // budget on an unsatisfiable ask.
        let toml = with_readiness(
            "kind = \"file-fresh\"\ntimeout_ms = 5000\npath = \"ready.marker\"\nmax_age_ms = 0",
        );
        let err = Manifest::from_toml_str(&toml)
            .expect("parse")
            .validate()
            .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("max_age_ms") && text.contains("freshness"),
            "must name the field and why: {text}"
        );
    }

    #[test]
    fn readiness_control_characters_are_rejected_in_url_path_and_line() {
        // The C0/C1 sweep across all three text fields — beyond NUL, which
        // keeps its own named pin above. A control byte would corrupt the
        // matched output stream or the path (and a newline in a URL is
        // request-smuggling material), so the whole set is refused.
        let cases: &[(&str, &str)] = &[
            ("url", "\\u0001"),
            ("url", "\\u001f"),
            ("url", "\\u007f"),
            ("url", "\\u009f"),
            ("path", "\\u000b"),
            ("path", "\\u0085"),
            ("line", "\\n"),
            ("line", "\\r"),
            ("line", "\\t"),
        ];
        for (field, escaped) in cases {
            let body = match *field {
                "url" => format!(
                    "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \
                     \"http://127.0.0.1:8080/health{escaped}\"\nexpected_status = 200"
                ),
                "path" => format!(
                    "kind = \"file-fresh\"\ntimeout_ms = 5000\npath = \
                     \"ready{escaped}.marker\"\nmax_age_ms = 250"
                ),
                _ => {
                    format!("kind = \"log-contains\"\ntimeout_ms = 5000\nline = \"ready{escaped}\"")
                }
            };
            let err = Manifest::from_toml_str(&with_readiness(&body))
                .expect("parse")
                .validate()
                .unwrap_err();
            assert!(
                err.to_string().contains("control character"),
                "{field} carrying {escaped} must name the control-character rule: {err}"
            );
        }
    }

    #[test]
    fn readiness_max_age_above_timeout_is_rejected() {
        let toml = with_readiness(
            "kind = \"file-fresh\"\ntimeout_ms = 500\npath = \"ready.marker\"\nmax_age_ms = 501",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("max_age_ms") && text.contains("timeout_ms"),
            "must name both fields: {text}"
        );
    }

    #[test]
    fn readiness_empty_or_nul_line_is_rejected() {
        let empty = with_readiness("kind = \"log-contains\"\ntimeout_ms = 5000\nline = \"  \"");
        let err = Manifest::from_toml_str(&empty)
            .expect("parse")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("`line`"), "got {err}");

        let nul = format!(
            "{}\n[readiness]\nkind = \"log-contains\"\ntimeout_ms = 5000\nline = \"a\\u0000b\"\n",
            VALID
        );
        let err = Manifest::from_toml_str(&nul)
            .expect("parse")
            .validate()
            .unwrap_err();
        assert!(
            err.to_string().contains("NUL"),
            "a NUL byte must be named: {err}"
        );
    }

    #[test]
    fn readiness_foreign_kind_fields_are_rejected_naming_the_field() {
        // Exactly ONE probe per manifest, and the kind's shape is exact: a
        // `file-fresh` manifest carrying `url` names the stray field (typo
        // protection — never a silently-ignored key).
        let toml = with_readiness(
            "kind = \"file-fresh\"\ntimeout_ms = 5000\npath = \"ready.marker\"\nmax_age_ms = 250\n\
             url = \"http://127.0.0.1:8080/health\"",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. } if field.contains("`url`")),
            "got {err}"
        );
    }

    #[test]
    fn readiness_missing_kind_fields_are_rejected_naming_the_field() {
        // http-get without expected_status.
        let toml = with_readiness(
            "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \"http://127.0.0.1:8080/health\"",
        );
        let err = Manifest::from_toml_str(&toml)
            .expect("parse")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("expected_status"), "got {err}");

        // file-fresh without max_age_ms.
        let toml =
            with_readiness("kind = \"file-fresh\"\ntimeout_ms = 5000\npath = \"ready.marker\"");
        let err = Manifest::from_toml_str(&toml)
            .expect("parse")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("max_age_ms"), "got {err}");

        // log-contains without line.
        let toml = with_readiness("kind = \"log-contains\"\ntimeout_ms = 5000");
        let err = Manifest::from_toml_str(&toml)
            .expect("parse")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("`line`"), "got {err}");
    }

    #[test]
    fn readiness_unknown_kind_fails_at_parse_naming_the_section() {
        // The closed kind vocabulary: an unrecognized value is an unknown
        // VARIANT at parse time, attributed to the section (the same
        // disambiguation the metering/interaction enums get).
        let toml = with_readiness("kind = \"tcp-connect\"\ntimeout_ms = 5000");
        let err = Manifest::from_toml_str(&toml).unwrap_err();
        assert!(
            matches!(&err, ManifestError::Toml { detail } if detail.contains("[readiness]")),
            "got {err}"
        );
    }

    // -- [liveness] (added at contract 1.0.0 → 1.1.0, alongside [readiness]) -

    /// Append a `[liveness]` section carrying exactly `body` (including the
    /// sub-table header) to [`VALID`].
    fn with_liveness(body: &str) -> String {
        format!("{VALID}\n[liveness]\n{body}\n")
    }

    #[test]
    fn contract_v1_0_0_engine_rejects_the_liveness_section() {
        // The v1.0.0-rejection PIN, liveness twin of the readiness one: an
        // engine built at contract 1.0.0 has a `Manifest` schema WITHOUT the
        // `liveness` field, and `deny_unknown_fields` refuses the unknown
        // section at PARSE time. Simulated with the exact pre-1.1.0 shape —
        // the same section list, minus `readiness` and `liveness` — against a
        // 1.1.0 manifest that declares the section. The diagnostic must NAME
        // it (`liveness`).
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct V1EngineManifest {
            #[allow(dead_code)]
            contract_version: Option<String>,
            #[allow(dead_code)]
            adapter: Option<AdapterIdentity>,
            #[allow(dead_code)]
            lifecycle: Option<Lifecycle>,
            #[allow(dead_code)]
            capabilities: Option<CapabilityDeclaration>,
            #[allow(dead_code)]
            metering: Option<Metering>,
            #[allow(dead_code)]
            interaction: Option<Interaction>,
            #[allow(dead_code)]
            config: Option<ConfigMapping>,
        }
        let toml =
            with_liveness("[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 5000");
        let err = toml::from_str::<V1EngineManifest>(&toml).unwrap_err();
        assert!(err.to_string().contains("liveness"), "got {err}");
    }

    #[test]
    fn no_liveness_section_is_valid_and_resolves_to_none() {
        // The pre-1.1.0 shape: no section, no declared check — the engine's
        // reaper behavior must stay byte-identical for it (process liveness
        // only).
        let m = Manifest::from_toml_str(VALID).expect("parse");
        m.validate().expect("validate");
        assert!(m.liveness_check().expect("resolve").is_none());
    }

    #[test]
    fn liveness_heartbeat_file_parses_and_resolves() {
        let toml = with_liveness(
            "[liveness.heartbeat_file]\npath = \"heartbeat.marker\"\nmax_age_ms = 5000",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("validate");
        assert_eq!(
            m.liveness_check().expect("resolve"),
            Some(LivenessCheck::HeartbeatFile(HeartbeatFileCheck {
                path: "heartbeat.marker".to_string(),
                max_age_ms: 5000,
            }))
        );
    }

    #[test]
    fn liveness_http_get_parses_and_resolves() {
        let toml = with_liveness(
            "[liveness.http_get]\nurl = \"http://127.0.0.1:8080/health\"\nexpected_status = 200\nmax_age_ms = 5000",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("validate");
        assert_eq!(
            m.liveness_check().expect("resolve"),
            Some(LivenessCheck::HttpGet(HttpGetCheck {
                url: "http://127.0.0.1:8080/health".to_string(),
                expected_status: 200,
                max_age_ms: 5000,
            }))
        );
    }

    #[test]
    fn liveness_max_age_ms_accessor_reads_both_kinds() {
        // `max_age_ms()` is the engine's one-number window read: it must see
        // through both variants (a third kind cannot forget to state its
        // window, but the accessor must not lie about the two that exist).
        let hb = LivenessCheck::HeartbeatFile(HeartbeatFileCheck {
            path: "hb.marker".to_string(),
            max_age_ms: 250,
        });
        assert_eq!(hb.max_age_ms(), 250);
        let http = LivenessCheck::HttpGet(HttpGetCheck {
            url: "http://127.0.0.1:8080/health".to_string(),
            expected_status: 200,
            max_age_ms: 750,
        });
        assert_eq!(http.max_age_ms(), 750);
    }

    #[test]
    fn liveness_with_both_sub_tables_is_rejected_naming_the_section() {
        // The exactly-one guard: two declared checks is a contradiction (which
        // signal governs?), refused naming the section.
        let toml = with_liveness(
            "[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 5000\n\
             [liveness.http_get]\nurl = \"http://127.0.0.1:8080/health\"\nexpected_status = 200\nmax_age_ms = 5000",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. } if field == "`[liveness]` section"),
            "got {err}"
        );
        assert!(err.to_string().contains("BOTH checks"), "got {err}");
    }

    #[test]
    fn liveness_empty_section_is_rejected_naming_the_section() {
        // An empty `[liveness]` declares nothing — it would silently supervise
        // nothing new, so it is refused naming the section.
        let toml = with_liveness("");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. } if field == "`[liveness]` section"),
            "got {err}"
        );
        assert!(err.to_string().contains("NO check"), "got {err}");
    }

    #[test]
    fn liveness_max_age_bounds_are_enforced() {
        // The shared window range `250..=600_000`, for BOTH kinds: below the
        // 250 ms floor the window is smaller than one reaper tick (~250 ms),
        // so expiry could go unobserved; above the cap a wedged agent would
        // hold supervision unboundedly. Both name the field.
        for body in [
            "[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 0",
            "[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 249",
            "[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 600_001",
            "[liveness.http_get]\nurl = \"http://127.0.0.1:8080/health\"\nexpected_status = 200\nmax_age_ms = 0",
            "[liveness.http_get]\nurl = \"http://127.0.0.1:8080/health\"\nexpected_status = 200\nmax_age_ms = 249",
            "[liveness.http_get]\nurl = \"http://127.0.0.1:8080/health\"\nexpected_status = 200\nmax_age_ms = 600_001",
        ] {
            let m = Manifest::from_toml_str(&with_liveness(body)).expect("parse");
            let err = m.validate().unwrap_err();
            assert!(
                matches!(&err, ManifestError::InvalidField { field, .. }
                    if field == "`max_age_ms` field in the `[liveness]` section"),
                "body `{body}`: got {err}"
            );
        }
    }

    #[test]
    fn liveness_min_window_is_accepted_at_the_floor() {
        // Exactly 250 is legal (the floor is inclusive) for both kinds.
        let toml =
            with_liveness("[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 250");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("250 ms is the inclusive floor");

        let toml = with_liveness(
            "[liveness.http_get]\nurl = \"http://127.0.0.1:8080/health\"\nexpected_status = 200\nmax_age_ms = 250",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("250 ms is the inclusive floor");
    }

    #[test]
    fn double_scheme_urls_are_rejected_in_both_sections() {
        // The strict single-strip rule: `strip_prefix` removes AT MOST one
        // `http://`, so a double-scheme URL keeps its inner scheme in the
        // authority and is rejected by the loopback-literal rule (an inner
        // scheme is not a literal loopback IP). `trim_start_matches` (the
        // pre-review shape) stripped every repetition and would have
        // LAUNDERED this URL into `127.0.0.1:8080/health`. Pinned for BOTH
        // sections — the shared posture cannot drift.
        let toml = with_liveness(
            "[liveness.http_get]\nurl = \"http://http://127.0.0.1:8080/health\"\nexpected_status = 200\nmax_age_ms = 5000",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. }
                if field == "`url` field in the `[liveness.http_get]` sub-table"),
            "got {err}"
        );

        let toml = with_readiness(
            "kind = \"http-get\"\ntimeout_ms = 5000\nurl = \"http://http://127.0.0.1:8080/health\"\nexpected_status = 200",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. }
                if field == "`url` field in the `[readiness]` section"),
            "got {err}"
        );
    }

    #[test]
    fn liveness_heartbeat_path_rules_are_enforced() {
        // Empty path (the value-is-empty refusal, naming the sub-table field).
        let toml = with_liveness("[liveness.heartbeat_file]\npath = \"\"\nmax_age_ms = 5000");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. }
                if field == "`path` field in the `[liveness.heartbeat_file]` sub-table"),
            "got {err}"
        );

        // Control characters (NUL named explicitly, like the readiness pin).
        let toml = with_liveness(
            "[liveness.heartbeat_file]\npath = \"hb\\u0001.marker\"\nmax_age_ms = 5000",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. }
                if field == "`path` field in the `[liveness.heartbeat_file]` sub-table"),
            "got {err}"
        );
        assert!(err.to_string().contains("control character"), "got {err}");
    }

    #[test]
    fn liveness_http_url_rules_are_enforced() {
        // Non-http scheme (no TLS, no other schemes — loopback only).
        let toml = with_liveness(
            "[liveness.http_get]\nurl = \"https://127.0.0.1:8080/health\"\nexpected_status = 200\nmax_age_ms = 5000",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. }
                if field == "`url` field in the `[liveness.http_get]` sub-table"),
            "got {err}"
        );
        assert!(err.to_string().contains("http://"), "got {err}");

        // Fragment.
        let toml = with_liveness(
            "[liveness.http_get]\nurl = \"http://127.0.0.1:8080/health#x\"\nexpected_status = 200\nmax_age_ms = 5000",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("fragment"), "got {err}");

        // No host at all.
        let toml = with_liveness(
            "[liveness.http_get]\nurl = \"http:///health\"\nexpected_status = 200\nmax_age_ms = 5000",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("no host"), "got {err}");

        // A hostname — the SHARED loopback-literal rule must name the
        // LIVENESS role ("liveness check URL"), proving the sections share
        // one helper without borrowing readiness's wording.
        let toml = with_liveness(
            "[liveness.http_get]\nurl = \"http://localhost:8080/health\"\nexpected_status = 200\nmax_age_ms = 5000",
        );
        let m = Manifest::from_toml_str(&toml).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, .. }
                if field == "`url` field in the `[liveness.http_get]` sub-table"),
            "got {err}"
        );
        assert!(
            err.to_string()
                .contains("the liveness check URL must target a literal loopback address"),
            "got {err}"
        );
    }

    #[test]
    fn liveness_expected_status_bounds_are_enforced() {
        for status in ["99", "600"] {
            let toml = with_liveness(&format!(
                "[liveness.http_get]\nurl = \"http://127.0.0.1:8080/health\"\nexpected_status = {status}\nmax_age_ms = 5000",
            ));
            let m = Manifest::from_toml_str(&toml).expect("parse");
            let err = m.validate().unwrap_err();
            assert!(
                matches!(&err, ManifestError::InvalidField { field, .. }
                    if field == "`expected_status` field in the `[liveness.http_get]` sub-table"),
                "status {status}: got {err}"
            );
        }
    }

    // -- [exit_codes] (added at contract 1.1.0) ------------------------------

    /// Append an `[exit_codes]` section carrying exactly `body` to [`VALID`].
    fn with_exit_codes(body: &str) -> String {
        format!("{VALID}\n[exit_codes]\n{body}\n")
    }

    #[test]
    fn contract_v1_0_0_engine_rejects_the_exit_codes_section() {
        // The v1.0.0-rejection PIN, exit-codes twin of the readiness/liveness
        // ones: an engine built at contract 1.0.0 has a `Manifest` schema
        // WITHOUT the `exit_codes` field, and `deny_unknown_fields` refuses
        // the unknown section at PARSE time. Simulated with the exact
        // pre-18-3 shape — the same section list, minus the three 1.1.0
        // sections — against a 1.1.0 manifest that declares the section. The
        // diagnostic must NAME it (`exit_codes`).
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct V1EngineManifest {
            #[allow(dead_code)]
            contract_version: Option<String>,
            #[allow(dead_code)]
            adapter: Option<AdapterIdentity>,
            #[allow(dead_code)]
            lifecycle: Option<Lifecycle>,
            #[allow(dead_code)]
            capabilities: Option<CapabilityDeclaration>,
            #[allow(dead_code)]
            metering: Option<Metering>,
            #[allow(dead_code)]
            interaction: Option<Interaction>,
            #[allow(dead_code)]
            config: Option<ConfigMapping>,
        }
        let toml = with_exit_codes("restart_requested = [75]\nclean = [0]");
        let err = toml::from_str::<V1EngineManifest>(&toml).unwrap_err();
        assert!(err.to_string().contains("exit_codes"), "got {err}");
    }

    #[test]
    fn no_exit_codes_section_is_valid_and_resolves_to_none() {
        // The pre-18-3 shape: no section, no classes — every unrequested exit
        // is a crash; the engine's behavior must stay byte-identical for it.
        let m = Manifest::from_toml_str(VALID).expect("parse");
        m.validate().expect("validate");
        assert!(m.exit_codes().expect("resolve").is_none());
    }

    #[test]
    fn exit_codes_parse_and_resolve_both_lists() {
        let toml = with_exit_codes("restart_requested = [75, 71]\nclean = [0]");
        let m = Manifest::from_toml_str(&toml).expect("parse");
        m.validate().expect("validate");
        let classes = m.exit_codes().expect("resolve").expect("declared");
        assert_eq!(
            classes,
            ExitCodeClasses {
                restart_requested: vec![75, 71],
                clean: vec![0],
            }
        );
        assert_eq!(classes.class_of(75), Some(ExitClass::RestartRequested));
        assert_eq!(classes.class_of(71), Some(ExitClass::RestartRequested));
        assert_eq!(classes.class_of(0), Some(ExitClass::Clean));
        assert_eq!(classes.class_of(99), None, "unclassified exits stay None");
    }

    #[test]
    fn exit_codes_lists_are_optional_and_default_empty() {
        // Both lists optional (serde defaults), and an entirely empty section
        // is legal: it declares nothing and classifies nothing — exactly like
        // an absent section (never a half-declared contradiction).
        let empty = Manifest::from_toml_str(&with_exit_codes("")).expect("parse");
        empty.validate().expect("validate");
        let resolved = empty.exit_codes().expect("resolve").expect("declared");
        assert_eq!(resolved, ExitCodeClasses::default());
        assert_eq!(resolved.class_of(0), None);

        let one_list = Manifest::from_toml_str(&with_exit_codes("clean = [0]")).expect("parse");
        one_list.validate().expect("validate");
        let resolved = one_list.exit_codes().expect("resolve").expect("declared");
        assert!(resolved.restart_requested.is_empty());
        assert_eq!(resolved.class_of(0), Some(ExitClass::Clean));
    }

    #[test]
    fn exit_codes_above_the_portable_domain_are_refused() {
        // 0..=255 is the portable exit-code domain: unix exit status carries
        // the low 8 bits, so a code above 255 could never be observed there.
        // 256 is the first refusal; 0 and 255 are the accepted edges.
        for body in [
            "restart_requested = [256]",
            "clean = [1000]",
            "restart_requested = [0, 65535]",
        ] {
            let m = Manifest::from_toml_str(&with_exit_codes(body)).expect("parse");
            let err = m.validate().unwrap_err();
            assert!(
                matches!(&err, ManifestError::InvalidField { field, detail }
                    if field.contains("list in the `[exit_codes]` section")
                        && detail.contains("0..=255")),
                "{body}: got {err}"
            );
        }
        let edges = Manifest::from_toml_str(&with_exit_codes(
            "restart_requested = [0, 255]\nclean = [128]",
        ))
        .expect("parse");
        edges.validate().expect("the 0..=255 edges are accepted");
    }

    #[test]
    fn exit_codes_duplicates_within_one_list_are_refused() {
        for body in ["restart_requested = [75, 75]", "clean = [0, 0, 0]"] {
            let m = Manifest::from_toml_str(&with_exit_codes(body)).expect("parse");
            let err = m.validate().unwrap_err();
            assert!(
                matches!(&err, ManifestError::InvalidField { field, detail }
                    if field.contains("list in the `[exit_codes]` section")
                        && detail.contains("more than once")),
                "{body}: got {err}"
            );
        }
    }

    #[test]
    fn exit_codes_overlapping_lists_are_refused_naming_both_fields() {
        let m = Manifest::from_toml_str(&with_exit_codes(
            "restart_requested = [75, 9]\nclean = [0, 9]",
        ))
        .expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, detail }
                if field == "`[exit_codes]` section"
                    && detail.contains("BOTH `restart_requested` and `clean`")
                    && detail.contains('9')),
            "the overlap must name both fields and the offending code: {err}"
        );
        // And the same overlap in the other declaration order.
        let m = Manifest::from_toml_str(&with_exit_codes("clean = [3]\nrestart_requested = [3]"))
            .expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("BOTH"), "got {err}");
    }

    #[test]
    fn exit_codes_negative_values_fail_at_parse() {
        // A negative code is not a process exit status at all — the `u16`
        // field type refuses it at PARSE time (surfaced as the section-naming
        // Toml error), like every other typed field.
        let err = Manifest::from_toml_str(&with_exit_codes("clean = [-1]")).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
    }

    #[test]
    fn class_of_refuses_a_code_in_both_lists_even_when_hand_built() {
        // 18-3 review: `ExitCodeClasses` is `pub` with `pub` fields, so a
        // construction path that skips `ExitCodes::classes` validation can
        // carry an OVERLAP. The classifier must stay deterministic and
        // single-answer: an ambiguous code classifies as NOTHING (a crash),
        // never "whatever the ordered check hit first" — while unambiguous
        // codes still classify normally.
        let ambiguous = ExitCodeClasses {
            restart_requested: vec![75, 9],
            clean: vec![0, 9],
        };
        assert_eq!(ambiguous.class_of(9), None, "an overlap classifies nothing");
        assert_eq!(ambiguous.class_of(75), Some(ExitClass::RestartRequested));
        assert_eq!(ambiguous.class_of(0), Some(ExitClass::Clean));
        assert_eq!(ambiguous.class_of(1), None, "a miss stays None");
    }

    // -- [lifecycle] pair rule + [drain] (story 18-4) ------------------------

    /// Append a `[drain]` section carrying exactly `body` to [`VALID`].
    fn with_drain(body: &str) -> String {
        format!("{VALID}\n[drain]\n{body}\n")
    }

    #[test]
    fn drain_section_parses_and_resolves() {
        let m = Manifest::from_toml_str(&with_drain(
            "timeout_ms = 5000\nidle_file = \"work/idle.marker\"",
        ))
        .expect("parse");
        m.validate().expect("validate");
        assert_eq!(
            m.drain().expect("resolve").expect("declared"),
            DrainSpec {
                idle_file: Some("work/idle.marker".to_string()),
                timeout_ms: 5000,
            }
        );
    }

    #[test]
    fn no_drain_section_is_valid_and_resolves_to_none() {
        // The pre-18-4 shape: no section — the engine's default drain posture
        // (a declared pause command gets its default bound; nothing declared
        // drains degenerately). Behavior must stay byte-identical for it.
        let m = Manifest::from_toml_str(VALID).expect("parse");
        m.validate().expect("validate");
        assert!(m.drain().expect("resolve").is_none());
    }

    #[test]
    fn drain_timeout_out_of_range_is_refused_naming_the_field() {
        // 0 cannot bound a wait; >600_000 busts the declared ten-minute
        // ceiling. The accepted edges are 1 and 600_000.
        for body in [
            "timeout_ms = 0",
            "timeout_ms = 600_001",
            "timeout_ms = 9_999_999",
        ] {
            let m = Manifest::from_toml_str(&with_drain(body)).expect("parse");
            let err = m.validate().unwrap_err();
            assert!(
                matches!(&err, ManifestError::InvalidField { field, detail }
                    if field == "`timeout_ms` field in the `[drain]` section"
                        && detail.contains("1..=600_000")),
                "{body}: got {err}"
            );
        }
        for body in ["timeout_ms = 1", "timeout_ms = 600_000"] {
            let m = Manifest::from_toml_str(&with_drain(body)).expect("parse");
            m.validate().expect("the range edges are accepted");
        }
    }

    #[test]
    fn drain_missing_timeout_is_refused_naming_the_field() {
        // `timeout_ms` is REQUIRED (not a serde default): an empty section is a
        // declaration typo, refused at PARSE time naming the field (the same
        // posture as every other required-by-deserialization field).
        let err = Manifest::from_toml_str(&with_drain("")).unwrap_err();
        assert!(
            matches!(&err, ManifestError::Toml { detail } if detail.contains("timeout_ms")),
            "got {err}"
        );
    }

    #[test]
    fn drain_idle_file_value_matrix_is_enforced() {
        // The idle_file rules: non-empty, control-char-free, RELATIVE, and
        // without a `..` escape. A good relative path passes.
        for (body, field_fragment, detail_fragment) in [
            ("timeout_ms = 100\nidle_file = \"\"", "idle_file", "empty"),
            (
                "timeout_ms = 100\nidle_file = \"idle\\u0001.marker\"",
                "idle_file",
                "control character",
            ),
            (
                "timeout_ms = 100\nidle_file = \"/etc/passwd\"",
                "idle_file",
                "ABSOLUTE",
            ),
            (
                "timeout_ms = 100\nidle_file = \"../outside.marker\"",
                "idle_file",
                "`..`",
            ),
        ] {
            let m = Manifest::from_toml_str(&with_drain(body)).expect("parse");
            let err = m.validate().unwrap_err();
            assert!(
                matches!(&err, ManifestError::InvalidField { field, detail }
                    if field.contains(field_fragment) && detail.contains(detail_fragment)),
                "{body}: got {err}"
            );
        }
        let ok = Manifest::from_toml_str(&with_drain("timeout_ms = 100\nidle_file = \"a/b\""))
            .expect("parse");
        ok.validate().expect("a relative in-home path is accepted");
    }

    #[test]
    fn drain_unknown_fields_fail_at_parse() {
        // deny_unknown_fields (the manifest style) catches typos at parse.
        let err = Manifest::from_toml_str(&with_drain("timeout_ms = 100\nbogus = 1")).unwrap_err();
        assert!(matches!(err, ManifestError::Toml { .. }), "got {err}");
    }

    #[test]
    fn contract_v1_0_0_engine_rejects_the_drain_section() {
        // The v1.0.0-rejection PIN, drain twin of the readiness/liveness/
        // exit-codes ones: an engine built at contract 1.0.0 has a `Manifest`
        // schema WITHOUT the `drain` field, and `deny_unknown_fields` refuses
        // the unknown section at PARSE time. Simulated with the exact pre-18-4
        // shape — the same section list, minus the 1.1.0 sections — against a
        // 1.1.0 manifest that declares the section. The diagnostic must NAME
        // it (`drain`).
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct V1EngineManifest {
            #[allow(dead_code)]
            contract_version: Option<String>,
            #[allow(dead_code)]
            adapter: Option<AdapterIdentity>,
            #[allow(dead_code)]
            lifecycle: Option<Lifecycle>,
            #[allow(dead_code)]
            capabilities: Option<CapabilityDeclaration>,
            #[allow(dead_code)]
            metering: Option<Metering>,
            #[allow(dead_code)]
            interaction: Option<Interaction>,
            #[allow(dead_code)]
            config: Option<ConfigMapping>,
        }
        let toml = with_drain("timeout_ms = 5000");
        let err = toml::from_str::<V1EngineManifest>(&toml).unwrap_err();
        assert!(err.to_string().contains("drain"), "got {err}");
    }

    #[test]
    fn pause_without_resume_is_refused_naming_both_fields() {
        // The 18-4 PAIR rule: exactly ONE of pause/resume is a legal-schema
        // declaration with a TRAP semantic (an unresumable cooperative pause),
        // so registration refuses it naming BOTH fields. Both directions are
        // pinned (pause-only and resume-only); declaring BOTH passes. The
        // splice point is `[lifecycle.stop]` — a full table header — so the
        // inserted tables never swallow `VALID`'s own fields.
        let pause_only = VALID.replacen(
            "[lifecycle.stop]",
            "[lifecycle.pause]\nexec = \"/bin/true\"\n\n[lifecycle.stop]",
            1,
        );
        let m = Manifest::from_toml_str(&pause_only).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(
            matches!(&err, ManifestError::InvalidField { field, detail }
                if field == "`[lifecycle]` section"
                    && detail.contains("`pause`")
                    && detail.contains("`resume`")),
            "the refusal must name both fields: {err}"
        );

        let resume_only = VALID.replacen(
            "[lifecycle.stop]",
            "[lifecycle.resume]\nexec = \"/bin/true\"\n\n[lifecycle.stop]",
            1,
        );
        let m = Manifest::from_toml_str(&resume_only).expect("parse");
        let err = m.validate().unwrap_err();
        assert!(err.to_string().contains("`resume`"), "got {err}");

        // The declared PAIR passes validation (the 18-4 target shape).
        let pair = VALID.replacen(
            "[lifecycle.stop]",
            "[lifecycle.pause]\nexec = \"/bin/true\"\n\n\
             [lifecycle.resume]\nexec = \"/bin/true\"\n\n[lifecycle.stop]",
            1,
        );
        let m = Manifest::from_toml_str(&pair).expect("parse");
        m.validate().expect("the declared pair is valid");
    }
}
