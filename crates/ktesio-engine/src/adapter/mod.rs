//! Adapter resolution + manifest loading/validation (spine AD-3).
//!
//! This module RESOLVES a requested adapter — a native builtin by kind, or a
//! manifest by path — into a [`ResolvedAdapter`]: a uniform, parsed view the
//! registry consumes (its kind, its effective current-OS Capability
//! Declaration, its Metering Source, and, for manifest adapters, the manifest
//! path 1-4 will need to launch it).
//!
//! ## Boundary: PARSE + VALIDATE only — EXECUTES NOTHING (CRITICAL)
//!
//! The loader reads, parses (via [`ktesio_adapter_api::Manifest`]), and
//! validates. It stores templates and declarations. It does **not** run any
//! lifecycle op, spawn any process, or add tokio — that is story 1-4's manifest
//! executor. The `AgentAdapter` lifecycle methods are never called here; only
//! the declaration accessors are read.
//!
//! ## Schema ownership (AD-3)
//!
//! The engine defines **no** manifest schema. It consumes
//! `ktesio-adapter-api`'s parsed [`Manifest`]/[`CapabilityDeclaration`]/
//! [`MeteringSource`] types. The manifest-adapter view built here is derived
//! entirely from that crate's validated output.
//!
//! ## Native builtins and the conformance boundary
//!
//! The shipping engine resolves native kinds through a small builtin table
//! ([`resolve_native`]). The `mock` kind resolves to an **engine-internal**
//! [`builtin::BuiltinMock`], deliberately NOT the `ktesio-conformance`
//! `MockAdapter`: a normal `engine → conformance` edge would be transitive into
//! `kt` and trip the AD-2 boundary gate. The conformance mock stays a
//! dev/test fixture (imported as a dev-dependency where tests need it); the two
//! share the same declared shape.

mod builtin;

use std::path::{Path, PathBuf};

use ktesio_adapter_api::{
    CapabilityDeclaration, ConfigMapping, ConfigTarget, EffectiveCapabilities, Manifest,
    ManifestError, MeteringSource, OsId,
};

use crate::domain::{pass_through_tail, EffectiveConfig};

use thiserror::Error;

pub use builtin::{native_config_mapping, native_launch};

/// The canonical `adapter.toml` filename inside a manifest-adapter directory.
pub const MANIFEST_FILE: &str = "adapter.toml";

/// A request to resolve an adapter for a registration (spine AD-3 "two kinds").
///
/// `kt` builds this from its flags: `--kind <kind>` → [`AdapterRef::Native`],
/// `--manifest <path>` → [`AdapterRef::Manifest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterRef {
    /// A native, compiled-in adapter selected by its kind string (e.g. `mock`).
    Native(String),
    /// A manifest adapter loaded from a directory (or a direct file) path.
    Manifest(PathBuf),
}

/// A resolved, validated adapter view the registry persists with an instance.
///
/// Uniform across both kinds (AD-3 "two kinds, one trait"): the registry never
/// branches on native-vs-manifest after resolution. Holds the effective
/// current-OS Capability Declaration (already projected) plus the full
/// declaration (for completeness) and the Metering Source.
#[derive(Clone, Debug)]
pub struct ResolvedAdapter {
    /// The value stored in the instance `kind` column.
    kind: String,
    /// The full per-OS Capability Declaration (AD-4).
    declaration: CapabilityDeclaration,
    /// The declared Metering Source (AD-7; always viable — validated).
    metering_source: MeteringSource,
    /// For a manifest adapter, the resolved absolute manifest path (1-4 needs
    /// it to launch). `None` for a native adapter.
    manifest_path: Option<PathBuf>,
    /// The resolved `start` launch (exec + args + env), captured from the SAME
    /// manifest parse that produced `declaration`. `None` for a native adapter
    /// (no launch command) or a manifest with no `[lifecycle.start]` template.
    /// The registry persists this into the adapter snapshot so the start path
    /// USES it instead of re-reading the manifest — removing the fragile
    /// start-time re-read that dropped `args` on hosted CI runners.
    launch: Option<StartLaunch>,
}

impl ResolvedAdapter {
    /// The kind string to store on the Agent Instance.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The full per-OS Capability Declaration.
    pub fn declaration(&self) -> &CapabilityDeclaration {
        &self.declaration
    }

    /// The declared Metering Source.
    pub fn metering_source(&self) -> MeteringSource {
        self.metering_source
    }

    /// The manifest path, if this is a manifest adapter.
    pub fn manifest_path(&self) -> Option<&Path> {
        self.manifest_path.as_deref()
    }

    /// The resolved `start` launch captured at resolution, if any. `None` for a
    /// native adapter (no launch command). The registry snapshots this at
    /// registration so the start path need not re-read the manifest.
    pub fn launch(&self) -> Option<&StartLaunch> {
        self.launch.as_ref()
    }

    /// Project the declaration onto `os` (the effective current-OS view).
    pub fn effective(&self, os: OsId) -> EffectiveCapabilities {
        self.declaration.effective(os)
    }
}

/// Why an adapter could not be resolved/validated (engine-internal).
///
/// The registry maps each variant to a `RegistryError` so `kt` can render a
/// remediation hint. `thiserror`, never `miette` (conventions). Each carries
/// enough context to name the problem (the kind, the path, or the section).
#[derive(Debug, Error)]
pub enum AdapterResolveError {
    /// A native kind was requested that no builtin provides.
    #[error("unknown adapter kind '{kind}'")]
    UnknownKind {
        /// The unrecognized kind string.
        kind: String,
    },

    /// The manifest file did not exist at the resolved path.
    #[error("no adapter.toml found at {path}")]
    ManifestNotFound {
        /// The path that was searched (the file, or `<dir>/adapter.toml`).
        path: String,
    },

    /// The manifest existed but could not be read.
    #[error("could not read adapter.toml at {path}: {detail}")]
    ManifestUnreadable {
        /// The manifest path.
        path: String,
        /// The underlying I/O error.
        detail: String,
    },

    /// The manifest parsed/validated with an error naming the failing section.
    #[error("adapter.toml at {path} is invalid: {detail}")]
    ManifestInvalid {
        /// The manifest path.
        path: String,
        /// The section-naming detail from [`ManifestError`].
        detail: String,
    },

    /// The manifest targets a different Adapter Contract MAJOR than this engine
    /// speaks (story 6-6, FR-30 — the v1 freeze). Registration REFUSES the load
    /// naming BOTH versions and quoting the compatibility rule; `detail`
    /// carries that message verbatim (rendered from
    /// [`ktesio_adapter_api::ContractVersionError`], so the rule text lives in
    /// ONE place). Distinct from [`AdapterResolveError::ManifestInvalid`]: the
    /// manifest is well-formed — it is the VERSION that does not negotiate.
    #[error("adapter.toml at {path} is incompatible: {detail}")]
    ContractIncompatible {
        /// The manifest path.
        path: String,
        /// The both-versions + rule message from the negotiation.
        detail: String,
    },

    /// The adapter declared no viable Metering Source (FR-19 hard line, AC4).
    ///
    /// For a manifest this is caught by [`Manifest::validate`] (a missing
    /// `[metering]` section), but the registry re-checks the resolved adapter so
    /// the FR-19 line is enforced uniformly for native adapters too.
    #[error("adapter '{adapter}' declares no viable Metering Source (the `[metering]` section)")]
    NoMeteringSource {
        /// The adapter kind/identity.
        adapter: String,
    },

    /// The adapter's Capability Declaration was empty (AC2 seed requirement).
    #[error("adapter '{adapter}' declares no capabilities (the `[capabilities]` section)")]
    NoCapabilities {
        /// The adapter kind/identity.
        adapter: String,
    },
}

/// The resolved `start` launch of an adapter (story 1.4): exec + args + env.
///
/// Built by [`resolve_start_launch`] from a manifest adapter's
/// `[lifecycle.start]` [`OpTemplate`](ktesio_adapter_api::OpTemplate) or a native
/// adapter's equivalent. The supervisor turns this into a
/// [`SpawnSpec`](crate::ports::SpawnSpec) (adding the working dir + log file) and
/// hands it to the [`ProcessBackend`](crate::ports::ProcessBackend).
///
/// Intentionally NOT `Serialize`/`Deserialize` (spine AD-10, the same discipline
/// as `SecretString`): after [`apply_config_mapping`] runs at start, this
/// launch's plain-`String` `args`/`env` can hold RESOLVED secret cleartext, so
/// the type must not be serializable — a compile-time guard against a
/// post-delivery launch leaking through a snapshot, log, or event. The registry
/// persists only the REGISTRATION-time launch (which carries no resolved secrets
/// — just the manifest's `[lifecycle.start]` exec/args/env) and does so through a
/// dedicated `LaunchSnapshot` DTO, mirroring the serialize-a-DTO-not-the-live-type
/// discipline of the effective-config snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartLaunch {
    /// The executable to run (program name or path).
    pub exec: String,
    /// Positional arguments.
    pub args: Vec<String>,
    /// Environment overrides.
    pub env: std::collections::BTreeMap<String, String>,
}

/// Why a `start` launch could not be resolved (story 1.4).
#[derive(Debug, Error)]
pub enum LaunchResolveError {
    /// The stored manifest could not be read/parsed (corrupt or now-missing).
    #[error("could not read the adapter manifest at {path}: {detail}")]
    ManifestUnreadable {
        /// The manifest path.
        path: String,
        /// The underlying detail.
        detail: String,
    },

    /// The manifest parsed but declares no `[lifecycle.start]` `exec` (should not
    /// happen for a manifest that passed registration validation, but guarded).
    #[error("adapter manifest at {path} declares no `[lifecycle.start]` exec")]
    NoStartTemplate {
        /// The manifest path.
        path: String,
    },

    /// The re-read manifest targets a different Adapter Contract MAJOR than
    /// this engine speaks (retro #161 — the fallback-path gate). The start
    /// fallback re-reads `adapter.toml` when no launch was snapshotted at
    /// registration (legacy snapshot); without this re-check, a manifest
    /// edited to a foreign major AFTER registering would start through the
    /// fallback, bypassing the 6-6 load gate. Mirrors
    /// [`AdapterResolveError::ContractIncompatible`]: names BOTH versions and
    /// quotes the rule; `detail` carries that message verbatim (rendered from
    /// [`ktesio_adapter_api::ContractVersionError`]).
    #[error("adapter.toml at {path} is incompatible: {detail}")]
    ContractIncompatible {
        /// The manifest path.
        path: String,
        /// The both-versions + rule message from the negotiation.
        detail: String,
    },

    /// The adapter is a native builtin with no launch command (e.g. `mock`, the
    /// inert conformance stand-in). Launchable native builtins (`hermes` since
    /// story 6-2) declare their start launch in code and resolve before this
    /// error can fire. A launchable agent without a code-declared launch is
    /// supplied as a manifest adapter whose `[lifecycle.start]` exec points at
    /// the real program (AD-3). Names the kind.
    #[error(
        "native adapter kind '{kind}' has no launch command; supply a manifest adapter (its `[lifecycle.start]` exec) to start a real process"
    )]
    NativeHasNoLaunch {
        /// The native kind.
        kind: String,
    },

    /// The adapter's config mapping (story 2-2, FR-12) is invalid — a malformed
    /// rule, or a `File` target whose `path` is absolute or escapes the Agent
    /// Home. Applied symmetrically to BOTH kinds ([`resolve_config_mapping`]): a
    /// manifest mapping is validated at registration, but re-checked here; a
    /// NATIVE (code-declared) mapping is validated here too, so a native `File`
    /// target can never escape the home (AD-6 — symmetric trust). Names the
    /// offending unified key + why.
    #[error("adapter '{adapter}' has an invalid config mapping for key '{key}': {detail}")]
    InvalidConfigMapping {
        /// The adapter kind/identity whose mapping is invalid.
        adapter: String,
        /// The offending unified key.
        key: String,
        /// Why the rule is invalid.
        detail: String,
    },
}

/// Resolve the `start` launch for an adapter, given its persisted snapshot
/// facts (story 1.4; native launch support story 6-2).
///
/// * A **manifest** adapter (`manifest_path` is `Some`) re-reads its
///   `adapter.toml` and returns the `[lifecycle.start]` template's exec/args/env
///   (reusing [`ktesio_adapter_api::Manifest`] — the same parser registration
///   used). This is where 1-3's stored `OpTemplate` is finally EXECUTED (AD-3).
/// * A **native** adapter (`manifest_path` is `None`) resolves from the builtin
///   table's code-declared launch ([`builtin::native_launch`]): launchable kinds
///   (e.g. `hermes`) yield their foreground gateway launch; inert kinds (e.g.
///   `mock`) still error with
///   [`LaunchResolveError::NativeHasNoLaunch`].
///
/// PARSE only — executes nothing here (the supervisor spawns).
///
/// The fallback re-read RE-NEGOTIATES the contract version (retro #161, FR-30):
/// this path runs when no launch was snapshotted at registration (a legacy
/// snapshot), so the file on disk may have drifted since the registration-time
/// gate ran — without the re-check, a manifest edited to a foreign major after
/// registering would start through the fallback and bypass the 6-6 load gate.
/// A native adapter cannot drift (it is compiled against this crate), so the
/// gate is manifest-path-only, exactly like registration.
pub fn resolve_start_launch(
    kind: &str,
    manifest_path: Option<&Path>,
) -> Result<StartLaunch, LaunchResolveError> {
    if manifest_path.is_none() {
        // Native: consult the builtin table's code-declared launch before
        // declaring the kind unstartable (story 6-2).
        return builtin::native_launch(kind).ok_or_else(|| LaunchResolveError::NativeHasNoLaunch {
            kind: kind.to_string(),
        });
    }
    let manifest_path = manifest_path.unwrap();
    let text = std::fs::read_to_string(manifest_path).map_err(|e| {
        LaunchResolveError::ManifestUnreadable {
            path: manifest_path.to_string_lossy().into_owned(),
            detail: e.to_string(),
        }
    })?;
    let manifest =
        Manifest::from_toml_str(&text).map_err(|e| LaunchResolveError::ManifestUnreadable {
            path: manifest_path.to_string_lossy().into_owned(),
            detail: e.to_string(),
        })?;
    // Fallback-path negotiation gate (retro #161): same rule, same error text
    // as registration (`AdapterResolveError::ContractIncompatible`) — both
    // versions named, the rule quoted — so a drifted manifest fails the start
    // instead of launching under a contract this engine does not speak.
    let manifest_version = manifest
        .contract_version
        .as_deref()
        .unwrap_or_default()
        .trim();
    if let Err(negotiation) = ktesio_adapter_api::negotiate_contract_version(manifest_version) {
        return Err(LaunchResolveError::ContractIncompatible {
            path: manifest_path.to_string_lossy().into_owned(),
            detail: negotiation.to_string(),
        });
    }
    start_launch_from_manifest(&manifest).ok_or_else(|| LaunchResolveError::NoStartTemplate {
        path: manifest_path.to_string_lossy().into_owned(),
    })
}

/// Extract the `[lifecycle.start]` launch (exec + args + env) from an
/// already-parsed manifest, if it declares one — `None` when there is no
/// `[lifecycle.start]` template.
///
/// Shared by [`resolve`] (which captures the launch at REGISTRATION into the
/// [`ResolvedAdapter`] the registry snapshots) and [`resolve_start_launch`] (the
/// fallback re-read), so both derive the launch identically from one parse. A
/// validated manifest always has a start template (`Manifest::validate` requires
/// it), so `resolve` yields `Some` for every manifest that passes registration;
/// the `Option` stays honest for the defensive re-read path.
fn start_launch_from_manifest(manifest: &Manifest) -> Option<StartLaunch> {
    manifest
        .lifecycle
        .as_ref()
        .and_then(|l| l.start.as_ref())
        .map(|start| StartLaunch {
            exec: start.exec.clone(),
            args: start.args.clone(),
            env: start.env.clone(),
        })
}

/// Why applying the adapter's config mapping to a launch failed (story 2-2). The
/// only failure is rendering a FILE target into the Agent Home — env/flag targets
/// are pure in-memory mutations of the launch that cannot fail. The supervisor
/// maps this into its launch error surface (never a panic).
///
/// ATOMICITY: the mapping is applied BEFORE the `starting` transition, so a
/// file-render failure REJECTS the start and the instance stays in its PRIOR
/// state (registered/stopped/failed) — the start STATE is atomic (all mapping
/// failures reject before any state change). Since story 11-2 (AI-28) each
/// rendered file is ALSO written atomically (temp in the target's directory +
/// one rename, via [`crate::paths::write_atomically`]): a crash mid-write can no
/// longer truncate a previously good native config file, and a failed write
/// leaves the previous bytes intact with no temp residue. Multi-file apply is
/// still not atomic ACROSS files, so a failure on a LATER file can leave an
/// EARLIER file already rendered in the Agent Home (a harmless stale artifact
/// the next successful start overwrites).
#[derive(Debug, Error)]
pub enum ConfigApplyError {
    /// A FILE-target config file could not be rendered/written into the Agent
    /// Home. Names the unified key, the target path, and the underlying detail.
    #[error(
        "could not render config key '{key}' into the file '{path}' in the Agent Home: {detail}"
    )]
    FileRender {
        /// The unified key whose file target failed.
        key: String,
        /// The target path (relative to the Agent Home).
        path: String,
        /// The underlying I/O or serialization detail.
        detail: String,
    },
}

/// Resolve the adapter's unified→native config [`ConfigMapping`] for a start
/// (story 2-2, AC3): a MANIFEST adapter's mapping comes from its parsed `[config]`
/// section; a NATIVE adapter's from the builtin table's code-declared mapping.
/// Both yield the same uniform [`ConfigMapping`] the start seam applies (AD-3
/// "two kinds, one trait"). A manifest that cannot be re-read/parsed surfaces the
/// same [`LaunchResolveError::ManifestUnreadable`] as [`resolve_start_launch`]
/// (the launch already re-read it, but this stays defensive + symmetric). An
/// unknown native kind yields an EMPTY mapping (delivers nothing) rather than an
/// error — the launch resolution already rejected an unknown/native-only kind.
pub fn resolve_config_mapping(
    kind: &str,
    manifest_path: Option<&Path>,
) -> Result<ConfigMapping, LaunchResolveError> {
    let mapping = match manifest_path {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                LaunchResolveError::ManifestUnreadable {
                    path: path.to_string_lossy().into_owned(),
                    detail: e.to_string(),
                }
            })?;
            let manifest = Manifest::from_toml_str(&text).map_err(|e| {
                LaunchResolveError::ManifestUnreadable {
                    path: path.to_string_lossy().into_owned(),
                    detail: e.to_string(),
                }
            })?;
            manifest.config_mapping()
        }
        // A native adapter's mapping is code-declared; an unknown kind → empty.
        None => native_config_mapping(kind).unwrap_or_default(),
    };
    // Validate the mapping SYMMETRICALLY for both kinds (AD-6 — symmetric trust):
    // a manifest mapping is validated at registration, but re-checked here; a
    // NATIVE code-declared mapping is validated here too, so a native `File`
    // target can never be absolute or escape the Agent Home. A malformed rule
    // rejects the start (a bad rule is an adapter authoring bug).
    if let Err((key, detail)) = mapping.validate() {
        return Err(LaunchResolveError::InvalidConfigMapping {
            adapter: kind.to_string(),
            key,
            detail,
        });
    }
    Ok(mapping)
}

/// What [`apply_config_mapping`] REPORTS back about the mapping it applied
/// (story 11-2, AI-39). The start seam turns these facts into ONE operator
/// diagnostic routed through `emit_diagnostic` (stderr by default, the host's
/// story-10-2 sink when installed) — resolving the old Flag-arm TODO: the
/// `start` seam now carries the secret-into-flag fact without new cross-boundary
/// machinery, riding the same return path the apply already had. Returned on
/// SUCCESS (an `Err` means the mapping did not fully apply — nothing to report).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MappingReport {
    /// The unified keys whose RESOLVED SECRET cleartext was delivered into a
    /// `flag` target — i.e. the keys that put cleartext on the spawned process's
    /// argv (world-readable cross-user via `ps` / `/proc/<pid>/cmdline`), the
    /// STRICTER exposure documented in docs/architecture.md Secrets. Empty on
    /// the common path; the caller emits ONE naming diagnostic, warn-only (the
    /// start still succeeds — no rejection semantics were ever ratified).
    pub secret_flag_keys: Vec<String>,
}

/// APPLY the adapter's config mapping to a [`StartLaunch`], from the resolved
/// [`EffectiveConfig`] (story 2-2 — the heart of AC-A/AC4/AC5/AC6). Runs at start,
/// after the launch's exec/args/env are read from the `[lifecycle.start]`
/// template and BEFORE the `SpawnSpec` is built, so the spawned process already
/// reflects the mapped native config. On success returns the
/// [`MappingReport`] facts the start seam surfaces as diagnostics.
///
/// For every resolved leaf (`dotted key → value`), in the effective config's
/// deterministic sorted order:
/// * a leaf under the `agent.*` PASS-THROUGH namespace is delivered VERBATIM
///   (AC6): the key-tail after `agent.` + the value, with NO known-key mapping
///   lookup and NO rewriting. The recorded delivery convention (Decision 5): a
///   pass-through key maps to an ENV var named by its verbatim key-tail
///   (`agent.FOO=bar` → env `FOO=bar`), the value as-is;
/// * any other leaf is a documented KNOWN key: if the adapter's `mapping` declares
///   a rule for it, the value lands in that rule's native target — **env** →
///   inserted into [`StartLaunch::env`]; **flag** → appended to
///   [`StartLaunch::args`] as two tokens (`--model` `gpt-4`); **file** →
///   rendered into a native TOML file in the Agent `home` (the engine is the sole
///   writer — path authority). A documented key with NO rule is a no-op
///   (Decision 6 — not every adapter maps every unified key).
///
/// PURE for env/flag (in-memory mutation); FILE targets are the only side effect
/// (a write into `home`), and a bad/unwritable target is a typed
/// [`ConfigApplyError::FileRender`] the supervisor lands as a launch failure. The
/// resolved-config → launch transform is deterministic (sorted iteration + a
/// per-file merge keyed by the native key), so the same inputs always yield the
/// same launch + files.
///
/// SECRET DELIVERY (story 2-4, AC9 — display and delivery DIVERGE). `secrets` maps
/// a dotted leaf key → the RESOLVED cleartext
/// [`SecretString`](crate::domain::SecretString) the supervisor resolved at start
/// (env → the 0600 file) for each `secret:NAME` leaf. For a secret-classified leaf,
/// the value placed into the native mechanism is `secrets[key].expose_secret()` —
/// the REAL key the agent needs — NOT `resolved.display()` (which now MASKS a
/// secret) and NOT the `secret:NAME` reference. Non-secret leaves keep
/// `resolved.display()`. This is the crux: the SAME leaf renders masked in
/// `config get`/the snapshot/logs while delivering cleartext into the adapter's
/// PRIVATE native config (the rendered file the agent reads holds cleartext by
/// necessity — an accepted FR-2/NFR-6 boundary, the Agent Home is
/// filesystem-isolated, not a sandbox). A secret leaf whose key is absent from
/// `secrets` (should not happen — the supervisor resolves every secret leaf before
/// calling this) falls back to the MASKED `display()` — fail-CLOSED, never a leak.
pub fn apply_config_mapping(
    launch: &mut StartLaunch,
    mapping: &ConfigMapping,
    effective: &EffectiveConfig,
    secrets: &std::collections::BTreeMap<String, crate::domain::SecretString>,
    home: &Path,
) -> Result<MappingReport, ConfigApplyError> {
    // Accumulate FILE-target writes keyed by target path, so multiple keys
    // mapping into the SAME file merge into one rendered document (deterministic:
    // the effective config iterates sorted, and each file's keys are set into a
    // sorted TOML table). Rendered + written once at the end.
    let mut files: std::collections::BTreeMap<String, FileDoc> = std::collections::BTreeMap::new();
    let mut report = MappingReport::default();

    for (dotted_key, resolved) in effective.iter() {
        // Secret delivery (AC9): a secret-classified leaf delivers the RESOLVED
        // CLEARTEXT (from the SecretString), never the mask. A non-secret leaf uses
        // the plain display(); a secret leaf missing from `secrets` fails CLOSED to
        // the masked display() (never a leak). This is the ONE place cleartext is
        // exposed for delivery — display() everywhere else stays masked.
        let value = match secrets.get(dotted_key) {
            Some(secret) => secret.expose_secret().to_string(),
            None => resolved.display(),
        };
        if let Some(tail) = pass_through_tail(dotted_key) {
            // AC6: pass-through delivered VERBATIM into the native mechanism (the
            // recorded convention: an env var named by the verbatim key-tail).
            // NO known-key mapping lookup, NO rewriting of the tail or value.
            launch.env.insert(tail.to_string(), value);
            continue;
        }
        // A documented known key: apply the adapter's rule if it declares one.
        let Some(target) = mapping.target(dotted_key) else {
            // Decision 6: a documented key the adapter does not map is a no-op.
            continue;
        };
        match target {
            ConfigTarget::Env { env } => {
                launch.env.insert(env.clone(), value);
            }
            ConfigTarget::Flag { .. } => {
                // A secret mapped to a `flag` target lands its CLEARTEXT here as a
                // command-line argument. This is a STRICTER exposure than the env /
                // rendered-file boundaries: argv is world-readable CROSS-USER via
                // `ps` / `/proc/<pid>/cmdline`, whereas those live in the
                // filesystem-isolated Agent Home. It is an accepted boundary
                // (documented in docs/architecture.md Secrets / AD-10) — the agent
                // needs a usable key and Ktesio's own surfaces stay masked — but
                // operators should prefer `env`/`file` targets for secret-carrying
                // keys. The fact is REPORTED (AI-39): a resolved secret delivered
                // here lands the key in `report.secret_flag_keys`, and the start
                // seam emits ONE warn-only diagnostic naming it (no rejection —
                // the argv boundary stays accepted; only the silence was a bug).
                if secrets.contains_key(dotted_key) {
                    report.secret_flag_keys.push(dotted_key.to_string());
                }
                if let Some([flag, val]) = target.render_flag_args(&value) {
                    launch.args.push(flag);
                    launch.args.push(val);
                }
            }
            ConfigTarget::File(file_target) => {
                let placement = &file_target.file;
                files
                    .entry(placement.path.clone())
                    .or_default()
                    .set(&placement.key, value);
            }
        }
    }

    // Render + write each accumulated file into the Agent Home (sole writer,
    // AD-6). The path was validated RELATIVE at manifest-load time; a native
    // mapping is trusted (code-declared). Join defensively onto the home. Each
    // write is ATOMIC (story 11-2, AI-28): temp in the target's directory + one
    // rename, so a crash mid-write cannot truncate a previously good native
    // config file and a failed write leaves no temp residue.
    for (rel_path, doc) in files {
        write_config_file(home, &rel_path, &doc)?;
    }
    Ok(report)
}

/// An in-progress native config FILE document being assembled by
/// [`apply_config_mapping`] (a set of dotted native keys → string values). Kept a
/// small newtype so the per-file merge is explicit + testable; serialized to TOML
/// once, deterministically (a [`std::collections::BTreeMap`] sorts its keys).
#[derive(Debug, Default)]
struct FileDoc {
    entries: std::collections::BTreeMap<String, String>,
}

impl FileDoc {
    /// Set a dotted native key to `value` (last write wins — the effective config
    /// yields one value per unified key, so a collision only arises if two unified
    /// keys map to the SAME file+native-key, which is an adapter authoring choice).
    fn set(&mut self, key: &str, value: String) {
        self.entries.insert(key.to_string(), value);
    }

    /// Render to a TOML document string: each dotted native key set as a nested
    /// TOML value (`llm.model = "gpt-4"` → `[llm]\nmodel = "gpt-4"`). Reuses the
    /// same dotted-set discipline the instance-config writer uses, so a native key
    /// path lands in the right nested table.
    fn to_toml_string(&self) -> Result<String, String> {
        let mut table = toml::value::Table::new();
        for (key, value) in &self.entries {
            set_dotted_string(&mut table, key, value.clone());
        }
        toml::to_string_pretty(&table).map_err(|e| e.to_string())
    }
}

/// Set a DOTTED native key (`a.b.c`) into a TOML table as a string value,
/// creating intermediate tables. A collision where an intermediate is already a
/// non-table value overwrites it with a table (the rendered native file is
/// engine-authored from the mapping, not user input — there is no existing scalar
/// to preserve, unlike the instance-config write path which fails closed).
fn set_dotted_string(table: &mut toml::value::Table, dotted_key: &str, value: String) {
    let mut segments = dotted_key.split('.').peekable();
    let mut current = table;
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            current.insert(segment.to_string(), toml::Value::String(value));
            return;
        }
        let entry = current
            .entry(segment.to_string())
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::value::Table::new());
        }
        current = entry
            .as_table_mut()
            .expect("intermediate ensured to be a table");
    }
}

/// Render `doc` into the file at `rel_path` inside the Agent `home` (the engine is
/// the sole writer — AD-6). Creates parent directories under the home as needed.
/// The write itself is ATOMIC (story 11-2, AI-28): the body lands in a
/// same-directory temp file ([`crate::paths::write_atomically`]) and one rename
/// flips it over the target, so a previously rendered native config file is never
/// observed (or left) truncated — a crash mid-write keeps the OLD bytes, and a
/// failed write leaves no temp residue. A write/serialize failure is a typed
/// [`ConfigApplyError::FileRender`] naming the key path + detail (never a panic).
/// `rel_path` was validated relative at manifest load; joining it onto `home`
/// therefore stays inside the home.
fn write_config_file(home: &Path, rel_path: &str, doc: &FileDoc) -> Result<(), ConfigApplyError> {
    let body = doc
        .to_toml_string()
        .map_err(|detail| ConfigApplyError::FileRender {
            key: rel_path.to_string(),
            path: rel_path.to_string(),
            detail,
        })?;
    let full = home.join(rel_path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigApplyError::FileRender {
            key: rel_path.to_string(),
            path: rel_path.to_string(),
            detail: e.to_string(),
        })?;
    }
    crate::paths::write_atomically(&full, body.as_bytes()).map_err(|e| {
        ConfigApplyError::FileRender {
            key: rel_path.to_string(),
            path: rel_path.to_string(),
            detail: e.to_string(),
        }
    })
}

/// Resolve an [`AdapterRef`] into a validated [`ResolvedAdapter`].
///
/// PARSE + VALIDATE only — executes nothing. On success the returned adapter is
/// guaranteed to have a non-empty Capability Declaration and a viable Metering
/// Source (the FR-19 hard line), so the registry can proceed to the atomic
/// row-insert / home-creation with confidence.
pub fn resolve(reference: &AdapterRef) -> Result<ResolvedAdapter, AdapterResolveError> {
    let resolved = match reference {
        AdapterRef::Native(kind) => resolve_native(kind)?,
        AdapterRef::Manifest(path) => resolve_manifest(path)?,
    };
    enforce_registration_invariants(&resolved)?;
    Ok(resolved)
}

/// Resolve a native kind through the engine's builtin table.
fn resolve_native(kind: &str) -> Result<ResolvedAdapter, AdapterResolveError> {
    let adapter = builtin::native(kind).ok_or_else(|| AdapterResolveError::UnknownKind {
        kind: kind.to_string(),
    })?;
    // Story 6-2: a launchable native builtin (e.g. `hermes`) declares its start
    // launch in code; `mock` stays inert (`None` — start still errors
    // NativeHasNoLaunch). Capturing it here persists the SAME registration-time
    // snapshot a manifest adapter gets, so the start path needs no special case.
    let launch = builtin::native_launch(kind);
    Ok(ResolvedAdapter {
        kind: adapter.kind().to_string(),
        declaration: adapter.capabilities().clone(),
        metering_source: adapter.metering_source(),
        manifest_path: None,
        launch,
    })
}

/// Load, parse, and validate a manifest adapter from a directory or file path.
fn resolve_manifest(path: &Path) -> Result<ResolvedAdapter, AdapterResolveError> {
    // A directory means "<dir>/adapter.toml"; a file path is used verbatim. This
    // lets an operator point at either the Agent's directory or the file.
    let manifest_path = if path.is_dir() {
        path.join(MANIFEST_FILE)
    } else {
        path.to_path_buf()
    };

    if !manifest_path.exists() {
        return Err(AdapterResolveError::ManifestNotFound {
            path: manifest_path.to_string_lossy().into_owned(),
        });
    }

    let text = std::fs::read_to_string(&manifest_path).map_err(|e| {
        AdapterResolveError::ManifestUnreadable {
            path: manifest_path.to_string_lossy().into_owned(),
            detail: e.to_string(),
        }
    })?;

    let manifest = Manifest::from_toml_str(&text).map_err(|e| to_invalid(&manifest_path, e))?;
    manifest
        .validate()
        .map_err(|e| to_invalid(&manifest_path, e))?;

    // Contract v1 negotiation (story 6-6, FR-30): registration REFUSES a
    // manifest whose contract major differs from this engine's, naming both
    // versions and quoting the rule. This is THE load gate — native builtins
    // are compiled against this crate (always compatible), and the start path
    // launches from the registration-time snapshot, so a manifest can only
    // enter the fleet through here. A pre-v1 `0.x` manifest is NOT
    // grandfathered: the contract was never published under 0.x.
    let manifest_version = manifest
        .contract_version
        .as_deref()
        .unwrap_or_default()
        .trim();
    if let Err(negotiation) = ktesio_adapter_api::negotiate_contract_version(manifest_version) {
        return Err(AdapterResolveError::ContractIncompatible {
            path: manifest_path.to_string_lossy().into_owned(),
            detail: negotiation.to_string(),
        });
    }

    // Validated: the declaration is non-empty and a viable source is present.
    let metering_source =
        manifest
            .metering_source()
            .ok_or_else(|| AdapterResolveError::NoMeteringSource {
                adapter: manifest.adapter_kind().unwrap_or("manifest").to_string(),
            })?;

    let kind = manifest.adapter_kind().unwrap_or("manifest").to_string();

    // Capture the `start` launch from the SAME parse that produced the
    // declaration, so the registry can snapshot it at registration and the start
    // path need never re-read the manifest (mirrors the declaration precedent).
    let launch = start_launch_from_manifest(&manifest);

    Ok(ResolvedAdapter {
        kind,
        declaration: manifest.capability_declaration(),
        metering_source,
        manifest_path: Some(manifest_path),
        launch,
    })
}

/// Re-check the FR-19 / AC2 registration invariants on a resolved adapter,
/// uniformly for both kinds. A manifest failure is already caught by
/// `validate()`, but native adapters must clear the same bar.
fn enforce_registration_invariants(resolved: &ResolvedAdapter) -> Result<(), AdapterResolveError> {
    // The same bar the manifest clears (F1): not merely "has a capability key"
    // but "declares real support on at least one OS". A native adapter whose
    // declaration is empty OR all-`unsupported` is rejected here, uniformly.
    if !resolved.declaration.has_any_support() {
        return Err(AdapterResolveError::NoCapabilities {
            adapter: resolved.kind.clone(),
        });
    }
    // The MeteringSource type only holds viable kinds, so its presence in a
    // ResolvedAdapter already proves viability; there is no "none" to reject
    // here. The variant exists for symmetry / future native adapters that might
    // compute a source dynamically.
    Ok(())
}

/// Map a [`ManifestError`] to the invalid-manifest resolve error, preserving the
/// section-naming message (AC2).
fn to_invalid(path: &Path, err: ManifestError) -> AdapterResolveError {
    AdapterResolveError::ManifestInvalid {
        path: path.to_string_lossy().into_owned(),
        detail: err.to_string(),
    }
}

impl From<AdapterResolveError> for crate::domain::RegistryError {
    /// Translate an adapter-resolution failure into the registry's error surface
    /// so `kt` can render a remediation hint per variant. Each resolve error maps
    /// to a distinct registry variant, including an unreadable manifest (whose
    /// remediation — check existence/readability — differs from "fix the
    /// section", F4).
    fn from(err: AdapterResolveError) -> Self {
        use crate::domain::RegistryError as R;
        match err {
            AdapterResolveError::UnknownKind { kind } => R::UnknownAdapterKind { kind },
            AdapterResolveError::ManifestNotFound { path } => R::ManifestNotFound { path },
            AdapterResolveError::ManifestUnreadable { path, detail } => {
                R::ManifestUnreadable { path, detail }
            }
            AdapterResolveError::ManifestInvalid { path, detail } => {
                R::ManifestInvalid { path, detail }
            }
            AdapterResolveError::ContractIncompatible { path, detail } => {
                R::ContractIncompatible { path, detail }
            }
            AdapterResolveError::NoMeteringSource { adapter } => R::NoMeteringSource { adapter },
            AdapterResolveError::NoCapabilities { adapter } => R::NoCapabilities { adapter },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    const VALID_MANIFEST: &str = r#"
contract_version = "1.0.0"

[adapter]
kind = "demo"

[lifecycle.start]
exec = "demo-agent"

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[metering]
source = "self-reported"
"#;

    fn write_manifest(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(MANIFEST_FILE);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn native_mock_resolves_with_declaration_and_metering() {
        let resolved = resolve(&AdapterRef::Native("mock".to_string())).unwrap();
        assert_eq!(resolved.kind(), "mock");
        assert_eq!(resolved.metering_source(), MeteringSource::SelfReported);
        assert!(!resolved.declaration().is_empty());
        assert!(resolved.manifest_path().is_none());

        // Per-OS projection works as data on any host.
        for os in OsId::MODELED {
            let eff = resolved.effective(os);
            assert!(!eff.is_empty(), "os={os}");
        }
    }

    #[test]
    fn unknown_native_kind_is_rejected() {
        let err = resolve(&AdapterRef::Native("nope".to_string())).unwrap_err();
        assert!(matches!(err, AdapterResolveError::UnknownKind { kind } if kind == "nope"));
    }

    #[test]
    fn manifest_dir_resolves_and_records_path() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), VALID_MANIFEST);
        // Point at the DIRECTORY — the loader appends adapter.toml.
        let resolved = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap();
        assert_eq!(resolved.kind(), "demo");
        assert_eq!(resolved.metering_source(), MeteringSource::SelfReported);
        assert_eq!(
            resolved.manifest_path().unwrap(),
            tmp.path().join(MANIFEST_FILE)
        );
    }

    #[test]
    fn manifest_file_path_resolves_verbatim() {
        let tmp = TempDir::new().unwrap();
        let file = write_manifest(tmp.path(), VALID_MANIFEST);
        // Point at the FILE directly.
        let resolved = resolve(&AdapterRef::Manifest(file.clone())).unwrap();
        assert_eq!(resolved.manifest_path().unwrap(), file.as_path());
    }

    #[test]
    fn missing_manifest_is_not_found() {
        let tmp = TempDir::new().unwrap();
        let err = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap_err();
        match err {
            AdapterResolveError::ManifestNotFound { path } => {
                assert!(path.ends_with(MANIFEST_FILE), "{path}")
            }
            other => panic!("expected ManifestNotFound, got {other}"),
        }
    }

    #[test]
    fn invalid_manifest_names_the_section() {
        let tmp = TempDir::new().unwrap();
        // Drop the [metering] section.
        let body = VALID_MANIFEST.replace("[metering]\nsource = \"self-reported\"\n", "");
        write_manifest(tmp.path(), &body);
        let err = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap_err();
        match err {
            AdapterResolveError::ManifestInvalid { detail, .. } => {
                assert!(detail.contains("[metering]"), "{detail}")
            }
            other => panic!("expected ManifestInvalid, got {other}"),
        }
    }

    // ---- Story 6-6: contract v1 negotiation at registration (FR-30) ----

    #[test]
    fn same_major_manifest_loads_under_contract_v1() {
        // The freeze fixture, positive leg: a manifest declaring the engine's
        // own contract major registers cleanly. Prerelease/build-metadata
        // suffixes of the same major negotiate by MAJOR only (AI-6 stance).
        for version in ["\"1.0.0\"", "\"1.9.9\"", "\"1.0.0-rc.1+build.5\""] {
            let tmp = TempDir::new().unwrap();
            let body = VALID_MANIFEST.replace(
                "contract_version = \"1.0.0\"",
                &format!("contract_version = {version}"),
            );
            write_manifest(tmp.path(), &body);
            let resolved = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf()))
                .unwrap_or_else(|e| panic!("{version} must load: {e}"));
            assert_eq!(resolved.kind(), "demo", "{version}");
        }
    }

    #[test]
    fn incompatible_major_fails_naming_both_versions_and_the_rule() {
        // The freeze fixture, negative leg (FR-30's informative rejection): a
        // manifest whose contract major differs from the engine's fails to load
        // naming BOTH versions AND quoting the compatibility rule. The set
        // includes prerelease/build-metadata spellings of a different major —
        // an AI-64 pass (M3, 2026-09-04) proved a `|| !pre.is_empty()` hole
        // survives when only plain majors are fixed, so suffixes must be
        // exercised here too (they parse fine and reach the negotiation).
        for version in ["2.1.0", "2.0.0-rc.1", "2.0.0+build.9"] {
            let tmp = TempDir::new().unwrap();
            let body = VALID_MANIFEST.replace(
                "contract_version = \"1.0.0\"",
                &format!("contract_version = \"{version}\""),
            );
            write_manifest(tmp.path(), &body);
            let err = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap_err();
            match err {
                AdapterResolveError::ContractIncompatible { path, detail } => {
                    assert!(path.ends_with(MANIFEST_FILE), "{path}");
                    assert!(
                        detail.contains(version),
                        "names the manifest version: {detail}"
                    );
                    assert!(
                        detail.contains(ktesio_adapter_api::CONTRACT_VERSION),
                        "names the engine version: {detail}"
                    );
                    assert!(
                        detail.contains(ktesio_adapter_api::COMPATIBILITY_RULE),
                        "quotes the rule: {detail}"
                    );
                }
                other => panic!("expected ContractIncompatible for {version}, got {other}"),
            }
        }
    }

    #[test]
    fn pre_v1_manifest_majors_are_not_grandfathered() {
        // Ratified at the 6-6 checkpoint: the 0.x seeds were never published, so
        // a manifest still targeting them is INCOMPATIBLE with contract v1 —
        // no back-compat obligation exists.
        for version in ["0.1.0", "0.3.0", "0.4.0"] {
            let tmp = TempDir::new().unwrap();
            let body = VALID_MANIFEST.replace(
                "contract_version = \"1.0.0\"",
                &format!("contract_version = \"{version}\""),
            );
            write_manifest(tmp.path(), &body);
            let err = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap_err();
            assert!(
                matches!(err, AdapterResolveError::ContractIncompatible { .. }),
                "{version} must be refused: {err}"
            );
        }
    }

    #[test]
    fn contract_negotiation_runs_after_per_field_validation() {
        // Ordering pin: an INCOMPATIBLE version on an otherwise-invalid
        // manifest reports the SECTION problem first (validate() precedes
        // negotiation), so the diagnostic names the deeper defect.
        let tmp = TempDir::new().unwrap();
        let body = r#"
contract_version = "2.0.0"

[adapter]
kind = "demo"

[capabilities.pause]
linux = "guaranteed"

[metering]
source = "self-reported"
"#;
        write_manifest(tmp.path(), body);
        let err = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap_err();
        assert!(
            matches!(err, AdapterResolveError::ManifestInvalid { .. }),
            "the missing [lifecycle] section must win: {err}"
        );
    }

    #[test]
    fn registry_maps_contract_incompatible_for_kt() {
        // The RegistryError surface keeps the both-versions + rule message so
        // kt's diagnostic quotes it verbatim.
        let err: crate::domain::RegistryError = AdapterResolveError::ContractIncompatible {
            path: "/x/adapter.toml".to_string(),
            detail: "manifest declares 2.1.0, engine speaks 1.0.0".to_string(),
        }
        .into();
        assert!(
            matches!(
                err,
                crate::domain::RegistryError::ContractIncompatible { .. }
            ),
            "{err}"
        );
        let text = err.to_string();
        assert!(text.contains("2.1.0") && text.contains("1.0.0"), "{text}");
    }

    #[test]
    fn manifest_with_no_metering_is_rejected_as_invalid_naming_metering() {
        // AC4 at the manifest layer: a missing [metering] fails validate() and
        // surfaces as ManifestInvalid naming the section (the manifest never
        // reaches the NoMeteringSource re-check because validate() rejects it
        // first — both name `[metering]`).
        let tmp = TempDir::new().unwrap();
        let body = VALID_MANIFEST.replace("[metering]\nsource = \"self-reported\"\n", "");
        write_manifest(tmp.path(), &body);
        let err = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap_err();
        assert!(err.to_string().contains("[metering]"), "{err}");
    }

    #[test]
    fn malformed_manifest_is_invalid() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), "not = = valid toml");
        let err = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap_err();
        assert!(matches!(err, AdapterResolveError::ManifestInvalid { .. }));
    }

    #[test]
    fn manifest_missing_capabilities_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let body = VALID_MANIFEST.replace(
            "[capabilities.pause]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"best-effort\"\n",
            "",
        );
        write_manifest(tmp.path(), &body);
        let err = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap_err();
        assert!(err.to_string().contains("[capabilities]"), "{err}");
    }

    #[test]
    fn adapter_ref_equality() {
        assert_eq!(
            AdapterRef::Native("mock".to_string()),
            AdapterRef::Native("mock".to_string())
        );
        assert_ne!(
            AdapterRef::Native("mock".to_string()),
            AdapterRef::Manifest(PathBuf::from("/x"))
        );
    }

    #[test]
    fn enforce_invariants_rejects_all_unsupported_native_declaration() {
        // F1: a native adapter whose declaration has keys but zero real support
        // (all `unsupported`) must be rejected at the registration bar, exactly
        // like the manifest path — not slip through because a key exists.
        use ktesio_adapter_api::{Capability, SupportLevel};
        let declaration = CapabilityDeclaration::new()
            .with(Capability::Pause, OsId::Linux, SupportLevel::Unsupported)
            .with(
                Capability::Interaction,
                OsId::Macos,
                SupportLevel::Unsupported,
            );
        assert!(!declaration.is_empty(), "keys are present");
        let resolved = ResolvedAdapter {
            kind: "hollow".to_string(),
            declaration,
            metering_source: MeteringSource::SelfReported,
            manifest_path: None,
            launch: None,
        };
        let err = enforce_registration_invariants(&resolved).unwrap_err();
        assert!(
            matches!(err, AdapterResolveError::NoCapabilities { adapter } if adapter == "hollow"),
            "expected NoCapabilities"
        );
    }

    // ---- Story 1.4: resolve_start_launch (the manifest OpTemplate → launch) ----

    #[test]
    fn resolve_start_launch_reads_the_manifest_start_template() {
        // A manifest adapter's [lifecycle.start] exec/args/env become the launch.
        let tmp = TempDir::new().unwrap();
        let body = r#"
contract_version = "1.0.0"
[adapter]
kind = "demo"
[lifecycle.start]
exec = "the-agent"
args = ["--serve", "--port", "0"]
env = { MODE = "test" }
[capabilities.pause]
linux = "guaranteed"
[metering]
source = "self-reported"
"#;
        let path = write_manifest(tmp.path(), body);
        let launch = resolve_start_launch("demo", Some(&path)).unwrap();
        assert_eq!(launch.exec, "the-agent");
        assert_eq!(launch.args, vec!["--serve", "--port", "0"]);
        assert_eq!(launch.env.get("MODE").map(String::as_str), Some("test"));
    }

    #[test]
    fn resolve_captures_the_start_launch_for_a_manifest_adapter() {
        // The FIX: `resolve` captures the `[lifecycle.start]` launch from the SAME
        // parse that produces the declaration, so the registry snapshots it at
        // registration and the start path need never re-read the manifest.
        let tmp = TempDir::new().unwrap();
        let body = r#"
contract_version = "1.0.0"
[adapter]
kind = "demo"
[lifecycle.start]
exec = "the-agent"
args = ["--serve", "--port", "0"]
env = { MODE = "test" }
[capabilities.pause]
linux = "guaranteed"
[metering]
source = "self-reported"
"#;
        write_manifest(tmp.path(), body);
        let resolved = resolve(&AdapterRef::Manifest(tmp.path().to_path_buf())).unwrap();
        let launch = resolved
            .launch()
            .expect("a manifest adapter captures its launch at resolution");
        assert_eq!(launch.exec, "the-agent");
        assert_eq!(launch.args, vec!["--serve", "--port", "0"]);
        assert_eq!(launch.env.get("MODE").map(String::as_str), Some("test"));
    }

    #[test]
    fn resolve_native_adapter_has_no_launch_snapshot() {
        // The inert native builtin (`mock`) carries no launch (start yields
        // NativeHasNoLaunch). Its snapshot launch is None, so the start path
        // falls back to the re-read — which keeps erroring NativeHasNoLaunch.
        let resolved = resolve(&AdapterRef::Native("mock".to_string())).unwrap();
        assert!(
            resolved.launch().is_none(),
            "a native adapter has no captured launch"
        );
    }

    #[test]
    fn resolve_native_hermes_captures_its_code_declared_launch() {
        // Story 6-2 (DC-1): a launchable native builtin captures its start
        // launch at REGISTRATION — the same persisted snapshot a manifest
        // adapter gets, so the supervisor's preferred path spawns it with no
        // special case.
        let resolved = resolve(&AdapterRef::Native("hermes".to_string())).unwrap();
        let launch = resolved
            .launch()
            .expect("hermes declares a code-declared start launch");
        assert_eq!(launch.exec, builtin::HERMES_EXEC);
        assert_eq!(launch.args, ["gateway", "run", "--external-supervisor"]);
        assert!(launch.env.is_empty());
    }

    #[test]
    fn resolve_start_launch_native_has_no_launch_command() {
        // An inert native adapter (manifest_path None) still has no launch
        // command; hermes resolves instead (story 6-2).
        let err = resolve_start_launch("mock", None).unwrap_err();
        assert!(
            matches!(&err, LaunchResolveError::NativeHasNoLaunch { kind } if kind == "mock"),
            "got {err}"
        );
        assert!(err.to_string().contains("no launch command"));
    }

    #[test]
    fn resolve_start_launch_resolves_the_hermes_builtin_launch() {
        // Story 6-2: resolve_start_launch consults the builtin table BEFORE
        // erroring, so a native hermes instance starts from the same fallback
        // seam a legacy snapshot would use.
        let launch = resolve_start_launch(ktesio_adapters_hermes::HERMES_KIND, None)
            .expect("hermes carries a launch");
        assert_eq!(launch.exec, ktesio_adapters_hermes::HERMES_KIND);
        assert_eq!(launch.args, vec!["gateway", "run", "--external-supervisor"]);
    }

    #[test]
    fn resolve_start_launch_hermes_native_launch_env_stays_empty() {
        // Review blind-18 drift guard: the PRODUCTION resolve path for a native
        // hermes instance (what the supervisor actually consults when the
        // snapshot is missing) must yield an env-EMPTY launch. The gateway
        // receives its config through `apply_config_mapping` (HERMES_HOME via
        // memory.dir), never through a code-declared launch env — a builtin
        // table edit that sneaks an env var into the launch fails HERE.
        let launch = resolve_start_launch(ktesio_adapters_hermes::HERMES_KIND, None)
            .expect("hermes carries a launch");
        assert!(
            launch.env.is_empty(),
            "the code-declared hermes launch must stay env-empty, got {launch:?}"
        );
    }

    #[test]
    fn resolve_start_launch_manifest_kind_takes_precedence_over_the_builtin_table() {
        // Review blind-19: the kind arg is only the builtin-table key for the
        // MANIFEST-LESS path. When a manifest is supplied, its OWN
        // [lifecycle.start] wins — a manifest whose [adapter].kind is "hermes"
        // but whose start template spawns something else resolves from the
        // TEMPLATE, not the hermes builtin launch (kind is metadata, not a
        // table override).
        let tmp = TempDir::new().unwrap();
        let body = r#"
contract_version = "1.0.0"
[adapter]
kind = "hermes"
[lifecycle.start]
exec = "not-the-gateway"
args = ["--own-thing"]
[capabilities.pause]
linux = "guaranteed"
[metering]
source = "self-reported"
"#;
        let path = write_manifest(tmp.path(), body);
        let launch = resolve_start_launch(ktesio_adapters_hermes::HERMES_KIND, Some(&path))
            .expect("a manifest hermes-kind adapter resolves from its manifest");
        assert_eq!(launch.exec, "not-the-gateway");
        assert_eq!(launch.args, vec!["--own-thing"]);
    }

    #[test]
    fn resolve_start_launch_unreadable_manifest_is_reported() {
        // A manifest path that does not exist → ManifestUnreadable.
        let missing = std::path::Path::new("/no/such/adapter.toml");
        let err = resolve_start_launch("demo", Some(missing)).unwrap_err();
        assert!(
            matches!(err, LaunchResolveError::ManifestUnreadable { .. }),
            "got {err}"
        );
    }

    #[test]
    fn resolve_start_launch_malformed_manifest_is_unreadable() {
        // A manifest that fails to PARSE surfaces as ManifestUnreadable (the
        // launch path re-reads it; a corrupt file cannot yield a launch).
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(tmp.path(), "not = = valid toml");
        let err = resolve_start_launch("demo", Some(&path)).unwrap_err();
        assert!(matches!(err, LaunchResolveError::ManifestUnreadable { .. }));
    }

    #[test]
    fn resolve_start_launch_missing_start_template_is_reported() {
        // A manifest with a [lifecycle] table but no start op → NoStartTemplate.
        // (Parses fine — validate() would reject it at registration, but the
        // launch resolver guards defensively.)
        let tmp = TempDir::new().unwrap();
        let body = r#"
contract_version = "1.0.0"
[adapter]
kind = "demo"
[lifecycle.stop]
exec = "stopper"
[capabilities.pause]
linux = "guaranteed"
[metering]
source = "self-reported"
"#;
        let path = write_manifest(tmp.path(), body);
        let err = resolve_start_launch("demo", Some(&path)).unwrap_err();
        assert!(
            matches!(err, LaunchResolveError::NoStartTemplate { .. }),
            "got {err}"
        );
    }

    #[test]
    fn resolve_start_launch_renegotiates_a_drifted_contract_major() {
        // Retro #161 (finding B1): the launch-less/legacy start fallback
        // re-reads adapter.toml. A manifest EDITED to a foreign major after
        // registering must FAIL the fallback start — naming BOTH versions and
        // quoting the rule — not slip through the re-read and bypass the 6-6
        // registration gate.
        let tmp = TempDir::new().unwrap();
        let body = r#"
contract_version = "2.1.0"
[adapter]
kind = "demo"
[lifecycle.start]
exec = "demo-agent"
[capabilities.pause]
linux = "guaranteed"
[metering]
source = "self-reported"
"#;
        let path = write_manifest(tmp.path(), body);
        let err = resolve_start_launch("demo", Some(&path)).unwrap_err();
        let LaunchResolveError::ContractIncompatible {
            path: err_path,
            detail,
        } = err
        else {
            panic!("expected ContractIncompatible on the fallback re-read, got {err:?}");
        };
        assert_eq!(err_path, path.to_string_lossy());
        // Both versions named + the rule quoted (the same message the
        // registration gate renders from ContractVersionError).
        assert!(detail.contains("2.1.0"), "manifest version named: {detail}");
        assert!(
            detail.contains(ktesio_adapter_api::CONTRACT_VERSION),
            "engine version named: {detail}"
        );
        assert!(
            detail.contains(ktesio_adapter_api::COMPATIBILITY_RULE),
            "rule quoted: {detail}"
        );
    }

    #[test]
    fn resolve_start_launch_fallback_still_accepts_same_major() {
        // The gate is major-only (FR-30): a same-major edit (e.g. a prerelease
        // spelling of 1.x) still resolves through the fallback.
        let tmp = TempDir::new().unwrap();
        let body = r#"
contract_version = "1.9.0-rc.1"
[adapter]
kind = "demo"
[lifecycle.start]
exec = "demo-agent"
[capabilities.pause]
linux = "guaranteed"
[metering]
source = "self-reported"
"#;
        let path = write_manifest(tmp.path(), body);
        let launch = resolve_start_launch("demo", Some(&path)).unwrap();
        assert_eq!(launch.exec, "demo-agent");
    }

    #[test]
    fn enforce_invariants_accepts_a_supported_declaration() {
        use ktesio_adapter_api::{Capability, SupportLevel};
        let declaration = CapabilityDeclaration::new().with(
            Capability::Pause,
            OsId::Linux,
            SupportLevel::Guaranteed,
        );
        let resolved = ResolvedAdapter {
            kind: "ok".to_string(),
            declaration,
            metering_source: MeteringSource::SelfReported,
            manifest_path: None,
            launch: None,
        };
        assert!(enforce_registration_invariants(&resolved).is_ok());
    }

    // ---- Story 2-2: apply_config_mapping (the resolved-config → launch transform) ----

    use crate::domain::{ConfigLayer, EffectiveConfig, SourceLayer};

    /// Build an [`EffectiveConfig`] from a single instance-layer TOML body (the
    /// other three layers empty) — the resolved input `apply_config_mapping`
    /// consumes.
    fn effective_from_instance(body: &str) -> EffectiveConfig {
        let layers = [
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::parse(SourceLayer::Instance, "<test>", body).unwrap(),
            ConfigLayer::empty(),
        ];
        crate::domain::resolve(layers)
    }

    /// An empty launch (exec only) to apply a mapping onto.
    fn empty_launch() -> StartLaunch {
        StartLaunch {
            exec: "the-agent".to_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }
    }

    /// The empty resolved-secrets map — no leaf is a secret (the story-2-2
    /// mapping-mechanics tests carry no `secret:` values). Story 2-4 added the
    /// `secrets` parameter to `apply_config_mapping`; these tests pass an empty map.
    fn no_secrets() -> std::collections::BTreeMap<String, crate::domain::SecretString> {
        std::collections::BTreeMap::new()
    }

    #[test]
    fn apply_maps_model_to_env_target() {
        // AC-A / AC4 (env): a `model` value lands in the declared env var.
        let mapping = ConfigMapping::new().with("model", ConfigTarget::env("MODEL"));
        let effective = effective_from_instance("model = \"gpt-4\"\n");
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        apply_config_mapping(&mut launch, &mapping, &effective, &no_secrets(), tmp.path()).unwrap();
        assert_eq!(launch.env.get("MODEL").map(String::as_str), Some("gpt-4"));
        assert!(launch.args.is_empty());
    }

    #[test]
    fn apply_maps_model_to_flag_target_as_two_args() {
        // AC-A / AC4 (flag): a `model` value appends `--model gpt-4` to the args.
        let mapping = ConfigMapping::new().with("model", ConfigTarget::flag("--model"));
        let effective = effective_from_instance("model = \"gpt-4\"\n");
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        apply_config_mapping(&mut launch, &mapping, &effective, &no_secrets(), tmp.path()).unwrap();
        assert_eq!(
            launch.args,
            vec!["--model".to_string(), "gpt-4".to_string()]
        );
        assert!(launch.env.is_empty());
    }

    #[test]
    fn apply_maps_model_to_file_target_rendered_into_home() {
        // AC-A / AC4 (file): a `model` value renders into a native TOML file in the
        // Agent Home, at the declared native key path.
        let mapping = ConfigMapping::new().with(
            "model",
            ConfigTarget::file("config/agent.toml", "llm.model"),
        );
        let effective = effective_from_instance("model = \"gpt-4\"\n");
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        apply_config_mapping(&mut launch, &mapping, &effective, &no_secrets(), tmp.path()).unwrap();
        // The launch env/args are untouched (the value went into the file).
        assert!(launch.env.is_empty());
        assert!(launch.args.is_empty());
        // The rendered file exists at the declared path with the native key set.
        let rendered = tmp.path().join("config/agent.toml");
        assert!(rendered.is_file(), "file target must render into the home");
        let text = std::fs::read_to_string(&rendered).unwrap();
        let parsed: toml::Table = text.parse().unwrap();
        assert_eq!(
            parsed["llm"]["model"].as_str(),
            Some("gpt-4"),
            "native key path set; got {text}"
        );
    }

    #[test]
    fn apply_delivers_agent_pass_through_verbatim_to_env() {
        // AC6: an `agent.*` leaf is delivered VERBATIM (the key-tail after `agent.`
        // + the value) into the native mechanism — NO known-key lookup, NO
        // rewriting. The recorded convention delivers it to an env var named by the
        // verbatim tail.
        let mapping = ConfigMapping::new(); // no known-key rules at all
        let effective = effective_from_instance("[agent]\nCUSTOM_FLAG = \"verbatim\"\n");
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        apply_config_mapping(&mut launch, &mapping, &effective, &no_secrets(), tmp.path()).unwrap();
        assert_eq!(
            launch.env.get("CUSTOM_FLAG").map(String::as_str),
            Some("verbatim"),
            "pass-through delivered verbatim by key-tail"
        );
    }

    #[test]
    fn apply_delivers_resolved_cleartext_for_a_secret_leaf_not_the_mask() {
        // Story 2-4 AC9 (delivery diverges from display): a `secret:NAME` leaf whose
        // resolved cleartext is in the `secrets` map delivers the REAL value into
        // the native env target, NOT the mask and NOT the reference. This is the
        // crux — `display()` would mask this same leaf.
        let mapping = ConfigMapping::new().with("model", ConfigTarget::env("MODEL"));
        let effective = effective_from_instance("model = \"secret:MODEL_KEY\"\n");
        // The supervisor would resolve `secret:MODEL_KEY` → this cleartext.
        let mut secrets = std::collections::BTreeMap::new();
        secrets.insert(
            "model".to_string(),
            crate::domain::SecretString::new("sk-real-key-123"),
        );
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        apply_config_mapping(&mut launch, &mapping, &effective, &secrets, tmp.path()).unwrap();
        // The adapter's native env holds the CLEARTEXT (usable key), not the mask.
        assert_eq!(
            launch.env.get("MODEL").map(String::as_str),
            Some("sk-real-key-123"),
            "a secret leaf must deliver resolved cleartext to the adapter"
        );
        // Sanity: the masked display of the same leaf is NOT what was delivered.
        assert_ne!(
            launch.env.get("MODEL").map(String::as_str),
            Some(ktesio_adapter_api::OsId::current().as_str()), // arbitrary non-equal
        );
        assert_ne!(
            launch.env.get("MODEL").map(String::as_str),
            Some("secret:MODEL_KEY")
        );
    }

    #[test]
    fn apply_delivers_resolved_cleartext_for_a_secret_leaf_into_a_flag_arg() {
        // Story 2-4 AC9 + the flag/argv boundary (documented in the Flag arm and
        // docs/architecture.md Secrets): a `secret:NAME` leaf mapped to a FLAG target
        // delivers its resolved CLEARTEXT as an argv token, NOT the mask and NOT the
        // reference. This is the STRICTER exposure the docs call out (argv is
        // cross-user readable via `ps`/`/proc/<pid>/cmdline`), so it is proven
        // explicitly alongside the env path. Story 11-2 (AI-39): the fact is also
        // REPORTED back to the start seam — the report names exactly the keys that
        // delivered cleartext into argv.
        let mapping = ConfigMapping::new().with("model", ConfigTarget::flag("--model"));
        let effective = effective_from_instance("model = \"secret:MODEL_KEY\"\n");
        let mut secrets = std::collections::BTreeMap::new();
        secrets.insert(
            "model".to_string(),
            crate::domain::SecretString::new("sk-real-key-123"),
        );
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        let report =
            apply_config_mapping(&mut launch, &mapping, &effective, &secrets, tmp.path()).unwrap();
        // The flag value token carries the CLEARTEXT (what the child sees in argv).
        assert_eq!(
            launch.args,
            vec!["--model".to_string(), "sk-real-key-123".to_string()],
            "a secret leaf mapped to a flag must deliver resolved cleartext into argv"
        );
        // Story 11-2 (AI-39): the report names the secret-into-flag key so the
        // start seam can emit its ONE warn-only diagnostic.
        assert_eq!(
            report.secret_flag_keys,
            vec!["model".to_string()],
            "the delivered cleartext-into-argv fact must be reported"
        );
        // Sanity: neither the mask nor the raw reference reaches argv.
        assert!(
            !launch.args.iter().any(|a| a == "secret:MODEL_KEY"),
            "the raw reference must not reach argv"
        );
        assert!(
            !launch.args.iter().any(|a| a == crate::domain::SECRET_MASK),
            "the mask must not reach argv (delivery diverges from display)"
        );
    }

    #[test]
    fn apply_report_is_empty_without_a_secret_flag_combination() {
        // The AI-39 report's quiet halves: a NON-secret value on a flag target,
        // a secret on an env target, and a secret on an unmapped key all report
        // NOTHING — only the cleartext-into-argv combination is named.
        let flag_mapping = ConfigMapping::new().with("model", ConfigTarget::flag("--model"));
        let tmp = tempfile::tempdir().unwrap();
        let mut launch = empty_launch();

        // Non-secret flag delivery: no report (plain values on argv are normal).
        let plain = effective_from_instance("model = \"gpt-4\"\n");
        let report = apply_config_mapping(
            &mut launch,
            &flag_mapping,
            &plain,
            &no_secrets(),
            tmp.path(),
        )
        .unwrap();
        assert!(report.secret_flag_keys.is_empty(), "plain flag is quiet");

        // Secret on an ENV target: cleartext, but into the home-scoped env — quiet.
        let env_mapping = ConfigMapping::new().with("model", ConfigTarget::env("MODEL"));
        let secret_leaf = effective_from_instance("model = \"secret:MODEL_KEY\"\n");
        let mut secrets = std::collections::BTreeMap::new();
        secrets.insert(
            "model".to_string(),
            crate::domain::SecretString::new("sk-quiet"),
        );
        let mut launch = empty_launch();
        let report = apply_config_mapping(
            &mut launch,
            &env_mapping,
            &secret_leaf,
            &secrets,
            tmp.path(),
        )
        .unwrap();
        assert!(report.secret_flag_keys.is_empty(), "env target is quiet");

        // Secret on an UNMAPPED key (Decision 6 no-op, review-1 patch 8): the
        // adapter declares NO rule for the key, so the leaf is delivered
        // nowhere — the Flag arm is never even reached, and the report stays
        // empty despite the resolved secret sitting in the map.
        let unmapped_leaf = effective_from_instance("temperature = \"secret:UNMAPPED_KEY\"\n");
        let mut unmapped_secrets = std::collections::BTreeMap::new();
        unmapped_secrets.insert(
            "temperature".to_string(),
            crate::domain::SecretString::new("sk-unmapped"),
        );
        let mut launch = empty_launch();
        let report = apply_config_mapping(
            &mut launch,
            &ConfigMapping::new(), // no rules at all
            &unmapped_leaf,
            &unmapped_secrets,
            tmp.path(),
        )
        .unwrap();
        assert!(
            report.secret_flag_keys.is_empty(),
            "an unmapped key is quiet"
        );
    }

    #[test]
    fn apply_secret_leaf_missing_from_map_fails_closed_to_the_mask() {
        // Defense-in-depth: if a secret leaf is (unexpectedly) absent from the
        // `secrets` map, the placement falls back to the MASKED display() — never
        // the cleartext, never the raw reference. Fail-closed: a bug in resolution
        // yields a broken-but-safe agent config, not a leak.
        let mapping = ConfigMapping::new().with("model", ConfigTarget::env("MODEL"));
        let effective = effective_from_instance("model = \"secret:MODEL_KEY\"\n");
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        // Empty secrets map (the leaf is secret but unresolved in the map).
        apply_config_mapping(&mut launch, &mapping, &effective, &no_secrets(), tmp.path()).unwrap();
        assert_eq!(
            launch.env.get("MODEL").map(String::as_str),
            Some(ktesio_engine_secret_mask()),
            "a secret leaf missing from the map must fail closed to the mask, not leak"
        );
    }

    /// The config-layer secret mask token (re-exported), for the fail-closed test.
    fn ktesio_engine_secret_mask() -> &'static str {
        crate::domain::SECRET_MASK
    }

    #[test]
    fn apply_unmapped_documented_key_is_a_noop() {
        // Decision 6 / AC5: a documented key the adapter declares NO rule for is
        // delivered nowhere — the launch is untouched.
        let mapping = ConfigMapping::new(); // model has no rule
        let effective = effective_from_instance("model = \"gpt-4\"\n");
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        apply_config_mapping(&mut launch, &mapping, &effective, &no_secrets(), tmp.path()).unwrap();
        assert!(launch.env.is_empty(), "unmapped key must not land anywhere");
        assert!(launch.args.is_empty());
        assert!(!tmp.path().join("config").exists(), "no file rendered");
    }

    #[test]
    fn apply_is_deterministic_and_empty_config_is_a_noop() {
        // An empty effective config leaves the launch untouched; and the transform
        // is deterministic (same inputs → same launch), reinforcing the pure-ish
        // start seam.
        let mapping = ConfigMapping::new()
            .with("model", ConfigTarget::flag("--model"))
            .with("agent.a", ConfigTarget::env("IGNORED")); // pass-through ignores the rule
        let empty = EffectiveConfig::default();
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        apply_config_mapping(&mut launch, &mapping, &empty, &no_secrets(), tmp.path()).unwrap();
        assert_eq!(launch, empty_launch(), "empty config is a no-op");

        // Determinism: two applies of the same non-empty config yield equal launches.
        let effective = effective_from_instance("model = \"m\"\n[agent]\nx = \"y\"\n");
        let mut a = empty_launch();
        let mut b = empty_launch();
        apply_config_mapping(&mut a, &mapping, &effective, &no_secrets(), tmp.path()).unwrap();
        apply_config_mapping(&mut b, &mapping, &effective, &no_secrets(), tmp.path()).unwrap();
        assert_eq!(a, b);
        // model flag present; the agent.* leaf delivered verbatim by its tail (NOT
        // via the env rule keyed at "agent.a").
        assert_eq!(a.args, vec!["--model".to_string(), "m".to_string()]);
        assert_eq!(a.env.get("x").map(String::as_str), Some("y"));
        assert!(
            !a.env.contains_key("IGNORED"),
            "pass-through bypasses the rule"
        );
    }

    #[test]
    fn resolve_config_mapping_reads_manifest_config_section() {
        // A manifest adapter's mapping comes from its parsed [config] section.
        let tmp = TempDir::new().unwrap();
        let body = r#"
contract_version = "1.0.0"
[adapter]
kind = "demo"
[lifecycle.start]
exec = "the-agent"
[capabilities.interaction]
linux = "guaranteed"
[metering]
source = "self-reported"
[config.model]
flag = "--model"
"#;
        let path = write_manifest(tmp.path(), body);
        let mapping = resolve_config_mapping("demo", Some(&path)).unwrap();
        assert_eq!(
            mapping.target("model").unwrap().render_flag_args("x"),
            Some(["--model".to_string(), "x".to_string()])
        );
    }

    #[test]
    fn resolve_config_mapping_native_reads_the_builtin_table() {
        // A native adapter's mapping comes from the builtin code-declared table:
        // the mock declares `model` → env `MODEL`.
        let mapping = resolve_config_mapping("mock", None).unwrap();
        assert_eq!(mapping.target("model").unwrap().env_var(), Some("MODEL"));
        // An unknown native kind → an empty mapping (delivers nothing), not an err.
        assert!(resolve_config_mapping("nope", None).unwrap().is_empty());
    }

    #[test]
    fn resolve_config_mapping_rejects_an_escaping_file_target_symmetric() {
        // Fix #2 / AD-6 (symmetric trust): `resolve_config_mapping` VALIDATES the
        // mapping for BOTH kinds. A `[config.model]` File target whose path escapes
        // the Agent Home is rejected at start with InvalidConfigMapping — even
        // though this arm parses the manifest with `from_toml_str` (not the
        // registration-time `validate`), so a post-registration manifest edit that
        // slips a bad path past registration is still caught before it can steer a
        // write outside the home. The same `mapping.validate()` gate covers a
        // native code-declared File target (a native adapter carries no manifest to
        // re-validate at registration, so this start-time check is its ONLY guard).
        let tmp = TempDir::new().unwrap();
        let body = r#"
contract_version = "1.0.0"
[adapter]
kind = "demo"
[lifecycle.start]
exec = "the-agent"
[capabilities.interaction]
linux = "guaranteed"
[metering]
source = "self-reported"
[config.model]
file = { path = "../escape.toml", key = "k" }
"#;
        let path = write_manifest(tmp.path(), body);
        let err = resolve_config_mapping("demo", Some(&path)).unwrap_err();
        match err {
            LaunchResolveError::InvalidConfigMapping {
                adapter,
                key,
                detail,
            } => {
                assert_eq!(adapter, "demo");
                assert_eq!(key, "model");
                assert!(detail.contains("RELATIVE"), "{detail}");
            }
            other => panic!("expected InvalidConfigMapping, got {other}"),
        }
    }

    #[test]
    fn resolve_config_mapping_unreadable_manifest_is_reported() {
        // A manifest path that does not exist → ManifestUnreadable (defensive: the
        // launch resolver re-read it first, but this stays symmetric).
        let missing = std::path::Path::new("/no/such/adapter.toml");
        let err = resolve_config_mapping("demo", Some(missing)).unwrap_err();
        assert!(
            matches!(err, LaunchResolveError::ManifestUnreadable { .. }),
            "got {err}"
        );
    }

    #[test]
    fn resolve_config_mapping_malformed_manifest_is_unreadable() {
        // A manifest that fails to PARSE surfaces as ManifestUnreadable.
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(tmp.path(), "not = = valid toml");
        let err = resolve_config_mapping("demo", Some(&path)).unwrap_err();
        assert!(matches!(err, LaunchResolveError::ManifestUnreadable { .. }));
    }

    #[test]
    fn apply_file_target_render_failure_is_a_typed_error() {
        // ConfigApplyError::FileRender: a file target whose PARENT path is blocked
        // (a regular file sits where a directory must be) fails the write with a
        // typed error naming the key/path — never a panic (the start then rejects
        // before the `starting` transition, so the instance state is unchanged).
        let mapping =
            ConfigMapping::new().with("model", ConfigTarget::file("blocked/agent.toml", "k"));
        let effective = effective_from_instance("model = \"gpt-4\"\n");
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        // Put a regular FILE at `blocked` so create_dir_all(blocked) fails.
        std::fs::write(tmp.path().join("blocked"), b"not a dir").unwrap();
        let err =
            apply_config_mapping(&mut launch, &mapping, &effective, &no_secrets(), tmp.path())
                .unwrap_err();
        match err {
            ConfigApplyError::FileRender { key, path, .. } => {
                assert_eq!(key, "blocked/agent.toml");
                assert_eq!(path, "blocked/agent.toml");
            }
        }
        // The error message names the key + path (defensive Display coverage).
        let msg = apply_config_mapping(
            &mut empty_launch(),
            &mapping,
            &effective,
            &no_secrets(),
            tmp.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(msg.contains("blocked/agent.toml"), "{msg}");
        assert!(msg.contains("Agent Home"), "{msg}");
    }

    #[test]
    fn apply_file_target_write_failure_is_a_typed_error_not_a_panic() {
        // The sibling of the blocked-PARENT case above, and the one that actually
        // exercises the write: the parent directory resolves fine, but the target
        // FILE path is occupied by a directory, so `fs::write` itself fails. This
        // is the shape a stale Agent Home takes after a manifest changes a file
        // target's name, and it happens on the START path — so it MUST be a typed
        // FileRender naming the key/path (the supervisor then rejects the start
        // before the `starting` transition, leaving the instance in its prior
        // state) rather than a panic that would take the engine down.
        let mapping = ConfigMapping::new().with("model", ConfigTarget::file("agent.toml", "k"));
        let effective = effective_from_instance("model = \"gpt-4\"\n");
        let tmp = tempfile::tempdir().unwrap();
        // A DIRECTORY where the rendered config file must go.
        std::fs::create_dir(tmp.path().join("agent.toml")).unwrap();

        let err = apply_config_mapping(
            &mut empty_launch(),
            &mapping,
            &effective,
            &no_secrets(),
            tmp.path(),
        )
        .unwrap_err();

        let ConfigApplyError::FileRender { key, path, detail } = &err;
        assert_eq!(key, "agent.toml");
        assert_eq!(path, "agent.toml");
        assert!(!detail.is_empty(), "the OS detail must be preserved");
        // Story 11-2 (AI-28): the write goes through the atomic helper, so a
        // failed render leaves NO temp residue in the home — the helper's temp
        // was renamed-at (and cleaned up), never abandoned.
        let residue: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(residue.is_empty(), "no temp residue on a failed render");
        // The blocking directory is left exactly as it was — a failed render must
        // not delete or replace whatever is already in the Agent Home.
        assert!(tmp.path().join("agent.toml").is_dir());
    }

    // NOTE (review-1 patch follow-up): the "failed re-render leaves the
    // PREVIOUS native file byte-identical" scenario moved to
    // `crates/ktesio-engine/tests/atomic_config_writes.rs`
    // (`apply_file_target_failed_write_leaves_the_previous_native_file_unchanged`,
    // unix-gated: the injection needs a read-only render directory). The
    // temp-path-planting injection this in-module test used cannot predict the
    // review-1 collision-safe temp name (`<file>.tmp-<pid>-<tid>-<seq>`).

    #[test]
    fn apply_two_file_keys_where_a_prefix_collides_overwrites_to_a_table() {
        // set_dotted_string's collision branch: two native keys into the SAME file
        // where one is a prefix of the other (`a` then `a.b`). The engine-authored
        // file has no scalar to preserve, so the prefix is overwritten with a table
        // (unlike the instance-config write path, which fails closed). Deterministic
        // sorted iteration means `a` is set first, then `a.b` masks it.
        let mapping = ConfigMapping::new()
            .with("model", ConfigTarget::file("f.toml", "a"))
            .with("temperature", ConfigTarget::file("f.toml", "a.b"));
        let effective = effective_from_instance("model = \"m\"\ntemperature = \"t\"\n");
        let mut launch = empty_launch();
        let tmp = tempfile::tempdir().unwrap();
        apply_config_mapping(&mut launch, &mapping, &effective, &no_secrets(), tmp.path()).unwrap();
        let rendered = tmp.path().join("f.toml");
        let parsed: toml::Table = std::fs::read_to_string(&rendered).unwrap().parse().unwrap();
        // `a` became a table with `b = "t"`; the scalar `a = "m"` was masked.
        assert_eq!(
            parsed["a"]["b"].as_str(),
            Some("t"),
            "prefix overwritten to a table"
        );
    }
}
