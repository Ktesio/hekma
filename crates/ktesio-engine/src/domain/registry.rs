//! The registry service — `register` / `remove` (spine AD-1, AD-2).
//!
//! This is the engine's public surface for the registration capability
//! (FR-1/FR-2/FR-3). It is re-exported at the crate root and IS part of the
//! Embedding Interface (AD-2): `kt` drives register/remove/list through these
//! methods and the returned domain types only.
//!
//! ## Facade-friendliness (forward contract for story 1.4)
//!
//! The service takes its state-dir base explicitly ([`Registry::open`]) and
//! holds no global or thread-local state, so when the engine goes async (AD-13,
//! story 1.4) these sync methods can sit behind the `blocking()` facade
//! unchanged. Do NOT introduce hidden globals or assume a running runtime.

use std::path::Path;

use ktesio_adapter_api::{
    Capability, CapabilityDeclaration, ConfigTarget, EffectiveCapabilities, OsId, SupportLevel,
};

use crate::adapter::{self, AdapterRef, ResolvedAdapter, StartLaunch};
use crate::paths::EnginePaths;
use crate::ports::{
    CompositeSecretResolver, GuaranteeLevel, MemoryBacking, MemoryBackingKind, MemoryBackingStatus,
    SecretError, SpawnRecord, StateStore, StoreError,
};
use crate::store::SqliteStore;
use crate::time::now_rfc3339;

use super::config::{self, ConfigError, ConfigLayer, EffectiveConfig, SourceLayer};
use super::error::RegistryError;
use super::instance::AgentInstance;
use super::lifecycle::LifecycleState;
use super::name::InstanceName;
use super::restart::RestartPolicy;
use super::secret::SecretString;

/// The engine-owned DEFAULTS layer (spine AD-9 layer 1), story 2-1.
///
/// `[ASSUMPTION]` recorded (Decision 1): the engine defaults are an EMBEDDED
/// `const` TOML string parsed once — there is no on-disk file to lose, corrupt,
/// or guard as a path-authority surface, and the layer is identical + fully
/// deterministic on every install.
///
/// HONESTY RULE (review decision #1, Islam): the engine-defaults layer ships
/// EMPTY in 2-1. It previously seeded `restart.policy = "on-failure"`, but the
/// reaper reads the Restart Policy from the SQLite spawn record (NOT config), so
/// config must not advertise/seed a key it does not control — that would be a
/// misleading no-op. The reaper's default keeps coming from
/// [`RestartPolicy::default`](super::restart::RestartPolicy) in the engine,
/// unchanged. Kept as a comment-only TOML (parses to an empty table) so the
/// four-layer plumbing stays real and the engine-defaults slot is honestly
/// present-but-empty; a later story that makes a unified key engine-controlled
/// will add it here. A parse failure is an ENGINE BUG (compile-time constant),
/// surfaced as a typed [`ConfigError::MalformedLayer`], never a panic.
const ENGINE_DEFAULTS_TOML: &str = "\
# Ktesio engine-owned config defaults (spine AD-9 layer 1: weakest).
# Embedded in the engine; the same for every Agent Instance. Intentionally EMPTY
# in Epic 2.1: config ships only keys it can honestly honor, and no engine-wide
# unified key is engine-controlled yet. The unified schema grows additively in
# later Epic-2 stories (this const gains keys as they become config-controlled).
";

/// A human label for the embedded engine-defaults layer in diagnostics (it has
/// no filesystem path — it is a compiled-in constant).
const ENGINE_DEFAULTS_LABEL: &str = "<engine-defaults>";

/// The reserved IDENTITY key `materialize_home` seeds into an instance
/// `config.toml` (`name = "<instance>"`), filtered out of the resolved effective
/// config so it is not presented as a settable unified key (review patch #4). It
/// is instance identity — the row + Agent Home directory name — not user config.
const RESERVED_IDENTITY_KEY: &str = "name";

/// Filename of the adapter snapshot inside an Agent Home (story 1.3).
///
/// Holds the effective (current-OS) Capability Declaration, the Metering Source,
/// and (for a manifest adapter) the manifest path 1.4 needs to launch it. JSON
/// so the engine avoids a `toml` dependency (AD-3: only adapter-api owns TOML).
/// `[ASSUMPTION]` on the filename.
const ADAPTER_SNAPSHOT_FILE: &str = "adapter.json";

/// The persisted adapter snapshot written into an Agent Home at registration.
///
/// This is the on-disk form of the resolved adapter. `kt agent show` reads it
/// back to render the effective per-OS Capability Declaration (AC1 "visible for
/// the instance") without re-resolving the adapter. Structured artifacts live
/// as files in the home (AD-6), keeping the DB lean.
///
/// ## Full declaration persisted; projection happens at READ time (F3)
///
/// The snapshot stores the **full** per-OS [`CapabilityDeclaration`], NOT a
/// single-OS projection frozen to the registering host. The effective
/// (current-OS) view is computed in [`Registry::effective_capabilities`] by
/// projecting onto [`OsId::current`] when the snapshot is read. This keeps the
/// state directory portable: a home registered on one OS and later read on
/// another projects correctly for the OS actually running, instead of returning
/// a stale projection captured at registration.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct AdapterSnapshot {
    /// The adapter kind (mirrors the instance `kind` column).
    kind: String,
    /// The declared Metering Source, as its wire string.
    metering_source: String,
    /// The manifest path, present only for a manifest adapter (1.4 needs it).
    manifest_path: Option<String>,
    /// The full per-OS Capability Declaration (projected at read time).
    declaration: CapabilityDeclaration,
    /// The resolved `start` launch (exec + args + env) captured at REGISTRATION,
    /// present only for a manifest adapter that declares a `[lifecycle.start]`
    /// template. The supervisor USES this at start instead of re-reading the
    /// manifest — removing a fragile start-time re-read (a hosted-runner
    /// filesystem quirk dropped the re-read `args`, so the agent spawned with the
    /// right binary but ZERO args). Persisted via the dedicated [`LaunchSnapshot`]
    /// DTO — the live [`StartLaunch`] is intentionally non-`Serialize` (AD-10:
    /// a post-`apply_config_mapping` launch can hold resolved secret cleartext).
    /// `#[serde(default)]` for backward compat: a snapshot written before this
    /// field existed (or a native adapter) has no `launch` key, deserializes to
    /// `None`, and the start path falls back to the existing manifest re-read
    /// (`resolve_start_launch`).
    #[serde(default)]
    launch: Option<LaunchSnapshot>,
}

/// The persisted form of a resolved [`StartLaunch`] inside the adapter snapshot.
///
/// A DEDICATED serialize-only DTO — NOT the live [`StartLaunch`], which is
/// deliberately non-`Serialize` (spine AD-10): a post-[`apply_config_mapping`](crate::adapter::apply_config_mapping)
/// launch can hold resolved secret cleartext in its plain-`String` `args`/`env`,
/// so the live type must never be serializable. Only the REGISTRATION-time launch
/// (no resolved secrets — just the manifest's `[lifecycle.start]` exec/args/env)
/// is snapshotted, and it rides through this DTO. Mirrors the
/// [`EffectiveConfigSnapshot`] precedent one function away: serialize a dedicated
/// DTO, keep the live domain type decoupled from the on-disk schema.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct LaunchSnapshot {
    /// The executable to run (program name or path).
    exec: String,
    /// Positional arguments.
    args: Vec<String>,
    /// Environment overrides.
    env: std::collections::BTreeMap<String, String>,
}

impl From<&StartLaunch> for LaunchSnapshot {
    fn from(launch: &StartLaunch) -> Self {
        Self {
            exec: launch.exec.clone(),
            args: launch.args.clone(),
            env: launch.env.clone(),
        }
    }
}

impl From<LaunchSnapshot> for StartLaunch {
    fn from(snapshot: LaunchSnapshot) -> Self {
        Self {
            exec: snapshot.exec,
            args: snapshot.args,
            env: snapshot.env,
        }
    }
}

/// The schema version stamped on the persisted [`EffectiveConfigSnapshot`]
/// (story 2-3). A monotonically-increasing integer (the `adapter.json` snapshot
/// has no version because it is engine-internal; the effective-config snapshot
/// is a PROMISED AD-9 artifact for Hosts/debugging, so it carries a version — a
/// later shape change bumps this, mirroring the `list`/`show` `--json`
/// versioned-document convention `kt` already exposes). Starts at 1.
const EFFECTIVE_CONFIG_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// One resolved leaf in the persisted effective-config snapshot (story 2-3): the
/// dotted key, the RENDERED value, and its winning source layer.
///
/// The `value` is the [`ResolvedValue::display`] string — the SAME single
/// display path the human `config get` table and `config get --json` render
/// (AC8): routing every surface through `display()` keeps the story-2-4 masking
/// seam to ONE choke point, so a `secret:NAME` value is MASKED (`secret:****`) in
/// this persisted snapshot exactly as in `config get` (never the cleartext — the
/// resolved secret reaches only the adapter, FR-14). `source` is the kebab-case
/// [`SourceLayer`] wire label (its serde form).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct EffectiveConfigEntry {
    /// The dotted leaf key (`model`, `agent.tools.web_search`, …).
    key: String,
    /// The rendered winning value (via the single `display()` path — AC8).
    value: String,
    /// The winning layer's stable label (`engine-default` / … / `invocation-override`).
    source: SourceLayer,
}

/// The persisted effective-config snapshot written into an Agent Home at START
/// (story 2-3, spine AD-9 "start resolves to an EffectiveConfig snapshot
/// persisted in the Agent Home, every value tagged with its source layer" +
/// AD-6 "effective-config snapshots are files inside the Agent Home").
///
/// This is the durable answer to FR-13's "what will actually apply on next
/// start, and where each value came from" — a Host/operator/debugging artifact.
/// It mirrors the [`AdapterSnapshot`] precedent EXACTLY (a dedicated
/// `#[derive(Serialize, Deserialize)]` DTO, written with
/// `serde_json::to_string_pretty` through path authority) — but written at start,
/// not registration, and OVERWRITTEN every start/restart (AC7), since "effective
/// config at start" does not exist until a start happens.
///
/// Built by ITERATING [`EffectiveConfig::iter`] into `entries` (the in-memory
/// [`EffectiveConfig`]/[`ResolvedValue`] are deliberately NOT `Serialize` — the
/// snapshot file schema stays decoupled from the internal type, matching the
/// `adapter.json` DTO decision).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct EffectiveConfigSnapshot {
    /// The snapshot schema version ([`EFFECTIVE_CONFIG_SNAPSHOT_SCHEMA_VERSION`]).
    schema_version: u32,
    /// Every resolved leaf, sorted by key (the [`EffectiveConfig`] iteration
    /// order is already deterministic via its `BTreeMap`).
    entries: Vec<EffectiveConfigEntry>,
}

impl EffectiveConfigSnapshot {
    /// Build the snapshot DTO from a resolved [`EffectiveConfig`] (story 2-3).
    ///
    /// Walks every leaf, rendering the value via the ONE [`ResolvedValue::display`]
    /// path (AC8) and tagging it with the winning [`SourceLayer`]. Pure (no I/O),
    /// so it is unit-testable in isolation; the writer serializes + persists it.
    fn from_effective(effective: &EffectiveConfig) -> Self {
        let entries = effective
            .iter()
            .map(|(key, resolved)| EffectiveConfigEntry {
                key: key.clone(),
                value: resolved.display(),
                source: resolved.source,
            })
            .collect();
        Self {
            schema_version: EFFECTIVE_CONFIG_SNAPSHOT_SCHEMA_VERSION,
            entries,
        }
    }
}

/// Retain or delete the Agent Home when removing an instance (AC4).
///
/// Named directly from the acceptance criterion's "retain or delete". The DB
/// row is always deleted; this only decides the fate of the on-disk Agent Home.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveDisposition {
    /// Leave the Agent Home directory intact on disk.
    Retain,
    /// Remove the Agent Home directory tree.
    Delete,
}

/// The registry service: the engine's registration capability.
///
/// Owns a resolved [`EnginePaths`] (path authority) and a [`StateStore`]. Open
/// one with [`Registry::open`]; each call is self-contained.
pub struct Registry {
    paths: EnginePaths,
    store: SqliteStore,
}

impl Registry {
    /// Open a registry rooted at an optional state-dir base.
    ///
    /// `base`:
    /// * `Some(path)` — use it (tests / explicit embedding).
    /// * `None` — resolve via `KTESIO_STATE_DIR` then the platform data dir
    ///   ([`EnginePaths::new`]).
    ///
    /// Ensures the state dir exists and opens (creating + migrating) the DB.
    pub fn open(base: Option<std::path::PathBuf>) -> Result<Self, RegistryError> {
        let paths = EnginePaths::new(base).map_err(|e| {
            // AI-4: name the ACTUAL path that failed. A rejected relative
            // `KTESIO_STATE_DIR` carries the offending value itself — not the
            // `<default via …>` placeholder, which describes a resolution mode,
            // not the path the operator typed. Only the genuinely path-less
            // failure (no env var and no platform data dir) keeps the
            // placeholder, because there is no concrete path to name.
            let path = match &e {
                crate::paths::PathError::RelativeStateDir { value } => value.clone(),
                crate::paths::PathError::NoStateDir => {
                    format!("<default via {}>", crate::paths::STATE_DIR_ENV)
                }
            };
            RegistryError::Io {
                name: "<state-dir>".to_string(),
                path,
                source: std::io::Error::other(e.to_string()),
            }
        })?;
        // Create the state base (and thus its parent chain) before opening the
        // DB — a fresh install has no directory yet (AC1 "fresh state").
        ensure_dir(paths.state_base(), "<state-dir>")?;
        let store = SqliteStore::open(&paths.state_db())?;
        Ok(Self { paths, store })
    }

    /// The engine-computed Agent Home path for `name` (path authority helper
    /// for callers that want to display it without a full lookup).
    pub fn agent_home(&self, name: &InstanceName) -> std::path::PathBuf {
        self.paths.agent_home(name)
    }

    /// Register a new Agent Instance under `name` of a native `kind`.
    ///
    /// Convenience wrapper over [`Registry::register_with_adapter`] for the
    /// `--kind <kind>` path: the kind now RESOLVES to a real native adapter
    /// (story 1.3) and its Capability Declaration + Metering Source are validated
    /// before any side effect. An unknown kind is rejected with
    /// [`RegistryError::UnknownAdapterKind`] and nothing is written.
    pub fn register(&self, name: &str, kind: &str) -> Result<AgentInstance, RegistryError> {
        self.register_with_adapter(name, &AdapterRef::Native(kind.to_string()))
    }

    /// Register a new Agent Instance, resolving `reference` to an adapter first.
    ///
    /// On success the instance is in [`LifecycleState::Registered`], its Agent
    /// Home exists with an instance `config.toml` and an `adapter.json` snapshot
    /// of the effective (current-OS) Capability Declaration + Metering Source
    /// (+ manifest path for a manifest adapter, which 1.4 needs to launch), and
    /// its Usage Ledger is empty. Returns the created [`AgentInstance`].
    ///
    /// ## Atomicity ordering (F2 lesson — adapter validation is a pure pre-step)
    ///
    /// 1. Validate the name (rejected here, nothing touched).
    /// 2. **Resolve + validate the adapter** — a pure, side-effect-free step. A
    ///    rejected adapter (unknown kind, missing/invalid manifest, no viable
    ///    Metering Source, no capabilities) returns HERE, before any row or
    ///    directory exists, so a rejection leaves ZERO partial state (AC2/AC4 +
    ///    the F2 orphan-row lesson).
    /// 3. Insert the DB row. The `UNIQUE` constraint detects a duplicate
    ///    atomically, still before any file is created.
    /// 4. Create the Agent Home, write `config.toml`, and write the effective
    ///    declaration snapshot.
    /// 5. If step 4 fails, delete the row and remove any partial directory, then
    ///    surface the error — leaving no orphan row and no half-created home.
    pub fn register_with_adapter(
        &self,
        name: &str,
        reference: &AdapterRef,
    ) -> Result<AgentInstance, RegistryError> {
        // (1) Validate the name.
        let name = InstanceName::new(name).map_err(|reason| RegistryError::InvalidName {
            name: name.to_string(),
            reason,
        })?;

        // (2) Resolve + validate the adapter FIRST (side-effect-free). Any
        // failure returns before a row or home is created — atomicity is
        // preserved by never starting the write. `?` maps AdapterResolveError
        // into RegistryError via the From impl (naming the section / kind).
        let resolved = adapter::resolve(reference)?;

        let home = self.paths.agent_home(&name);
        let now = now_rfc3339();
        let instance = AgentInstance {
            name: name.clone(),
            kind: resolved.kind().to_string(),
            state: LifecycleState::Registered,
            agent_home: home.to_string_lossy().into_owned(),
            created_at: now.clone(),
            updated_at: now,
        };

        // (3) Insert the row. Duplicate -> DuplicateName, nothing on disk yet.
        self.store.create_instance(&instance).map_err(|e| match e {
            StoreError::DuplicateName { name } => RegistryError::DuplicateName { name },
            other => RegistryError::Store(other),
        })?;

        // (4) Create the Agent Home + config + adapter snapshot; (5) roll back
        // on failure.
        if let Err(io_err) = self.materialize_home(&name, &resolved) {
            // Rollback: remove the row first (restoring atomicity), then any
            // partial directory. The row delete is the load-bearing step — if
            // it fails we would leak an orphan `registered` row with no home,
            // breaking the atomicity contract, so we surface that distinctly
            // (naming the orphaned row + remediation, NFR-1). The partial
            // directory cleanup is best-effort.
            if let Err(rollback_err) = self.store.delete_instance(&name) {
                let _ = std::fs::remove_dir_all(&home);
                return Err(RegistryError::RegisterOrphanRow {
                    name: name.as_str().to_string(),
                    home_error: io_err.to_string(),
                    rollback_error: rollback_err.to_string(),
                });
            }
            let _ = std::fs::remove_dir_all(&home);
            return Err(io_err);
        }

        Ok(instance)
    }

    /// The effective (current-OS) Capability Declaration for a registered
    /// instance, read back from its Agent Home snapshot (AC1 "visible for the
    /// instance"). `kt agent show` renders this.
    ///
    /// Returns [`RegistryError::NotFound`] if the instance is not registered, or
    /// [`RegistryError::Io`] if the snapshot is missing/unreadable (a corrupt
    /// home). The snapshot stores the FULL per-OS declaration; the effective view
    /// is projected onto [`OsId::current`] HERE, at read time (F3), so a home
    /// registered on one OS still projects correctly when read on another.
    pub fn effective_capabilities(
        &self,
        name: &str,
    ) -> Result<EffectiveCapabilities, RegistryError> {
        let name = InstanceName::new(name).map_err(|reason| RegistryError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        // Confirm the instance exists (distinguish NotFound from a read error).
        let _instance = self
            .store
            .get_instance(&name)?
            .ok_or_else(|| RegistryError::NotFound {
                name: name.as_str().to_string(),
            })?;

        let snapshot = self.read_adapter_snapshot(&name)?;
        // Project onto the OS actually running now (not the registering OS).
        Ok(snapshot.declaration.effective(OsId::current()))
    }

    /// Read + parse an instance's adapter snapshot from its Agent Home.
    ///
    /// Maps a missing/unreadable file or corrupt JSON to [`RegistryError::Io`]
    /// naming the snapshot path (never panics on a corrupt home).
    fn read_adapter_snapshot(&self, name: &InstanceName) -> Result<AdapterSnapshot, RegistryError> {
        let snapshot_path = self.adapter_snapshot_path(name);
        let text = std::fs::read_to_string(&snapshot_path).map_err(|source| RegistryError::Io {
            name: name.as_str().to_string(),
            path: snapshot_path.to_string_lossy().into_owned(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|e| RegistryError::Io {
            name: name.as_str().to_string(),
            path: snapshot_path.to_string_lossy().into_owned(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
        })
    }

    /// Absolute path to an instance's adapter snapshot file inside its home.
    fn adapter_snapshot_path(&self, name: &InstanceName) -> std::path::PathBuf {
        self.paths.agent_home(name).join(ADAPTER_SNAPSHOT_FILE)
    }

    /// Create the Agent Home directory, write the instance `config.toml`, and
    /// write the adapter snapshot (`adapter.json`).
    ///
    /// `[ASSUMPTION]` the filenames `config.toml` / `adapter.json` and a minimal
    /// instance-level config body. Full layered config resolution is Epic 2;
    /// here we persist the instance layer plus the effective declaration so
    /// "the effective Capability Declaration is visible for the instance" (AC1)
    /// holds. Writing the snapshot inside `materialize_home` keeps it covered by
    /// the same registration rollback (AD-6 atomicity).
    fn materialize_home(
        &self,
        name: &InstanceName,
        resolved: &ResolvedAdapter,
    ) -> Result<(), RegistryError> {
        let home = self.paths.agent_home(name);
        ensure_dir(&home, name.as_str())?;
        let config_path = self.paths.instance_config(name);
        // Minimal instance-level TOML (AD-9: TOML at every layer). Kept tiny;
        // Epic 2 owns the real config schema and layering.
        let body = format!(
            "# Ktesio Agent Instance config (instance layer).\n\
             # Managed by the engine; edit with care.\n\
             name = \"{name}\"\n",
            name = name.as_str(),
        );
        // ATOMIC (story 11-2 review-1, AI-24 family): this is the SAME instance
        // `config.toml` the `set_config` write persists, so it gets the same
        // temp-file + rename treatment — a crash mid-registration cannot leave
        // a truncated config.toml behind. The error mapping is unchanged
        // (RegistryError::Io naming the path); a failure here is covered by the
        // registration rollback below (row delete + home removal), which also
        // sweeps any residue the helper's own best-effort cleanup could miss.
        crate::paths::write_atomically(&config_path, body.as_bytes()).map_err(|source| {
            RegistryError::Io {
                name: name.as_str().to_string(),
                path: config_path.to_string_lossy().into_owned(),
                source,
            }
        })?;

        // Persist the FULL per-OS declaration + metering source (+ manifest path
        // for a manifest adapter). The effective (current-OS) view is projected
        // at read time (F3), so the snapshot stays OS-portable. JSON keeps the
        // engine free of a `toml` dependency (AD-3: only adapter-api owns TOML).
        let snapshot = AdapterSnapshot {
            kind: resolved.kind().to_string(),
            metering_source: resolved.metering_source().as_str().to_string(),
            manifest_path: resolved
                .manifest_path()
                .map(|p| p.to_string_lossy().into_owned()),
            declaration: resolved.declaration().clone(),
            // Snapshot the resolved launch (captured from the same manifest parse
            // as the declaration) so the start path uses it instead of re-reading
            // the manifest. Persisted through the `LaunchSnapshot` DTO (the live
            // `StartLaunch` is non-`Serialize`, AD-10). `None` for a native adapter.
            launch: resolved.launch().map(LaunchSnapshot::from),
        };
        let snapshot_path = self.adapter_snapshot_path(name);
        let json = serde_json::to_string_pretty(&snapshot).map_err(|e| RegistryError::Io {
            name: name.as_str().to_string(),
            path: snapshot_path.to_string_lossy().into_owned(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()),
        })?;
        std::fs::write(&snapshot_path, json).map_err(|source| RegistryError::Io {
            name: name.as_str().to_string(),
            path: snapshot_path.to_string_lossy().into_owned(),
            source,
        })?;
        Ok(())
    }

    /// Remove an Agent Instance, honoring the retain/delete disposition (AC4)
    /// and the running-guard (AC5).
    ///
    /// ## Running-guard (AC5) + process teardown (AI-11)
    ///
    /// This method is the RECORD + Agent-Home half of remove: it validates the
    /// state-machine guard — if the stored Lifecycle State is `running` and
    /// `force` is false, it returns [`RegistryError::RunningRequiresForce`] — then
    /// deletes the row (FK-cascading the write-ahead spawn record) and, for
    /// `Delete`, the Agent Home. It does NOT touch live processes: stopping a
    /// live/adopted instance's process BEFORE the row is deleted (so `remove` never
    /// leaves an unsupervised orphan — the AI-11 invariant, for both plain and
    /// `--force` remove) is the caller's job and lives in [`Engine::remove`], which
    /// holds the supervisor's in-memory handle map. Tests that exercise the guard
    /// here still seed a `running`/`paused` row directly via the store (no live
    /// process needed) — the guard is pure state-machine validation.
    ///
    /// ## Removal ordering
    ///
    /// Delete the DB row first; then, for [`RemoveDisposition::Delete`], remove
    /// the Agent Home tree. If the tree removal fails after the row is gone,
    /// return [`RegistryError::RemoveLeftoverHome`] naming the leftover path
    /// (NFR-1) rather than reporting silent success.
    pub fn remove(
        &self,
        name: &str,
        disposition: RemoveDisposition,
        force: bool,
    ) -> Result<(), RegistryError> {
        // Validate the name shape so a malformed name yields InvalidName rather
        // than a confusing NotFound.
        let name = InstanceName::new(name).map_err(|reason| RegistryError::InvalidName {
            name: name.to_string(),
            reason,
        })?;

        // Look up the instance (also gives us its Lifecycle State for the guard).
        let instance = self
            .store
            .get_instance(&name)?
            .ok_or_else(|| RegistryError::NotFound {
                name: name.as_str().to_string(),
            })?;

        // Running-guard (AC5): refuse a running instance unless --force.
        if !force && !instance.state.is_removable_without_force() {
            return Err(RegistryError::RunningRequiresForce {
                name: name.as_str().to_string(),
            });
        }

        // Always delete the row first.
        self.store.delete_instance(&name)?;

        // Then handle the Agent Home per disposition.
        if disposition == RemoveDisposition::Delete {
            let home = self.paths.agent_home(&name);
            // Delete the tree directly (no exists() pre-check — that would be a
            // TOCTOU race). An already-absent home is success: the desired end
            // state (no home) already holds. Only surface real removal failures.
            if let Err(err) = std::fs::remove_dir_all(&home) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    return Err(RegistryError::RemoveLeftoverHome {
                        name: name.as_str().to_string(),
                        path: home.to_string_lossy().into_owned(),
                        detail: err.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// List the whole Fleet, ordered by name (delegates to the store).
    pub fn list(&self) -> Result<Vec<AgentInstance>, RegistryError> {
        Ok(self.store.list_instances()?)
    }

    // ---- Memory Backing attachments (story 5-1, spine AD-11) ----
    //
    // The registry owns BOTH halves of a memory attach — path authority (the
    // managed directory inside the Agent Home) and the SQLite state — so the
    // operation lives here, not on the supervisor. Ordering copies `remove`'s
    // discipline: validate the name → look up (→ NotFound) → STATE GUARD → only
    // then any side effect. Attach/detach are NOT lifecycle verbs and are never
    // added to the transition table (AD-15) — the guard is pure persisted-state
    // validation with no live process required (DC-3), and there is deliberately
    // NO force escape: AD-11 forbids hot-swap outright.

    /// Attach a Memory Backing of `kind` to the named instance (story 5-1,
    /// AC1/AC3). Permitted ONLY from a TERMINAL persisted state
    /// (`registered`/`stopped`/`failed`); every non-terminal state is refused
    /// ([`RegistryError::MemoryBackingHotSwap`] — no `--force`, AD-11).
    ///
    /// Side effects, in order, AFTER every fallible check passes:
    /// 1. for [`MemoryBackingKind::Filesystem`] — create the managed directory
    ///    (`<Agent Home>/memory`, one idempotent `create_dir_all`, no recursion);
    /// 2. persist the attachment row.
    ///
    /// If the row write fails after the directory exists, the directory is LEFT
    /// in place — it is inert and idempotent, and inventing a rollback that
    /// deletes operator-shaped data is exactly what a supervisor must not do.
    ///
    /// Idempotence (A-6): re-attaching the SAME kind is an idempotent success
    /// (the row's original timestamp stands; the directory is self-healed if a
    /// hand-delete removed it). Attaching a DIFFERENT kind over an existing one
    /// is rejected ([`RegistryError::MemoryBackingKindConflict`]) — detach first.
    ///
    /// Returns the engine-computed managed directory path (path authority): `kt`
    /// displays it but never constructs it (DC-1). For a non-`filesystem` kind
    /// the path is returned uncreated (only `filesystem` materializes it).
    pub fn attach_memory(
        &self,
        name: &str,
        kind: MemoryBackingKind,
    ) -> Result<std::path::PathBuf, RegistryError> {
        // (1) Validate the name shape so a malformed name yields InvalidName
        // rather than a confusing NotFound.
        let name = InstanceName::new(name).map_err(|reason| RegistryError::InvalidName {
            name: name.to_string(),
            reason,
        })?;

        // (2) Look up the instance (→ NotFound when unregistered).
        let instance = self.lookup(&name)?;

        // (3) State guard: terminal states only (pure persisted-state check).
        ensure_terminal_for_memory_backing(&name, &instance.state)?;

        let dir = self.paths.agent_memory_dir(&name);

        // (4) Conflict / idempotence check against the EXISTING attachment —
        // still before any side effect.
        if let Some(existing) = self.store.get_memory_backing(&name)? {
            if existing.kind != kind {
                return Err(RegistryError::MemoryBackingKindConflict {
                    name: name.as_str().to_string(),
                    attached: existing.kind.as_str().to_string(),
                    requested: kind.as_str().to_string(),
                });
            }
            // Same kind again: idempotent success. Still self-heal the managed
            // directory (a manual delete must not wedge future starts), but do
            // NOT rewrite the row — the original attached_at stands.
            if kind == MemoryBackingKind::Filesystem {
                ensure_managed_memory_dir(&dir, name.as_str())?;
            }
            return Ok(dir);
        }

        // (5) Side effects, in order. If step 6 fails, step 5's directory stays
        // (inert + idempotent; see this method's docs — no data-deleting rollback).
        if kind == MemoryBackingKind::Filesystem {
            ensure_managed_memory_dir(&dir, name.as_str())?;
        }
        let backing = MemoryBacking {
            name: name.clone(),
            kind,
            attached_at: now_rfc3339(),
        };
        self.store.upsert_memory_backing(&backing)?;
        Ok(dir)
    }

    /// Detach the named instance's Memory Backing (story 5-1, AC3/DC-7). Same
    /// validate → look up → terminal-guard order as [`Registry::attach_memory`].
    ///
    /// METADATA ONLY (A-4): this clears the attachment row and NOTHING else —
    /// the managed directory and its contents REMAIN on disk. Rationale: (a) it
    /// is operator data — silent deletion is a surprise a supervisor must never
    /// spring; (b) it avoids adding a second unbounded tree removal under the
    /// engine locks (AD-17); (c) it matches `kt agent remove`'s shipped default
    /// where retention is the default and deletion is opt-in. A future
    /// `detach --delete` is a deliberate deferral, not an oversight. Re-attaching
    /// later simply re-adopts the existing contents. Detaching when nothing is
    /// attached is a successful no-op.
    pub fn detach_memory(&self, name: &str) -> Result<(), RegistryError> {
        let name = InstanceName::new(name).map_err(|reason| RegistryError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        let instance = self.lookup(&name)?;
        ensure_terminal_for_memory_backing(&name, &instance.state)?;
        self.store.clear_memory_backing(&name)?;
        Ok(())
    }

    /// Read the instance's Memory Backing through the public API (story 5-1,
    /// Task 4.5): `None` when nothing is attached; otherwise the kind, the
    /// engine-computed managed directory, the DC-10 delivery fact — whether
    /// the adapter's declared `[config]` mapping targets the reserved key, i.e.
    /// whether the injected path will actually reach the agent (Q-1 honesty
    /// rule) — and story 5-2's typed guarantee level. Shaped for reuse by
    /// future status surfaces.
    ///
    /// Computing the delivery fact resolves the adapter's declared mapping (from
    /// the persisted snapshot facts — no manifest re-parse beyond what the start
    /// path itself does). A corrupt/unreadable home surfaces as
    /// [`RegistryError::Io`], exactly like every other snapshot read.
    ///
    /// For a NON-`filesystem` kind nothing is ever delivered at start, so the
    /// resolution is SKIPPED and `declared` reads `false` honestly: not "the
    /// adapter declined", but "no delivery is offered to decline" (story 5-2,
    /// DC-2 — `declared` must never read as a promise for a native backing).
    pub fn memory_status(&self, name: &str) -> Result<Option<MemoryBackingStatus>, RegistryError> {
        let name = InstanceName::new(name).map_err(|reason| RegistryError::InvalidName {
            name: name.to_string(),
            reason,
        })?;
        // Confirm existence FIRST so an unknown instance is NotFound, not None
        // (None honestly means "exists, nothing attached").
        self.lookup(&name)?;
        let Some(backing) = self.store.get_memory_backing(&name)? else {
            return Ok(None);
        };

        let dir = self.paths.agent_memory_dir(&name);
        let declared = if backing.kind == MemoryBackingKind::Filesystem {
            self.memory_key_declared(&name)?
        } else {
            false
        };
        Ok(Some(MemoryBackingStatus {
            kind: backing.kind,
            dir,
            declared,
            guarantee: GuaranteeLevel::for_kind(backing.kind),
        }))
    }

    /// Whether the instance's adapter declares a `[config]` target for the
    /// reserved memory key (the DC-10 fact, shared by [`Registry::memory_status`]
    /// here and by the supervisor's start-time notice).
    fn memory_key_declared(&self, name: &InstanceName) -> Result<bool, RegistryError> {
        let (kind, manifest_path, _) = self.adapter_launch_facts(name)?;
        let mapping =
            adapter::resolve_config_mapping(&kind, manifest_path.as_deref()).map_err(|e| {
                RegistryError::Io {
                    name: name.as_str().to_string(),
                    path: manifest_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("<native adapter '{kind}'>")),
                    source: std::io::Error::other(e.to_string()),
                }
            })?;
        Ok(mapping.target(super::config::MEMORY_DIR_KEY).is_some())
    }

    // ---- Supervisor collaboration surface (story 1.4; crate-internal) ----
    //
    // The lifecycle supervisor drives transitions through these; they stay
    // `pub(crate)` so the public Embedding Interface is only the Engine facade,
    // not the raw store. All speak domain types (AD-1).

    /// Look up an instance, mapping absence to [`RegistryError::NotFound`].
    pub(crate) fn lookup(&self, name: &InstanceName) -> Result<AgentInstance, RegistryError> {
        self.store
            .get_instance(name)?
            .ok_or_else(|| RegistryError::NotFound {
                name: name.as_str().to_string(),
            })
    }

    /// Persist a Lifecycle State change (supervisor transition). Bumps
    /// `updated_at` in the store.
    pub(crate) fn set_state(
        &self,
        name: &InstanceName,
        state: LifecycleState,
    ) -> Result<(), RegistryError> {
        self.store.set_state(name, state)?;
        Ok(())
    }

    /// The persisted adapter facts the supervisor needs to launch an instance:
    /// its kind, (for a manifest adapter) the manifest path 1-3 recorded, and the
    /// `start` launch (exec + args + env) SNAPSHOTTED at registration.
    ///
    /// The snapshotted launch is the supervisor's PRIMARY start-spec source —
    /// using it removes the fragile start-time manifest re-read (a hosted-runner
    /// filesystem quirk returned empty `args` on re-read, so the agent spawned
    /// with no args). It is `None` for an inert native adapter (`mock` — no
    /// launch command) OR a snapshot written before the launch was persisted;
    /// in that case the supervisor falls back to
    /// [`crate::adapter::resolve_start_launch`] using the kind + manifest path
    /// (which resolves a launchable native builtin's code-declared launch since
    /// story 6-2, keeps erroring `NativeHasNoLaunch` for inert natives like
    /// `mock`, re-reads the manifest for a legacy snapshot, and RE-NEGOTIATES
    /// the contract version on that re-read — retro #161 — so a manifest
    /// drifted to a foreign major after registration fails the start).
    pub(crate) fn adapter_launch_facts(
        &self,
        name: &InstanceName,
    ) -> Result<(String, Option<std::path::PathBuf>, Option<StartLaunch>), RegistryError> {
        let snapshot = self.read_adapter_snapshot(name)?;
        let manifest_path = snapshot.manifest_path.map(std::path::PathBuf::from);
        // Convert the persisted DTO back into the live `StartLaunch` the start path
        // consumes (so `apply_config_mapping` still runs on a `StartLaunch`).
        Ok((
            snapshot.kind,
            manifest_path,
            snapshot.launch.map(StartLaunch::from),
        ))
    }

    /// The instance's declared Metering Source as its wire string (story 3-1, AD-7):
    /// read from the persisted adapter snapshot (`self-reported` / `engine-observed`).
    /// The supervisor stamps this on every ingested `UsageEvent` during the Run, and
    /// the Fleet read surfaces it (AC-C). A corrupt/missing snapshot surfaces the same
    /// [`RegistryError::Io`] as the other snapshot reads.
    pub(crate) fn metering_source(&self, name: &InstanceName) -> Result<String, RegistryError> {
        Ok(self.read_adapter_snapshot(name)?.metering_source)
    }

    /// The effective (current-OS) [`SupportLevel`] for `capability` on an
    /// instance (story 1-5, AC5). Reads the persisted [`AdapterSnapshot`]'s FULL
    /// per-OS declaration and projects it onto [`OsId::current`] at READ time (the
    /// F3 mechanism `effective_capabilities` uses) — it does NOT re-parse the
    /// manifest or re-resolve the adapter, and it does NOT freeze a level at
    /// register time. This is THE read the supervisor uses to pick the pause
    /// dispatch level. An absent declaration for the current OS projects to
    /// [`SupportLevel::Unsupported`] — the honest default (a manifest that omits
    /// pause for this OS correctly fails fast).
    pub(crate) fn effective_support(
        &self,
        name: &InstanceName,
        capability: Capability,
    ) -> Result<SupportLevel, RegistryError> {
        let snapshot = self.read_adapter_snapshot(name)?;
        Ok(snapshot.declaration.support(capability, OsId::current()))
    }

    // ---- Write-ahead spawn records + restart policy (story 1-6, AD-5/AD-6) ----
    //
    // Thin `pub(crate)` pass-throughs to the store's spawn-record methods (same
    // pattern as `set_state` → `store.set_state`). Each store call is one
    // transaction (AD-6). The supervisor commits the record BEFORE declaring an
    // instance supervised, clears it on a clean stop, and reads every record on
    // engine start to reconcile orphans.

    /// Commit a write-ahead spawn record (AD-5) — one transaction. Called by the
    /// supervisor between `spawn` and declaring the instance `running`.
    pub(crate) fn write_spawn_record(&self, record: &SpawnRecord) -> Result<(), RegistryError> {
        self.store.upsert_spawn_record(record)?;
        Ok(())
    }

    /// Clear an instance's write-ahead spawn record (a clean stop, so it is not
    /// later adopted/failed as an orphan). Idempotent.
    pub(crate) fn clear_spawn_record(&self, name: &InstanceName) -> Result<(), RegistryError> {
        self.store.clear_spawn_record(name)?;
        Ok(())
    }

    /// Read an instance's write-ahead spawn record, or `None` if absent.
    pub(crate) fn spawn_record(
        &self,
        name: &InstanceName,
    ) -> Result<Option<SpawnRecord>, RegistryError> {
        Ok(self.store.get_spawn_record(name)?)
    }

    /// List every write-ahead spawn record (the orphan-reconcile input on engine
    /// start).
    pub(crate) fn list_spawn_records(&self) -> Result<Vec<SpawnRecord>, RegistryError> {
        Ok(self.store.list_spawn_records()?)
    }

    /// Persist a new restart count + last-known cause for an instance (a restart
    /// bump or a reset) — one transaction. No-op if the instance has no record.
    pub(crate) fn set_restart_count(
        &self,
        name: &InstanceName,
        restart_count: u32,
        last_known_cause: Option<&str>,
    ) -> Result<(), RegistryError> {
        self.store
            .set_restart_count(name, restart_count, last_known_cause)?;
        Ok(())
    }

    /// Set the per-instance [`RestartPolicy`] (story 1-6, AC4 "per-instance
    /// configurable") — the config SEED. Persists via the store (creating a
    /// minimal policy-only record if the instance was never started). One
    /// transaction (AD-6).
    pub(crate) fn set_restart_policy(
        &self,
        name: &InstanceName,
        policy: RestartPolicy,
    ) -> Result<(), RegistryError> {
        self.store.set_restart_policy(name, policy)?;
        Ok(())
    }

    /// The effective per-instance [`RestartPolicy`] (story 1-6, AC4/AC9).
    ///
    /// RECOMMENDED seed (AD-9 layered TOML config is Epic 2): read the policy from
    /// the persisted spawn record; when there is no record yet (or its policy is
    /// absent), fall back to the AD-15 default ([`RestartPolicy::default`] =
    /// `on-failure`). This is a per-instance SOURCE (the DB), not a value
    /// hard-wired at the call site.
    pub(crate) fn effective_restart_policy(
        &self,
        name: &InstanceName,
    ) -> Result<RestartPolicy, RegistryError> {
        Ok(self
            .store
            .get_spawn_record(name)?
            .map(|r| r.restart_policy)
            .unwrap_or_default())
    }

    // ---- Unified layered config (story 2-1, spine AD-9, FR-11) ----
    //
    // Path authority for config: the engine is the SOLE reader/writer of every
    // config layer. These `pub(crate)` methods sit behind the `Engine` facade
    // (mirroring `set_restart_policy` / `effective_restart_policy`) and speak
    // domain types only (AD-1/AD-2). Config stays as TOML FILES under path
    // authority (AD-9's "TOML at every layer"); it does NOT move into SQLite
    // (AD-6 governs registry/lifecycle/ledger state, not config).

    /// The effective (resolved) config for an instance (spine AD-9, AC-A).
    ///
    /// Loads the four layers THROUGH PATH AUTHORITY — embedded engine defaults +
    /// the instance's kind defaults + its Agent Home `config.toml` +
    /// `overrides` — and folds them with the pure [`config::resolve`]. This is the
    /// read `kt agent config get` uses. `overrides` is the ephemeral invocation
    /// layer (empty for a plain `get`); it is threaded here so a future
    /// `start --set k=v` (a later story) can supply it WITHOUT an API change
    /// (Decision 8, recorded). A malformed on-disk layer surfaces a typed
    /// [`ConfigError`] naming the layer + path (never a panic — AC8).
    pub(crate) fn effective_config(
        &self,
        name: &InstanceName,
        overrides: ConfigLayer,
    ) -> Result<EffectiveConfig, ConfigError> {
        // Confirm the instance exists (so `get` on a ghost is NotFound, not an
        // empty resolve) AND capture its kind in ONE lookup — the kind selects the
        // kind-defaults layer, so there is no second store round-trip.
        let instance = self.require_instance(name)?;

        let engine = engine_defaults_layer()?;
        let kind = kind_defaults_layer(&instance.kind);
        let instance_layer = self.instance_config_layer(name)?;
        let mut effective = config::resolve([engine, kind, instance_layer, overrides]);
        // Drop the reserved IDENTITY key (review patch #4): `materialize_home`
        // seeds `name = "<instance>"` into the instance config.toml, but that is
        // instance identity (already the row + the Agent Home dir name), NOT user
        // config. Presenting it via `config get` while `config set … name …` is
        // rejected as unknown would be incoherent — so it is not surfaced as a
        // settable key. (It stays in the on-disk file as a human-readable marker;
        // we only filter the RESOLVED view.)
        effective.remove(RESERVED_IDENTITY_KEY);
        Ok(effective)
    }

    /// Set one config key on the INSTANCE layer (spine AD-9, AC-B/AC10).
    ///
    /// Validates at WRITE time FIRST ([`config::validate_write`]): an unknown key
    /// outside the `agent.*` pass-through namespace is rejected BEFORE any
    /// persistence, so the instance `config.toml` is left byte-unchanged (the
    /// registry's "reject before side effect" atomicity). On acceptance the key is
    /// set into the parsed instance table (a DEEP dotted-path set, so a nested key
    /// like `restart.policy` writes the right nested table and existing siblings
    /// survive) and the whole instance layer is re-serialized to disk through path
    /// authority. Pass-through (`agent.*`) keys round-trip verbatim (AC7); the
    /// value is stored as an ordinary TOML STRING — a `secret:NAME` REFERENCE is
    /// what is persisted here (story 2-4 resolves + masks it at start/read, FR-14;
    /// this write neither resolves nor echoes a secret).
    ///
    /// The persisted write is ATOMIC (story 11-2, AI-24): the serialized layer is
    /// written to a same-directory temp file and renamed over the
    /// instance `config.toml` ([`crate::paths::write_atomically`]), so a crash
    /// mid-write leaves the OLD bytes — never a truncated file — and a failed
    /// write leaves no temp residue. The error mapping is unchanged: a
    /// write/rename failure surfaces as the same [`ConfigError::MalformedLayer`]
    /// naming the layer + path as before.
    ///
    /// On SUCCESS the returned vec carries ZERO OR MORE WARN-ONLY steering
    /// warnings (story 11-2, AI-33). The one warning today: a `secret:NAME` value
    /// set on a key the adapter maps to a FLAG target — the resolved cleartext
    /// would ride the process argv (world-readable cross-user), so the operator
    /// is steered toward an env/file target. The write itself is NEVER rejected
    /// (warning semantics were never ratified as errors); the caller (the CLI)
    /// prints each warning to stderr. Resolution of the adapter mapping is
    /// BEST-EFFORT: a snapshot/manifest that cannot be read simply yields no
    /// warning — steering must never fail (or complicate) a valid write.
    pub(crate) fn set_config(
        &self,
        name: &InstanceName,
        key: &str,
        value: &str,
    ) -> Result<Vec<String>, ConfigError> {
        self.require_instance(name)?;

        // (1) Validate BEFORE touching disk (AC-B). A rejection returns here with
        // nothing written.
        config::validate_write(key, value)?;

        // (2) Compute the AI-33 steering warnings (warn-only, before the write so
        // an Err return below can never carry a half-computed warning state).
        let warnings = self.secret_flag_steering(name, key, value);

        // (3) Load the current instance layer, set the dotted key (deep), and
        // re-serialize. All through path authority — the engine owns the path.
        // set_dotted FAILS CLOSED on a scalar-intermediate collision (patch #3),
        // BEFORE the write below, so a conflicting write leaves config unchanged.
        let path = self.paths.instance_config(name);
        let mut table = self.instance_config_layer(name)?.as_table().clone();
        set_dotted(&mut table, key, toml::Value::String(value.to_string()))?;

        let serialized =
            toml::to_string_pretty(&table).map_err(|e| ConfigError::MalformedLayer {
                layer: SourceLayer::Instance,
                path: path.to_string_lossy().into_owned(),
                detail: format!("could not serialize the updated instance config: {e}"),
            })?;
        // ATOMIC write (AI-24): temp in the target's directory + one rename, so
        // a crash mid-write cannot truncate the instance config. The error shape
        // is unchanged (MalformedLayer naming the layer + path).
        crate::paths::write_atomically(&path, serialized.as_bytes()).map_err(|source| {
            ConfigError::MalformedLayer {
                layer: SourceLayer::Instance,
                path: path.to_string_lossy().into_owned(),
                detail: format!("could not write the instance config: {source}"),
            }
        })?;
        Ok(warnings)
    }

    /// The AI-33 set-time steering fact (story 11-2): does THIS write put a
    /// `secret:NAME` value onto a key whose adapter mapping target is a FLAG?
    /// Returns zero or one rendered warning (the CLI prints it to stderr).
    ///
    /// Only a genuine secret REFERENCE steers ([`config::secret_name`] — the same
    /// predicate masking and resolution use, so the three can never disagree),
    /// and only on a DOCUMENTED known key: an `agent.*` pass-through key is
    /// delivered verbatim to an env var named by its tail regardless of any
    /// declared rule, so a flag rule on it is never consulted and a warning
    /// would be wrong. The key's target is learned from the adapter's declared
    /// mapping via [`Self::adapter_launch_facts`] +
    /// [`adapter::resolve_config_mapping`] — the same `memory_key_declared`
    /// pattern the memory-status read uses. Both reads are BEST-EFFORT here: an
    /// unreadable snapshot/manifest suppresses the warning (an advisory must
    /// never fail a valid write).
    ///
    /// HONESTY BOUNDARY (review-1 patch 9): this warning reflects the CURRENT
    /// manifest declaration at SET time — a manifest EDITED after registration
    /// can change a key's target and make this warning stale in either
    /// direction (a now-env target keeps an unneeded silence; a now-flag target
    /// warns only at the next set). The START-time half of the steering reads
    /// the persisted REGISTRATION snapshot — the launch/mapping the start seam
    /// actually uses — so the runtime diagnostic always matches delivery.
    fn secret_flag_steering(&self, name: &InstanceName, key: &str, value: &str) -> Vec<String> {
        if config::secret_name(value).is_none() || config::pass_through_tail(key).is_some() {
            return Vec::new();
        }
        let Ok((kind, manifest_path, _)) = self.adapter_launch_facts(name) else {
            return Vec::new();
        };
        let Ok(mapping) = adapter::resolve_config_mapping(&kind, manifest_path.as_deref()) else {
            return Vec::new();
        };
        match mapping.target(key) {
            Some(ConfigTarget::Flag { .. }) => vec![format!(
                "config key '{key}' holds a secret:NAME reference, but this adapter delivers \
                 it to a FLAG target: the resolved cleartext will be passed on the agent's \
                 command line, where argv is readable by other local users (ps, \
                 /proc/<pid>/cmdline). Prefer an env or file target for secret-carrying keys.",
            )],
            _ => Vec::new(),
        }
    }

    /// Persist the effective-config snapshot for an instance at START (story 2-3,
    /// spine AD-9 + AD-6). Builds the [`EffectiveConfigSnapshot`] DTO from the
    /// already-resolved `effective`, serializes it with
    /// `serde_json::to_string_pretty`, and writes it to the Agent Home through
    /// path authority ([`EnginePaths::effective_config_snapshot`]) — the engine is
    /// the SOLE writer (AD-6). OVERWRITTEN in place every call (AC7): the supervisor
    /// calls it on every successful start/restart so it always reflects the config
    /// resolved for the CURRENT run. Mirrors `materialize_home`'s `adapter.json`
    /// write mechanics; a write failure surfaces a typed
    /// [`RegistryError::SnapshotWrite`] naming the instance + snapshot path (never a
    /// panic — the caller rejects the start cleanly).
    ///
    /// The Agent Home already exists (created at registration), so no directory
    /// creation is needed; the filename is engine-owned (not manifest-supplied), so
    /// no `..`-escape check is required (unlike a rendered native config file).
    pub(crate) fn write_effective_config_snapshot(
        &self,
        name: &InstanceName,
        effective: &EffectiveConfig,
    ) -> Result<(), RegistryError> {
        let snapshot = EffectiveConfigSnapshot::from_effective(effective);
        let path = self.paths.effective_config_snapshot(name);
        let json =
            serde_json::to_string_pretty(&snapshot).map_err(|e| RegistryError::SnapshotWrite {
                name: name.as_str().to_string(),
                path: path.to_string_lossy().into_owned(),
                detail: format!("could not serialize the effective-config snapshot: {e}"),
            })?;
        std::fs::write(&path, json).map_err(|source| RegistryError::SnapshotWrite {
            name: name.as_str().to_string(),
            path: path.to_string_lossy().into_owned(),
            detail: source.to_string(),
        })?;
        Ok(())
    }

    /// Resolve every `secret:NAME` leaf in an already-resolved effective config
    /// into a cleartext [`SecretString`], keyed by dotted leaf key (story 2-4, spine
    /// AD-10, AC-A/AC5/AC9). The engine is the path authority, so it builds the
    /// composite resolver (env → the 0600 secrets file at
    /// [`EnginePaths::secrets_file`]) HERE and resolves each secret-classified leaf
    /// (identified by [`config::is_secret_ref`] on the leaf's underlying string
    /// value, using [`config::secret_name`] for the lookup key).
    ///
    /// A leaf that is NOT a `secret:` reference contributes nothing to the map (it
    /// is delivered via its plain `display()`). A `secret:NAME` that resolves in
    /// NEITHER resolver is a hard [`SecretError`] the caller maps to a
    /// start-rejecting [`EngineError::Secret`] (no half-launch). The returned map is
    /// consumed by [`crate::adapter::apply_config_mapping`], which places
    /// `expose_secret()` (the REAL cleartext) into the adapter's native mechanism —
    /// so the SAME leaf renders MASKED via `display()` (the snapshot/`config get`)
    /// while delivering cleartext to the agent (display and delivery diverge, AC9).
    ///
    /// `kt` never calls this — it reads rendered (masked or `--reveal`ed) strings
    /// (AD-2). The resolved secrets are transient (resolved at start, handed to the
    /// adapter, dropped) and NEVER persisted by the engine except into the agent's
    /// own native config file it needs to run (the accepted FR-2 boundary, AC9).
    pub(crate) fn resolve_secrets(
        &self,
        effective: &EffectiveConfig,
    ) -> Result<std::collections::BTreeMap<String, SecretString>, SecretError> {
        let resolver = CompositeSecretResolver::env_then_file(self.paths.secrets_file());
        let mut resolved = std::collections::BTreeMap::new();
        for (key, value) in effective.iter() {
            // Only a STRING leaf can be a secret reference; classify on its raw
            // string (the same predicate masking uses, so the two never disagree).
            let toml::Value::String(s) = &value.value else {
                continue;
            };
            let Some(name) = config::secret_name(s) else {
                continue;
            };
            let secret = resolver.require(name)?;
            resolved.insert(key.clone(), secret);
        }
        Ok(resolved)
    }

    /// Resolve secret leaves for a `--reveal` READ (story 2-4, AC-C/AC11): the
    /// dotted key → REVEALED cleartext string for each `secret:NAME` leaf of the
    /// instance's effective config, so the on-demand `config get --reveal` surface
    /// can emit the real value. Resolves the effective config THEN reveals each
    /// secret leaf's cleartext (LIVE re-resolution — Assumption 11). Mirrors
    /// [`resolve_secrets`] but returns the exposed cleartext string directly (the
    /// read path renders it; it is not handed to an adapter). `kt` never resolves
    /// secrets itself — it asks the engine (AD-2).
    ///
    /// A resolution failure surfaces as [`ConfigError::SecretReveal`] the CLI turns
    /// into a stderr diagnostic (never a crash). `--reveal` NEVER touches the
    /// snapshot/logs/events (this is a read-only path — resolution here writes
    /// nothing). Only the secret leaves are returned; a caller overlays them onto
    /// the (masked) effective config, un-masking exactly the secret leaves.
    pub(crate) fn reveal_secrets(
        &self,
        name: &InstanceName,
        overrides: ConfigLayer,
    ) -> Result<std::collections::BTreeMap<String, String>, ConfigError> {
        let effective = self.effective_config(name, overrides)?;
        let resolved = self
            .resolve_secrets(&effective)
            .map_err(|e| ConfigError::SecretReveal {
                detail: e.to_string(),
            })?;
        Ok(resolved
            .into_iter()
            .map(|(key, secret)| (key, secret.expose_secret().to_string()))
            .collect())
    }

    /// Confirm an instance is registered, returning it (for its kind) — mapping
    /// absence/read failure into a [`ConfigError`] so the config surface never
    /// leaks a `RegistryError` across the AD-1 boundary.
    fn require_instance(&self, name: &InstanceName) -> Result<AgentInstance, ConfigError> {
        match self.store.get_instance(name) {
            Ok(Some(instance)) => Ok(instance),
            Ok(None) => Err(ConfigError::NotFound {
                name: name.as_str().to_string(),
            }),
            Err(e) => Err(ConfigError::Store {
                name: name.as_str().to_string(),
                detail: e.to_string(),
            }),
        }
    }

    /// Load + parse an instance's `config.toml` (the AD-9 INSTANCE layer) through
    /// path authority. A missing file resolves to an EMPTY layer (an instance
    /// whose config was removed still resolves to defaults, not an error); a
    /// present-but-malformed file surfaces [`ConfigError::MalformedLayer`] naming
    /// the path (never a panic — AC8).
    fn instance_config_layer(&self, name: &InstanceName) -> Result<ConfigLayer, ConfigError> {
        let path = self.paths.instance_config(name);
        match std::fs::read_to_string(&path) {
            Ok(text) => ConfigLayer::parse(SourceLayer::Instance, &path.to_string_lossy(), &text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConfigLayer::empty()),
            Err(e) => Err(ConfigError::MalformedLayer {
                layer: SourceLayer::Instance,
                path: path.to_string_lossy().into_owned(),
                detail: format!("could not read the instance config: {e}"),
            }),
        }
    }

    /// The per-instance log directory inside the Agent Home (AD-12 seed).
    pub(crate) fn instance_log_dir(&self, name: &InstanceName) -> std::path::PathBuf {
        self.paths.agent_home(name).join("logs")
    }

    /// The per-instance ENGINE log FILE — the JSON-Lines transition-event log
    /// (AD-12/AD-14 seed). Engine-owned; distinct from the agent's own
    /// stdout/stderr capture so the two never interleave.
    pub(crate) fn instance_log_path(&self, name: &InstanceName) -> std::path::PathBuf {
        self.instance_log_dir(name).join("instance.log")
    }

    /// The per-instance AGENT output log FILE — the spawned process's STDOUT
    /// capture (AD-12 seed). Kept separate from the engine event log.
    ///
    /// Fix pass (review of #80): this is now a DIRECT, crash-immune
    /// redirect target for the child's stdout ALONE (`Stdio::from(file)`) —
    /// the write path Epic 3's `drain_usage_for` depends on for its
    /// billing-critical `KTESIO_USAGE` sentinel read, which is a
    /// stdout-only convention (`docs/manifest.md`/`docs/architecture.md`).
    /// Stderr is captured SEPARATELY (see
    /// [`Self::agent_stderr_log_path`]) rather than merged back into this
    /// file, so this file's write path never depends on any engine-side
    /// hop (thread, tailer, or otherwise) — only the OS and the agent
    /// process itself.
    pub(crate) fn agent_output_log_path(&self, name: &InstanceName) -> std::path::PathBuf {
        self.instance_log_dir(name).join("agent.log")
    }

    /// The per-instance RAW STDERR capture FILE (fix pass, review of #80) —
    /// the crash-immune counterpart of [`Self::agent_output_log_path`] for
    /// the child's stderr: a DIRECT redirect target (`Stdio::from(file)`),
    /// never a live pipe, so the agent's stderr writes never depend on the
    /// engine's liveness either. Feeds the attributed capture's background
    /// tailer (`agent-err` lines); NOT merged into the legacy `agent.log`
    /// (which stays stdout-only — see that path's docs for why).
    pub(crate) fn agent_stderr_log_path(&self, name: &InstanceName) -> std::path::PathBuf {
        self.instance_log_dir(name).join("agent-stderr.log")
    }

    /// The per-instance BUDGET-BREACH log FILE — the JSON-Lines
    /// [`crate::domain::BudgetBreachEvent`] log (story 3-2, AD-14). Engine-owned,
    /// SEPARATE from the transition log so the ALWAYS-recorded breach event is a
    /// durable fact independent of any lifecycle transition (FR-21 "breaches are
    /// always recorded as events regardless of action") — a `warn` breach (no
    /// transition) still lands here, and a breach whose pause/stop fails is still
    /// recorded here. Full subscription delivery is 7-2's; this is the durable
    /// record + the seed the future bus reads.
    pub(crate) fn instance_breach_log_path(&self, name: &InstanceName) -> std::path::PathBuf {
        self.instance_log_dir(name).join("breaches.log")
    }

    /// The per-instance ATTRIBUTED, ROTATED output-capture FILE — the
    /// CURRENT generation (story 4-2, AD-12). A timestamped, per-stream-
    /// attributed (`agent-out`/`agent-err`/`engine`) JSON-Lines view of the
    /// spawned process's output, distinct from [`Self::agent_output_log_path`]
    /// (`agent.log`): that legacy file stays raw, unattributed, and
    /// BYTE-IDENTICAL to before this story (CRITICAL SCOPING #3 — Epic 3's
    /// `drain_usage_for` keeps reading it exactly as today). Bounded to
    /// [`crate::ports::LOG_ROTATE_MAX_BYTES`] per generation before rotating
    /// (see [`Self::attributed_output_log_generation_path`]).
    pub(crate) fn attributed_output_log_path(&self, name: &InstanceName) -> std::path::PathBuf {
        self.instance_log_dir(name).join("output.log")
    }

    /// A ROTATED generation of the attributed output log (story 4-2, AD-12) —
    /// `generation` `1` is the most-recently-rotated predecessor, `2` the
    /// oldest still retained (AD-12's fixed "10MB × 3": current +
    /// [`Self::attributed_output_log_path`] + generations `1` and `2`).
    /// Mirrors `ports::process_backend`'s internal rotation naming exactly
    /// (`output.log.<n>`) so the read side and the write side agree on
    /// every generation's path without either depending on the other.
    pub(crate) fn attributed_output_log_generation_path(
        &self,
        name: &InstanceName,
        generation: u8,
    ) -> std::path::PathBuf {
        self.instance_log_dir(name)
            .join(format!("output.log.{generation}"))
    }

    /// Count Usage Ledger events for an instance (Epic 1's empty-ledger proof;
    /// story 3-1 populates the table so this returns a real count).
    pub fn usage_event_count(&self, name: &InstanceName) -> Result<u64, RegistryError> {
        Ok(self.store.count_usage_events(name)?)
    }

    // ---- Usage Ledger writes + reads (story 3-1, spine AD-6/AD-7) ----
    //
    // Thin `pub(crate)` pass-throughs to the store's Usage-Ledger methods (same
    // pattern as `set_state` → `store.set_state`). The commit choke point in the
    // supervisor is the SOLE caller of `record_usage_event` (the AD-7 single-writer
    // invariant); the Fleet read is the sole caller of the total reads.

    /// Append ONE [`UsageEvent`] to the Usage Ledger in its own transaction (AD-6),
    /// returning whether it was inserted or was a recognized replay (AC-A dedup).
    /// The supervisor's ingestion choke point is the only caller.
    pub(crate) fn record_usage_event(
        &self,
        event: &crate::domain::UsageEvent,
        rate: Option<crate::domain::Rate>,
    ) -> Result<crate::domain::RecordOutcome, RegistryError> {
        Ok(self.store.record_usage_event(event, rate)?)
    }

    /// The committed [`UsageEvent`] ROWS for an instance in commit order
    /// (story 10-3: the resync helper's ledger read) — one struct per
    /// `usage_events` row, `rowid`-ordered (insertion order). An absent
    /// instance / empty ledger reads as an empty vec.
    pub(crate) fn usage_rows(
        &self,
        name: &InstanceName,
    ) -> Result<Vec<crate::domain::UsageEvent>, RegistryError> {
        Ok(self.store.usage_events(name)?)
    }

    /// The CUMULATIVE token totals for an instance (sum over all its Runs) — the
    /// Fleet-detail `usage` read (AC-C/AC11). An absent instance totals zero.
    pub(crate) fn usage_totals(
        &self,
        name: &InstanceName,
    ) -> Result<crate::domain::UsageTotals, RegistryError> {
        Ok(self.store.usage_totals(name)?)
    }

    /// The PER-RUN token totals for an instance scoped to one Run (AC-B). An absent
    /// instance / unknown Run totals zero.
    pub(crate) fn run_usage_totals(
        &self,
        name: &InstanceName,
        run_id: &crate::domain::RunId,
    ) -> Result<crate::domain::UsageTotals, RegistryError> {
        Ok(self.store.run_usage_totals(name, run_id)?)
    }

    /// The CUMULATIVE derived DOLLAR cost for an instance (story 3-3, AC-A) — each
    /// row priced at its own persisted Rate (no retro-repricing). An absent instance
    /// totals `$0`.
    pub(crate) fn cost_totals(
        &self,
        name: &InstanceName,
    ) -> Result<crate::domain::Micros, RegistryError> {
        Ok(self.store.cost_totals(name)?)
    }

    /// The PER-RUN derived DOLLAR cost for an instance scoped to one Run (story
    /// 3-3). An absent instance / unknown Run totals `$0`.
    pub(crate) fn run_cost_totals(
        &self,
        name: &InstanceName,
        run_id: &crate::domain::RunId,
    ) -> Result<crate::domain::Micros, RegistryError> {
        Ok(self.store.run_cost_totals(name, run_id)?)
    }

    /// Test-only escape hatch to seed an instance in an arbitrary Lifecycle
    /// State (e.g. `running`) so the AC5 guard can be exercised without a real
    /// supervision core (which does not exist until story 1.4).
    #[cfg(test)]
    pub(crate) fn seed_instance(&self, instance: &AgentInstance) -> Result<(), RegistryError> {
        self.store.create_instance(instance)?;
        Ok(())
    }

    /// Test-only accessor to the resolved paths for assertions.
    #[cfg(test)]
    pub(crate) fn paths(&self) -> &EnginePaths {
        &self.paths
    }

    /// The engine STATE DB path — the `usage_events` ledger's home. Names the
    /// ledger family's record location in errors that need it (the story-10-3
    /// resync helper's truncation guard).
    pub(crate) fn state_db_path(&self) -> std::path::PathBuf {
        self.paths.state_db()
    }

    // ---- Memory Backing collaboration surface (story 5-1; crate-internal) ----

    /// The attached [`MemoryBacking`] for an instance, or `None` — read by the
    /// supervisor's start path (in its pre-transition block) to build the
    /// descriptor override + run the DC-10 honesty check.
    pub(crate) fn memory_backing(
        &self,
        name: &InstanceName,
    ) -> Result<Option<MemoryBacking>, RegistryError> {
        Ok(self.store.get_memory_backing(name)?)
    }

    /// The managed memory directory through path authority (the supervisor's
    /// defensive start-time self-heal target).
    pub(crate) fn agent_memory_dir(&self, name: &InstanceName) -> std::path::PathBuf {
        self.paths.agent_memory_dir(name)
    }
}

/// Create `dir` (and parents) if absent, mapping failures to [`RegistryError::Io`].
fn ensure_dir(dir: &Path, name: &str) -> Result<(), RegistryError> {
    std::fs::create_dir_all(dir).map_err(|source| RegistryError::Io {
        name: name.to_string(),
        path: dir.to_string_lossy().into_owned(),
        source,
    })
}

/// Create the MANAGED memory directory, refusing to follow a pre-existing
/// symlink at that path (story 5-1 review).
///
/// The managed directory is an engine-authority artifact INSIDE the Agent Home;
/// if `<home>/memory` already exists as a symlink, creating "through" it would
/// aim every managed write — and the path delivered to the agent — wherever the
/// link points, escaping the home's containment. Unlike [`ensure_dir`] (shared
/// with the state dir and Agent Home roots, which an operator may legitimately
/// place on another volume via a symlink), this path is engine-owned, so the
/// refusal costs nothing legitimate.
fn ensure_managed_memory_dir(dir: &Path, name: &str) -> Result<(), RegistryError> {
    if let Ok(meta) = std::fs::symlink_metadata(dir) {
        if meta.is_symlink() {
            return Err(RegistryError::Io {
                name: name.to_string(),
                path: dir.to_string_lossy().into_owned(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "the managed memory directory path is a symlink; refusing to follow it \
                     (remove the symlink so the engine can own this path)",
                ),
            });
        }
    }
    ensure_dir(dir, name)
}

/// The Memory Backing terminal-state guard (story 5-1, A-5): attach/detach are
/// permitted ONLY from a TERMINAL persisted state (`registered`/`stopped`/
/// `failed`) and refused in every non-terminal one (`running`, `starting`,
/// `stopping`, `paused`). Pure state-machine validation from the persisted row —
/// no live process is consulted (DC-3), so it is deterministically testable via
/// seeded rows exactly like `remove`'s running-guard.
fn ensure_terminal_for_memory_backing(
    name: &InstanceName,
    state: &LifecycleState,
) -> Result<(), RegistryError> {
    match state {
        LifecycleState::Registered | LifecycleState::Stopped | LifecycleState::Failed => Ok(()),
        other => Err(RegistryError::MemoryBackingHotSwap {
            name: name.as_str().to_string(),
            state: other.as_str().to_string(),
        }),
    }
}

/// Parse the embedded engine-defaults TOML into the AD-9 ENGINE-DEFAULTS layer
/// (story 2-1, Decision 1). A parse failure is an ENGINE BUG (the source is a
/// compile-time [`ENGINE_DEFAULTS_TOML`] constant), surfaced as a typed
/// [`ConfigError::MalformedLayer`] rather than a panic.
fn engine_defaults_layer() -> Result<ConfigLayer, ConfigError> {
    ConfigLayer::parse(
        SourceLayer::EngineDefault,
        ENGINE_DEFAULTS_LABEL,
        ENGINE_DEFAULTS_TOML,
    )
}

/// The AD-9 KIND-DEFAULTS layer for a `kind` (story 2-1, Decision 2, recorded).
///
/// `[ASSUMPTION]`: in story 2-1 NO kind ships config defaults — the adapter
/// contract does not yet carry a config-defaults document (that is 2-2+
/// territory, FR-12). So EVERY kind (including `mock`) resolves to an EMPTY layer,
/// a valid "no defaults" layer and NOT an error (AC8). The `kind` is passed in
/// (already looked up by the caller) so the seam is real — a later story maps
/// `kind` → real per-kind defaults here WITHOUT changing `effective_config`. Pure
/// (no I/O), so it adds no error branch.
fn kind_defaults_layer(_kind: &str) -> ConfigLayer {
    ConfigLayer::empty()
}

/// Set a DOTTED key (`a.b.c`) into a TOML table, creating intermediate tables as
/// needed and preserving existing siblings (the per-leaf write that mirrors the
/// resolver's per-leaf merge — AC-B/AC4). The leaf segment is overwritten with
/// `value`.
///
/// FAILS CLOSED (review patch #3): if an intermediate segment currently holds a
/// NON-table SCALAR value (e.g. `set X agent.a.b` after `agent.a` is a scalar),
/// this returns [`ConfigError::WriteShapeConflict`] naming the conflicting
/// ancestor and mutates NOTHING — accepting it would silently destroy the
/// existing scalar. The caller runs this on an in-memory clone BEFORE writing, so
/// the error leaves the on-disk config byte-unchanged (AC-B atomicity). This is a
/// WRITE-time rule; the READ-time resolver still masks such collisions across
/// layers (a strong scalar prunes a weak subtree) — but a single instance-layer
/// write must never clobber its own existing value.
fn set_dotted(
    table: &mut toml::value::Table,
    dotted_key: &str,
    value: toml::Value,
) -> Result<(), ConfigError> {
    let mut segments = dotted_key.split('.').peekable();
    let mut current = table;
    // Track the dotted path walked so far, to name the conflicting ancestor.
    let mut walked = String::new();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            // Leaf: overwrite (or insert) the value at this exact key.
            current.insert(segment.to_string(), value);
            return Ok(());
        }
        if walked.is_empty() {
            walked.push_str(segment);
        } else {
            walked.push('.');
            walked.push_str(segment);
        }
        // Intermediate: descend, creating a table node if absent.
        let entry = current
            .entry(segment.to_string())
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
        // FAIL CLOSED on a scalar intermediate — do NOT clobber it.
        if !entry.is_table() {
            return Err(ConfigError::WriteShapeConflict {
                key: dotted_key.to_string(),
                conflicting_ancestor: walked,
            });
        }
        current = entry.as_table_mut().expect("just ensured a table");
    }
    // Unreachable: an empty key is caught by validate_write's empty-segment guard
    // before set_dotted is called, so the leaf branch above always returns.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_temp() -> (TempDir, Registry) {
        let tmp = TempDir::new().unwrap();
        let reg = Registry::open(Some(tmp.path().to_path_buf())).unwrap();
        (tmp, reg)
    }

    #[test]
    fn register_happy_path_creates_home_config_row_and_empty_ledger() {
        let (_tmp, reg) = open_temp();
        let instance = reg.register("demo", "mock").unwrap();
        assert_eq!(instance.state, LifecycleState::Registered);
        assert_eq!(instance.kind, "mock");

        // Agent Home + config file exist.
        let name = InstanceName::new("demo").unwrap();
        let home = reg.paths().agent_home(&name);
        assert!(home.is_dir(), "home dir should exist");
        let config = reg.paths().instance_config(&name);
        assert!(config.is_file(), "config.toml should exist");
        let body = std::fs::read_to_string(&config).unwrap();
        assert!(body.contains("name = \"demo\""));

        // agent_home reported is the engine-computed path, and the public
        // agent_home() helper agrees with it (path-authority display helper).
        assert_eq!(instance.agent_home, home.to_string_lossy());
        assert_eq!(reg.agent_home(&name), home);

        // Row present, ledger empty.
        assert!(reg
            .list()
            .unwrap()
            .iter()
            .any(|i| i.name.as_str() == "demo"));
        assert_eq!(reg.usage_event_count(&name).unwrap(), 0);
    }

    #[test]
    fn register_invalid_name_rejected_without_touching_disk() {
        let (_tmp, reg) = open_temp();
        let err = reg.register("Bad Name", "mock").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidName { .. }));
        // No agents dir entries created.
        let agents = reg.paths().agents_dir();
        assert!(!agents.join("Bad Name").exists());
        assert!(reg.list().unwrap().is_empty());
    }

    #[test]
    fn duplicate_registration_leaves_no_partial_state() {
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let home = reg.paths().agent_home(&InstanceName::new("demo").unwrap());
        let config_before = std::fs::read_to_string(
            reg.paths()
                .instance_config(&InstanceName::new("demo").unwrap()),
        )
        .unwrap();

        // Re-register the same NAME (kind must still resolve, so reuse `mock`);
        // the UNIQUE-name constraint yields DuplicateName after adapter
        // resolution passes.
        let err = reg.register("demo", "mock").unwrap_err();
        assert!(matches!(err, RegistryError::DuplicateName { name } if name == "demo"));

        // The original is untouched; exactly one row; config byte-identical.
        assert_eq!(reg.list().unwrap().len(), 1);
        assert!(home.is_dir());
        let config_after = std::fs::read_to_string(
            reg.paths()
                .instance_config(&InstanceName::new("demo").unwrap()),
        )
        .unwrap();
        assert_eq!(config_before, config_after);
    }

    #[test]
    fn two_instances_same_kind_get_disjoint_homes() {
        // AC3: two instances of the same kind get distinct, independent homes.
        let (_tmp, reg) = open_temp();
        reg.register("alpha", "mock").unwrap();
        reg.register("beta", "mock").unwrap();
        let alpha_home = reg.paths().agent_home(&InstanceName::new("alpha").unwrap());
        let beta_home = reg.paths().agent_home(&InstanceName::new("beta").unwrap());
        assert_ne!(alpha_home, beta_home);
        assert!(alpha_home.is_dir() && beta_home.is_dir());

        // Writing into alpha's home leaves beta's config byte-unchanged.
        let beta_config = reg
            .paths()
            .instance_config(&InstanceName::new("beta").unwrap());
        let beta_before = std::fs::read(&beta_config).unwrap();
        std::fs::write(alpha_home.join("scratch.txt"), b"hello").unwrap();
        let beta_after = std::fs::read(&beta_config).unwrap();
        assert_eq!(beta_before, beta_after);
    }

    #[test]
    fn remove_retain_keeps_home_and_deletes_row() {
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let home = reg.paths().agent_home(&InstanceName::new("demo").unwrap());
        reg.remove("demo", RemoveDisposition::Retain, false)
            .unwrap();
        assert!(home.is_dir(), "retain leaves the home on disk");
        assert!(reg.list().unwrap().is_empty(), "row is gone");
    }

    #[test]
    fn remove_delete_removes_home_and_row() {
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let home = reg.paths().agent_home(&InstanceName::new("demo").unwrap());
        reg.remove("demo", RemoveDisposition::Delete, false)
            .unwrap();
        assert!(!home.exists(), "delete removes the home tree");
        assert!(reg.list().unwrap().is_empty());
    }

    #[test]
    fn remove_delete_isolation_other_home_byte_identical() {
        // AC4 isolation proof: removing one instance leaves every other Agent
        // Home byte-identical.
        let (_tmp, reg) = open_temp();
        reg.register("keep", "mock").unwrap();
        reg.register("drop", "mock").unwrap();
        let keep_config = reg
            .paths()
            .instance_config(&InstanceName::new("keep").unwrap());
        let before = std::fs::read(&keep_config).unwrap();

        reg.remove("drop", RemoveDisposition::Delete, false)
            .unwrap();

        let after = std::fs::read(&keep_config).unwrap();
        assert_eq!(
            before, after,
            "other home must be byte-identical after removal"
        );
        let drop_home = reg.paths().agent_home(&InstanceName::new("drop").unwrap());
        assert!(!drop_home.exists());
    }

    #[test]
    fn remove_missing_returns_not_found() {
        let (_tmp, reg) = open_temp();
        let err = reg
            .remove("ghost", RemoveDisposition::Delete, false)
            .unwrap_err();
        assert!(matches!(err, RegistryError::NotFound { name } if name == "ghost"));
    }

    #[test]
    fn remove_invalid_name_returns_invalid_name() {
        let (_tmp, reg) = open_temp();
        let err = reg
            .remove("Bad Name", RemoveDisposition::Delete, false)
            .unwrap_err();
        assert!(matches!(err, RegistryError::InvalidName { .. }));
    }

    #[test]
    fn remove_running_without_force_is_rejected() {
        // AC5: seed a running instance directly (no supervision core yet) and
        // confirm the guard fires without --force.
        let (_tmp, reg) = open_temp();
        let now = now_rfc3339();
        let running = AgentInstance {
            name: InstanceName::new("live").unwrap(),
            kind: "mock".to_string(),
            state: LifecycleState::Running,
            agent_home: reg
                .paths()
                .agent_home(&InstanceName::new("live").unwrap())
                .to_string_lossy()
                .into_owned(),
            created_at: now.clone(),
            updated_at: now,
        };
        reg.seed_instance(&running).unwrap();

        let err = reg
            .remove("live", RemoveDisposition::Delete, false)
            .unwrap_err();
        assert!(matches!(err, RegistryError::RunningRequiresForce { name } if name == "live"));
        // Still present (removal refused).
        assert_eq!(reg.list().unwrap().len(), 1);
    }

    #[test]
    fn remove_running_with_force_succeeds() {
        // AC5: --force bypasses the running-guard.
        let (_tmp, reg) = open_temp();
        let now = now_rfc3339();
        let running = AgentInstance {
            name: InstanceName::new("live").unwrap(),
            kind: "mock".to_string(),
            state: LifecycleState::Running,
            agent_home: reg
                .paths()
                .agent_home(&InstanceName::new("live").unwrap())
                .to_string_lossy()
                .into_owned(),
            created_at: now.clone(),
            updated_at: now,
        };
        reg.seed_instance(&running).unwrap();

        reg.remove("live", RemoveDisposition::Retain, true).unwrap();
        assert!(reg.list().unwrap().is_empty());
    }

    #[test]
    fn register_rolls_back_row_when_home_cannot_be_created() {
        // Force materialize_home to fail by pre-creating a *file* where the
        // agents dir must be a directory, so create_dir_all errors.
        let tmp = TempDir::new().unwrap();
        let reg = Registry::open(Some(tmp.path().to_path_buf())).unwrap();
        // Place a regular file at <base>/agents so create_dir_all(<base>/agents/demo) fails.
        let agents = reg.paths().agents_dir();
        std::fs::write(&agents, b"not a dir").unwrap();

        let err = reg.register("demo", "mock").unwrap_err();
        assert!(matches!(err, RegistryError::Io { .. }), "got {err:?}");
        // Row must have been rolled back — the Fleet is empty.
        assert!(
            reg.list().unwrap().is_empty(),
            "row should be rolled back after home creation failure"
        );
    }

    #[test]
    fn register_rolls_back_when_config_file_cannot_be_written() {
        // Force the config *write* (not the dir creation) to fail by placing a
        // directory where config.toml must be a file. Since story 11-2
        // (review-1 patch 5) the config write is ATOMIC (temp + rename), so the
        // injected failure is the RENAME failing onto the occupied path: the
        // helper's own temp is cleaned up, the typed Io branch + its rollback
        // run exactly as before, and the rollback sweeps the whole home — no
        // partial config, no temp litter, nothing partial left in agents/.
        let tmp = TempDir::new().unwrap();
        let reg = Registry::open(Some(tmp.path().to_path_buf())).unwrap();
        let name = InstanceName::new("demo").unwrap();
        // Pre-create <base>/agents/demo/config.toml AS A DIRECTORY.
        let config_as_dir = reg.paths().instance_config(&name);
        std::fs::create_dir_all(&config_as_dir).unwrap();

        let err = reg.register("demo", "mock").unwrap_err();
        match err {
            RegistryError::Io { name: n, path, .. } => {
                assert_eq!(n, "demo");
                assert!(
                    path.ends_with("config.toml"),
                    "the error must name the config write; path={path}"
                );
            }
            other => panic!("expected Io, got {other:?}"),
        }
        // Row rolled back.
        assert!(reg.list().unwrap().is_empty());
        assert!(matches!(
            reg.lookup(&name),
            Err(RegistryError::NotFound { .. })
        ));
        // Nothing partial left behind: the rolled-back home is gone entirely —
        // no half-written config.toml, no `.tmp-*` residue anywhere under it.
        assert!(
            !reg.paths().agent_home(&name).exists(),
            "the registration rollback must remove the partial home"
        );
    }

    #[test]
    fn register_orphan_row_when_rollback_delete_also_fails() {
        // F2 compound failure: materialize_home fails (file at the agents dir)
        // AND the rollback delete fails (delete-blocking trigger). The result
        // must be the distinct RegisterOrphanRow error naming the leaked row,
        // not the bare Io error (which would hide the orphaned row).
        let tmp = TempDir::new().unwrap();
        let reg = Registry::open(Some(tmp.path().to_path_buf())).unwrap();
        // (a) make materialize_home fail: a regular file where agents/ must be.
        let agents = reg.paths().agents_dir();
        std::fs::write(&agents, b"not a dir").unwrap();
        // (b) make the compensating delete fail deterministically.
        reg.store.break_deletes_for_test();

        let err = reg.register("demo", "mock").unwrap_err();
        match err {
            RegistryError::RegisterOrphanRow {
                name,
                home_error,
                rollback_error,
            } => {
                assert_eq!(name, "demo");
                assert!(!home_error.is_empty());
                assert!(!rollback_error.is_empty());
            }
            other => panic!("expected RegisterOrphanRow, got {other:?}"),
        }
        // The orphan row is indeed still present (the delete was blocked), which
        // is exactly the partial-failure state the error names for the operator.
        assert_eq!(reg.list().unwrap().len(), 1);
    }

    #[test]
    fn open_maps_path_resolution_failure_and_names_the_base() {
        // F8 + AI-4: when EnginePaths::new fails (here: a relative
        // KTESIO_STATE_DIR), Registry::open must surface RegistryError::Io whose
        // `path` names the OFFENDING VALUE the operator supplied — not blank and
        // not the `<default via KTESIO_STATE_DIR>` placeholder, which would hide
        // the actual path that failed. Save/restore the shared env.
        const OFFENDING: &str = "relative/base";
        let prev = std::env::var_os(crate::paths::STATE_DIR_ENV);
        std::env::set_var(crate::paths::STATE_DIR_ENV, OFFENDING);
        let result = Registry::open(None);
        std::env::set_var(crate::paths::STATE_DIR_ENV, ""); // neutralize before restore
        match prev {
            Some(v) => std::env::set_var(crate::paths::STATE_DIR_ENV, v),
            None => std::env::remove_var(crate::paths::STATE_DIR_ENV),
        }
        // (Registry has no Debug impl, so inspect the Err arm without binding
        // the Ok value into a formatted panic.)
        let err = match result {
            Ok(_) => panic!("expected Io from a relative env base, got Ok"),
            Err(e) => e,
        };
        match err {
            RegistryError::Io { path, .. } => {
                // AI-4: the diagnostic names the actual relative path that was
                // rejected (the source error still carries the full context).
                assert_eq!(path, OFFENDING, "path must name the offending value");
            }
            other => panic!("expected Io from a relative env base, got {other:?}"),
        }
    }

    #[test]
    fn remove_delete_succeeds_when_home_already_gone() {
        // F6 TOCTOU: if the Agent Home is already absent, a Delete removal must
        // succeed (desired end state already holds) rather than fail with
        // RemoveLeftoverHome. Register, delete the home out from under the
        // engine, then remove --delete.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let home = reg.paths().agent_home(&InstanceName::new("demo").unwrap());
        std::fs::remove_dir_all(&home).unwrap();
        assert!(!home.exists());

        // Must succeed: the home is already gone.
        reg.remove("demo", RemoveDisposition::Delete, false)
            .unwrap();
        assert!(reg.list().unwrap().is_empty(), "row is gone");
        assert!(!home.exists());
    }

    #[test]
    fn remove_delete_reports_leftover_home_on_failure() {
        // Exercise the RemoveLeftoverHome branch portably: register, then
        // replace the Agent Home directory with a regular *file*. A Delete
        // removal deletes the row, then calls remove_dir_all on a path that
        // exists but is not a directory, which errors -> RemoveLeftoverHome
        // (naming the leftover path, NFR-1).
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let home = reg.paths().agent_home(&InstanceName::new("demo").unwrap());
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::write(&home, b"now a file").unwrap();

        let err = reg
            .remove("demo", RemoveDisposition::Delete, false)
            .unwrap_err();
        assert!(
            matches!(&err, RegistryError::RemoveLeftoverHome { name, .. } if name == "demo"),
            "got {err:?}"
        );
        // Row was still deleted (removal proceeds past the row).
        assert!(reg.list().unwrap().is_empty());
    }

    // ---- Story 1.3: adapter resolution + validation at registration ----

    const VALID_MANIFEST: &str = r#"
contract_version = "1.0.0"

[adapter]
kind = "demo-manifest"

[lifecycle.start]
exec = "demo-agent"

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[metering]
source = "self-reported"
"#;

    fn write_manifest_dir(dir: &std::path::Path, body: &str) {
        std::fs::write(dir.join(crate::adapter::MANIFEST_FILE), body).unwrap();
    }

    #[test]
    fn register_native_mock_persists_effective_declaration() {
        // AC1: the mock kind resolves; the effective (current-OS) declaration is
        // persisted and re-readable via effective_capabilities.
        let (_tmp, reg) = open_temp();
        let instance = reg.register("demo", "mock").unwrap();
        assert_eq!(instance.kind, "mock");

        // The adapter snapshot exists in the home.
        let name = InstanceName::new("demo").unwrap();
        let snap = reg.paths().agent_home(&name).join(ADAPTER_SNAPSHOT_FILE);
        assert!(snap.is_file(), "adapter.json snapshot should exist");

        let eff = reg.effective_capabilities("demo").unwrap();
        assert_eq!(eff.os, OsId::current());
        // The mock declares pause + interaction, so both project onto this OS.
        assert_eq!(eff.entries.len(), 2);
    }

    #[test]
    fn register_unknown_kind_leaves_no_partial_state() {
        // AC2 + atomicity: an unknown native kind is rejected BEFORE any row or
        // home is created.
        let (_tmp, reg) = open_temp();
        let err = reg.register("demo", "does-not-exist").unwrap_err();
        assert!(
            matches!(&err, RegistryError::UnknownAdapterKind { kind } if kind == "does-not-exist"),
            "got {err:?}"
        );
        assert!(reg.list().unwrap().is_empty(), "no row on rejected adapter");
        let home = reg.paths().agent_home(&InstanceName::new("demo").unwrap());
        assert!(!home.exists(), "no home on rejected adapter");
    }

    #[test]
    fn register_manifest_from_dir_succeeds_and_records_effective_declaration() {
        // AC1: register a manifest adapter from a temp dir; read back the
        // effective declaration.
        let (_tmp, reg) = open_temp();
        let manifest_dir = TempDir::new().unwrap();
        write_manifest_dir(manifest_dir.path(), VALID_MANIFEST);

        let instance = reg
            .register_with_adapter(
                "m",
                &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
            )
            .unwrap();
        assert_eq!(instance.kind, "demo-manifest");

        let eff = reg.effective_capabilities("m").unwrap();
        assert_eq!(eff.os, OsId::current());
        assert!(!eff.is_empty());

        // The snapshot records the manifest path for 1.4.
        let name = InstanceName::new("m").unwrap();
        let snap_text =
            std::fs::read_to_string(reg.paths().agent_home(&name).join(ADAPTER_SNAPSHOT_FILE))
                .unwrap();
        assert!(
            snap_text.contains(crate::adapter::MANIFEST_FILE),
            "snapshot should record the manifest path: {snap_text}"
        );
    }

    #[test]
    fn register_manifest_not_found_leaves_no_partial_state() {
        let (_tmp, reg) = open_temp();
        let empty = TempDir::new().unwrap();
        let err = reg
            .register_with_adapter("m", &AdapterRef::Manifest(empty.path().to_path_buf()))
            .unwrap_err();
        assert!(
            matches!(&err, RegistryError::ManifestNotFound { path } if path.ends_with(crate::adapter::MANIFEST_FILE)),
            "got {err:?}"
        );
        assert!(reg.list().unwrap().is_empty());
    }

    #[test]
    fn register_manifest_invalid_names_the_section_and_leaves_no_partial_state() {
        // AC2: a manifest missing [capabilities] is rejected naming the section,
        // with zero partial state.
        let (_tmp, reg) = open_temp();
        let manifest_dir = TempDir::new().unwrap();
        let body = VALID_MANIFEST.replace(
            "[capabilities.pause]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"best-effort\"\n",
            "",
        );
        write_manifest_dir(manifest_dir.path(), &body);

        let err = reg
            .register_with_adapter(
                "m",
                &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
            )
            .unwrap_err();
        match &err {
            RegistryError::ManifestInvalid { detail, .. } => {
                assert!(detail.contains("[capabilities]"), "detail={detail}")
            }
            other => panic!("expected ManifestInvalid, got {other:?}"),
        }
        assert!(reg.list().unwrap().is_empty());
        let home = reg.paths().agent_home(&InstanceName::new("m").unwrap());
        assert!(!home.exists());
    }

    #[test]
    fn register_manifest_no_metering_is_rejected_ac4() {
        // AC4: a manifest with no [metering] section is rejected at
        // registration; no "register anyway" path. The diagnostic names
        // [metering], and nothing is written.
        let (_tmp, reg) = open_temp();
        let manifest_dir = TempDir::new().unwrap();
        let body = VALID_MANIFEST.replace("[metering]\nsource = \"self-reported\"\n", "");
        write_manifest_dir(manifest_dir.path(), &body);

        let err = reg
            .register_with_adapter(
                "m",
                &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("[metering]"),
            "AC4 diagnostic must name [metering]: {err}"
        );
        assert!(reg.list().unwrap().is_empty());
    }

    #[test]
    fn effective_capabilities_unknown_instance_is_not_found() {
        let (_tmp, reg) = open_temp();
        let err = reg.effective_capabilities("ghost").unwrap_err();
        assert!(matches!(&err, RegistryError::NotFound { name } if name == "ghost"));
    }

    #[test]
    fn effective_capabilities_invalid_name_is_invalid_name() {
        let (_tmp, reg) = open_temp();
        let err = reg.effective_capabilities("Bad Name").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidName { .. }));
    }

    #[test]
    fn effective_capabilities_missing_snapshot_reports_io() {
        // A registered instance whose snapshot file was removed (corrupt home)
        // surfaces an Io error naming the missing snapshot path, not a panic.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        let snap = reg.paths().agent_home(&name).join(ADAPTER_SNAPSHOT_FILE);
        std::fs::remove_file(&snap).unwrap();

        let err = reg.effective_capabilities("demo").unwrap_err();
        assert!(
            matches!(&err, RegistryError::Io { path, .. } if path.ends_with(ADAPTER_SNAPSHOT_FILE)),
            "got {err:?}"
        );
    }

    #[test]
    fn effective_capabilities_corrupt_snapshot_reports_io() {
        // A snapshot that is not valid JSON surfaces an Io(InvalidData) error
        // rather than panicking.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        let snap = reg.paths().agent_home(&name).join(ADAPTER_SNAPSHOT_FILE);
        std::fs::write(&snap, b"{ not valid json").unwrap();

        let err = reg.effective_capabilities("demo").unwrap_err();
        assert!(matches!(err, RegistryError::Io { .. }), "got {err:?}");
    }

    #[test]
    fn adapter_snapshot_round_trips_through_json() {
        // F3: the persisted snapshot carries the FULL per-OS declaration and
        // round-trips (kind, metering source, manifest path, declaration).
        use ktesio_adapter_api::{Capability, SupportLevel};
        let declaration = CapabilityDeclaration::new()
            .with(Capability::Pause, OsId::Linux, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Windows, SupportLevel::BestEffort)
            .with(
                Capability::Interaction,
                OsId::Macos,
                SupportLevel::Guaranteed,
            );
        // The resolved `start` launch is snapshotted alongside the declaration.
        let launch = StartLaunch {
            exec: "the-agent".to_string(),
            args: vec!["--serve".to_string(), "--port".to_string(), "0".to_string()],
            env: std::collections::BTreeMap::from([("MODE".to_string(), "test".to_string())]),
        };
        let snapshot = AdapterSnapshot {
            kind: "demo".to_string(),
            metering_source: "self-reported".to_string(),
            manifest_path: Some("/some/adapter.toml".to_string()),
            declaration: declaration.clone(),
            // Persisted through the DTO (the live StartLaunch is non-Serialize).
            launch: Some(LaunchSnapshot::from(&launch)),
        };
        let json = serde_json::to_string_pretty(&snapshot).unwrap();
        let back: AdapterSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, "demo");
        assert_eq!(back.metering_source, "self-reported");
        assert_eq!(back.manifest_path.as_deref(), Some("/some/adapter.toml"));
        // The full declaration survives, all three OS entries intact.
        assert_eq!(back.declaration, declaration);
        assert_eq!(
            back.declaration.support(Capability::Pause, OsId::Windows),
            SupportLevel::BestEffort
        );
        // The launch (exec + args + env) survives the full StartLaunch → DTO →
        // JSON → DTO → StartLaunch round-trip.
        assert_eq!(back.launch.map(StartLaunch::from), Some(launch));
    }

    #[test]
    fn adapter_snapshot_without_launch_field_deserializes_to_none() {
        // Backward compat: a snapshot written BEFORE the `launch` field existed has
        // no `launch` key. It must deserialize to `None` (never a hard parse error)
        // so the start path falls back to re-reading the manifest, exactly as it
        // did before this change. Simulate a legacy file by serializing a snapshot
        // and dropping the `launch` key.
        let declaration = CapabilityDeclaration::new().with(
            Capability::Pause,
            OsId::Linux,
            SupportLevel::Guaranteed,
        );
        let snapshot = AdapterSnapshot {
            kind: "demo".to_string(),
            metering_source: "self-reported".to_string(),
            manifest_path: Some("/some/adapter.toml".to_string()),
            declaration,
            launch: Some(LaunchSnapshot {
                exec: "x".to_string(),
                args: Vec::new(),
                env: std::collections::BTreeMap::new(),
            }),
        };
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value.as_object_mut().unwrap().remove("launch");
        let back: AdapterSnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(back.kind, "demo");
        assert!(
            back.launch.is_none(),
            "an absent launch key must deserialize to None (fallback to re-read)"
        );
    }

    #[test]
    fn effective_capabilities_projects_at_read_time_not_register_time() {
        // F3: a snapshot persisted with a FULL declaration whose pause level
        // differs on every modeled OS must project onto the CURRENTLY running OS
        // at read time — never a level frozen to some other (registering) OS.
        // We hand-write the snapshot (simulating "registered as if on any OS")
        // and confirm effective_capabilities returns the current-OS level.
        use ktesio_adapter_api::{Capability, SupportLevel};
        let (_tmp, reg) = open_temp();
        // Register normally to create the row + home, then overwrite the snapshot
        // with a full declaration that distinguishes every OS.
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        let declaration = CapabilityDeclaration::new()
            .with(Capability::Pause, OsId::Linux, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Macos, SupportLevel::BestEffort)
            .with(Capability::Pause, OsId::Windows, SupportLevel::Unsupported);
        let snapshot = AdapterSnapshot {
            kind: "demo".to_string(),
            metering_source: "self-reported".to_string(),
            manifest_path: None,
            declaration,
            launch: None,
        };
        let snap_path = reg.adapter_snapshot_path(&name);
        std::fs::write(&snap_path, serde_json::to_string_pretty(&snapshot).unwrap()).unwrap();

        let eff = reg.effective_capabilities("demo").unwrap();
        assert_eq!(eff.os, OsId::current(), "projection is for the running OS");
        // The pause level returned must equal the current OS's declared level —
        // proving the projection happened at read time from the full map.
        let expected = match OsId::current() {
            OsId::Linux => SupportLevel::Guaranteed,
            OsId::Macos => SupportLevel::BestEffort,
            OsId::Windows => SupportLevel::Unsupported,
            OsId::Other => SupportLevel::Unsupported,
        };
        let pause = eff
            .entries
            .iter()
            .find(|(c, _)| *c == Capability::Pause)
            .map(|(_, l)| *l);
        // On Other, pause has no entry → not present; on modeled OSes it matches.
        if OsId::current() == OsId::Other {
            assert!(pause.is_none() || pause == Some(SupportLevel::Unsupported));
        } else {
            assert_eq!(pause, Some(expected), "read-time projection for current OS");
        }
    }

    #[test]
    fn effective_support_reads_the_current_os_pause_level_at_read_time() {
        // AC5 (1-5): effective_support projects the FULL persisted declaration
        // onto OsId::current() at READ time. Overwrite the snapshot with a
        // declaration that distinguishes every OS and assert the current OS's
        // level is returned — proving it is read, not re-derived or frozen.
        use ktesio_adapter_api::{Capability, SupportLevel};
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        let declaration = CapabilityDeclaration::new()
            .with(Capability::Pause, OsId::Linux, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Macos, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Windows, SupportLevel::BestEffort);
        let snapshot = AdapterSnapshot {
            kind: "demo".to_string(),
            metering_source: "self-reported".to_string(),
            manifest_path: None,
            declaration,
            launch: None,
        };
        std::fs::write(
            reg.adapter_snapshot_path(&name),
            serde_json::to_string_pretty(&snapshot).unwrap(),
        )
        .unwrap();

        let level = reg.effective_support(&name, Capability::Pause).unwrap();
        let expected = match OsId::current() {
            OsId::Linux | OsId::Macos => SupportLevel::Guaranteed,
            OsId::Windows => SupportLevel::BestEffort,
            OsId::Other => SupportLevel::Unsupported,
        };
        assert_eq!(level, expected, "read-time projection for the current OS");
    }

    #[test]
    fn effective_support_defaults_to_unsupported_when_capability_absent_for_this_os() {
        // AC5/AC3 default: a declaration that omits pause for the current OS
        // projects to Unsupported (the honest default that drives fail-fast).
        use ktesio_adapter_api::{Capability, SupportLevel};
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        // Declare pause only for OsId::Other so no modeled host has an entry.
        let declaration = CapabilityDeclaration::new().with(
            Capability::Interaction,
            OsId::current(),
            SupportLevel::Guaranteed,
        );
        let snapshot = AdapterSnapshot {
            kind: "demo".to_string(),
            metering_source: "self-reported".to_string(),
            manifest_path: None,
            declaration,
            launch: None,
        };
        std::fs::write(
            reg.adapter_snapshot_path(&name),
            serde_json::to_string_pretty(&snapshot).unwrap(),
        )
        .unwrap();

        let level = reg.effective_support(&name, Capability::Pause).unwrap();
        assert_eq!(level, SupportLevel::Unsupported);
    }

    // ---- Story 2-1: unified layered config through path authority ----

    #[test]
    fn engine_defaults_layer_parses_and_is_empty_in_2_1() {
        // Review decision #1: the embedded engine defaults parse (compile-time
        // constant) but ship EMPTY — config no longer seeds restart.policy (the
        // reaper owns the policy default via RestartPolicy::default, not config).
        let layer = engine_defaults_layer().unwrap();
        assert!(layer.is_empty(), "engine defaults must be empty in 2-1");
        let eff = config::resolve([
            layer,
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::empty(),
        ]);
        assert!(eff.is_empty());
        assert_eq!(eff.value("restart.policy"), None);
    }

    #[test]
    fn set_config_then_effective_config_reflects_it_at_the_instance_layer() {
        // AC-A/AC10 round trip: set a known key, then the effective config shows
        // it tagged as the INSTANCE layer (beating the engine default).
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        reg.set_config(&name, "model", "gpt-4").unwrap();
        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, toml::Value::String("gpt-4".into()));
        assert_eq!(r.source, SourceLayer::Instance);
    }

    #[test]
    fn resolve_secrets_maps_only_secret_leaves_to_resolved_cleartext() {
        // Story 2-4 (AC5/AC9): resolve_secrets returns the resolved cleartext for a
        // `secret:NAME` leaf (env resolver) and IGNORES non-secret leaves. The
        // effective config's display() still MASKS the secret leaf (delivery vs
        // display diverge). Uses a unique env-var name (in-process env is global).
        let env_key = "KTESIO_REG_SECRET_TEST_KEY";
        let prev = std::env::var_os(env_key);
        std::env::set_var(env_key, "resolved-cleartext");

        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        reg.set_config(&name, "model", &format!("secret:{env_key}"))
            .unwrap();
        reg.set_config(&name, "agent.plain", "visible").unwrap();

        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        // display() masks the secret leaf, leaves the plain leaf alone.
        assert_eq!(eff.value_display("model").as_deref(), Some("secret:****"));
        assert_eq!(eff.value_display("agent.plain").as_deref(), Some("visible"));

        // resolve_secrets returns the CLEARTEXT for the secret leaf ONLY.
        let secrets = reg.resolve_secrets(&eff).unwrap();
        assert_eq!(
            secrets.get("model").map(|s| s.expose_secret()),
            Some("resolved-cleartext")
        );
        assert!(!secrets.contains_key("agent.plain"), "only secret leaves");

        // reveal_secrets returns the same cleartext as plain strings for the read.
        let revealed = reg.reveal_secrets(&name, ConfigLayer::empty()).unwrap();
        assert_eq!(
            revealed.get("model").map(String::as_str),
            Some("resolved-cleartext")
        );

        match prev {
            Some(v) => std::env::set_var(env_key, v),
            None => std::env::remove_var(env_key),
        }
    }

    #[test]
    fn resolve_secrets_walks_past_non_string_leaves_without_failing_the_start() {
        // resolve_secrets iterates EVERY leaf of the effective config on EVERY
        // start, and only a STRING leaf can be a `secret:` reference. Non-string
        // leaves are not exotic — a budgeted instance has integer token ceilings
        // and a boolean-ish flag is a hand-edit away — so the skip arm is on the
        // hot start path for ordinary configs. If it mis-handled them, budgeting an
        // agent would break starting it. Assert the walk both SKIPS them and still
        // finds a secret sitting after them in dotted-key order.
        let env_key = "KTESIO_REG_NON_STRING_LEAF_TEST_KEY";
        let prev = std::env::var_os(env_key);
        std::env::set_var(env_key, "resolved-cleartext");

        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        reg.set_config(&name, "model", &format!("secret:{env_key}"))
            .unwrap();
        // `set_config` stores every value as a TOML string, so a genuine
        // non-string leaf has to come from parsed TOML — exactly how a
        // hand-edited config.toml or a kind-default layer supplies one.
        // `budget.*` sorts BEFORE `model`, so it is walked first.
        let typed = ConfigLayer::parse(
            SourceLayer::InvocationOverride,
            "<test>",
            "[budget.tokens]\ncumulative = 500\n",
        )
        .unwrap();

        let eff = reg.effective_config(&name, typed).unwrap();
        // The ceiling really is a non-string leaf (not a quoted "500"), otherwise
        // this test would not exercise the skip at all.
        assert!(
            matches!(
                eff.value("budget.tokens.cumulative"),
                Some(toml::Value::Integer(500))
            ),
            "expected an integer leaf, got {:?}",
            eff.value("budget.tokens.cumulative")
        );

        let secrets = reg.resolve_secrets(&eff).unwrap();
        assert_eq!(
            secrets.get("model").map(|s| s.expose_secret()),
            Some("resolved-cleartext")
        );
        assert!(
            !secrets.contains_key("budget.tokens.cumulative"),
            "a non-string leaf must contribute nothing to the secret map"
        );

        match prev {
            Some(v) => std::env::set_var(env_key, v),
            None => std::env::remove_var(env_key),
        }
    }

    #[test]
    fn resolve_secrets_unresolved_is_a_typed_error_naming_the_name() {
        // A `secret:NAME` unresolved by env + the (absent) secrets file is a typed
        // SecretError::Unresolved naming NAME + resolvers, NEVER a value; reveal
        // surfaces it as ConfigError::SecretReveal.
        let env_key = "KTESIO_REG_UNSET_SECRET_KEY_XYZ";
        std::env::remove_var(env_key);
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        reg.set_config(&name, "model", &format!("secret:{env_key}"))
            .unwrap();
        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();

        let err = reg.resolve_secrets(&eff).unwrap_err();
        assert!(err.to_string().contains(env_key), "names NAME: {err}");

        // reveal surfaces the SecretReveal config error (a read diagnostic).
        let rev_err = reg.reveal_secrets(&name, ConfigLayer::empty()).unwrap_err();
        assert!(
            matches!(rev_err, ConfigError::SecretReveal { .. }),
            "got {rev_err:?}"
        );
    }

    #[test]
    fn set_config_instance_beats_invocation_and_records_provenance() {
        // AC-A end-to-end through the engine: set `model` at the instance layer;
        // with NO overrides it resolves from the instance layer (tagged Instance),
        // and an invocation override for the same key beats it (tagged
        // InvocationOverride). (Engine + kind defaults are empty in 2-1, so
        // instance-vs-override is the precedence pair observable via the engine;
        // the full 4-layer precedence is covered by the pure-resolver tests.)
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        reg.set_config(&name, "model", "instance-model").unwrap();
        let plain = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        let r = plain.get("model").unwrap();
        assert_eq!(r.value, toml::Value::String("instance-model".into()));
        assert_eq!(r.source, SourceLayer::Instance);

        let overrides =
            ConfigLayer::parse(SourceLayer::InvocationOverride, "<ov>", "model = \"ov\"").unwrap();
        let overridden = reg.effective_config(&name, overrides).unwrap();
        let r = overridden.get("model").unwrap();
        assert_eq!(r.value, toml::Value::String("ov".into()));
        assert_eq!(r.source, SourceLayer::InvocationOverride);
    }

    #[test]
    fn set_config_unknown_key_is_rejected_and_config_is_byte_unchanged() {
        // AC-B atomicity: a rejected write persists NOTHING — the on-disk
        // config.toml is byte-identical before and after.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        let path = reg.paths().instance_config(&name);
        let before = std::fs::read(&path).unwrap();

        let err = reg.set_config(&name, "notakey", "x").unwrap_err();
        assert!(matches!(err, ConfigError::UnknownKey { key, .. } if key == "notakey"));

        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "rejected write must leave config byte-unchanged"
        );
    }

    // ---- Story 11-2: AI-24 (atomic set_config) + AI-33 (secret→flag steering) ----

    /// Story 11-2 (AI-33): register a manifest adapter whose `[config.model]`
    /// maps onto a FLAG target — the shape the set-time steering fires on (the
    /// native `mock` maps `model` → env `MODEL`, the quiet path).
    fn register_flag_target_instance(reg: &Registry, name: &str, manifest_dir: &Path) {
        let body = format!(
            "contract_version = \"1.0.0\"\n\n\
             [adapter]\nkind = \"{name}\"\n\n\
             [lifecycle.start]\nexec = \"the-agent\"\n\n\
             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n\n\
             [metering]\nsource = \"self-reported\"\n\n\
             [config.model]\nflag = \"--model\"\n"
        );
        std::fs::write(manifest_dir.join("adapter.toml"), body).unwrap();
        reg.register_with_adapter(name, &AdapterRef::Manifest(manifest_dir.to_path_buf()))
            .unwrap();
    }

    // NOTE (review-1 patch 5 follow-up): the set_config WRITE-failure shape
    // (forced write error -> old bytes intact, no temp residue, typed
    // MalformedLayer) is pinned in
    // `crates/ktesio-engine/tests/atomic_config_writes.rs`
    // (`config_set_write_failure_leaves_old_bytes_and_no_temp_residue`,
    // unix-gated: the injection needs a read-only home directory). The
    // temp-path-planting injection this in-module test used cannot predict the
    // review-1 collision-safe temp name (`<file>.tmp-<pid>-<tid>-<seq>`), and
    // dir-at-target here would trip the earlier READ arm instead of the write.

    #[test]
    fn set_config_secret_on_flag_target_warns_but_succeeds() {
        // AI-33 (the positive): a `secret:NAME` value on a FLAG-targeted key set
        // SUCCEEDS (warn-only) and the returned warnings name the key + the
        // argv-leak risk + the env/file alternative. The value still persists.
        let (_tmp, reg) = open_temp();
        let manifest = TempDir::new().unwrap();
        register_flag_target_instance(&reg, "flgsec", manifest.path());
        let name = InstanceName::new("flgsec").unwrap();

        let warnings = reg.set_config(&name, "model", "secret:MY_SECRET").unwrap();
        assert_eq!(warnings.len(), 1, "exactly one steering warning");
        assert!(
            warnings[0].contains("model"),
            "names the key: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("FLAG"),
            "names the flag-target leak risk: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("env") && warnings[0].contains("file"),
            "names the env/file alternative: {}",
            warnings[0]
        );

        // The value is persisted as the reference (the write is unaffected).
        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        assert_eq!(
            eff.value("model"),
            Some(&toml::Value::String("secret:MY_SECRET".into()))
        );
    }

    #[test]
    fn set_config_stays_quiet_without_a_secret_flag_combination() {
        // AI-33 (the quiet paths — the common case must stay silent): a plain
        // value on a flag-targeted key, a secret on an ENV-targeted key (the
        // mock maps model → env MODEL), a secret on a pass-through key (delivered
        // to env by tail regardless of any rule), and a secret on an unmapped
        // documented key (Decision 6) all warn NOTHING.
        let (_tmp, reg) = open_temp();
        let manifest = TempDir::new().unwrap();
        register_flag_target_instance(&reg, "flgq", manifest.path());
        let flag_name = InstanceName::new("flgq").unwrap();
        reg.register("mck", "mock").unwrap();
        let mock_name = InstanceName::new("mck").unwrap();

        assert!(
            reg.set_config(&flag_name, "model", "plain-value")
                .unwrap()
                .is_empty(),
            "a non-secret value on a flag target is quiet"
        );
        assert!(
            reg.set_config(&mock_name, "model", "secret:QUIET_ENV")
                .unwrap()
                .is_empty(),
            "a secret on an env target is quiet"
        );
        assert!(
            reg.set_config(&flag_name, "agent.TAIL", "secret:QUIET_PT")
                .unwrap()
                .is_empty(),
            "a secret on a pass-through key is quiet (delivered to env by tail)"
        );
        assert!(
            reg.set_config(
                &flag_name,
                "metering.upstream_base_url",
                "secret:QUIET_UNMAPPED"
            )
            .unwrap()
            .is_empty(),
            "a secret on a known key the adapter maps NOWHERE is quiet (Decision 6 no-op)"
        );
    }

    #[test]
    fn set_config_agent_pass_through_key_round_trips_verbatim() {
        // AC7: an agent.* key writes successfully and round-trips verbatim at the
        // instance layer (no native mapping — that is 2-2).
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        reg.set_config(&name, "agent.custom_flag", "verbatim-value")
            .unwrap();
        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        let r = eff.get("agent.custom_flag").unwrap();
        assert_eq!(r.value, toml::Value::String("verbatim-value".into()));
        assert_eq!(r.source, SourceLayer::Instance);
    }

    #[test]
    fn set_config_preserves_sibling_keys_deep_set() {
        // AC4/AC-B: setting a scalar (model), then a nested agent.* key, then a
        // SIBLING under the same nested table preserves all three — the deep dotted
        // set does not clobber siblings, and neither does re-serialization.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        reg.set_config(&name, "model", "gpt-4").unwrap();
        reg.set_config(&name, "agent.tools.web", "on").unwrap();
        reg.set_config(&name, "agent.tools.shell", "off").unwrap();

        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        assert_eq!(
            eff.value("model"),
            Some(&toml::Value::String("gpt-4".into()))
        );
        // Both nested siblings survive (deep per-leaf set).
        assert_eq!(
            eff.value("agent.tools.web"),
            Some(&toml::Value::String("on".into()))
        );
        assert_eq!(
            eff.value("agent.tools.shell"),
            Some(&toml::Value::String("off".into()))
        );
    }

    #[test]
    fn set_config_child_under_existing_scalar_fails_closed_byte_unchanged() {
        // Review patch #3: `set agent.a v1` then `set agent.a.b v2` would destroy
        // the scalar `agent.a` — instead it FAILS CLOSED with WriteShapeConflict
        // (naming the conflicting ancestor) and the on-disk config is byte-unchanged.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        reg.set_config(&name, "agent.a", "v1").unwrap();
        let path = reg.paths().instance_config(&name);
        let before = std::fs::read(&path).unwrap();

        let err = reg.set_config(&name, "agent.a.b", "v2").unwrap_err();
        match err {
            ConfigError::WriteShapeConflict {
                key,
                conflicting_ancestor,
            } => {
                assert_eq!(key, "agent.a.b");
                assert_eq!(conflicting_ancestor, "agent.a");
            }
            other => panic!("expected WriteShapeConflict, got {other:?}"),
        }
        // AC-B atomicity: nothing persisted — the scalar agent.a survives intact.
        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "failed write must leave config byte-unchanged"
        );
        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        assert_eq!(
            eff.value("agent.a"),
            Some(&toml::Value::String("v1".into()))
        );
        assert_eq!(eff.value("agent.a.b"), None);
    }

    #[test]
    fn set_config_rejects_empty_dotted_segment_byte_unchanged() {
        // Patch #5 at the write API: an empty-segment key is rejected and nothing
        // is persisted.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        let path = reg.paths().instance_config(&name);
        let before = std::fs::read(&path).unwrap();

        let err = reg.set_config(&name, "agent..b", "v").unwrap_err();
        assert!(matches!(err, ConfigError::UnknownKey { key, .. } if key == "agent..b"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn effective_config_filters_the_seeded_name_identity_key() {
        // Review patch #4: materialize_home seeds `name = "<instance>"` into
        // config.toml; the resolved effective config must NOT surface it as a
        // settable key (it is identity, and `config set … name …` is rejected).
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        // The on-disk file still has it (a human-readable marker)...
        let body = std::fs::read_to_string(reg.paths().instance_config(&name)).unwrap();
        assert!(
            body.contains("name = \"demo\""),
            "seed still on disk: {body}"
        );
        // ...but the resolved view filters it out.
        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        assert_eq!(eff.value("name"), None, "identity key must not be surfaced");
        // And `config set … name …` is rejected as unknown (coherent now).
        assert!(matches!(
            reg.set_config(&name, "name", "renamed").unwrap_err(),
            ConfigError::UnknownKey { .. }
        ));
    }

    #[test]
    fn effective_config_threads_invocation_overrides_strongest() {
        // Decision 8 + AC-A: an invocation-override layer beats the instance
        // layer for the same key.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        reg.set_config(&name, "model", "instance-model").unwrap();

        let overrides = ConfigLayer::parse(
            SourceLayer::InvocationOverride,
            "<ov>",
            "model = \"override-model\"",
        )
        .unwrap();
        let eff = reg.effective_config(&name, overrides).unwrap();
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, toml::Value::String("override-model".into()));
        assert_eq!(r.source, SourceLayer::InvocationOverride);
    }

    // ---- Story 2-3: the persisted effective-config snapshot (AC5/AC7, AD-9/AD-6) ----

    #[test]
    fn effective_config_snapshot_dto_has_one_entry_per_leaf_with_value_and_source() {
        // The DTO built from a multi-layer EffectiveConfig carries every resolved
        // leaf (rendered value via the ONE display path + its source-layer label)
        // and the schema version. Instance beats kind for the same key; a sibling
        // from the weaker layer survives with its own provenance.
        let layers = [
            config::ConfigLayer::empty(),
            config::ConfigLayer::parse(
                SourceLayer::KindDefault,
                "<k>",
                "[a]\nb = \"kind-b\"\nc = \"kind-c\"\n",
            )
            .unwrap(),
            config::ConfigLayer::parse(SourceLayer::Instance, "<i>", "[a]\nb = \"inst-b\"\n")
                .unwrap(),
            config::ConfigLayer::empty(),
        ];
        let eff = config::resolve(layers);
        let snap = EffectiveConfigSnapshot::from_effective(&eff);
        assert_eq!(
            snap.schema_version,
            EFFECTIVE_CONFIG_SNAPSHOT_SCHEMA_VERSION
        );
        // Two leaves: a.b (instance) and a.c (kind), sorted by key.
        assert_eq!(snap.entries.len(), 2);
        let ab = snap.entries.iter().find(|e| e.key == "a.b").unwrap();
        assert_eq!(ab.value, "inst-b"); // rendered bare via display()
        assert_eq!(ab.source, SourceLayer::Instance);
        let ac = snap.entries.iter().find(|e| e.key == "a.c").unwrap();
        assert_eq!(ac.value, "kind-c");
        assert_eq!(ac.source, SourceLayer::KindDefault);
    }

    #[test]
    fn effective_config_snapshot_round_trips_through_json() {
        // The snapshot serializes to JSON and parses back identically; the source
        // label is the kebab-case wire form (SourceLayer serde) and the value is
        // the rendered string (a non-string scalar renders in TOML inline form).
        let eff = config::resolve([
            config::ConfigLayer::parse(SourceLayer::EngineDefault, "<e>", "n = 42\n").unwrap(),
            config::ConfigLayer::empty(),
            config::ConfigLayer::parse(SourceLayer::Instance, "<i>", "model = \"gpt-4\"\n")
                .unwrap(),
            config::ConfigLayer::empty(),
        ]);
        let snap = EffectiveConfigSnapshot::from_effective(&eff);
        let json = serde_json::to_string_pretty(&snap).unwrap();
        // The kebab-case source label appears in the wire form.
        assert!(json.contains("\"instance\""), "json={json}");
        assert!(json.contains("\"engine-default\""), "json={json}");
        // A non-string scalar renders in inline form via display().
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let n = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == "n")
            .unwrap();
        assert_eq!(n["value"], serde_json::json!("42"));
        assert_eq!(n["source"], serde_json::json!("engine-default"));
    }

    #[test]
    fn write_effective_config_snapshot_writes_to_path_authority_and_parses_back() {
        // The writer persists the snapshot at EnginePaths::effective_config_snapshot
        // (path authority — the engine is the sole writer); the file parses back and
        // carries the resolved leaf tagged with its source layer.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        reg.set_config(&name, "model", "claude-opus").unwrap();

        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        reg.write_effective_config_snapshot(&name, &eff).unwrap();

        let path = reg.paths().effective_config_snapshot(&name);
        assert!(path.is_file(), "the snapshot file should exist at {path:?}");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["schema_version"], serde_json::json!(1));
        let model = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == "model")
            .unwrap();
        assert_eq!(model["value"], serde_json::json!("claude-opus"));
        assert_eq!(model["source"], serde_json::json!("instance"));
    }

    #[test]
    fn write_effective_config_snapshot_overwrites_in_place() {
        // AC7: the snapshot is OVERWRITTEN each write, always reflecting the latest
        // resolved config — never a stale earlier resolution.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();

        reg.set_config(&name, "model", "first").unwrap();
        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        reg.write_effective_config_snapshot(&name, &eff).unwrap();

        reg.set_config(&name, "model", "second").unwrap();
        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        reg.write_effective_config_snapshot(&name, &eff).unwrap();

        let path = reg.paths().effective_config_snapshot(&name);
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let model = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == "model")
            .unwrap();
        assert_eq!(
            model["value"],
            serde_json::json!("second"),
            "the snapshot must reflect the latest resolved value (overwrite)"
        );
    }

    #[test]
    fn write_effective_config_snapshot_write_failure_is_typed_not_panic() {
        // AC6: a snapshot-write failure surfaces a typed RegistryError::SnapshotWrite
        // naming the instance + path (never a panic). Force the write to fail by
        // making the snapshot path a DIRECTORY (std::fs::write on a dir errors).
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        let path = reg.paths().effective_config_snapshot(&name);
        std::fs::create_dir(&path).unwrap();

        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        let err = reg
            .write_effective_config_snapshot(&name, &eff)
            .unwrap_err();
        match err {
            RegistryError::SnapshotWrite {
                name: n, path: p, ..
            } => {
                assert_eq!(n, "demo");
                assert!(p.ends_with("effective-config.json"), "path={p}");
            }
            other => panic!("expected SnapshotWrite, got {other:?}"),
        }
    }

    #[test]
    fn config_ops_on_unknown_instance_are_not_found() {
        let (_tmp, reg) = open_temp();
        let ghost = InstanceName::new("ghost").unwrap();
        assert!(matches!(
            reg.effective_config(&ghost, ConfigLayer::empty()).unwrap_err(),
            ConfigError::NotFound { name } if name == "ghost"
        ));
        assert!(matches!(
            reg.set_config(&ghost, "model", "x").unwrap_err(),
            ConfigError::NotFound { name } if name == "ghost"
        ));
    }

    #[test]
    fn config_ops_surface_a_store_error_without_leaking_registry_error() {
        // The require_instance Store arm: if the store read fails (table dropped),
        // both effective_config and set_config surface ConfigError::Store naming
        // the instance — never a RegistryError across the AD-1 boundary.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        reg.store.break_instance_reads_for_test();

        assert!(matches!(
            reg.effective_config(&name, ConfigLayer::empty()).unwrap_err(),
            ConfigError::Store { name: n, .. } if n == "demo"
        ));
        assert!(matches!(
            reg.set_config(&name, "model", "x").unwrap_err(),
            ConfigError::Store { name: n, .. } if n == "demo"
        ));
    }

    #[test]
    fn instance_config_layer_read_failure_is_a_typed_error_not_panic() {
        // instance_config_layer's non-NotFound read-failure arm: a config.toml
        // that is a DIRECTORY makes read_to_string fail with a non-NotFound error
        // → MalformedLayer, never a panic. This is the read arm BOTH config reads
        // funnel through, exercised via both entry points:
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        let path = reg.paths().instance_config(&name);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        // effective_config surfaces it (read at resolve time).
        let err = reg
            .effective_config(&name, ConfigLayer::empty())
            .unwrap_err();
        match err {
            ConfigError::MalformedLayer { layer, detail, .. } => {
                assert_eq!(layer, SourceLayer::Instance);
                assert!(detail.contains("could not read"), "detail={detail}");
            }
            other => panic!("expected MalformedLayer, got {other:?}"),
        }
        // set_config surfaces it too (it reads the current layer before writing) —
        // so a rejected write never blindly clobbers an unreadable config.
        let err = reg.set_config(&name, "model", "gpt-4").unwrap_err();
        assert!(
            matches!(err, ConfigError::MalformedLayer { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn kind_defaults_layer_is_empty_for_every_kind_in_2_1() {
        // Decision 2: no per-kind config defaults in 2-1 — every kind resolves to
        // an EMPTY layer (a valid "no defaults", not an error).
        assert!(kind_defaults_layer("mock").is_empty());
        assert!(kind_defaults_layer("anything-else").is_empty());
    }

    #[test]
    fn effective_config_reports_malformed_instance_layer_without_panic() {
        // AC8: a present-but-malformed config.toml surfaces MalformedLayer naming
        // the instance layer + path, never a panic.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        let path = reg.paths().instance_config(&name);
        std::fs::write(&path, "this is = = not valid toml").unwrap();

        let err = reg
            .effective_config(&name, ConfigLayer::empty())
            .unwrap_err();
        match err {
            ConfigError::MalformedLayer { layer, path: p, .. } => {
                assert_eq!(layer, SourceLayer::Instance);
                assert!(p.ends_with("config.toml"), "path={p}");
            }
            other => panic!("expected MalformedLayer, got {other:?}"),
        }
    }

    #[test]
    fn effective_config_missing_instance_file_resolves_without_error() {
        // A registered instance whose config.toml was removed still RESOLVES
        // (the instance layer is treated as empty) rather than erroring. With the
        // engine + kind defaults both empty in 2-1 and no instance file, the
        // effective config is empty — but the read succeeds (no panic, no error).
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let name = InstanceName::new("demo").unwrap();
        std::fs::remove_file(reg.paths().instance_config(&name)).unwrap();

        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        assert!(eff.is_empty());
        // A later-set key still resolves after the file is recreated by set_config.
        reg.set_config(&name, "model", "gpt-4").unwrap();
        let eff = reg.effective_config(&name, ConfigLayer::empty()).unwrap();
        assert_eq!(
            eff.value("model"),
            Some(&toml::Value::String("gpt-4".into()))
        );
    }

    #[test]
    fn set_dotted_creates_nested_tables_and_preserves_siblings() {
        // Unit-test the deep dotted set helper directly: a.b then a.c coexist;
        // setting a.b again overwrites only a.b. Each set returns Ok.
        let mut t = toml::value::Table::new();
        set_dotted(&mut t, "a.b", toml::Value::Integer(1)).unwrap();
        set_dotted(&mut t, "a.c", toml::Value::Integer(2)).unwrap();
        set_dotted(&mut t, "a.b", toml::Value::Integer(9)).unwrap();
        let a = t.get("a").unwrap().as_table().unwrap();
        assert_eq!(a.get("b"), Some(&toml::Value::Integer(9)));
        assert_eq!(a.get("c"), Some(&toml::Value::Integer(2)));
        // Overwriting a leaf with another leaf at the same key is fine.
        set_dotted(&mut t, "a.b", toml::Value::String("s".into())).unwrap();
        assert_eq!(
            t.get("a").unwrap().as_table().unwrap().get("b"),
            Some(&toml::Value::String("s".into()))
        );
    }

    #[test]
    fn set_dotted_fails_closed_on_scalar_intermediate_and_mutates_nothing() {
        // Review patch #3: nesting a child under an existing scalar returns
        // WriteShapeConflict (naming the conflicting ancestor) and mutates the
        // table NOT AT ALL — the scalar survives.
        let mut t = toml::value::Table::new();
        set_dotted(&mut t, "x", toml::Value::Integer(5)).unwrap();
        let err = set_dotted(&mut t, "x.y", toml::Value::Integer(6)).unwrap_err();
        match err {
            ConfigError::WriteShapeConflict {
                key,
                conflicting_ancestor,
            } => {
                assert_eq!(key, "x.y");
                assert_eq!(conflicting_ancestor, "x");
            }
            other => panic!("expected WriteShapeConflict, got {other:?}"),
        }
        // The scalar `x` is untouched (fail-closed, no partial mutation).
        assert_eq!(t.get("x"), Some(&toml::Value::Integer(5)));

        // A DEEPER conflict names the deepest scalar ancestor walked: a.b is a
        // scalar; setting a.b.c.d conflicts at a.b.
        let mut t2 = toml::value::Table::new();
        set_dotted(&mut t2, "a.b", toml::Value::Integer(1)).unwrap();
        let err = set_dotted(&mut t2, "a.b.c.d", toml::Value::Integer(2)).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::WriteShapeConflict { conflicting_ancestor, .. }
                if conflicting_ancestor == "a.b"
        ));
    }

    #[test]
    fn register_manifest_rejection_when_adapter_toml_is_a_directory() {
        // Defensive: if adapter.toml exists but is a directory, reading it fails
        // with an I/O error and is surfaced as ManifestUnreadable (its own
        // variant — F4), leaving no partial state.
        let (_tmp, reg) = open_temp();
        let manifest_dir = TempDir::new().unwrap();
        std::fs::create_dir(manifest_dir.path().join(crate::adapter::MANIFEST_FILE)).unwrap();
        let err = reg
            .register_with_adapter(
                "m",
                &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
            )
            .unwrap_err();
        assert!(
            matches!(&err, RegistryError::ManifestUnreadable { path, .. } if path.ends_with(crate::adapter::MANIFEST_FILE)),
            "got {err:?}"
        );
        assert!(reg.list().unwrap().is_empty());
    }

    // ---- Story 5-1: Memory Backing attach/detach/read (AD-11) ----

    /// Seed an instance in an arbitrary state directly into the store (no home
    /// materialization) — the exact `remove_running_without_force_is_rejected`
    /// pattern (DC-3: the guard is pure persisted-state validation).
    fn seed_in_state(reg: &Registry, n: &str, state: LifecycleState) -> InstanceName {
        let iname = InstanceName::new(n).unwrap();
        let now = now_rfc3339();
        reg.seed_instance(&AgentInstance {
            name: iname.clone(),
            kind: "mock".to_string(),
            state,
            agent_home: reg
                .paths()
                .agent_home(&iname)
                .to_string_lossy()
                .into_owned(),
            created_at: now.clone(),
            updated_at: now,
        })
        .unwrap();
        iname
    }

    #[test]
    fn attach_creates_the_managed_directory_and_the_read_reports_it() {
        // AC1 + DC-1: the directory is created INSIDE the Agent Home at the
        // engine-computed path (returned by the engine — never computed by the
        // caller), and the public read reports kind + path.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();

        let dir = reg
            .attach_memory("demo", MemoryBackingKind::Filesystem)
            .unwrap();
        let expected = reg
            .paths()
            .agent_home(&InstanceName::new("demo").unwrap())
            .join(crate::paths::MEMORY_DIR);
        assert_eq!(dir, expected);
        assert!(dir.is_dir(), "managed directory should exist");

        let status = reg.memory_status("demo").unwrap().unwrap();
        assert_eq!(status.kind, MemoryBackingKind::Filesystem);
        assert_eq!(status.dir, dir);
        // The builtin mock declares a mapping for the reserved key (story 5-1
        // lockstep), so delivery IS declared for this adapter.
        assert!(status.declared, "mock maps memory.dir → env");
    }

    #[test]
    fn re_attaching_the_same_kind_is_an_idempotent_success() {
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let first = reg
            .attach_memory("demo", MemoryBackingKind::Filesystem)
            .unwrap();
        // Read the attached_at, re-attach, and confirm the row did NOT change.
        let before = reg
            .store
            .get_memory_backing(&InstanceName::new("demo").unwrap())
            .unwrap();
        let second = reg
            .attach_memory("demo", MemoryBackingKind::Filesystem)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            reg.store
                .get_memory_backing(&InstanceName::new("demo").unwrap())
                .unwrap(),
            before,
            "re-attach must not rewrite the row"
        );
    }

    #[test]
    fn attaching_a_different_kind_over_an_existing_one_is_rejected() {
        // A-6: kinds never hot-swap; detach first. Nothing changes on rejection.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        reg.attach_memory("demo", MemoryBackingKind::Filesystem)
            .unwrap();
        let err = reg
            .attach_memory("demo", MemoryBackingKind::Native)
            .unwrap_err();
        match &err {
            RegistryError::MemoryBackingKindConflict {
                name,
                attached,
                requested,
            } => {
                assert_eq!(name, "demo");
                assert_eq!(attached, "filesystem");
                assert_eq!(requested, "native");
            }
            other => panic!("expected MemoryBackingKindConflict, got {other:?}"),
        }
        // The original attachment stands untouched.
        assert_eq!(
            reg.memory_status("demo").unwrap().unwrap().kind,
            MemoryBackingKind::Filesystem
        );
    }

    #[test]
    fn attaching_filesystem_over_an_attached_native_backing_is_rejected_symmetrically() {
        // A5 (story 11-3): the REVERSE conflict direction — a `native` backing
        // attached, `filesystem` requested. Symmetric to the forward case
        // above: the same typed error naming attached/requested, and NOTHING
        // mutates — the native row stands and the rejected request creates no
        // managed directory (the side effects run only after every fallible
        // check passes).
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        reg.attach_memory("demo", MemoryBackingKind::Native)
            .unwrap();
        let err = reg
            .attach_memory("demo", MemoryBackingKind::Filesystem)
            .unwrap_err();
        match &err {
            RegistryError::MemoryBackingKindConflict {
                name,
                attached,
                requested,
            } => {
                assert_eq!(name, "demo");
                assert_eq!(attached, "native");
                assert_eq!(requested, "filesystem");
            }
            other => panic!("expected MemoryBackingKindConflict, got {other:?}"),
        }
        // The original attachment stands untouched ...
        assert_eq!(
            reg.memory_status("demo").unwrap().unwrap().kind,
            MemoryBackingKind::Native
        );
        // ... and no managed directory was created by the rejected request.
        assert!(
            !reg.paths()
                .agent_home(&InstanceName::new("demo").unwrap())
                .join(crate::paths::MEMORY_DIR)
                .exists(),
            "a rejected conflict must not create the managed directory"
        );
    }

    #[test]
    fn native_attach_persists_the_row_without_a_directory_and_status_reports_delegation() {
        // Story 5-2, AC1 + DC-1/DC-2: the registry round-trip for `native` —
        // metadata only. The row persists (visible on the public read), NO
        // managed directory is created (the delegation marker is not a
        // directory), `declared` reads FALSE honestly (nothing is delivered, so
        // there is no mapping fact to resolve), and the typed guarantee level
        // names the delegation boundary.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();

        let dir = reg
            .attach_memory("demo", MemoryBackingKind::Native)
            .unwrap();
        assert!(!dir.exists(), "native attaches metadata, never a directory");

        let status = reg.memory_status("demo").unwrap().unwrap();
        assert_eq!(status.kind, MemoryBackingKind::Native);
        assert_eq!(status.dir, dir);
        assert!(
            !dir.exists(),
            "memory_status must not materialize anything for native"
        );
        assert!(
            !status.declared,
            "nothing is delivered for native — declared must not read as a promise"
        );
        assert_eq!(
            status.guarantee,
            crate::ports::GuaranteeLevel::HomePersistenceOnly
        );

        // The filesystem contrast on the SAME surface: same struct, opposite
        // guarantee + a resolved delivery fact.
        let (_tmp2, reg2) = open_temp();
        reg2.register("fs", "mock").unwrap();
        let fs_dir = reg2
            .attach_memory("fs", MemoryBackingKind::Filesystem)
            .unwrap();
        assert!(fs_dir.is_dir());
        let fs_status = reg2.memory_status("fs").unwrap().unwrap();
        assert!(fs_status.declared);
        assert_eq!(
            fs_status.guarantee,
            crate::ports::GuaranteeLevel::ManagedDirByteDurable
        );
    }

    #[test]
    fn native_idempotent_re_attach_stays_metadata_only() {
        // Same-kind re-attach for native: idempotent success, row untouched,
        // and STILL no directory (the filesystem self-heal branch must not
        // leak into the native path).
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        reg.attach_memory("demo", MemoryBackingKind::Native)
            .unwrap();
        let before = reg
            .store
            .get_memory_backing(&InstanceName::new("demo").unwrap())
            .unwrap();
        let again = reg
            .attach_memory("demo", MemoryBackingKind::Native)
            .unwrap();
        assert!(!again.exists(), "re-attach of native creates no directory");
        assert_eq!(
            reg.store
                .get_memory_backing(&InstanceName::new("demo").unwrap())
                .unwrap(),
            before,
            "re-attach must not rewrite the row"
        );
    }

    #[test]
    fn attach_on_every_non_terminal_state_is_rejected_and_changes_nothing() {
        // AC3 + DC-3: the guard reads ONLY the persisted Lifecycle State — seeded
        // rows, no live process — and a rejection leaves NO side effect (no
        // directory, no row). Terminal-only per A-5: registered/stopped/failed are
        // permitted; running/starting/stopping/paused are all refused.
        for state in [
            LifecycleState::Running,
            LifecycleState::Starting,
            LifecycleState::Stopping,
            LifecycleState::Paused,
        ] {
            let (tmp, reg) = open_temp();
            let label = state.as_str();
            let iname = seed_in_state(&reg, "live", state);

            let err = reg
                .attach_memory("live", MemoryBackingKind::Filesystem)
                .unwrap_err();
            match &err {
                RegistryError::MemoryBackingHotSwap {
                    name,
                    state: reported,
                } => {
                    assert_eq!(name, "live");
                    assert_eq!(reported, label);
                }
                other => panic!("expected MemoryBackingHotSwap for {label}, got {other:?}"),
            }
            // NO side effect: neither the directory nor the row exists.
            assert!(
                !reg.paths().agent_memory_dir(&iname).exists(),
                "{label}: guard must reject BEFORE creating the directory"
            );
            assert!(
                reg.store.get_memory_backing(&iname).unwrap().is_none(),
                "{label}: guard must reject BEFORE persisting the row"
            );
            drop(tmp);
        }
    }

    #[test]
    fn detach_on_every_non_terminal_state_is_rejected() {
        for state in [
            LifecycleState::Running,
            LifecycleState::Starting,
            LifecycleState::Stopping,
            LifecycleState::Paused,
        ] {
            let (_tmp, reg) = open_temp();
            seed_in_state(&reg, "live", state);
            let err = reg.detach_memory("live").unwrap_err();
            assert!(
                matches!(&err, RegistryError::MemoryBackingHotSwap { name, state: s }
                    if name == "live" && s == state.as_str()),
                "detach must refuse {state:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn attach_succeeds_from_stopped_and_failed_terminal_states() {
        for state in [LifecycleState::Stopped, LifecycleState::Failed] {
            let (_tmp, reg) = open_temp();
            seed_in_state(&reg, "svc", state);
            let dir = reg
                .attach_memory("svc", MemoryBackingKind::Filesystem)
                .unwrap();
            assert!(
                dir.is_dir(),
                "{state}: attach permitted from a terminal state"
            );
            // The attachment ROW persisted (store-level read; the delivery-fact
            // read needs the adapter snapshot, which a seeded row's home
            // deliberately lacks — real instances get that coverage in the
            // register()-based tests below and in tests/memory.rs).
            assert_eq!(
                reg.store
                    .get_memory_backing(&InstanceName::new("svc").unwrap())
                    .unwrap()
                    .expect("row persisted")
                    .kind,
                MemoryBackingKind::Filesystem
            );
        }
    }

    #[test]
    fn detach_clears_the_attachment_but_leaves_directory_contents() {
        // A-4 / DC-7: detach is metadata-only; operator data survives untouched
        // and a later re-attach re-adopts it.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let dir = reg
            .attach_memory("demo", MemoryBackingKind::Filesystem)
            .unwrap();
        std::fs::write(dir.join("memory.txt"), b"operator data").unwrap();

        reg.detach_memory("demo").unwrap();
        assert!(reg.memory_status("demo").unwrap().is_none(), "row cleared");
        assert_eq!(
            std::fs::read(dir.join("memory.txt")).unwrap(),
            b"operator data",
            "detach must never touch the managed directory contents"
        );

        // Re-attaching re-adopts the existing contents (same directory).
        let again = reg
            .attach_memory("demo", MemoryBackingKind::Filesystem)
            .unwrap();
        assert_eq!(again, dir);
        assert!(dir.join("memory.txt").is_file());
    }

    #[test]
    fn detaching_when_nothing_is_attached_is_a_successful_noop() {
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        reg.detach_memory("demo").unwrap();
        assert!(reg.memory_status("demo").unwrap().is_none());
    }

    #[test]
    fn attaching_to_a_native_backing_records_the_row_without_creating_the_directory() {
        // Only `filesystem` materializes a managed directory; `native` is the
        // delegation marker (reserved for story 5-2's behavior) — the vocabulary
        // ships now so 5-2 adds behavior without an enum-shape break.
        let (_tmp, reg) = open_temp();
        reg.register("demo", "mock").unwrap();
        let dir = reg
            .attach_memory("demo", MemoryBackingKind::Native)
            .unwrap();
        assert!(!dir.exists(), "native attaches no directory");
        assert_eq!(
            reg.memory_status("demo").unwrap().unwrap().kind,
            MemoryBackingKind::Native
        );
    }

    #[test]
    fn memory_ops_report_not_found_for_unregistered_names() {
        let (_tmp, reg) = open_temp();
        let err = reg
            .attach_memory("ghost", MemoryBackingKind::Filesystem)
            .unwrap_err();
        assert!(matches!(err, RegistryError::NotFound { name } if name == "ghost"));
        let err = reg.detach_memory("ghost").unwrap_err();
        assert!(matches!(err, RegistryError::NotFound { name } if name == "ghost"));
        assert!(reg
            .memory_status("ghost")
            .unwrap_err()
            .to_string()
            .contains("ghost"));
    }

    #[test]
    fn memory_status_surfaces_an_unresolvable_mapping_as_io_not_a_panic() {
        // The DC-10 delivery fact resolves the adapter's declared mapping; when
        // that resolution FAILS (here: the manifest file vanished after
        // registration), the read must surface the typed Io error — never panic
        // and never report a fabricated delivered/undelivered fact.
        let (_tmp, reg) = open_temp();
        let manifest_dir = TempDir::new().unwrap();
        write_manifest_dir(manifest_dir.path(), VALID_MANIFEST);
        reg.register_with_adapter(
            "m",
            &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
        )
        .unwrap();
        reg.attach_memory("m", MemoryBackingKind::Filesystem)
            .unwrap();

        std::fs::remove_file(manifest_dir.path().join(crate::adapter::MANIFEST_FILE)).unwrap();

        let err = reg.memory_status("m").unwrap_err();
        assert!(matches!(err, RegistryError::Io { .. }), "got {err:?}");
    }

    #[test]
    fn memory_ops_reject_malformed_names_as_invalid_name() {
        let (_tmp, reg) = open_temp();
        let err = reg
            .attach_memory("Bad Name", MemoryBackingKind::Filesystem)
            .unwrap_err();
        assert!(matches!(err, RegistryError::InvalidName { .. }));
        let err = reg.detach_memory("Bad Name").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidName { .. }));
        assert!(matches!(
            reg.memory_status("Bad Name").unwrap_err(),
            RegistryError::InvalidName { .. }
        ));
    }

    #[test]
    fn memory_read_reports_undelivered_for_an_adapter_without_the_reserved_mapping() {
        // DC-10: the delivery fact distinguishes declared vs undeclared mappings.
        // The VALID_MANIFEST fixture declares no `[config]` section → empty
        // mapping → the reserved key is NOT targeted.
        let (_tmp, reg) = open_temp();
        let manifest_dir = TempDir::new().unwrap();
        write_manifest_dir(manifest_dir.path(), VALID_MANIFEST);
        reg.register_with_adapter(
            "m",
            &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
        )
        .unwrap();
        reg.attach_memory("m", MemoryBackingKind::Filesystem)
            .unwrap();
        let status = reg.memory_status("m").unwrap().unwrap();
        assert!(!status.declared, "no declared target ⇒ not delivered");
    }
}
