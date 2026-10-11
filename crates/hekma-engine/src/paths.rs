//! Engine path authority (spine "Filesystem layout" convention).
//!
//! The engine is the SOLE path authority: the state-dir location and the
//! Agent Home layout are computed only here. `kt`, adapters, and Hosts receive
//! paths from the API and never construct them.
//!
//! ## cfg-free rule (CRITICAL)
//!
//! The state-dir base is resolved through the [`directories`] crate, which
//! hides every OS-conditional compilation attribute inside itself. This engine
//! code therefore stays free of platform `#[cfg]` attributes, satisfying the
//! OS-cfg CI gate (which allows such attributes only under `src/backends/`).
//! Do NOT hand-roll platform branches here — reach for `directories`.
//!
//! ## Base-dir resolution order
//!
//! 1. An explicit override passed to [`EnginePaths::new`] (tests pass a
//!    `TempDir`; the registry facade threads it through).
//! 2. Else the `HEKMA_STATE_DIR` / `KTESIO_STATE_DIR` environment
//!    variables, if set — `HEKMA_STATE_DIR` is the forward-looking name
//!    (v0.8.0 Hekma rename); the legacy `KTESIO_STATE_DIR` keeps working
//!    so existing installations and scripts are unaffected. Both set to
//!    the SAME absolute path is accepted; both set to DIFFERENT paths is
//!    a hard error (an ambiguous state root must never be silently
//!    resolved). Either name makes CLI integration tests (which spawn the
//!    real binary) hermetic, mirroring the existing
//!    `KTESIO_NO_UPDATE_CHECK` / `XDG_CACHE_HOME` precedent.
//! 3. Else the platform data dir via `ProjectDirs::from("", "", "ktesio")`
//!    — the LEGACY directory name on purpose: the Hekma rename (v0.8.0)
//!    keeps every existing installation's data IN PLACE. Existing state
//!    is never moved, merged, reset, or deleted by the rename.
//!
//! ## Layout (`[ASSUMPTION]`: exact names not spine-fixed)
//!
//! Recorded CURRENT (Q-4 ruling: this module OWNS the Agent Home layout doc —
//! every story that adds an entry records it here in the same commit):
//!
//! ```text
//! <state_base>/
//!   state.db                 # the one SQLite state store (AD-6)
//!   secrets.toml             # the engine-SHARED 0600 secret store (AD-10; optional)
//!   agents/
//!     <instance_name>/       # one Agent Home per instance
//!       config.toml          # instance-level config (AD-9)
//!       adapter.json         # persisted adapter snapshot (story 1-3)
//!       effective-config.json  # resolved config + per-value provenance, written
//!                            #   at START, overwritten every start (story 2-3)
//!       <rendered files>     # native config FILE targets of a manifest `[config]`
//!                            #   mapping (story 2-2; paths are adapter-declared,
//!                            #   validated relative to the home)
//!       logs/                # per-instance logs (AD-12): instance.log (JSON-Lines
//!                            #   transitions), agent.log (raw stdout capture),
//!                            #   agent-stderr.log (raw stderr capture),
//!                            #   output.log[.1|.2] (attributed, rotated, story 4-2),
//!                            #   breaches.log (JSON-Lines budget breaches, story 3-2)
//!       memory/              # the managed Memory Backing directory (story 5-1,
//!                            #   spine AD-11 — engine-managed, survives restarts
//!                            #   byte-identically; contents are OPERATOR data the
//!                            #   engine never touches)
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use directories::ProjectDirs;

use crate::domain::InstanceName;

/// Monotonic per-process counter disambiguating atomic-write temp names inside
/// ONE process (review-1 patch 1): combined with the pid (cross-process) and
/// the writing thread's id, two threads writing the SAME target concurrently
/// can never collide on a temp path. Same discipline as `domain/usage.rs`'s
/// `RUN_NONCE` — the embed-clean audit's other named global: never read for
/// behavior, coupled to nothing, consulted only to mint a unique name; it
/// guarantees PER-PROCESS uniqueness only (cross-process uniqueness comes from
/// the pid; cross-restart residue is a dead pid's litter, never a live
/// collision).
static TEMP_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `target` ATOMICALLY (story 11-2, AI-24/AI-28): the bytes
/// land in a temporary file in the TARGET's own directory (same filesystem, so
/// the final rename cannot degrade into a copy), flushed to the device, and a
/// single [`std::fs::rename`] flips the temp over the target. A process that
/// dies at ANY point therefore leaves the target holding either the complete
/// OLD bytes or the complete NEW bytes — never a truncated half-write — which
/// is the durability contract every durable config write in the engine must
/// keep (the instance `config.toml` a `config set` persists, and the native
/// config FILE targets the start seam renders into the Agent Home).
///
/// The temp file is named `<target-file-name>.tmp-<pid>-<tid>-<seq>` (review-1
/// patch 1): same directory as the target (same-FS rename), pid-suffixed so
/// two engine processes cannot collide, and thread-id + a monotonic
/// [`TEMP_WRITE_SEQ`] counter so two THREADS of one process overwriting the
/// same target concurrently cannot collide either. On ANY failure (temp write,
/// mode copy, or rename) the helper removes its temp residue best-effort — a
/// failed write leaves the previous target bytes untouched AND no `.tmp`
/// litter behind; the original error is returned either way (the caller maps
/// it into its typed error shape). On success the temp no longer exists (it
/// IS the target).
///
/// DURABILITY BOUNDARY (honest, review-1 patch 2): the temp is `sync_all`ed
/// BEFORE the rename, so the content of whichever bytes survive is on the
/// device. Process-death atomicity is guaranteed by the rename alone; POWER
/// LOSS is best-effort at the file level — std has no portable DIRECTORY
/// fsync, so the rename's directory entry itself is not made durable, and a
/// power cut in the rename's window can leave either version (both complete)
/// on disk. This is the strongest cross-platform guarantee std offers.
///
/// PERMISSIONS (review-1 patch 3): overwriting an EXISTING target preserves
/// its Unix permissions — the target's mode is copied onto the temp before
/// the flip (so a hand-tightened `0600 config.toml` survives every re-set).
/// A target that does not exist keeps the process-default mode. On Windows
/// the copy is a no-op (no unix mode bits — the documented portable posture).
///
/// WINDOWS caveat (review-1 patch 4): a rename over a target that another
/// process holds open without `FILE_SHARE_DELETE` fails with a sharing
/// violation; the helper retries ONCE after a short backoff (the common
/// transient window) and then surfaces the honest error — the write FAILS
/// with the target untouched, where the old in-place `fs::write` would have
/// silently overwritten the bytes under the reader.
///
/// std-only by constraint (the spec forbids the `tempfile` dependency):
/// `std::fs::rename` replaces an existing destination on BOTH Unix (`rename`)
/// and Windows (`MoveFileExW` + `MOVEFILE_REPLACE_EXISTING`), so this stays
/// inside the cross-platform std API rule — the per-OS bits live behind the
/// [`crate::backends`] cfg home, keeping this module cfg-free. A target whose
/// path carries no file name is a caller bug and is rejected with a typed
/// [`std::io::Error`] instead of a panic.
pub fn write_atomically(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temp = reserve_atomic_temp(target);
    write_atomic_via(&temp, target, bytes)
}

/// Compute the temp path for the NEXT atomic write of `target`: the target's
/// directory + `<name>.tmp-<pid>-<tid>-<seq>`, unique among this process's
/// live writers ([`TEMP_WRITE_SEQ`]). The core is split out (below) so tests
/// can drive a chosen temp path deterministically without racing the counter.
fn reserve_atomic_temp(target: &Path) -> PathBuf {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let seq = TEMP_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    // `ThreadId::as_u64` is still unstable, so the thread number is taken from
    // its `ThreadId(<n>)` Debug form — only the digits reach the file name.
    let tid: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    dir.join(format!(
        "{file_name}.tmp-{}-{tid}-{seq}",
        std::process::id()
    ))
}

/// The atomic-write core ([`write_atomically`]'s composition) with the temp
/// path chosen by the caller — crate-internal so the failure-injection tests
/// (paths/registry/adapter) can pin a temp path deterministically instead of
/// racing the live [`TEMP_WRITE_SEQ`] counter.
pub(crate) fn write_atomic_via(temp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if target.file_name().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "cannot write atomically: {} names no file",
                target.to_string_lossy()
            ),
        ));
    }
    let outcome = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(temp)?;
        file.write_all(bytes)?;
        // Durability boundary (see the caller's doc): flush the temp's CONTENT
        // to the device before the flip; the directory entry itself is not
        // fsyncable portably, so power-loss atomicity stays best-effort.
        file.sync_all()?;
        drop(file);
        // Preserve an existing target's permissions across the flip (Unix; a
        // no-op where mode bits do not exist). Runs BEFORE the rename so a
        // mode-copy failure can never publish an un-adjusted temp.
        crate::backends::preserve_target_mode(target, temp)?;
        crate::backends::rename_over_target(temp, target)
    })();
    if outcome.is_err() {
        // Best-effort residue cleanup: the temp must not outlive a failed
        // write. A remove failure is swallowed — the write/rename error the
        // caller receives is the one that matters, and the cleanup must never
        // mask it.
        let _ = std::fs::remove_file(temp);
    }
    outcome
}

/// Environment override for the state-dir base (integration-test hermeticity).
/// The LEGACY name — preserved as-is so existing installations, scripts, and
/// CI jobs keep working through the Hekma rename.
pub const STATE_DIR_ENV: &str = "KTESIO_STATE_DIR";

/// The forward-looking alias for [`STATE_DIR_ENV`] (v0.8.0 Hekma rename).
/// Honored identically; see the module docs for the conflict rule when both
/// names are set.
pub const STATE_DIR_ENV_ALIAS: &str = "HEKMA_STATE_DIR";

/// File name of the SQLite state store inside the state base. `[ASSUMPTION]`
pub const STATE_DB_FILE: &str = "state.db";

/// File name of the engine SECRETS store inside the state base (story 2-4, spine
/// AD-10 "the engine secrets file, mode 0600"). A state-dir-level file (NOT
/// per-Agent-Home — it is the engine's SHARED secret store, resolving every
/// instance's `secret:NAME` references), beside [`STATE_DB_FILE`]. `[ASSUMPTION]`
/// recorded (Assumption 5): TOML `NAME = "value"` (reuses the engine's `toml`
/// dep), at `<state base>/secrets.toml`, expected mode `0600` (owner-only —
/// enforced on Unix by the backend permission check, AD-4). It is NOT a SQLite
/// blob (AD-6): secrets are files under path authority, never a DB column.
pub const SECRETS_FILE: &str = "secrets.toml";

/// Directory (under the state base) that holds all Agent Homes. `[ASSUMPTION]`
pub const AGENTS_DIR: &str = "agents";

/// File name of the per-instance config file inside an Agent Home. `[ASSUMPTION]`
pub const INSTANCE_CONFIG_FILE: &str = "config.toml";

/// File name of the persisted effective-config snapshot inside an Agent Home
/// (story 2-3, spine AD-9 "the effective-config snapshot persisted in the Agent
/// Home" + AD-6 "effective-config snapshots are files inside the Agent Home").
/// Written at START (the resolved four-layer config + per-value provenance),
/// OVERWRITTEN every start/restart. `[ASSUMPTION]` recorded (Decision 5): JSON,
/// mirroring the `adapter.json` snapshot convention — OS-portable, serializes the
/// provenance tags cleanly, and kept DISTINCT from the editable `config.toml`
/// (this file is engine-owned, read-only-to-humans, never hand-edited).
pub const EFFECTIVE_CONFIG_SNAPSHOT_FILE: &str = "effective-config.json";

/// Directory name of the managed Memory Backing inside an Agent Home (story 5-1,
/// spine AD-11 "`filesystem` — engine-managed directory inside the Agent Home;
/// survives restarts byte-identically"). The ONE true name for the directory: the
/// path is computed only through [`EnginePaths::agent_memory_dir`] (path
/// authority, conventions row) and `kt`/adapters/Hosts never join this segment
/// themselves. The engine CREATES the directory (attach + a defensive start-time
/// self-heal, each one idempotent `create_dir_all`) but NEVER touches its
/// CONTENTS — they are operator data that must survive byte-identically (DC-7).
pub const MEMORY_DIR: &str = "memory";

/// Computes engine-owned paths from a resolved state-dir base.
///
/// Construct with [`EnginePaths::new`]; every path method derives from the
/// single stored base, so there is no global or thread-local state (this keeps
/// the API facade-friendly for the async migration in story 1.4 and satisfies
/// FR-34 "no global-state collisions").
#[derive(Clone, Debug)]
pub struct EnginePaths {
    state_base: PathBuf,
}

/// Reasons the state-dir base could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// No override, neither `HEKMA_STATE_DIR` nor `KTESIO_STATE_DIR` set,
    /// and the platform data dir could not be determined (e.g. no `HOME`
    /// on Unix).
    #[error("could not determine a state directory; set {STATE_DIR_ENV} to an explicit path")]
    NoStateDir,

    /// A state-dir environment variable (`HEKMA_STATE_DIR` or
    /// `KTESIO_STATE_DIR`) was set to a relative path. A relative base
    /// would be resolved against the current working directory — a
    /// non-portable, surprising state location that would also get baked
    /// into the stored absolute Agent Home paths — so we reject it with an
    /// explicit error.
    #[error(
        "the state-dir environment variable ({STATE_DIR_ENV_ALIAS} or {STATE_DIR_ENV}) must be an absolute path, but was '{value}'; set it to an absolute path"
    )]
    RelativeStateDir {
        /// The offending (relative) value.
        value: String,
    },

    /// Both state-dir environment names are set, naming DIFFERENT
    /// directories. The state root would be ambiguous, so resolution
    /// refuses rather than silently picking a winner.
    #[error(
        "both {STATE_DIR_ENV_ALIAS} and {STATE_DIR_ENV} are set but name different state directories ('{alias}' vs '{legacy}'); set only one"
    )]
    ConflictingStateDir {
        /// The value of `HEKMA_STATE_DIR`.
        alias: PathBuf,
        /// The value of `KTESIO_STATE_DIR`.
        legacy: PathBuf,
    },
}

/// Read the state-dir base from the environment: `HEKMA_STATE_DIR`
/// (forward-looking) or `KTESIO_STATE_DIR` (legacy, unchanged through the
/// Hekma rename). `Ok(None)` when neither name is set to a non-empty
/// value. Both names set to the SAME path is accepted (either spelling);
/// both set to DIFFERENT paths is [`PathError::ConflictingStateDir`] — an
/// ambiguous state root is never silently resolved. A relative value under
/// either name is [`PathError::RelativeStateDir`].
fn state_dir_env_base() -> Result<Option<PathBuf>, PathError> {
    let alias = non_empty_env_path(STATE_DIR_ENV_ALIAS);
    let legacy = non_empty_env_path(STATE_DIR_ENV);
    let selected = match (alias, legacy) {
        (Some(alias), Some(legacy)) => {
            if alias != legacy {
                return Err(PathError::ConflictingStateDir { alias, legacy });
            }
            Some(alias)
        }
        (alias, legacy) => alias.or(legacy),
    };
    // Reject a relative env-provided base: it would resolve CWD-relative
    // and leak a non-portable path into the stored Agent Home paths. An
    // explicit override (the Some arm of EnginePaths::new) is trusted;
    // the environment is not.
    match selected {
        Some(base) if !base.is_absolute() => Err(PathError::RelativeStateDir {
            value: base.to_string_lossy().into_owned(),
        }),
        selected => Ok(selected),
    }
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

impl EnginePaths {
    /// Resolve the state-dir base and build an [`EnginePaths`].
    ///
    /// `override_base`:
    /// * `Some(path)` — use it verbatim (tests / explicit embedding).
    /// * `None` — consult `HEKMA_STATE_DIR` / `KTESIO_STATE_DIR`
    ///   ([`state_dir_env_base`]), then the platform data dir.
    pub fn new(override_base: Option<PathBuf>) -> Result<Self, PathError> {
        let state_base = match override_base {
            Some(base) => base,
            None => match state_dir_env_base()? {
                Some(base) => base,
                None => ProjectDirs::from("", "", "ktesio")
                    .map(|dirs| dirs.data_dir().to_path_buf())
                    .ok_or(PathError::NoStateDir)?,
            },
        };
        Ok(Self { state_base })
    }

    /// The resolved state-dir base (holds the DB and the `agents/` tree).
    pub fn state_base(&self) -> &Path {
        &self.state_base
    }

    /// Absolute path to the SQLite state store.
    pub fn state_db(&self) -> PathBuf {
        self.state_base.join(STATE_DB_FILE)
    }

    /// Absolute path to the engine secrets file (story 2-4, AD-10) — the
    /// state-dir-level TOML `NAME = "value"` store the 0600-file
    /// [`crate::ports::SecretResolver`] reads. Mirrors [`state_db`](Self::state_db);
    /// the engine is the SOLE path authority (AD-6). The file is optional (a
    /// missing secrets file is not an error — env may resolve every reference);
    /// only its PRESENCE triggers the permission check + lookup.
    pub fn secrets_file(&self) -> PathBuf {
        self.state_base.join(SECRETS_FILE)
    }

    /// Directory holding all Agent Homes.
    pub fn agents_dir(&self) -> PathBuf {
        self.state_base.join(AGENTS_DIR)
    }

    /// Absolute Agent Home directory for `name` (keyed by the unique name).
    ///
    /// Two distinct names always yield two distinct directories — the
    /// isolation guarantee behind FR-2 / AC3.
    pub fn agent_home(&self, name: &InstanceName) -> PathBuf {
        self.agents_dir().join(name.as_str())
    }

    /// Absolute path to an Agent Home's instance config file.
    pub fn instance_config(&self, name: &InstanceName) -> PathBuf {
        self.agent_home(name).join(INSTANCE_CONFIG_FILE)
    }

    /// Absolute path to an Agent Home's persisted effective-config snapshot
    /// (story 2-3, AD-9/AD-6). The engine is the SOLE writer (path authority);
    /// `kt`/Hosts/adapters read it back but never construct the path. Mirrors
    /// [`instance_config`](Self::instance_config), rooted at the same Agent Home.
    pub fn effective_config_snapshot(&self, name: &InstanceName) -> PathBuf {
        self.agent_home(name).join(EFFECTIVE_CONFIG_SNAPSHOT_FILE)
    }

    /// Absolute path to an Agent Home's managed Memory Backing directory (story
    /// 5-1, spine AD-11). The engine is the SOLE path authority: `kt` receives the
    /// path from the public API and never constructs it; adapters receive it via
    /// the reserved unified-config key injected at start. Mirrors
    /// [`effective_config_snapshot`](Self::effective_config_snapshot), rooted at
    /// the same Agent Home.
    pub fn agent_memory_dir(&self, name: &InstanceName) -> PathBuf {
        self.agent_home(name).join(MEMORY_DIR)
    }
}

#[cfg(test)]
pub(crate) mod tests;
