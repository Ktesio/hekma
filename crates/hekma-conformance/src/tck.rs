//! # The Conformance Test Kit (TCK) — story 6-4, FR-27
//!
//! A **compliance report**, not a test framework: the harness registers a
//! caller-provided adapter with a FRESH engine over a hermetic temp state root,
//! drives every section a live adapter can be held to, and returns a
//! machine-readable [`ConformanceReport`] whose per-section entries are
//! `pass` / `fail(reason)` / `not_applicable(reason)`. The caller's `#[test]`
//! asserts on the report — this keeps the TCK usable from any third-party
//! adapter crate's dev-tests against `hekma-conformance` alone.
//!
//! ## Applicability is derived, never hardcoded
//!
//! Every section consults the adapter's DECLARATION (the effective per-OS
//! projection + [`MeteringSource`], read from the engine's registered
//! snapshot) to decide pass/fail/not-applicable. That is what makes the same
//! harness honest for the shipping `hermes` builtin (Pause BestEffort is still
//! APPLICABLE — it must demonstrate the best-effort cause tags, not skip) and
//! for a manifest adapter (EngineObserved metering is not_applicable while it
//! declares SelfReported).
//!
//! ## Who is under test in each section (subject vs probe twins)
//!
//! Plainly: **four sections demonstrate the CALLER'S adapter itself**, and
//! **four prove the engine seam through TCK-authored probe twins**. Nothing
//! mutates the caller's adapter.
//!
//! * Caller's adapter (registered as `tck-subject`): `capability_edges`
//!   (its persisted projection), `lifecycle` (its start/stop transitions; the
//!   crash leg uses a TCK-authored twin so the caller's process is never
//!   deliberately killed), `pause` (its process is paused/resumed; the
//!   real-suspension freeze proof uses a TCK heartbeat probe), and
//!   `config_mapping` for manifest subjects (its OWN declared rules are set
//!   on it and proven through its dump artifact).
//! * TCK-authored probe twins (fresh registered instances in the same
//!   engine, each proving the SAME engine seam under the probe's own
//!   declaration): `metering_self_reported`, `metering_engine_observed`,
//!   `memory` (attach/deliver/detach on a twin declaring the reserved key),
//!   and `interaction` (echo on a twin declaring the level under test).
//!
//! A native (non-manifest) subject's config section reads not_applicable —
//! its code-declared launch offers no `--dump <path>` seam to observe
//! delivered config through.
//!
//! ## Configured runs (spec 15-3)
//!
//! [`run_conformance_with`] takes a [`TckConfig`]: a per-poll section
//! timeout (default 30s), a caller-owned state root (default: a fresh
//! hermetic tempdir), and a section subset (default: all eight —
//! [`run_conformance`] delegates to the default config byte-for-byte). A
//! subset yields FEWER report entries in the same fixed order and the same
//! shape; completeness is "complete for the selected set". A caller-owned
//! root is bound by the FRESH-ROOT CONTRACT: it must be empty (or not yet
//! exist) and is exclusively one run's — a non-empty or uncreatable root
//! fails the run's state-root stage with a named reason (stale dumps,
//! fixed-name collisions, and residual state are all refused rather than
//! inherited). Reads route through public engine seams where they exist
//! ([`Blocking::read_agent_log`] for log polls, `fleet_entry` for the
//! registered metering source, `adapter::resolve` for the `--dump` pair,
//! the engine's public path layout for every state-root-relative path); the
//! two frozen-v1 residual reads (usage-event row counts; the
//! registered-snapshot launch boolean) are documented on their helpers.
//!
//! ## Sections
//!
//! * `capability_edges` — the persisted effective projection matches the
//!   declaration the adapter actually carries (OS + stability checks).
//! * `lifecycle` — start → running → stop → stopped, full transition-event
//!   sequence, plus the CRASH leg (a `Never`-policy twin whose process exits
//!   7 past the readiness window lands `failed` with a `crashed` cause,
//!   restart_count 0).
//! * `pause` — honest per the declared level: Guaranteed (Unix) proves a REAL
//!   suspension (heartbeat freeze) + plain command causes; BestEffort proves
//!   the transitions AND the `pause-best-effort` / `resume-best-effort`
//!   qualifier causes; Unsupported checks the level BEFORE starting the
//!   subject and reports `not_applicable` with a reason naming the
//!   declaration (the engine's fail-fast `CapabilityUnsupported` is honest
//!   behavior — the DECLARATION is the input the harness tests).
//! * `config_mapping` — the SUBJECT's own declared `[config]` env rules are
//!   set on the subject before start and proven delivered through the
//!   subject's own `--dump <path>` artifact; the `agent.*` pass-through key
//!   is proven verbatim (its tail IS the env var name). A manifest that
//!   accepts no `--dump <path>` cannot prove delivery and reports `fail`
//!   naming the gap.
//! * `metering_self_reported` — a probe emits 3 sentinel batches (10 in /
//!   20 out); the ledger and Fleet totals agree exactly; a replayed
//!   sequence-0 batch does not double-count. The Fleet-equality failure
//!   reason is itself pinned by a harness test that drives the same pipeline
//!   with a LYING subject (off-sentinel token counts via the probe's
//!   `--usage-input-tokens`), so the totals check cannot be deleted
//!   silently.
//! * `metering_engine_observed` — for EngineObserved adapters only: a
//!   loopback upstream stub (Content-Length responses, the fixed 30/70/100
//!   usage body), the operator sets the real upstream, and 3 forwarded calls
//!   commit exactly.
//! * `memory` — attach → status reports the attachment AND the declared
//!   delivery fact → a probe with the same declaration receives the
//!   managed dir through its declared env var → detach clears it. Adapters
//!   that declare no `memory.dir` mapping read `not_applicable` (delivery is
//!   offered, not imposed); a declaration that cannot be re-read FAILS the
//!   section naming the cause (never a fabricated "maps nothing"). The
//!   SUBJECT's own declared delivery is proven for the shipping hermes
//!   builtin by the hermes pass (`hermes_tck.rs`: the subject's process
//!   receives the managed dir as `HERMES_HOME`) and for manifest subjects by
//!   the `--dump` subject leg, whose applicability rules are stated on
//!   [`run_memory`] (dump-less → probe leg, not penalized; dump + env target
//!   → proven on the subject; dump + non-env target → surfaced skip naming
//!   the probe twin as the proof).
//! * `interaction` — Guaranteed/BestEffort: send_input reaches a running
//!   agent (echo proof in its log); Unsupported: fails fast
//!   `CapabilityUnsupported` on a probe.
//!
//! A failed section NEVER aborts the suite: each section records its first
//! failure reason and the remaining sections still run (the report is the
//! product, not a panic).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use hekma_adapter_api::{Capability, MeteringSource, OsId, SupportLevel};
use hekma_engine::domain::InstanceName;
use hekma_engine::paths::EnginePaths;
use hekma_engine::{AdapterRef, Engine, LifecycleState, MemoryBackingKind, RestartPolicy};

/// How long any single section's poll may run by DEFAULT before it fails the
/// section (never the harness) with a timeout reason. Generous: this covers a
/// loaded CI runner plus the engine reaper's ~250ms cadence. A configured
/// run overrides it via [`TckConfig::section_timeout`] (spec 15-3). NOT the
/// whole default story: [`default_section_timeout`] scales the
/// [`TckConfig`] default up on Windows (see that fn for why), and the
/// pre-config harness calls (`run_conformance` / `run_mock_conformance`)
/// ride that default — this constant remains the fast-leg budget and the
/// fallback for unmodeled platforms.
const SECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// The platform-aware DEFAULT section budget (what [`TckConfig::default`]
/// and every pre-config harness call actually use).
///
/// Windows gets 90s. The windows-latest leg runs this same nextest matrix on
/// a smaller runner, and under contention the engine-observed metering
/// pipeline (fake helper → loopback proxy → the test process's upstream
/// stub → engine reaper commit) has repeatedly needed more than the flat
/// 30s to commit its third usage row — the Windows CI flake family charter
/// (#236), escalated 2026-10-24/25 on the loaded branches to consume every
/// nextest retry of a job (a slow RUNNER is slow for the whole job, so all
/// three retries of the same test miss the same way). The larger budget is
/// also the charter's discriminating instrument: a row that lands inside
/// 90s was contention latency; one that still never lands is a dropped
/// call the helper's bounded retry envelope owns, and the section still
/// names the failure with the same honesty. Resolved at runtime per the
/// per-OS-as-data rule — no compile-time platform selection.
fn default_section_timeout() -> Duration {
    if OsId::current() == OsId::Windows {
        Duration::from_secs(90)
    } else {
        SECTION_TIMEOUT
    }
}

/// The `agent.*` pass-through key the config-mapping section sets. The engine
/// delivers it to the env var named by the VERBATIM key tail — so env
/// `TCK_PROBE` must show up in the agent's dump.
const CONFIG_PASSTHROUGH_KEY: &str = "agent.TCK_PROBE";
const CONFIG_PASSTHROUGH_TAIL: &str = "TCK_PROBE";
/// The env value the pass-through probe sets.
const CONFIG_PASSTHROUGH_VALUE: &str = "verbatim-1";

/// The token sentinels `fake_agent --emit-usage` stamps on every event (its
/// fixed `USAGE_INPUT_TOKENS`/`USAGE_OUTPUT_TOKENS`), so ledger totals are
/// exact-match assertions.
const USAGE_INPUT_TOKENS: u64 = 10;
const USAGE_OUTPUT_TOKENS: u64 = 20;

/// The loopback upstream stub's fixed tokens (30 in / 70 out / 100 total).
const OBSERVED_PROMPT_TOKENS: u64 = 30;
const OBSERVED_COMPLETION_TOKENS: u64 = 70;
/// How many completion requests the observed probe makes.
const OBSERVED_CALLS: u64 = 3;

/// The env var the TCK's memory probe declares for the reserved `memory.dir`
/// key (deliberately NOT a copy of any engine/mock constant — proving the
/// DECLARED target is what carries the delivered path).
const MEMORY_PROBE_ENV_VAR: &str = "AGENT_MEMORY_DIR";

/// How long the crash twin's process runs before exiting (PAST the engine's
/// 300ms readiness window, so the crash lands as a reaper failure, not a
/// failed spawn).
const CRASH_AFTER_MS: u64 = 450;
/// The exit code the crash twin exits with (asserted in the failed cause).
const CRASH_WITH: i32 = 7;

// ---------------------------------------------------------------------
// Report types (the machine-readable contract)
// ---------------------------------------------------------------------

/// The report schema version. Bumped ONLY on a breaking shape change of
/// [`ConformanceReport`] (additive fields keep it); a consumer gate pins this
/// before trusting the entries.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// The outcome of ONE TCK section.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SectionResult {
    /// The section ran and every assertion held.
    Pass,
    /// The section ran and an assertion failed; carries the first failure
    /// reason. The report still completes (never aborts the suite).
    Fail {
        /// The first failing assertion's reason.
        reason: String,
    },
    /// The section does not apply to THIS adapter's declaration; carries the
    /// justification derived from the declaration (never a hardcoded skip).
    NotApplicable {
        /// Why the declaration makes this section inapplicable.
        reason: String,
    },
}

impl SectionResult {
    /// Record a failure with a formatted reason (first failure wins — a
    /// section reports ONE reason, the earliest).
    fn fail(reason: impl Into<String>) -> Self {
        Self::Fail {
            reason: reason.into(),
        }
    }

    /// `true` when the section passed.
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// One named entry in the report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionReport {
    /// Stable section id (machine-readable; e.g. `"pause"`).
    pub section: String,
    /// The outcome.
    pub result: SectionResult,
}

/// The full conformance report for one adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceReport {
    /// The report schema version ([`REPORT_SCHEMA_VERSION`]) — pinned by
    /// consuming gates before the entries are trusted.
    pub schema_version: u32,
    /// The Adapter Contract version the run was governed by — the engine's
    /// negotiated [`hekma_adapter_api::CONTRACT_VERSION`] (retro #163,
    /// finding B8). A report from a contract-gated engine can now SAY which
    /// contract major produced the pass instead of leaving a wrong-major
    /// manifest as a generic all-fail "registration failed". Additive field
    /// under the documented bump policy: `schema_version` stays put, and
    /// `#[serde(default)]` keeps archived pre-field reports (same
    /// schema_version 1) deserializing — an empty string means "produced by
    /// a pre-`contract_version` harness".
    #[serde(default)]
    pub contract_version: String,
    /// The adapter kind under test.
    pub adapter_kind: String,
    /// Per-section outcomes, in the fixed section order.
    pub sections: Vec<SectionReport>,
}

impl ConformanceReport {
    /// Look up one section's outcome by id.
    pub fn section(&self, id: &str) -> Option<&SectionResult> {
        self.sections
            .iter()
            .find(|s| s.section == id)
            .map(|s| &s.result)
    }

    /// `true` when every section is pass or not_applicable — the "conformant"
    /// verdict a third-party `#[test]` asserts. An EMPTY report is NOT
    /// conformant (the 15-3 review): "every section passed" over zero
    /// sections would be a vacuous pass — the harness never emits one (an
    /// empty selection is a named failure at the run level), and a
    /// hand-built empty report must not read conformant either.
    pub fn is_conformant(&self) -> bool {
        !self.sections.is_empty()
            && self
                .sections
                .iter()
                .all(|s| !matches!(s.result, SectionResult::Fail { .. }))
    }

    /// The named failures (section id + reason) — the human-readable triage
    /// surface.
    pub fn failures(&self) -> Vec<(&str, &str)> {
        self.sections
            .iter()
            .filter_map(|s| match &s.result {
                SectionResult::Fail { reason } => Some((s.section.as_str(), reason.as_str())),
                _ => None,
            })
            .collect()
    }
}

/// The adapter under test: a manifest directory (the third-party shape) or a
/// native builtin registered by kind (the hermes pass).
#[derive(Clone, Debug)]
pub enum TckAdapter {
    /// A manifest adapter: the path to the directory holding `adapter.toml`.
    Manifest(PathBuf),
    /// A native builtin adapter the engine already knows (e.g. `"hermes"`),
    /// registered by kind.
    Native(String),
}

impl TckAdapter {
    /// The [`AdapterRef`] the engine resolves.
    fn adapter_ref(&self) -> AdapterRef {
        match self {
            TckAdapter::Manifest(dir) => AdapterRef::Manifest(dir.clone()),
            TckAdapter::Native(kind) => AdapterRef::Native(kind.clone()),
        }
    }

    /// The adapter kind the report labels — known without resolving anything
    /// (used when registration itself failed).
    fn kind_label(&self) -> String {
        match self {
            TckAdapter::Manifest(dir) => dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned()),
            TckAdapter::Native(kind) => kind.clone(),
        }
    }

    /// The subject manifest path (manifest adapters only) — the config section
    /// reads the DECLARED mapping rules from it.
    fn manifest_path(&self) -> Option<PathBuf> {
        match self {
            TckAdapter::Manifest(dir) => Some(dir.join("adapter.toml")),
            TckAdapter::Native(_) => None,
        }
    }

    /// The declaration's mapping target for the reserved `memory.dir` key —
    /// the ONE parsed declaration read that serves every memory-section
    /// question (the 15-3 review's TOCTOU fix: `declares_memory_dir` and
    /// [`TckAdapter::memory_dir_env_target`] formerly re-read and re-parsed
    /// the manifest independently, so the section's applicability gate and
    /// its subject leg could disagree if the file changed between reads; the
    /// pipeline now calls this ONCE and threads the answer down). For a
    /// MANIFEST adapter this re-reads the caller-supplied `adapter.toml`
    /// (that file IS the declaration the engine registered — the engine has
    /// no separate persisted copy of the mapping); a re-read/parse failure is
    /// `Err` (retro #163, finding B11) — never a collapsed `None`, which
    /// would lie "maps nothing" about a declaration the harness could not
    /// read. The NATIVE arm routes through the same helper shape (no dead
    /// arm): the engine's compiled-in builtin table IS the native
    /// declaration (no I/O — cannot fail).
    fn memory_dir_target(&self) -> Result<Option<::hekma_adapter_api::ConfigTarget>, String> {
        match self {
            TckAdapter::Manifest(dir) => {
                let path = dir.join("adapter.toml");
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("could not re-read {}: {e}", path.display()))?;
                let manifest = ::hekma_adapter_api::Manifest::from_toml_str(&text)
                    .map_err(|e| format!("could not re-parse {}: {e}", path.display()))?;
                Ok(manifest
                    .config_mapping()
                    .target(::hekma_engine::domain::MEMORY_DIR_KEY)
                    .cloned())
            }
            TckAdapter::Native(kind) => Ok(::hekma_engine::adapter::native_config_mapping(kind)
                .and_then(|mapping| {
                    mapping
                        .target(::hekma_engine::domain::MEMORY_DIR_KEY)
                        .cloned()
                })),
        }
    }

    /// The env var the subject's declaration maps the reserved `memory.dir`
    /// key to, when it maps an ENV target — a thin view over the ONE parsed
    /// read ([`TckAdapter::memory_dir_target`]); same error honesty (a
    /// manifest re-read/parse failure is `Err`, never a collapsed `None`).
    /// Public for third-party kit consumers that surface their subject's
    /// declared delivery channel next to the report.
    pub fn memory_dir_env_target(&self) -> Result<Option<String>, String> {
        Ok(self
            .memory_dir_target()?
            .and_then(|target| target.env_var().map(str::to_string)))
    }
}

/// The sections' stable ids (the report's machine-readable contract).
pub mod section_ids {
    /// The persisted effective projection matches the declaration.
    pub const CAPABILITY_EDGES: &str = "capability_edges";
    /// Lifecycle transitions (start → running → stop; crash leg included).
    pub const LIFECYCLE: &str = "lifecycle";
    /// Pause/resume per the declared support level.
    pub const PAUSE: &str = "pause";
    /// Unified config keys reach the agent's native mechanism.
    pub const CONFIG_MAPPING: &str = "config_mapping";
    /// Self-reported usage batches + replay dedup.
    pub const METERING_SELF_REPORTED: &str = "metering_self_reported";
    /// Engine-observed loopback metering.
    pub const METERING_ENGINE_OBSERVED: &str = "metering_engine_observed";
    /// Memory Backing attach + delivery + detach.
    pub const MEMORY: &str = "memory";
    /// send_input delivery / unsupported fail-fast.
    pub const INTERACTION: &str = "interaction";
    /// The pseudo-section id a malformed selection reports under (the 15-3
    /// review): an empty [`SectionSelection::Only`] list fails the RUN with
    /// ONE named entry under this id instead of a vacuously-conformant empty
    /// report. Not a real section — it never appears for any valid
    /// selection, and the report's JSON shape is unchanged.
    pub const SELECTION: &str = "selection";
}

/// One selectable TCK section — the typed form of a [`section_ids`] stable
/// id (spec 15-3). Selecting sections by TYPE keeps a misconfigured run a
/// compile error instead of a runtime typo the report would have to surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TckSection {
    /// See [`section_ids::CAPABILITY_EDGES`].
    CapabilityEdges,
    /// See [`section_ids::LIFECYCLE`].
    Lifecycle,
    /// See [`section_ids::PAUSE`].
    Pause,
    /// See [`section_ids::CONFIG_MAPPING`].
    ConfigMapping,
    /// See [`section_ids::METERING_SELF_REPORTED`].
    MeteringSelfReported,
    /// See [`section_ids::METERING_ENGINE_OBSERVED`].
    MeteringEngineObserved,
    /// See [`section_ids::MEMORY`].
    Memory,
    /// See [`section_ids::INTERACTION`].
    Interaction,
}

impl TckSection {
    /// Every section, in the fixed pipeline order (the report's documented
    /// entry order — selection never reorders).
    pub const ALL: [TckSection; 8] = [
        TckSection::CapabilityEdges,
        TckSection::Lifecycle,
        TckSection::Pause,
        TckSection::ConfigMapping,
        TckSection::MeteringSelfReported,
        TckSection::MeteringEngineObserved,
        TckSection::Memory,
        TckSection::Interaction,
    ];

    /// The section's stable report id (the machine-readable contract).
    pub fn id(&self) -> &'static str {
        match self {
            TckSection::CapabilityEdges => section_ids::CAPABILITY_EDGES,
            TckSection::Lifecycle => section_ids::LIFECYCLE,
            TckSection::Pause => section_ids::PAUSE,
            TckSection::ConfigMapping => section_ids::CONFIG_MAPPING,
            TckSection::MeteringSelfReported => section_ids::METERING_SELF_REPORTED,
            TckSection::MeteringEngineObserved => section_ids::METERING_ENGINE_OBSERVED,
            TckSection::Memory => section_ids::MEMORY,
            TckSection::Interaction => section_ids::INTERACTION,
        }
    }
}

/// Which sections a configured run executes (spec 15-3). Selection is a
/// FILTER over the fixed pipeline order: the report's entries stay in that
/// order, and completeness is re-specified as "complete for the selected
/// set" — a subset run yields FEWER [`SectionReport`] entries, never a new
/// shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SectionSelection {
    /// Every section — today's behavior (the [`Default`]).
    #[default]
    All,
    /// Only the listed sections. An EMPTY list is a named misconfiguration
    /// failure, never a vacuous pass: the run returns a one-entry report
    /// under [`section_ids::SELECTION`] carrying the reason (a zero-section
    /// report would otherwise read `is_conformant() == true` vacuously — a
    /// lie). The harness's contract is a report, never a panic.
    Only(Vec<TckSection>),
}

impl SectionSelection {
    /// The selected stable section ids, in the fixed pipeline order.
    fn selected_ids(&self) -> Vec<&'static str> {
        match self {
            SectionSelection::All => TckSection::ALL.iter().map(|s| s.id()).collect(),
            SectionSelection::Only(list) => TckSection::ALL
                .iter()
                .filter(|s| list.contains(s))
                .map(|s| s.id())
                .collect(),
        }
    }
}

/// A configured conformance run (spec 15-3): today's behavior as the
/// [`Default`], with the knobs third-party CI environments asked for
/// (Fibery 173). Additive surface — [`run_conformance`] and
/// [`run_mock_conformance`] delegate to a `Default` config and are
/// byte-for-byte unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TckConfig {
    /// The per-poll budget every section's bounded wait uses (state polls,
    /// dump/log/row polls). The default is platform-aware
    /// ([`default_section_timeout`]): 30s on the fast legs, 90s on Windows
    /// (#236). A tight CI can shrink it with the usual timeout-flake
    /// trade-off.
    pub section_timeout: Duration,
    /// A caller-owned state root used AS-IS instead of the harness's fresh
    /// hermetic tempdir: `None` (the default) creates the tempdir, `Some(p)`
    /// hands `p` to the engine verbatim (no swap, no cleanup — the caller
    /// owns the root and whatever the run leaves in it). THE FRESH-ROOT
    /// CONTRACT (the 15-3 review): a configured root must be an EMPTY
    /// directory (or not yet exist — the harness's creatability probe
    /// creates it); a root that exists but is not a directory, a root that
    /// cannot be created (missing parent, unwritable), and a root holding
    /// ANY pre-existing entry each fail the run's state-root stage with a
    /// named reason. The root is EXCLUSIVELY this run's: the harness
    /// registers fixed instance names (`tck-subject`, probe twins) and
    /// writes dump artifacts at fixed paths, so a reused root would
    /// collide with prior names and let a STALE artifact (a previous
    /// run's dump or rows) false-pass this run's proofs — the fresh check
    /// closes the stale-dump false-pass, the fixed-name collisions, and
    /// the residual-state class in one rule.
    pub state_root: Option<PathBuf>,
    /// Which sections execute (the default runs all eight).
    pub sections: SectionSelection,
}

impl Default for TckConfig {
    fn default() -> Self {
        Self {
            section_timeout: default_section_timeout(),
            state_root: None,
            sections: SectionSelection::default(),
        }
    }
}

impl TckConfig {
    /// [`TckConfig::default`] — the pre-config behavior (the platform-scaled
    /// timeout, fresh hermetic tempdir, every section).
    pub fn new() -> Self {
        Self::default()
    }
}

/// The full mock/manifest conformance pass: run every section against a
/// manifest adapter and return the report. A thin wrapper over
/// [`run_conformance`].
pub fn run_mock_conformance(manifest_dir: &Path) -> ConformanceReport {
    run_conformance(&TckAdapter::Manifest(manifest_dir.to_path_buf()))
}

/// The public harness entry point: register the caller's adapter with a fresh
/// engine over a hermetic temp state root, run every applicable section, and
/// return the report. Byte-for-byte today's behavior — a thin delegation to
/// [`run_conformance_with`] over a [`Default`] [`TckConfig`] (spec 15-3's
/// additivity proof: this signature and its consumer suites never change).
pub fn run_conformance(adapter: &TckAdapter) -> ConformanceReport {
    run_conformance_with(adapter, &TckConfig::default())
}

/// [`run_conformance`] with caller configuration (spec 15-3): a custom
/// per-poll section timeout, a caller-owned state root used as-is, and/or a
/// subset of the sections. A pre-section pipeline failure still yields the
/// COMPLETE all-`fail` report — for the SELECTED set — with each stage's own
/// prefix naming where the pipeline stopped (never a blanket "registration
/// failed").
pub fn run_conformance_with(adapter: &TckAdapter, config: &TckConfig) -> ConformanceReport {
    // An EMPTY Only-list selects nothing: a zero-entry report would read
    // `is_conformant() == true` VACUOUSLY — a lie about a run that proved
    // nothing. The misconfiguration gets a NAMED, complete failure instead
    // (the 15-3 review): exactly one entry, under the `selection`
    // pseudo-id, carrying the remedy.
    if let SectionSelection::Only(list) = &config.sections {
        if list.is_empty() {
            return ConformanceReport {
                schema_version: REPORT_SCHEMA_VERSION,
                contract_version: ::hekma_adapter_api::CONTRACT_VERSION.to_string(),
                adapter_kind: adapter.kind_label(),
                sections: vec![SectionReport {
                    section: section_ids::SELECTION.to_string(),
                    result: SectionResult::fail(
                        "the section selection is empty: SectionSelection::Only([]) selects \
                         nothing, so no conformance claim can be made — pass \
                         SectionSelection::All or a non-empty list",
                    ),
                }],
            };
        }
    }
    catch_report(adapter, config, || {
        // A stage failure IS a complete all-`fail` report for the selected
        // set, not a separate error surface: the rendered stage reason just
        // fills every selected section's entry.
        conformance_inner(adapter, config)
            .map_err(|(stage, detail)| stage_failure_report(adapter, config, stage.render(&detail)))
            .unwrap_or_else(|report| report)
    })
}

/// Which pre-section pipeline stage failed (spec 15-3: stage-differentiated
/// failures). Before this, EVERY post-tempdir error rendered as "registration
/// failed" across all eight sections — a mislabel that hid where the pipeline
/// actually stopped. Every selected section's failure reason now carries the
/// stage's OWN rendered prefix, so a report consumer can tell an engine-open
/// failure from a registration failure without parsing prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Preparing the harness state root (the hermetic tempdir, or validating
    /// a caller-configured root before the engine touches it).
    StateRoot,
    /// Opening the engine over the state root.
    EngineOpen,
    /// Registering the subject adapter.
    Registration,
    /// Reading the subject's effective capability projection.
    CapabilitiesRead,
    /// Reading the registered adapter facts (the public Fleet view + the
    /// registered launch snapshot).
    SnapshotRead,
}

impl Stage {
    /// The rendered failure prefix for this stage. The Registration arm's
    /// string is the pre-15-3 reason prefix verbatim — the one compatibility
    /// point with reports consumers may have pinned.
    fn prefix(self) -> &'static str {
        match self {
            Stage::StateRoot => "state root preparation failed",
            Stage::EngineOpen => "engine open failed",
            Stage::Registration => "registration failed",
            Stage::CapabilitiesRead => "capability projection read failed",
            Stage::SnapshotRead => "registered adapter read failed",
        }
    }

    /// Render the stage's all-sections failure reason for `detail`
    /// (`"<stage prefix>: <detail>"`). Pure — unit-tested over every arm.
    fn render(self, detail: &str) -> String {
        format!("{}: {detail}", self.prefix())
    }

    /// The detail half of a stage raise, with any redundant leading
    /// restatement of THIS stage's own rendered prefix stripped — no
    /// `"stage: stage: detail"` double-stacking when a helper error already
    /// carries the stage's prefix (the 15-3 review). Helper prefixes that
    /// NAME their helper (e.g. `register: …`, the byte-parity infix) add
    /// information and stay.
    fn strip_own_prefix(self, raw: String) -> String {
        let prefix = format!("{}: ", self.prefix());
        match raw.strip_prefix(&prefix) {
            Some(rest) => rest.to_string(),
            None => raw,
        }
    }
}

/// Raise a pipeline stage with `detail`, applying [`Stage::strip_own_prefix`]
/// (the single choke point every stage raise flows through — a helper whose
/// error already restates the stage cannot double-stack it).
fn stage_err(stage: Stage, detail: String) -> (Stage, String) {
    (stage, stage.strip_own_prefix(detail))
}

/// The never-panic boundary: run `op` and, if it panics, return the complete
/// all-`fail` report for the selected set naming the panic (the payload's
/// message when it is a string, a generic note otherwise). One place — the
/// entry point flows through here. A panic is NOT a stage failure, so the
/// reason carries no stage prefix — just the honest "the harness itself
/// panicked" naming.
fn catch_report<F>(adapter: &TckAdapter, config: &TckConfig, op: F) -> ConformanceReport
where
    F: FnOnce() -> ConformanceReport,
{
    // The closure only reads `adapter`; unwind safety is immaterial because
    // the failure path never touches the captured state again — it builds a
    // fresh report naming the panic.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)) {
        Ok(report) => report,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            stage_failure_report(
                adapter,
                config,
                format!("the harness itself panicked: {message}"),
            )
        }
    }
}

/// The harness's state root: the hermetic tempdir (the default) or a
/// caller-owned root used as-is ([`TckConfig::state_root`]).
enum StateRoot {
    /// The harness-created tempdir (dropped — cleaned up — with the run).
    Hermetic(tempfile::TempDir),
    /// The caller's root: never swapped, never cleaned up by the harness.
    Caller(PathBuf),
}

impl StateRoot {
    fn path(&self) -> &Path {
        match self {
            StateRoot::Hermetic(dir) => dir.path(),
            StateRoot::Caller(root) => root,
        }
    }
}

/// Prepare the run's state root per `config` (the [`Stage::StateRoot`]
/// pipeline step): the default hermetic tempdir, or the caller's root
/// validated against the FRESH-ROOT CONTRACT (the 15-3 review) —
///
/// * a root that exists but is not a directory fails HERE, with the root
///   named;
/// * a root holding ANY pre-existing entry fails HERE (the root is
///   exclusively this run's: the harness registers fixed instance names and
///   writes dump artifacts at fixed paths, so residual state would collide
///   with prior names and could let a STALE artifact false-pass this run's
///   proofs);
/// * a root that does not exist is probed for CREATABILITY with
///   `create_dir_all` (which also creates it), so a missing parent or an
///   unwritable location fails HERE at the state-root stage — named with
///   the root — instead of deep inside the engine's open, where the reason
///   would mislead.
fn prepare_state_root(config: &TckConfig) -> Result<StateRoot, String> {
    match &config.state_root {
        None => tempfile::Builder::new()
            .prefix("ktesio-tck-")
            .tempdir()
            .map(StateRoot::Hermetic)
            .map_err(|e| e.to_string()),
        Some(root) => {
            if root.exists() && !root.is_dir() {
                return Err(format!(
                    "the configured state root {} exists but is not a directory",
                    root.display()
                ));
            }
            if root.is_dir() {
                // FRESH: any entry — a prior run's dump, state.db, or agent
                // tree — is residual state this exclusive-single-run harness
                // must not inherit (named class: stale-dump false-pass,
                // fixed-instance-name collisions, residual state).
                let nonempty = root
                    .read_dir()
                    .map_err(|e| format!("read the configured state root {}: {e}", root.display()))?
                    .next()
                    .is_some();
                if nonempty {
                    return Err(format!(
                        "the configured state root {} is not empty: a configured root is \
                         exclusively one run's state (fixed instance names and dump paths \
                         would collide with prior content, and a stale artifact could \
                         false-pass this run's proofs) — pass an empty (or not-yet-existing) \
                         directory",
                        root.display()
                    ));
                }
            } else {
                // Not yet existing: probe creatability HERE so a missing
                // parent or an unwritable location fails at the state-root
                // stage, never as a mysterious engine-open failure.
                std::fs::create_dir_all(root).map_err(|e| {
                    format!(
                        "the configured state root {} cannot be created: {e}",
                        root.display()
                    )
                })?;
            }
            Ok(StateRoot::Caller(root.clone()))
        }
    }
}

/// [`run_conformance_with`]'s pipeline; `Err` carries the failed
/// [`Stage`] plus its detail — every selected section's reason will carry
/// the stage's rendered prefix.
fn conformance_inner(
    adapter: &TckAdapter,
    config: &TckConfig,
) -> Result<ConformanceReport, (Stage, String)> {
    let state = prepare_state_root(config).map_err(|e| stage_err(Stage::StateRoot, e))?;
    let engine = Engine::open(Some(state.path().to_path_buf()))
        .map_err(|e| stage_err(Stage::EngineOpen, e.to_string()))?;
    let facade = engine.blocking();
    let timeout = config.section_timeout;

    // The `register: ` infix is byte-parity with the pre-15-3 default-path
    // reason ("registration failed: register: …" — the one compatibility
    // point beyond the pinned stage prefix; the 15-3 review).
    let registered = facade
        .register_with_adapter("tck-subject", &adapter.adapter_ref())
        .map_err(|e| stage_err(Stage::Registration, format!("register: {e}")))?;

    // The declaration under test, read from the engine's own projection (the
    // adapter as REGISTERED — the source of every applicability decision).
    let capabilities = facade
        .effective_capabilities("tck-subject")
        .map_err(|e| stage_err(Stage::CapabilitiesRead, e.to_string()))?;

    // The subject's declared `memory.dir` mapping target — the memory
    // section's applicability input, read ONCE here (the single parsed
    // declaration read; a re-read failure is carried as `Err` and FAILS the
    // memory section, retro #163 finding B11 — never a collapsed "declares
    // nothing") and THREADED down to the subject leg, so the section's gate
    // and its proof can never disagree between reads (the 15-3 TOCTOU fix).
    let memory_dir = adapter.memory_dir_target();

    // The registered adapter facts (the launchability + metering truth).
    let snapshot = read_registered_facts(&facade, state.path(), "tck-subject")
        .map_err(|e| stage_err(Stage::SnapshotRead, e))?;

    // Launchable ⇔ the registration snapshotted a resolved `launch` (present
    // AND non-null): a manifest adapter that declares `[lifecycle.start]`. A
    // native builtin (or a launchless manifest) is not launchable — the
    // harness's declaration probes still cover the shared seams.
    let launchable = snapshot.launch;

    // An EngineObserved SUBJECT cannot legally start until the operator names
    // the real upstream (the engine's fail-fast: "no upstream provider URL
    // configured"). Provision a throwaway loopback stub through the PUBLIC
    // operator-config seam so the subject's live sections can drive it — the
    // engine-observed METERING section provisions its own stub for its own
    // probe. Held for the harness's lifetime; the agent under test makes no
    // calls unless its own launch does.
    let _subject_upstream =
        if launchable && snapshot.metering_source == MeteringSource::EngineObserved {
            let stub = UpstreamStub::start();
            // A provisioning failure is not fatal here: the affected sections
            // report the engine's own fail-fast reason (machine-readable).
            let _ = facade.set_config("tck-subject", "metering.upstream_base_url", &stub.base_url);
            Some(stub)
        } else {
            None
        };

    let pause_level = capabilities
        .entries
        .iter()
        .find(|(c, _)| *c == Capability::Pause)
        .map(|(_, level)| *level);
    let interaction_level = capabilities
        .entries
        .iter()
        .find(|(c, _)| *c == Capability::Interaction)
        .map(|(_, level)| *level);

    // Section selection (spec 15-3): a FILTER over the fixed pipeline order.
    // A section not in the selected set is neither run nor reported — the
    // report is complete FOR THE SELECTED SET (fewer entries, same shape).
    let selected = config.sections.selected_ids();
    let selected = |id: &str| selected.contains(&id);

    let mut sections = Vec::new();

    if selected(section_ids::CAPABILITY_EDGES) {
        sections.push(SectionReport {
            section: section_ids::CAPABILITY_EDGES.to_string(),
            result: run_capability_edges(&facade, "tck-subject", &capabilities),
        });
    }

    if selected(section_ids::LIFECYCLE) {
        sections.push(SectionReport {
            section: section_ids::LIFECYCLE.to_string(),
            result: if launchable {
                run_lifecycle(&facade, "tck-subject", timeout)
            } else {
                SectionResult::NotApplicable {
                    reason: "the registered adapter declares no launchable [lifecycle.start] \
                             template, so no live lifecycle can be driven"
                        .to_string(),
                }
            },
        });
    }

    if selected(section_ids::PAUSE) {
        sections.push(SectionReport {
            section: section_ids::PAUSE.to_string(),
            result: run_pause_section(&facade, "tck-subject", launchable, pause_level, timeout),
        });
    }

    if selected(section_ids::CONFIG_MAPPING) {
        sections.push(SectionReport {
            section: section_ids::CONFIG_MAPPING.to_string(),
            result: if launchable {
                run_config_mapping(&facade, state.path(), adapter, timeout)
            } else {
                SectionResult::NotApplicable {
                    reason: "no launchable agent to observe the delivered config".to_string(),
                }
            },
        });
    }

    if selected(section_ids::METERING_SELF_REPORTED) {
        sections.push(SectionReport {
            section: section_ids::METERING_SELF_REPORTED.to_string(),
            result: match (&snapshot.metering_source, launchable) {
                (MeteringSource::SelfReported, true) => {
                    run_self_reported_metering(&facade, state.path(), timeout)
                }
                (_, false) => SectionResult::NotApplicable {
                    reason: "no launchable agent to emit usage".to_string(),
                },
                (other, true) => SectionResult::NotApplicable {
                    reason: format!(
                        "the declaration declares {} metering, so the self-reported section \
                         does not apply",
                        other.as_str()
                    ),
                },
            },
        });
    }

    if selected(section_ids::METERING_ENGINE_OBSERVED) {
        sections.push(SectionReport {
            section: section_ids::METERING_ENGINE_OBSERVED.to_string(),
            result: match (&snapshot.metering_source, launchable) {
                (MeteringSource::EngineObserved, true) => {
                    run_engine_observed_metering(&facade, state.path(), timeout)
                }
                (_, false) => SectionResult::NotApplicable {
                    reason: "no launchable agent to observe".to_string(),
                },
                (other, true) => SectionResult::NotApplicable {
                    reason: format!(
                        "the declaration declares {} metering, so the engine-observed section \
                         does not apply",
                        other.as_str()
                    ),
                },
            },
        });
    }

    if selected(section_ids::MEMORY) {
        sections.push(SectionReport {
            section: section_ids::MEMORY.to_string(),
            result: match &memory_dir {
                // A declaration we cannot re-read is a FAIL naming the cause —
                // never a `not_applicable` derived from a fabricated "maps
                // nothing" (retro #163, finding B11).
                Err(detail) => SectionResult::fail(format!(
                    "the declaration could not be re-read to decide the memory section's \
                     applicability: {detail}"
                )),
                Ok(memory_dir) => run_memory(
                    &facade,
                    state.path(),
                    adapter,
                    launchable,
                    memory_dir.as_ref(),
                    timeout,
                ),
            },
        });
    }

    if selected(section_ids::INTERACTION) {
        sections.push(SectionReport {
            section: section_ids::INTERACTION.to_string(),
            result: match (launchable, interaction_level) {
                (true, Some(level)) => run_interaction(&facade, level, timeout),
                (false, _) => SectionResult::NotApplicable {
                    reason: "no launchable agent to interact with".to_string(),
                },
                (true, None) => SectionResult::NotApplicable {
                    reason: "the declaration declares no interaction capability on this OS"
                        .to_string(),
                },
            },
        });
    }

    // Finalize: stop everything the harness left running — the subject and
    // any probe twin a failed section orphaned via `?` on its way out.
    stop_leftovers(&facade);

    Ok(ConformanceReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: ::hekma_adapter_api::CONTRACT_VERSION.to_string(),
        adapter_kind: registered.kind.clone(),
        sections,
    })
}

/// Stop every instance still live in the harness engine (Running, Starting,
/// Paused, or mid-Stopping). Sections stop their own probes on success; this
/// is the orphan sweep for the `?` paths and the crash twin.
fn stop_leftovers(facade: &::hekma_engine::Blocking<'_>) {
    for instance in facade.list().unwrap_or_default() {
        if matches!(
            instance.state,
            LifecycleState::Running
                | LifecycleState::Starting
                | LifecycleState::Paused
                | LifecycleState::Stopping
        ) {
            let _ = facade.stop(instance.name.as_str(), Some(Duration::from_secs(5)));
        }
    }
}

/// The report shape when a pre-section pipeline stage failed: every section
/// OF THE SELECTED SET reports the same rendered stage failure — the caller
/// sees a complete, machine-readable report (complete FOR THE SELECTED SET,
/// spec 15-3) rather than an error type. `reason` is the fully rendered
/// stage detail (or, for the panic path, the panic naming).
fn stage_failure_report(
    adapter: &TckAdapter,
    config: &TckConfig,
    reason: String,
) -> ConformanceReport {
    let kind = adapter.kind_label();
    ConformanceReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: ::hekma_adapter_api::CONTRACT_VERSION.to_string(),
        adapter_kind: kind,
        sections: config
            .sections
            .selected_ids()
            .into_iter()
            .map(|id| SectionReport {
                section: id.to_string(),
                result: SectionResult::Fail {
                    reason: reason.clone(),
                },
            })
            .collect(),
    }
}

/// The registered adapter facts: the parts the TCK's applicability decisions
/// are derived from (metering source + launch presence).
#[derive(Debug)]
struct AdapterSnapshot {
    /// The declared Metering Source.
    metering_source: MeteringSource,
    /// Whether the registration snapshotted a resolved launch (present AND
    /// non-null).
    launch: bool,
}

/// Read the registered adapter facts for `name` through PUBLIC seams where
/// they exist (spec 15-3):
///
/// * the Metering Source comes from `fleet_entry(name).metering_source` —
///   the PERSISTED registered snapshot through the public Fleet view, a
///   strictly better read than re-parsing `adapter.json` by hand;
/// * whether the registration snapshotted a launch has NO public seam (the
///   engine's `AdapterSnapshot` is private and no public API surfaces the
///   launch-boolean) — that ONE read stays direct, with the residual
///   coupling documented on [`registered_launch_present`] below.
fn read_registered_facts(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    name: &str,
) -> Result<AdapterSnapshot, String> {
    let entry = fleet_entry(facade, name)?;
    let metering_source = match entry.metering_source.as_str() {
        "self-reported" => MeteringSource::SelfReported,
        "engine-observed" => MeteringSource::EngineObserved,
        other => {
            return Err(format!(
                "the fleet entry carries an unknown metering_source {other:?}"
            ))
        }
    };
    let launch = registered_launch_present(state_root, name)?;
    Ok(AdapterSnapshot {
        metering_source,
        launch,
    })
}

/// The engine-written adapter snapshot's file name. RESIDUAL COUPLING
/// (documented, spec 15-3): the registered-snapshot launch read has no
/// public seam, so the TCK reads the engine's private-by-design
/// `adapter.json` layout directly. Frozen contract v1, intra-workspace, and
/// CI-tested (the engine's own suites break first if the layout moves);
/// a layout change breaks THIS read loudly, never silently.
const REGISTERED_SNAPSHOT_FILE: &str = "adapter.json";

/// Whether the registration snapshotted a resolved launch for `name`
/// (present AND non-null ⇔ a manifest adapter that declared
/// `[lifecycle.start]`). Read from the engine-written registered snapshot
/// through the PUBLIC path layout ([`engine_home`] — never a hand-joined
/// `agents/<name>` literal); the JSON key is the frozen-v1 snapshot shape
/// (see [`REGISTERED_SNAPSHOT_FILE`]'s residual-coupling note).
fn registered_launch_present(state_root: &Path, name: &str) -> Result<bool, String> {
    let home = engine_home(state_root, name)?;
    let text = std::fs::read_to_string(home.join(REGISTERED_SNAPSHOT_FILE))
        .map_err(|e| format!("read adapter snapshot: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse adapter snapshot: {e}"))?;
    Ok(value.get("launch").map(|l| !l.is_null()).unwrap_or(false))
}

/// The subject's Agent Home through the engine's PUBLIC path layout
/// ([`EnginePaths`]) — the spec 15-3 rule: no `agents/<name>` (or
/// `state.db`) path literal is hand-joined anywhere in the kit.
fn engine_home(state_root: &Path, name: &str) -> Result<PathBuf, String> {
    let name = InstanceName::new(name).map_err(|e| format!("instance name '{name}': {e}"))?;
    Ok(EnginePaths::new(Some(state_root.to_path_buf()))
        .map_err(|e| format!("resolve the engine path layout: {e}"))?
        .agent_home(&name))
}

// ---------------------------------------------------------------------
// Shared polling helpers (all on COMMITTED artifacts / public reads)
// ---------------------------------------------------------------------

/// Poll the committed lifecycle state (via the public read) until it reaches
/// `want`, or report timeout. `budget` is the run's configured section
/// timeout (spec 15-3) — the harness tests pass a tiny budget to prove the
/// timeout arm.
fn wait_for_state(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    want: LifecycleState,
    budget: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let state = match facade.instance_status(name) {
            Ok(status) => status.instance.state,
            Err(e) => return Err(format!("instance_status read failed: {e}")),
        };
        if state == want {
            return Ok(());
        }
        // A terminal state other than the wanted one can never become it:
        // fail fast with the actual state instead of spinning out the budget.
        if matches!(state, LifecycleState::Failed | LifecycleState::Stopped) {
            return Err(format!(
                "instance reached terminal state {state:?} while waiting for {want:?}"
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for state {want:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether the agent's dump artifact shows `env=<VAR>=<VALUE>` exactly —
/// bounded poll on a committed artifact; `false` once the budget expires (a
/// missing artifact is just "not delivered yet").
fn dump_contains_env_for(dump: &Path, var: &str, value: &str, budget: Duration) -> bool {
    let needle = format!("env={var}={value}");
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Ok(text) = std::fs::read_to_string(dump) {
            if text.lines().any(|line| line == needle) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// RESIDUAL COUPLING (documented, spec 15-3): the usage-event ROW COUNT — the
// replay-dedup proof's committed evidence — has NO public engine seam (the
// ledger reads expose token totals, not row counts). The two counters below
// query the frozen contract-v1 `usage_events` / `agent_instances` schema
// (`hekma-engine`'s store) directly. The coupling is intra-workspace and
// CI-tested: an engine schema change breaks this kit's build/tests loudly,
// never silently. What 15-3 DID fix here: the state.db path routes through
// the engine's public path layout (never a hand-joined literal), and every
// open/query error is a NAMED `Err` propagated immediately — never collapsed
// to `0` and never timed out as a misleading "have 0".

/// Open a read-only-purpose connection to the state DB through the engine's
/// public path layout. `busy_budget` bounds the connection's sqlite
/// `busy_timeout` (the 15-3 review): the engine's writer can hold the state
/// DB lock for a moment mid-commit, and a transient `SQLITE_BUSY` on a poll
/// must WAIT within the caller's budget — never fail the poll immediately
/// with a spurious "database is locked" section failure. The budget is the
/// run's configured section timeout (or the remaining poll budget), so the
/// wait is bounded by the same overall bound that bounds the poll itself.
fn open_state_db(state_root: &Path, busy_budget: Duration) -> Result<rusqlite::Connection, String> {
    let db = EnginePaths::new(Some(state_root.to_path_buf()))
        .map_err(|e| format!("resolve the engine path layout: {e}"))?
        .state_db();
    let connection =
        rusqlite::Connection::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
    connection
        .busy_timeout(busy_budget)
        .map_err(|e| format!("set busy_timeout on {}: {e}", db.display()))?;
    Ok(connection)
}

/// Count committed usage rows for `name` via a direct read-only connection to
/// the same state DB the engine commits to (committed STATE, never a guess).
/// A DB that fails to open or query is `Err` with the error named (spec
/// 15-3's error honesty: never conflated with "zero rows").
fn usage_row_count(state_root: &Path, name: &str, busy_budget: Duration) -> Result<u64, String> {
    open_state_db(state_root, busy_budget)?
        .query_row(
            "SELECT COUNT(*) FROM usage_events e \
             JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n.max(0) as u64)
        .map_err(|e| format!("count usage rows for '{name}': {e}"))
}

/// Count committed ENGINE-OBSERVED usage rows for `name` (same residual
/// coupling as [`usage_row_count`]; same error honesty; same bounded
/// busy-wait).
fn observed_row_count(state_root: &Path, name: &str, busy_budget: Duration) -> Result<u64, String> {
    open_state_db(state_root, busy_budget)?
        .query_row(
            "SELECT COUNT(*) FROM usage_events e \
             JOIN agent_instances i ON i.id = e.instance_id \
             WHERE i.name = ?1 AND e.metering_source = 'engine-observed'",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n.max(0) as u64)
        .map_err(|e| format!("count engine-observed rows for '{name}': {e}"))
}

/// Poll a committed row count (either counter) until it reaches `expected`.
/// The FIRST read error propagates IMMEDIATELY as the section's named fail
/// reason — spec 15-3's error honesty: a DB error is never hidden behind a
/// 30s "have 0" timeout. This is the shared poll both metering sections use
/// (the engine-observed section's former inline loop was deduped into it).
/// A transient BUSY lock is not an error at all: the connection's
/// `busy_timeout` (bounded by this poll's budget) waits it out inside the
/// single count call.
fn wait_for_rows_for(
    state_root: &Path,
    name: &str,
    expected: u64,
    budget: Duration,
    what: &str,
    count: fn(&Path, &str, Duration) -> Result<u64, String>,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let have = count(state_root, name, budget)?;
        if have >= expected {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for {expected} {what} (have {have})"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// [`wait_for_rows_for`] over the all-rows counter, with the run's budget.
fn wait_for_usage_rows_for(
    state_root: &Path,
    name: &str,
    expected: u64,
    budget: Duration,
) -> Result<(), String> {
    wait_for_rows_for(
        state_root,
        name,
        expected,
        budget,
        "committed usage rows",
        usage_row_count,
    )
}

/// [`wait_for_rows_for`] over the engine-observed counter, with the run's
/// budget.
fn wait_for_observed_rows_for(
    state_root: &Path,
    name: &str,
    expected: u64,
    budget: Duration,
) -> Result<(), String> {
    wait_for_rows_for(
        state_root,
        name,
        expected,
        budget,
        "engine-observed rows",
        observed_row_count,
    )
}

/// The fleet entry for `name` through the engine's O(1) keyed public read
/// (spec 15-3: replaces the kit's own linear scan over `fleet()`). The
/// entry's `metering_source` cell reads the PERSISTED registered snapshot.
fn fleet_entry(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
) -> Result<::hekma_engine::FleetEntry, String> {
    facade
        .fleet_entry(name)
        .map_err(|e| format!("fleet entry read failed: {e}"))
}

/// The agent's captured-output log lines through the PUBLIC read seam —
/// `read_agent_log`'s attributed view (spec 15-3: the kit no longer reads
/// the `agents/<name>/logs/agent.log` layout directly). The exact-line and
/// `heartbeat`-prefix checks over these texts are unchanged from the
/// file-read era; a read error is the section's named fail reason.
fn read_log_texts(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
) -> Result<Vec<String>, String> {
    // DISCARDED CURSOR, deliberately (the 15-3 review's naming rule): the
    // seam returns `(lines, cursor)` and we drop the cursor. The cursor
    // exists for `read_agent_log_since` follow-loops that want to avoid
    // rescanning; this kit's polls are bounded WHOLE-VIEW checks (a whole
    // line appears; a `heartbeat ` prefix count) over a log the harness
    // itself bounds — one section's lifetime, one subject — and
    // `read_agent_log` returns the FULL retained view (the current
    // generation PLUS any rotated predecessors, in on-disk append order),
    // so a rotation mid-poll can never hide a line these checks wait for.
    // A since-cursor continuation would be current-generation-only and
    // would need rotation-shrink handling to be no LESS safe than this
    // full-view rescan; for the counting checks (freeze-proof baselines) a
    // cursor window would be actively wrong. Dropping the cursor here is
    // the safe choice, named.
    let (lines, _cursor) = facade
        .read_agent_log(name)
        .map_err(|e| format!("read_agent_log('{name}') failed: {e}"))?;
    Ok(lines.into_iter().map(|line| line.text).collect())
}

/// Poll the agent's attributed log (the public read) until `wanted` appears
/// as a WHOLE line, or the budget expires naming what was waited for. A read
/// error propagates immediately as the named fail reason.
fn wait_for_log_line(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    wanted: &str,
    budget: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let texts = read_log_texts(facade, name)?;
        if texts.iter().any(|text| text == wanted) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "never observed {wanted:?} in {name}'s captured log"
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Count `heartbeat <n>` lines in an agent's attributed log (the public
/// read) — the real-suspension freeze proof's counter.
fn heartbeat_count(facade: &::hekma_engine::Blocking<'_>, name: &str) -> Result<usize, String> {
    Ok(read_log_texts(facade, name)?
        .iter()
        .filter(|text| text.starts_with("heartbeat "))
        .count())
}

// ---------------------------------------------------------------------
// Probe-manifest authoring (TCK-owned fixtures, built from scratch)
// ---------------------------------------------------------------------

/// The shared probe-manifest builder. Mirrors the engine tests'
/// `write_pause_manifest` / `write_observed_manifest` shapes: a fresh TOML
/// manifest whose `[lifecycle.start]` exec is the conformance `fake_agent`
/// with `args`, declaring `capabilities` per-OS, a metering source, and an
/// optional trailing `[config.*]` section body.
///
/// `pause_current_os` declares the pause level for the CURRENT OS only
/// (`None` = no pause declaration at all); `interaction_current_os` declares
/// the interaction level for the current OS, and guarantees it on the other
/// modeled OSes (probes never rely on interaction — the level only shapes the
/// projection). `contract_version` is `"1.0.0"` for every probe fixture:
/// contract v1 (story 6-6) is the frozen, negotiated surface, and the
/// engine's registration gate refuses any other major.
#[allow(clippy::too_many_arguments)]
fn write_probe_manifest(
    dir: &Path,
    kind: &str,
    args: &[&str],
    pause_current_os: Option<&str>,
    interaction_current_os: &str,
    metering_source: &str,
    contract_version: &str,
    config_section: Option<&str>,
) -> Result<PathBuf, String> {
    let bin = crate::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let os = current_os_key();
    // Pause: declared ONLY for the current OS (or not at all — the
    // unsupported-pause twin uses the OTHER-OS declaration instead).
    let pause_table = match pause_current_os {
        Some(level) => format!("{os} = \"{level}\"\n"),
        None => String::new(),
    };
    // Interaction: guaranteed on every MODELED OS except that the CURRENT OS
    // carries the requested level (so a probe can declare, say, unsupported
    // interaction at home while registering cleanly everywhere). Each key is
    // emitted exactly ONCE — the manifest parser rejects duplicate keys.
    let target_oses: &[&str] = if os == "other" {
        &["other"]
    } else {
        &["linux", "macos", "windows"]
    };
    let interaction_body = target_oses
        .iter()
        .map(|o| {
            let level = if *o == os {
                interaction_current_os
            } else {
                "guaranteed"
            };
            format!("{o} = \"{level}\"")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!(
        r#"
contract_version = "{contract_version}"

[adapter]
kind = "{kind}"

[lifecycle.start]
exec = {exec:?}
args = [{args_toml}]

[capabilities.pause]
{pause_table}
[capabilities.interaction]
{interaction_body}

[metering]
source = "{metering_source}"
{config_section}"#,
        exec = bin.to_string_lossy(),
        config_section = config_section.unwrap_or(""),
    );
    let manifest_path = dir.join("adapter.toml");
    std::fs::write(&manifest_path, &body).map_err(|e| format!("write probe manifest: {e}"))?;
    Ok(manifest_path)
}

/// The current-OS key as the manifest wire string (`linux`/`macos`/`windows`/
/// `other`), via [`OsId::as_str`] — runtime data, never cfg.
fn current_os_key() -> &'static str {
    OsId::current().as_str()
}

/// A modeled OS that is NOT the current one (for the unsupported-pause twin's
/// declaration: pause exists elsewhere, not here).
#[cfg(test)]
fn other_os_key() -> &'static str {
    OsId::MODELED
        .iter()
        .find(|os| **os != OsId::current())
        .map(OsId::as_str)
        .unwrap_or("windows")
}

// ---------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------

/// Capability edges: the engine's persisted effective projection for the
/// instance must name the running OS, carry only well-formed entries, and be
/// stable across reads (the snapshot, not a live probe).
fn run_capability_edges(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    effective: &::hekma_engine::EffectiveCapabilities,
) -> SectionResult {
    into_section_result(capability_edges_inner(facade, name, effective))
}

/// [`run_capability_edges`]'s pipeline: `Ok(())` ⇔ the projection is honest.
fn capability_edges_inner(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    effective: &::hekma_engine::EffectiveCapabilities,
) -> Result<(), String> {
    // The projection names the OS actually running.
    if effective.os != OsId::current() {
        return Err(format!(
            "effective projection names OS {:?} but the host is {:?}",
            effective.os,
            OsId::current()
        ));
    }
    // A re-read is stable (the snapshot, not a live probe).
    let again = facade
        .effective_capabilities(name)
        .map_err(|e| format!("re-read failed: {e}"))?;
    capability_edge_defect(effective, &again)
}

/// The capability-edge contract as a PURE decision over two projection reads:
/// the persisted effective projection must name the running OS and be stable
/// across reads. Unit-tested directly so the detection itself is pinned —
/// deleting the wiring cannot go green.
fn capability_edge_defect(
    effective: &::hekma_engine::EffectiveCapabilities,
    again: &::hekma_engine::EffectiveCapabilities,
) -> Result<(), String> {
    // The projection names the OS actually running.
    if effective.os != OsId::current() {
        return Err(format!(
            "effective projection names OS {:?} but the host is {:?}",
            effective.os,
            OsId::current()
        ));
    }
    if again.entries != effective.entries || again.os != effective.os {
        return Err("effective projection is not stable across reads".to_string());
    }
    Ok(())
}

/// The common section shape: a fallible pipeline reads `Ok(())` ⇔ pass, and
/// its first error string BECOMES the section's failure reason — so a failed
/// section never aborts the suite, it just names its earliest failure.
fn into_section_result(result: Result<(), String>) -> SectionResult {
    match result {
        Ok(()) => SectionResult::Pass,
        Err(reason) => SectionResult::fail(reason),
    }
}

/// Lifecycle: start → running, full event sequence on stop, and the CRASH leg
/// on a twin instance (Never policy → failed, no restart).
fn run_lifecycle(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    timeout: Duration,
) -> SectionResult {
    into_section_result(lifecycle_inner(facade, name, timeout))
}

/// [`run_lifecycle`]'s pipeline.
fn lifecycle_inner(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    timeout: Duration,
) -> Result<(), String> {
    // Start → running.
    facade
        .start(name)
        .map_err(|e| format!("start failed: {e}"))?;
    wait_for_state(facade, name, LifecycleState::Running, timeout)?;

    // Stop → stopped (bounded window; the fixture agents linger until told).
    facade
        .stop(name, Some(Duration::from_secs(5)))
        .map_err(|e| format!("stop failed: {e}"))?;
    wait_for_state(facade, name, LifecycleState::Stopped, timeout)?;

    // The full transition-event sequence for one clean run.
    let events = facade
        .transition_events(name)
        .map_err(|e| format!("transition_events failed: {e}"))?;
    let states: Vec<(LifecycleState, LifecycleState)> = events
        .iter()
        .map(|e| (e.prior_state, e.new_state))
        .collect();
    let expected = vec![
        (LifecycleState::Registered, LifecycleState::Starting),
        (LifecycleState::Starting, LifecycleState::Running),
        (LifecycleState::Running, LifecycleState::Stopping),
        (LifecycleState::Stopping, LifecycleState::Stopped),
    ];
    if states != expected {
        return Err(format!(
            "clean-run transition sequence mismatch: got {states:?}, want {expected:?}"
        ));
    }

    // The CRASH leg on a TCK-authored twin.
    run_crash_leg(facade, timeout).map_or(Ok(()), Err)
}

/// The crash leg. The harness registers a second instance from its OWN crash
/// twin manifest (`fake_agent --crash-after-ms 450 --crash-with 7` — past the
/// readiness window, exit code 7), arms a Never policy, starts it, and waits
/// for the reaper to land `failed`. Returns `None` on success, `Some(reason)`
/// on failure.
fn run_crash_leg(facade: &::hekma_engine::Blocking<'_>, timeout: Duration) -> Option<String> {
    crash_leg_inner(facade, timeout).err()
}

/// [`run_crash_leg`]'s pipeline.
fn crash_leg_inner(facade: &::hekma_engine::Blocking<'_>, timeout: Duration) -> Result<(), String> {
    let dir = tempfile::Builder::new()
        .prefix("ktesio-tck-crash-")
        .tempdir()
        .map_err(|e| format!("crash manifest tempdir: {e}"))?;
    // The crash twin declares pause for an OS OTHER than the current one (it
    // is never paused) and guarantees interaction everywhere.
    write_probe_manifest(
        dir.path(),
        "tck-crash-adapter",
        &[
            "--crash-after-ms",
            &CRASH_AFTER_MS.to_string(),
            "--crash-with",
            &CRASH_WITH.to_string(),
        ],
        None,
        "guaranteed",
        "self-reported",
        "1.0.0",
        None,
    )
    .map_err(|e| format!("crash manifest: {e}"))?;
    facade
        .register_with_adapter("tck-crash", &AdapterRef::Manifest(dir.path().to_path_buf()))
        .map_err(|e| format!("crash-leg register failed: {e}"))?;
    facade
        .set_restart_policy("tck-crash", RestartPolicy::Never)
        .map_err(|e| format!("crash-leg set_restart_policy failed: {e}"))?;
    facade
        .start("tck-crash")
        .map_err(|e| format!("crash-leg start failed: {e}"))?;
    wait_for_state(facade, "tck-crash", LifecycleState::Failed, timeout)
        .map_err(|e| format!("crash-leg never detected the crash: {e}"))?;
    let status = facade
        .instance_status("tck-crash")
        .map_err(|e| format!("crash-leg status read failed: {e}"))?;
    let events = facade
        .transition_events("tck-crash")
        .map_err(|e| format!("crash-leg event read failed: {e}"))?;
    crash_leg_defect(
        status.restart_count,
        status.failed_cause.as_deref(),
        &events,
    )
}

/// The crash-leg contract as a PURE decision over the committed evidence: a
/// Never-policy crash lands `failed` with restart_count 0, the exit code
/// preserved in the failed cause, and a terminal event carrying the `crashed`
/// cause. Unit-tested directly so the detection itself is pinned.
fn crash_leg_defect(
    restart_count: u32,
    failed_cause: Option<&str>,
    events: &[::hekma_engine::TransitionEvent],
) -> Result<(), String> {
    if restart_count != 0 {
        return Err(format!(
            "crash-leg restart_count must be 0 under a Never policy, got {restart_count}"
        ));
    }
    let cause = failed_cause.unwrap_or_default();
    if !cause.contains("code 7") {
        return Err(format!(
            "crash-leg failed cause must preserve the exit code (7), got: {cause}"
        ));
    }
    let last = events
        .last()
        .ok_or("crash-leg produced no transition events")?;
    let cause_json = serde_json::to_string(&last.cause)
        .map_err(|e| format!("crash-leg cause serialization failed: {e}"))?;
    if !cause_json.contains("crashed") {
        return Err(format!(
            "crash-leg terminal event must carry the crashed cause, got {cause_json}"
        ));
    }
    Ok(())
}

/// Pause, honest per the declared level. The level is read from the
/// DECLARATION (the `capabilities.entries` projection) BEFORE the subject even
/// starts:
/// * Guaranteed — the subject pauses/resumes with states + PLAIN command
///   causes, and the real-suspension freeze is proven on a TCK-owned probe
///   (a live 50ms heartbeat must not grow while paused).
/// * BestEffort — the subject transitions AND carries the
///   `pause-best-effort` / `resume-best-effort` qualifier causes (applicable:
///   demonstrated, never skipped).
/// * Unsupported — `NotApplicable` naming the declaration (pause + the
///   current OS + the unsupported level): the engine's failed-fast
///   `CapabilityUnsupported` behavior for this input is proven by the engine's
///   own pause suite; the DECLARATION is the TCK's input, and an adapter that
///   honestly declares "unsupported here" is CONFORMANT.
fn run_pause_section(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    launchable: bool,
    level: Option<SupportLevel>,
    timeout: Duration,
) -> SectionResult {
    // A launch-less registration has no process to suspend — the section does
    // not apply (derived from the registration shape, never a per-adapter
    // hardcode).
    if !launchable {
        return SectionResult::NotApplicable {
            reason: "the registered adapter declares no launch command, so there is no \
                     live process whose pause semantics could be demonstrated"
                .to_string(),
        };
    }
    let level = match level {
        Some(level) => level,
        None => {
            return SectionResult::NotApplicable {
                reason: "the declaration declares no pause capability on this OS".to_string(),
            }
        }
    };
    // An UNSUPPORTED declaration is honest and conformant: the section does
    // not apply (nothing to demonstrate on this OS). The reason names the
    // declaration — pause, the level, and the current OS.
    if level == SupportLevel::Unsupported {
        return SectionResult::NotApplicable {
            reason: format!(
                "the declaration declares pause 'unsupported' on {} (see its Capability \
                 Declaration), so there is no pause behavior to demonstrate here",
                current_os_key()
            ),
        };
    }

    // The WINDOWS CEILING, decided purely and honestly (unit-tested cross-OS):
    // a Guaranteed declaration on Windows cannot be demonstrated — the
    // engine's Windows process backend has no true suspension — and running
    // the best-effort assertions instead would be a FALSE PASS for a
    // Guaranteed declaration. The section reports not_applicable naming BOTH
    // the declaration and the engine limitation.
    if let PauseDemo::NotApplicable { reason } = pause_demo(level, OsId::current()) {
        return SectionResult::NotApplicable { reason };
    }

    into_section_result(pause_applicable_inner(facade, name, level, timeout))
}

/// Which pause semantics an APPLICABLE declaration can honestly demonstrate
/// on a given OS — the pure, cross-OS-testable heart of the pause section's
/// honesty rules.
#[derive(Debug)]
enum PauseDemo {
    /// The engine's signal path can prove a REAL suspension (Unix
    /// Guaranteed).
    RealSuspension,
    /// The best-effort qualifier-tag path is demonstrable (BestEffort on any
    /// OS; the demonstrated-not-skipped arm).
    BestEffort,
    /// Nothing can be honestly demonstrated: the report must name the
    /// declaration AND the engine limitation (Guaranteed on Windows).
    NotApplicable { reason: String },
}

/// [`PauseDemo`] for a declared level + host OS.
fn pause_demo(level: SupportLevel, os: OsId) -> PauseDemo {
    match (level, os) {
        (SupportLevel::Guaranteed, OsId::Windows) => PauseDemo::NotApplicable {
            reason: "the declaration declares pause 'guaranteed' on windows, but the engine's                      Windows process backend cannot prove a real suspension, so this harness                      cannot honestly demonstrate Guaranteed here (see the engine's process                      backends)"
                .to_string(),
        },
        (SupportLevel::Guaranteed, _) => PauseDemo::RealSuspension,
        // BestEffort is APPLICABLE on every OS — demonstrated via the
        // qualifier causes, never skipped.
        (SupportLevel::BestEffort, _) | (SupportLevel::Unsupported, _) => PauseDemo::BestEffort,
    }
}

/// [`run_pause_section`]'s pipeline for an APPLICABLE level (start the
/// subject, prove the declared semantics, stop exactly once at the end).
fn pause_applicable_inner(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    level: SupportLevel,
    timeout: Duration,
) -> Result<(), String> {
    facade
        .start(name)
        .map_err(|e| format!("start failed: {e}"))?;
    wait_for_state(facade, name, LifecycleState::Running, timeout)?;

    // Guaranteed on Windows never reaches here (the section gate reports it
    // not_applicable via `pause_demo`); everything else is applicable.
    let result = if matches!(
        pause_demo(level, OsId::current()),
        PauseDemo::RealSuspension
    ) {
        pause_guaranteed_inner(facade, name, timeout)
    } else {
        pause_best_effort_inner(facade, name)
    };

    // Teardown: exactly ONE stop at section end (the finishers leave the
    // instance RUNNING on both success and failure).
    let stop = facade
        .stop(name, Some(Duration::from_secs(5)))
        .map(|_| ())
        .map_err(|e| format!("stop failed: {e}"));
    result.and(stop)
}

/// The Guaranteed arm: a real suspension + clean resume with PLAIN causes,
/// and the heartbeat freeze proof (baseline → 1s watch never grows → resume
/// → growth). Does NOT stop the instance (the caller owns the single stop).
fn pause_guaranteed_inner(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    timeout: Duration,
) -> Result<(), String> {
    // 1) The SUBJECT's own pause/resume (states + plain command causes).
    let paused = facade
        .pause(name)
        .map_err(|e| format!("pause failed: {e}"))?;
    if paused.state != LifecycleState::Paused {
        return Err(format!(
            "guaranteed pause must reach paused, got {:?}",
            paused.state
        ));
    }
    facade
        .resume(name)
        .map_err(|e| format!("resume failed: {e}"))?;
    wait_for_state(facade, name, LifecycleState::Running, timeout)?;

    // 2) The REAL-suspension proof on a TCK-OWNED probe (a 50ms heartbeat the
    // caller's manifest need not carry), mirroring the engine suite's freeze
    // proof: pause → settle → baseline → 1s watch never grows → unchanged →
    // resume → growth resumes. The probe mirrors the caller's level so the
    // subject's declaration stays the tested input.
    let probe_kind = "tck-pause-probe";
    let dir = tempfile::Builder::new()
        .prefix("ktesio-tck-pause-")
        .tempdir()
        .map_err(|e| format!("probe tempdir: {e}"))?;
    write_probe_manifest(
        dir.path(),
        probe_kind,
        &["--heartbeat-ms", "50", "--linger-ms", "600000"],
        Some("guaranteed"),
        "guaranteed",
        "self-reported",
        "1.0.0",
        None,
    )
    .map_err(|e| format!("pause probe manifest: {e}"))?;
    facade
        .register_with_adapter(probe_kind, &AdapterRef::Manifest(dir.path().to_path_buf()))
        .map_err(|e| format!("pause probe register failed: {e}"))?;
    facade
        .start(probe_kind)
        .map_err(|e| format!("pause probe start failed: {e}"))?;
    wait_for_state(facade, probe_kind, LifecycleState::Running, timeout)?;

    let freeze_failure = prove_heartbeat_freeze(facade, probe_kind, timeout);

    // Teardown of the probe (subject is the caller's; it stops at section end).
    let _ = facade.stop(probe_kind, Some(Duration::from_secs(5)));

    freeze_failure.map_or(Ok(()), Err)?;

    // 3) The SUBJECT's causes are PLAIN commands (no best-effort qualifier on
    // a true suspension). Re-read the events (resume above appended more).
    let events = facade
        .transition_events(name)
        .map_err(|e| format!("event read failed: {e}"))?;
    guaranteed_cause_defect(&events)
}

/// The guaranteed-pause cause contract as a PURE decision over the committed
/// transition events: the paused and resumed events must exist and carry
/// PLAIN command causes (naming pause/resume) with NO best-effort qualifier —
/// a qualifier on a Guaranteed declaration is the engine lying about the
/// suspension. Unit-tested directly so the detection itself is pinned.
fn guaranteed_cause_defect(events: &[::hekma_engine::TransitionEvent]) -> Result<(), String> {
    let pause_evt = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Paused)
        .ok_or("no paused event after a guaranteed pause")?;
    let cause =
        serde_json::to_string(&pause_evt.cause).map_err(|e| format!("cause serialization: {e}"))?;
    if !cause.contains("\"kind\":\"command\"") || !cause.contains("pause") {
        return Err(format!(
            "guaranteed pause must be a plain command cause, got {cause}"
        ));
    }
    if cause.contains("best-effort") {
        return Err(format!(
            "guaranteed pause must carry NO best-effort qualifier: {cause}"
        ));
    }
    let resume_evt = events
        .iter()
        .find(|e| e.prior_state == LifecycleState::Paused && e.new_state == LifecycleState::Running)
        .ok_or("no paused→running event after a guaranteed resume")?;
    let resume_cause = serde_json::to_string(&resume_evt.cause)
        .map_err(|e| format!("cause serialization: {e}"))?;
    if !resume_cause.contains("\"kind\":\"command\"") || !resume_cause.contains("resume") {
        return Err(format!(
            "guaranteed resume must be a plain command cause, got {resume_cause}"
        ));
    }
    if resume_cause.contains("best-effort") {
        return Err(format!(
            "guaranteed resume must carry NO best-effort qualifier: {resume_cause}"
        ));
    }
    Ok(())
}

/// The heartbeat-freeze proof against a RUNNING probe with a live 50ms
/// heartbeat: pause → settle → baseline → 1s watch never grows → after equals
/// baseline → resume → grows again. `budget` bounds the two bounded waits (the
/// first-heartbeat startup wait and the post-SIGCONT growth wait) — the run's
/// configured section timeout. Reads the heartbeat lines through the PUBLIC
/// `read_agent_log` seam (spec 15-3). Returns `None` on success.
fn prove_heartbeat_freeze(
    facade: &::hekma_engine::Blocking<'_>,
    probe: &str,
    budget: Duration,
) -> Option<String> {
    heartbeat_freeze_inner(facade, probe, budget).err()
}

/// [`prove_heartbeat_freeze`]'s pipeline.
fn heartbeat_freeze_inner(
    facade: &::hekma_engine::Blocking<'_>,
    probe: &str,
    budget: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if heartbeat_count(facade, probe)? >= 2 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "the pause probe's heartbeat never started (no heartbeat lines in {probe}'s \
                 captured log after {}ms)",
                budget.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    facade
        .pause(probe)
        .map_err(|e| format!("probe pause failed: {e}"))?;

    std::thread::sleep(Duration::from_millis(200)); // let SIGSTOP + in-flight lines settle
    let baseline = heartbeat_count(facade, probe)?;
    let watch_until = std::time::Instant::now() + Duration::from_millis(1000);
    while std::time::Instant::now() < watch_until {
        let now = heartbeat_count(facade, probe)?;
        if now > baseline {
            return Err(format!(
                "heartbeat must NOT grow while paused (real SIGSTOP): baseline {baseline}, saw {now}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let after = heartbeat_count(facade, probe)?;
    if after != baseline {
        return Err(format!(
            "heartbeat count must be unchanged across the whole paused window: \
             {baseline} → {after}"
        ));
    }

    let resumed = facade
        .resume(probe)
        .map_err(|e| format!("probe resume failed: {e}"))?;
    if resumed.state != LifecycleState::Running {
        return Err(format!(
            "probe resume must reach running, got {:?}",
            resumed.state
        ));
    }

    // The heartbeat grows again after SIGCONT.
    let deadline = std::time::Instant::now() + budget;
    loop {
        if heartbeat_count(facade, probe)? > after {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "heartbeat must resume growing after SIGCONT (stuck at {after})"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// The BestEffort arm: the transitions happen AND the qualifier cause tags
/// are surfaced. Does NOT stop the instance (the caller owns the single stop).
fn pause_best_effort_inner(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
) -> Result<(), String> {
    facade
        .pause(name)
        .map_err(|e| format!("pause failed: {e}"))?;
    facade
        .resume(name)
        .map_err(|e| format!("resume failed: {e}"))?;
    let events = facade
        .transition_events(name)
        .map_err(|e| format!("event read failed: {e}"))?;
    best_effort_cause_defect(&events)
}

/// The best-effort pause contract as a PURE decision over the committed
/// transition events: the pause must have happened carrying the
/// `pause-best-effort` qualifier tag, and the resume the
/// `resume-best-effort` one — surfaced, never silent. Unit-tested directly
/// (missing events, wrong tags, and the honest path) so the detection itself
/// is pinned.
fn best_effort_cause_defect(events: &[::hekma_engine::TransitionEvent]) -> Result<(), String> {
    let pause_evt = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Paused)
        // A missing event is a FAILURE (never silent success).
        .ok_or("best-effort pause reported success but appended NO paused event")?;
    let cause =
        serde_json::to_string(&pause_evt.cause).map_err(|e| format!("cause serialization: {e}"))?;
    if !cause.contains("\"kind\":\"pause-best-effort\"") {
        return Err(format!(
            "best-effort pause must carry the pause-best-effort cause tag, got {cause}"
        ));
    }
    let resume_evt = events
        .iter()
        .rev()
        .find(|e| e.prior_state == LifecycleState::Paused && e.new_state == LifecycleState::Running)
        .ok_or("best-effort resume reported success but appended NO resumed event")?;
    let cause = serde_json::to_string(&resume_evt.cause)
        .map_err(|e| format!("cause serialization: {e}"))?;
    if !cause.contains("\"kind\":\"resume-best-effort\"") {
        return Err(format!(
            "best-effort resume must carry the resume-best-effort cause tag, got {cause}"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Probe-driven sections (config, metering, memory, interaction)
// ---------------------------------------------------------------------

/// Config mapping: the SUBJECT's declared `[config]` rules (unified key →
/// native mechanism) are read from the registered adapter's manifest, set on
/// the subject itself, and proven delivered through the subject's own dump
/// artifact. The `agent.*` pass-through namespace is additionally proven
/// verbatim (its tail IS the env var name, no mapping involved).
fn run_config_mapping(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    adapter: &TckAdapter,
    timeout: Duration,
) -> SectionResult {
    let Some(path) = adapter.manifest_path() else {
        // A NATIVE subject's launch is code-declared contract (the adapter
        // crate owns it — e.g. the hermes gateway argv): the harness cannot
        // author a `--dump <path>` pair into it, so delivered config has no
        // observable artifact. The section does not apply (derived from the
        // registration shape, never a per-adapter hardcode); the reserved
        // `memory.dir` delivery the declaration MAY make is proven by the
        // memory section.
        return SectionResult::NotApplicable {
            reason: "the registered adapter is a native builtin whose code-declared launch \
                     offers no `--dump <path>` seam the harness could observe delivered \
                     config through; the declared 'memory.dir' delivery, if any, is proven \
                     by the memory section"
                .to_string(),
        };
    };
    // The declaration decides applicability (pure, unit-tested): nothing
    // declared, or nothing ENV-deliverable, means there is nothing this
    // section could honestly prove.
    match config_probe_scope(&path) {
        Err(e) => SectionResult::fail(e),
        Ok(ConfigScope::Undeclared) => SectionResult::NotApplicable {
            reason: "the declaration declares no [config] rules, so there is no unified-key \
                     delivery to prove"
                .to_string(),
        },
        Ok(ConfigScope::NoEnvRules) => SectionResult::NotApplicable {
            reason: "the declaration's [config] rules target only flag/file channels, and \
                     ENV delivery is the only channel this harness can prove through the \
                     agent's dump artifact, so there is nothing provable here"
                .to_string(),
        },
        Ok(ConfigScope::Env(rules)) => into_section_result(config_mapping_inner(
            facade,
            state_root,
            &adapter.adapter_ref(),
            rules,
            timeout,
        )),
    }
}

/// What the config section can honestly prove for a manifest subject, derived
/// purely from its DECLARED `[config]` rules (unit-tested).
#[derive(Debug)]
enum ConfigScope {
    /// The manifest declares no `[config]` rules at all.
    Undeclared,
    /// Rules exist but none targets an ENV var (the only channel whose
    /// delivery the dump artifact can prove).
    NoEnvRules,
    /// The env-mapped, non-reserved rules to set and prove, as
    /// `(unified key, env var)` pairs.
    Env(Vec<(String, String)>),
}

/// [`ConfigScope`] for the manifest at `path` (read + parse errors are the
/// section's failure reasons).
fn config_probe_scope(subject_manifest: &Path) -> Result<ConfigScope, String> {
    let text = std::fs::read_to_string(subject_manifest)
        .map_err(|e| format!("read subject manifest: {e}"))?;
    let manifest = ::hekma_adapter_api::Manifest::from_toml_str(&text)
        .map_err(|e| format!("parse subject manifest: {e}"))?;
    let mapping = manifest.config_mapping();
    if mapping.is_empty() {
        return Ok(ConfigScope::Undeclared);
    }
    // The RESERVED engine-computed keys are delivery mechanisms, not
    // operator-configurable values: `memory.dir` carries the managed
    // dir (the MEMORY section's input) and `metering.base_url` carries
    // the engine's loopback listener address for an EngineObserved
    // agent (the METERING section's end-to-end proof). Their start-time
    // injection always wins over a set value, so probing them here
    // would prove nothing — the config section proves the adapter's
    // OWN documented-key delivery.
    let env_rules: Vec<(String, String)> = mapping
        .iter()
        .filter_map(|(key, target)| match target {
            ::hekma_adapter_api::ConfigTarget::Env { env } => Some((key.clone(), env.clone())),
            _ => None,
        })
        .filter(|(key, _)| {
            key != ::hekma_engine::domain::MEMORY_DIR_KEY
                && key != ::hekma_engine::domain::METERING_BASE_URL_KEY
        })
        .collect();
    if env_rules.is_empty() {
        Ok(ConfigScope::NoEnvRules)
    } else {
        Ok(ConfigScope::Env(env_rules))
    }
}

/// [`run_config_mapping`]'s pipeline for a MANIFEST subject with the given
/// env-mapped rules. `poll_budget` bounds the dump-artifact poll (the run's
/// configured section timeout; the harness tests pass a tiny budget to prove
/// the not-delivered arm).
fn config_mapping_inner(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    adapter_ref: &AdapterRef,
    declared: Vec<(String, String)>,
    poll_budget: Duration,
) -> Result<(), String> {
    // The subject IS the probe: config is set on the subject, started, and the
    // delivered env proven through the subject's own dump artifact — the
    // section exercises the adapter's OWN declaration end to end. The
    // `--dump <path>` flag the subject carries (every TCK manifest twin
    // includes it) is the delivery evidence; a subject that cannot expose the
    // delivered config fails here (the declared-but-failing row of the spec
    // matrix), never aborts the suite.
    let name = "tck-subject";

    // The subject's own `--dump <path>` pair, RESOLVED FROM SOURCE through
    // the public `adapter::resolve` seam (spec 15-3). SOURCE semantics,
    // stated plainly: the engine STARTS from the registration-time launch
    // snapshot (private — no public seam), so a manifest edited AFTER
    // registration diverges from what this read sees; if you edit a
    // manifest, re-register (the same advice the contract's versioning
    // section gives). A manifest that accepts no `--dump <path>` cannot
    // prove delivered config — the section reports the gap (the TCK does
    // not invent config delivery).
    let dump_token = resolve_launch_dump_pair(adapter_ref)?.ok_or(
        "the resolved launch carries no `--dump <path>` pair; the TCK requires \
         manifest subjects to accept `--dump <path>` so delivered config is provable",
    )?;
    // The dump path is the MANIFEST'S choice: absolute, or relative to the
    // subject's Agent Home (the process CWD the engine execs it with) — the
    // home routed through the engine's public path layout, never a
    // hand-joined `agents/<name>` literal (spec 15-3).
    let home = engine_home(state_root, name)?;
    let dump = if std::path::Path::new(&dump_token).is_absolute() {
        std::path::PathBuf::from(dump_token)
    } else {
        home.join(&dump_token)
    };
    // Only THIS run's dump can satisfy the proof (the 15-3 review): remove
    // any artifact an earlier run left at the path BEFORE start, so a dump
    // that never regenerates this run cannot false-pass the delivery checks
    // on stale bytes.
    let _ = std::fs::remove_file(&dump);

    // Set the unified keys BEFORE start (the invocation override the start
    // seam resolves into the native mechanism): every DECLARED env-mapped key
    // gets the probe value; the agent.* pass-through is delivered verbatim
    // regardless of mapping (its tail IS the env var name).
    let probe_value = "tck-model-value";
    for (key, _) in &declared {
        facade
            .set_config(name, key, probe_value)
            .map_err(|e| format!("set_config('{key}') failed: {e}"))?;
    }
    facade
        .set_config(name, CONFIG_PASSTHROUGH_KEY, CONFIG_PASSTHROUGH_VALUE)
        .map_err(|e| format!("set_config('{CONFIG_PASSTHROUGH_KEY}') failed: {e}"))?;

    facade
        .start(name)
        .map_err(|e| format!("start failed: {e}"))?;
    wait_for_state(facade, name, LifecycleState::Running, poll_budget)?;

    // Every DECLARED env rule must receive the set value under its OWN env
    // var; a miss names the dump lines that DID land (triage without a rerun)
    // plus the resolve-from-source remedy (the dump pair is re-resolved from
    // the manifest while the start uses the registration-time snapshot, so a
    // manifest edited after registration diverges exactly here).
    for (key, env) in &declared {
        if !dump_contains_env_for(&dump, env, probe_value, poll_budget) {
            return Err(format!(
                "the mapped value for '{key}' never reached the agent's environment \
                 as env={env}={probe_value} (dump at {} showed: {}) — manifest edited \
                 after registration? re-register the adapter",
                dump.display(),
                dump_env_lines(&dump)
            ));
        }
    }
    if !dump_contains_env_for(
        &dump,
        CONFIG_PASSTHROUGH_TAIL,
        CONFIG_PASSTHROUGH_VALUE,
        poll_budget,
    ) {
        return Err(format!(
            "the agent.* pass-through key was not delivered verbatim (no \
             env=TCK_PROBE=verbatim-1 line; dump at {} showed: {}) — manifest edited \
             after registration? re-register the adapter",
            dump.display(),
            dump_env_lines(&dump)
        ));
    }

    facade
        .stop(name, Some(Duration::from_secs(5)))
        .map_err(|e| format!("stop failed: {e}"))?;
    Ok(())
}

/// The token following `--dump` in a launch's args, when present. Pure over
/// the args slice — unit-tested for every missing shape.
fn dump_token_from_args(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--dump" {
            return args.next().cloned();
        }
    }
    None
}

/// The token following `--dump` in the subject's launch, resolved FROM
/// SOURCE through the public `hekma_engine::adapter::resolve` seam (spec
/// 15-3): the engine parses + validates the adapter's declaration and
/// carries its resolved `[lifecycle.start]` launch. A resolve failure is a
/// NAMED fail reason (the I/O matrix's "--dump args check" row). See
/// [`config_mapping_inner`]'s comment for the source-vs-registered-snapshot
/// semantics (post-registration manifest edits diverge; re-register).
fn resolve_launch_dump_pair(adapter_ref: &AdapterRef) -> Result<Option<String>, String> {
    let resolved = ::hekma_engine::adapter::resolve(adapter_ref)
        .map_err(|e| format!("resolve the adapter's declaration: {e}"))?;
    Ok(resolved
        .launch()
        .and_then(|l| dump_token_from_args(&l.args)))
}

/// The env lines the dump artifact actually carries (bounded sample) — or an
/// explicit note when it carries none — for failure-reason triage.
fn dump_env_lines(dump: &Path) -> String {
    match std::fs::read_to_string(dump) {
        Ok(text) => {
            let lines: Vec<&str> = text
                .lines()
                .filter(|line| line.starts_with("env="))
                .take(5)
                .collect();
            if lines.is_empty() {
                "no env= lines".to_string()
            } else {
                lines.join("; ")
            }
        }
        Err(_) => "the dump file does not exist".to_string(),
    }
}

/// How many ledger rows the self-reported probe's sentinel script must
/// produce (3 distinct batches; the replayed sequence-0 line dedups away).
const EXPECTED_SELF_REPORTED_ROWS: u64 = 3;

/// The replay-dedup contract as a PURE decision over the committed row count:
/// the ledger must hold EXACTLY the expected rows — an over-count means a
/// replayed batch double-counted; an under-count means rows were lost.
/// Unit-tested directly so the detection itself is pinned.
fn replay_row_defect(rows: u64) -> Result<(), String> {
    if rows != EXPECTED_SELF_REPORTED_ROWS {
        return Err(format!(
            "a replayed batch must not add a row: expected exactly \
             {EXPECTED_SELF_REPORTED_ROWS} usage rows, got {rows}"
        ));
    }
    Ok(())
}

/// The Fleet-equality contract as a PURE decision over the observed Fleet
/// cells: totals must equal `batches ×` the per-event sentinels EXACTLY (the
/// FR-22 discipline — a fabricated or dropped cell is a billing lie), and the
/// metering-source cell must read the wire string the section ran under.
/// Unit-tested directly (including the mismatched-source arm a healthy engine
/// cannot exhibit) so the detection itself is pinned.
fn fleet_totals_defect(
    cumulative_input: u64,
    cumulative_output: u64,
    batches: u64,
    metering_source: &str,
) -> Result<(), String> {
    let want_in = batches * USAGE_INPUT_TOKENS;
    let want_out = batches * USAGE_OUTPUT_TOKENS;
    if cumulative_input != want_in || cumulative_output != want_out {
        return Err(format!(
            "fleet totals must equal the ledger exactly: got ({cumulative_input}, \
             {cumulative_output}), want ({want_in}, {want_out})"
        ));
    }
    if metering_source != "self-reported" {
        return Err(format!(
            "fleet metering_source must be 'self-reported', got {metering_source:?}"
        ));
    }
    Ok(())
}

/// Self-reported metering: a probe emits 3 sentinel batches (10 in / 20 out);
/// the ledger gains EXACTLY 3 rows; a replayed sequence-0 batch does not
/// double-count; Fleet totals equal the ledger exactly.
fn run_self_reported_metering(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    timeout: Duration,
) -> SectionResult {
    into_section_result(self_reported_metering_inner(facade, state_root, timeout))
}

/// [`run_self_reported_metering`]'s pipeline.
fn self_reported_metering_inner(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    timeout: Duration,
) -> Result<(), String> {
    self_reported_metering_inner_with(facade, state_root, timeout, &[])
}

/// [`self_reported_metering_inner`] with EXTRA probe args — the seam the
/// failure-reason tests ride to drive the very same pipeline with a LYING
/// subject (off-sentinel token counts via `fake_agent --usage-input-tokens`)
/// without weakening the honest section run.
fn self_reported_metering_inner_with(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    timeout: Duration,
    extra_probe_args: &[&str],
) -> Result<(), String> {
    let probe_kind = "tck-meter-probe";
    let dir = tempfile::Builder::new()
        .prefix("ktesio-tck-meter-")
        .tempdir()
        .map_err(|e| format!("probe tempdir: {e}"))?;
    // ONE probe proves BOTH halves (landing + dedup): `--emit-usage 3` emits
    // the 3 distinct events, `--replay-usage` re-sends sequence 0 afterward.
    let mut probe_args = vec![
        "--emit-usage",
        "3",
        "--replay-usage",
        "--linger-ms",
        "600000",
    ];
    probe_args.extend_from_slice(extra_probe_args);
    write_probe_manifest(
        dir.path(),
        probe_kind,
        &probe_args,
        None,
        "guaranteed",
        "self-reported",
        "1.0.0",
        None,
    )?;
    facade
        .register_with_adapter(probe_kind, &AdapterRef::Manifest(dir.path().to_path_buf()))
        .map_err(|e| format!("probe register failed: {e}"))?;
    facade
        .start(probe_kind)
        .map_err(|e| format!("probe start failed: {e}"))?;

    // The proof itself, isolated so the STOP below runs on EVERY exit: an
    // Err from any `?` step used to return PAST the trailing stop and leak
    // the probe process for its whole linger window — the helper batteries
    // drive exactly these failure arms on purpose (a lying subject, a
    // never-echoing probe), and their helper engines are deliberately
    // `Box::leak`ed (they never drop, so kill-on-drop cleanup never fires).
    // Stopping on the Err path changes nothing observable for the caller —
    // the same Err is returned — it only ends the fixture.
    let outcome = (|| {
        // The 3 DISTINCT events commit; the replayed sequence-0 line is then
        // drained as a no-op. Wait for 3 rows, settle past the replay drain,
        // and require the count to STAY exactly 3 (an absolute count — a
        // floor-poll would hide a genuine over-count). A row-count read
        // error propagates immediately as this section's named fail reason
        // (spec 15-3).
        wait_for_usage_rows_for(state_root, probe_kind, EXPECTED_SELF_REPORTED_ROWS, timeout)?;
        std::thread::sleep(Duration::from_millis(800));
        let rows = usage_row_count(state_root, probe_kind, timeout)?;
        replay_row_defect(rows)?;

        // Fleet totals EQUAL the ledger exactly: 3 × (10 in, 20 out), and
        // the metering-source cell reads the wire string the section ran
        // under.
        let entry = fleet_entry(facade, probe_kind)?;
        fleet_totals_defect(
            entry.usage.cumulative_input_tokens,
            entry.usage.cumulative_output_tokens,
            EXPECTED_SELF_REPORTED_ROWS,
            &entry.metering_source,
        )
    })();
    let _ = facade.stop(probe_kind, Some(Duration::from_secs(5)));
    outcome
}

/// Engine-observed metering: the operator sets the real upstream
/// (`metering.upstream_base_url`); the engine's loopback proxy forwards the
/// agent's calls and commits observed rows. The upstream is a loopback TCP
/// stub (pure std) serving the known 30/70/100 usage with a proper
/// Content-Length.
fn run_engine_observed_metering(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    timeout: Duration,
) -> SectionResult {
    into_section_result(engine_observed_metering_inner(facade, state_root, timeout))
}

/// [`run_engine_observed_metering`]'s pipeline.
fn engine_observed_metering_inner(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let probe_kind = "tck-obs-probe";
    let dir = tempfile::Builder::new()
        .prefix("ktesio-tck-obs-")
        .tempdir()
        .map_err(|e| format!("probe tempdir: {e}"))?;
    // The observed probe: contract "1.0.0" (contract v1, which carries
    // `metering.base_url`), the observed-call flags, the
    // `metering.base_url` → env `OPENAI_BASE_URL` mapping the engine's
    // injection rides, and the `model` mapping.
    let config_section = concat!(
        "\n[config.\"metering.base_url\"]\nenv = \"OPENAI_BASE_URL\"\n",
        "\n[config.model]\nenv = \"MODEL\"\n"
    );
    let observed_calls = OBSERVED_CALLS.to_string();
    write_probe_manifest(
        dir.path(),
        probe_kind,
        &["--observed-calls", &observed_calls, "--linger-ms", "600000"],
        None,
        "guaranteed",
        "engine-observed",
        "1.0.0",
        Some(config_section),
    )?;
    facade
        .register_with_adapter(probe_kind, &AdapterRef::Manifest(dir.path().to_path_buf()))
        .map_err(|e| format!("probe register failed: {e}"))?;

    // The loopback upstream stub (the same shape the engine's own test runs).
    let stub = UpstreamStub::start();

    // Operator config: the real upstream (the engine forwards there).
    facade
        .set_config(probe_kind, "metering.upstream_base_url", &stub.base_url)
        .map_err(|e| format!("set upstream url failed: {e}"))?;
    facade
        .start(probe_kind)
        .map_err(|e| format!("probe start failed: {e}"))?;

    // Wait for the observed rows to commit (the reaper drains the listener) —
    // through the SAME shared poll as the self-reported section (spec 15-3
    // deduped the former inline loop), so a row-count read error propagates
    // immediately instead of timing out as "have 0".
    wait_for_observed_rows_for(state_root, probe_kind, OBSERVED_CALLS, timeout)?;

    // Fleet totals EQUAL the ledger exactly: N × (30 in, 70 out).
    let entry = fleet_entry(facade, probe_kind)?;
    let want_in = OBSERVED_CALLS * OBSERVED_PROMPT_TOKENS;
    let want_out = OBSERVED_CALLS * OBSERVED_COMPLETION_TOKENS;
    if entry.usage.cumulative_input_tokens != want_in
        || entry.usage.cumulative_output_tokens != want_out
    {
        return Err(format!(
            "observed fleet totals must equal {OBSERVED_CALLS} × ({OBSERVED_PROMPT_TOKENS}, \
             {OBSERVED_COMPLETION_TOKENS}), got ({}, {})",
            entry.usage.cumulative_input_tokens, entry.usage.cumulative_output_tokens
        ));
    }
    if entry.metering_source != "engine-observed" {
        return Err(format!(
            "fleet metering_source must be 'engine-observed', got {:?}",
            entry.metering_source
        ));
    }
    if !stub.served_sufficient() {
        return Err("the upstream stub never served the forwarded calls".to_string());
    }

    let _ = facade.stop(probe_kind, Some(Duration::from_secs(5)));
    Ok(())
}

/// Memory Backing: attach → status reports the attachment AND the declared
/// delivery fact → a probe twin with the same declaration RECEIVES the
/// managed dir through ITS declared env var → detach clears it. NotApplicable
/// when the adapter declares no `memory.dir` mapping (delivery is offered,
/// not imposed) or registers nothing launchable.
///
/// Spec 15-3 (Fibery 174) adds the manifest-subject `--dump` leg: a MANIFEST
/// subject whose launch carries the `--dump <path>` convention ALSO gets its
/// own delivery proven on the subject itself (the subject's dump must echo
/// the managed dir through the subject's OWN declared env var) — closing the
/// gap where only the hermes builtin's subject delivery had a proof leg.
/// The leg's applicability rules, stated together (the 15-3 review):
///
/// * dump-LESS manifest (or a native builtin) → the leg is a documented,
///   surfaced SKIP; the probe twin's env leg is the delivery proof (not
///   penalized — no dump seam is not a conformance failure);
/// * dump-carrying + `memory.dir` mapped to an ENV target → the leg RUNS and
///   the subject's own delivery is proven (a swallowed var fails the
///   section);
/// * dump-carrying + `memory.dir` mapped to a NON-env target → the leg
///   SKIPS with the named reason (the dump echo has no env var to observe;
///   the probe twin remains the proof) — a SKIP, never a FAIL: pre-15-3
///   this shape passed, and the spec's Never clause keeps WHAT the section
///   verifies unchanged.
fn run_memory(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    adapter: &TckAdapter,
    launchable: bool,
    memory_dir: Option<&::hekma_adapter_api::ConfigTarget>,
    timeout: Duration,
) -> SectionResult {
    if !launchable {
        return SectionResult::NotApplicable {
            reason: "no launchable agent to observe the delivered memory path".to_string(),
        };
    }
    // The declaration is the input: an adapter that maps no `memory.dir` key
    // declares NO Memory Backing delivery, so there is nothing to demonstrate
    // (the engine must not impose delivery on it — DC-10 / the honesty rule).
    if memory_dir.is_none() {
        return SectionResult::NotApplicable {
            reason: "the declaration maps no reserved 'memory.dir' key, so Memory Backing \
                     delivery is not declared (delivery is offered, not imposed)"
                .to_string(),
        };
    }
    into_section_result(memory_inner(
        facade, state_root, adapter, memory_dir, timeout,
    ))
}

/// The subject memory leg's outcome: proven on the subject itself, or
/// skipped with the surfaced reason (the probe twin remains the delivery
/// proof — a skip is honest, a silent no-op is not, and a skip is never a
/// failure: none of the skip shapes is a conformance defect).
#[derive(Debug, PartialEq, Eq)]
enum SubjectLegOutcome {
    /// The subject's own dump echoed the managed dir through its declared
    /// env var.
    Proven,
    /// The leg did not apply, with the honest reason (surfaced as a stderr
    /// note by the caller — the report's JSON shape has no note channel on
    /// `pass`).
    Skipped { reason: String },
}

/// [`run_memory`]'s pipeline (the declaration gates already passed).
/// `poll_budget` bounds the dump-artifact polls (the run's configured section
/// timeout; the harness tests pass a tiny budget to prove the not-delivered
/// arm).
fn memory_inner(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    adapter: &TckAdapter,
    memory_dir: Option<&::hekma_adapter_api::ConfigTarget>,
    poll_budget: Duration,
) -> Result<(), String> {
    // Attach BEFORE any start (hot-swap is rejected by design). The SECTION's
    // fixture is the probe twin — but the attach/status/detach contract is
    // proven on the probe itself (a fresh registered instance in this engine).
    let probe_kind = "tck-memory-probe";
    let dir = tempfile::Builder::new()
        .prefix("ktesio-tck-memory-")
        .tempdir()
        .map_err(|e| format!("probe tempdir: {e}"))?;
    let dump = state_root.join("tck-memory-dump.txt");
    let dump_str = dump.to_string_lossy().into_owned();
    // The probe DECLARES the reserved key → its own env var (never a mock
    // constant), so the dump proof checks THAT var only.
    let config_section = format!("\n[config.\"memory.dir\"]\nenv = \"{MEMORY_PROBE_ENV_VAR}\"\n");
    write_probe_manifest(
        dir.path(),
        probe_kind,
        &["--dump", &dump_str, "--linger-ms", "600000"],
        None,
        "guaranteed",
        "self-reported",
        "1.0.0",
        Some(&config_section),
    )?;
    facade
        .register_with_adapter(probe_kind, &AdapterRef::Manifest(dir.path().to_path_buf()))
        .map_err(|e| format!("probe register failed: {e}"))?;

    // Attach BEFORE any start (hot-swap is rejected by design).
    let managed = facade
        .attach_memory(probe_kind, MemoryBackingKind::Filesystem)
        .map_err(|e| format!("attach_memory failed: {e}"))?;
    let status = facade
        .memory_status(probe_kind)
        .map_err(|e| format!("memory_status failed: {e}"))?
        .ok_or("memory_status read None after attach")?;
    if status.kind != MemoryBackingKind::Filesystem || status.dir != managed {
        return Err("memory_status does not report the attached backing".to_string());
    }
    // The probe declares the reserved key → delivery IS declared.
    if !status.declared {
        let _ = facade.detach_memory(probe_kind);
        return Err(
            "the probe declares a mapping for the reserved 'memory.dir' key, so \
             MemoryBackingStatus.declared must read true"
                .to_string(),
        );
    }

    // Idempotence: a same-kind re-attach is a contract-guaranteed success
    // that keeps the path — a rejection (or a different path) Fails the
    // section, never swallowed.
    let again = facade
        .attach_memory(probe_kind, MemoryBackingKind::Filesystem)
        .map_err(|e| format!("idempotent re-attach failed: {e}"))?;
    if again != managed {
        let _ = facade.detach_memory(probe_kind);
        return Err("re-attach returned a different path".to_string());
    }

    // The delivery proof: start the probe and poll its dump for the declared
    // env var receiving the managed path.
    if let Err(e) = facade.start(probe_kind) {
        let _ = facade.detach_memory(probe_kind);
        return Err(format!("probe start failed: {e}"));
    }
    // EXACT-value proof: the declared env var carries precisely the managed
    // path the engine attached (not merely some value with that prefix).
    let managed_str = managed.to_string_lossy().into_owned();
    if !dump_contains_env_for(&dump, MEMORY_PROBE_ENV_VAR, &managed_str, poll_budget) {
        let _ = facade.stop(probe_kind, Some(Duration::from_secs(5)));
        let _ = facade.detach_memory(probe_kind);
        return Err(format!(
            "the agent never received the managed memory path via its declared env \
             var {MEMORY_PROBE_ENV_VAR} (expected env={MEMORY_PROBE_ENV_VAR}={managed_str}; \
             dump at {} showed: {})",
            dump.display(),
            dump_env_lines(&dump)
        ));
    }
    let _ = facade.stop(probe_kind, Some(Duration::from_secs(5)));

    // Detach clears the attachment.
    facade
        .detach_memory(probe_kind)
        .map_err(|e| format!("detach_memory failed: {e}"))?;
    if facade
        .memory_status(probe_kind)
        .map_err(|e| format!("memory_status failed: {e}"))?
        .is_some()
    {
        return Err("memory_status still reports an attachment".to_string());
    }

    // The manifest-subject `--dump` leg (spec 15-3, Fibery 174): the probe
    // twin above proves the ENGINE seam; this proves the SUBJECT's own
    // declared delivery when the subject carries the `--dump <path>`
    // convention. The skip shapes are surfaced, never silent.
    match subject_memory_delivery(facade, state_root, adapter, memory_dir, poll_budget)? {
        SubjectLegOutcome::Proven => {}
        SubjectLegOutcome::Skipped { reason } => {
            // Surfaced, not silent: the report's JSON shape is frozen (no
            // note channel on `pass`), so the skip note goes to stderr —
            // the surface a `#[test]` operator actually watches. The
            // section still passes: the probe leg proved the engine seam,
            // and none of the skip shapes is a conformance defect.
            eprintln!("note: tck memory subject leg skipped: {reason}");
        }
    }
    Ok(())
}

/// Poll the committed lifecycle state until `name` is terminal for attach
/// purposes — anything but a LIVE state (Starting/Running/Paused/Stopping):
/// Registered (never started), Stopped, and Failed all allow an attach. The
/// subject memory leg terminalizes the subject BEFORE attaching (the 15-3
/// review): an orphaned RUNNING subject left by an earlier section's `?` path
/// must not misattribute the engine's by-design hot-swap refusal to the
/// memory section.
fn wait_for_terminal(
    facade: &::hekma_engine::Blocking<'_>,
    name: &str,
    budget: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let state = facade
            .instance_status(name)
            .map_err(|e| format!("instance_status read failed: {e}"))?
            .instance
            .state;
        if !matches!(
            state,
            LifecycleState::Starting
                | LifecycleState::Running
                | LifecycleState::Paused
                | LifecycleState::Stopping
        ) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "the subject never reached a terminal state before the memory attach \
                 (stuck in {state:?})"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The SUBJECT's own declared `memory.dir` delivery, proven through the
/// `--dump <path>` seam the adapter's launch declares (the convention
/// `docs/adapter-contract.md` documents for manifest adapters): terminalize
/// the subject, attach a filesystem backing, start it, and require the
/// subject's dump to echo the managed dir under the subject's OWN declared
/// env var — then stop and detach. The leg's skip shapes (native subject,
/// dump-less launch, non-env `memory.dir` target) return
/// [`SubjectLegOutcome::Skipped`] with the named reason — the probe twin
/// already proved the engine seam, and a skip is never a failure.
fn subject_memory_delivery(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    adapter: &TckAdapter,
    memory_dir: Option<&::hekma_adapter_api::ConfigTarget>,
    poll_budget: Duration,
) -> Result<SubjectLegOutcome, String> {
    let name = "tck-subject";
    let TckAdapter::Manifest(_) = adapter else {
        // A NATIVE subject's launch is code-declared contract (no harness-
        // authorable `--dump` pair); the shipping builtin's own subject
        // delivery is proven by the engine's hermes pass over its shim seam.
        return Ok(SubjectLegOutcome::Skipped {
            reason: "the subject is a native builtin whose code-declared launch offers no \
                     --dump seam; the engine-side hermes pass proves the shipping \
                     builtin's own subject delivery"
                .to_string(),
        });
    };
    // Resolve-from-source, the same seam + semantics the config section uses
    // (a post-registration manifest edit diverges; re-register).
    let Some(dump_token) = resolve_launch_dump_pair(&adapter.adapter_ref())? else {
        // No dump seam declared: the probe leg above already proved delivery
        // on a twin with the same declaration shape.
        return Ok(SubjectLegOutcome::Skipped {
            reason: "the subject's launch declares no --dump pair, so its own delivery \
                     has no dump-observable channel; the probe twin's env leg remains \
                     the delivery proof"
                .to_string(),
        });
    };
    // The subject's own declared env target for the reserved key — taken
    // from the SAME single parsed read that gated the section (no re-read,
    // no TOCTOU between the gate and the proof).
    let Some(env_var) = memory_dir.and_then(|target| target.env_var()) else {
        // Dump-carrying + NON-env `memory.dir` target: the dump echo has no
        // env var to observe. A SKIP with the named reason, never a FAIL —
        // pre-15-3 this shape passed, and WHAT the section verifies does not
        // change (the spec's Never clause; the 15-3 review).
        return Ok(SubjectLegOutcome::Skipped {
            reason: "the subject's declaration maps 'memory.dir' to a non-env target, so \
                     its own delivery has no dump-observable channel; the probe twin's \
                     env leg remains the delivery proof"
                .to_string(),
        });
    };

    // Terminalize BEFORE attach (stop is idempotent): an orphaned RUNNING
    // subject must not surface as a memory-section attach failure.
    let _ = facade.stop(name, Some(Duration::from_secs(5)));
    wait_for_terminal(facade, name, poll_budget)?;
    let managed = facade
        .attach_memory(name, MemoryBackingKind::Filesystem)
        .map_err(|e| format!("subject attach_memory failed: {e}"))?;
    let outcome = subject_memory_dump_echo(
        facade,
        state_root,
        name,
        env_var,
        &managed,
        &dump_token,
        poll_budget,
    );
    // The subject must be TERMINAL before detach (the hermes pass's rule);
    // stop is idempotent, and a failed proof must not orphan the attachment.
    let _ = facade.stop(name, Some(Duration::from_secs(5)));
    let detach = facade
        .detach_memory(name)
        .map_err(|e| format!("subject detach_memory failed: {e}"));
    // Either failure propagates; BOTH errors surface when both fail — the
    // detach error is never dropped (the 15-3 review).
    match (outcome, detach) {
        (Ok(()), Ok(())) => Ok(SubjectLegOutcome::Proven),
        (Err(proof), Err(detach)) => Err(format!("{proof}; additionally, {detach}")),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
    }
}

/// Start the subject and prove its dump echoes `env=<env_var>=<managed>`
/// exactly (the same exact-value proof the probe leg and the hermes pass
/// use). Stops the subject again on every exit path.
fn subject_memory_dump_echo(
    facade: &::hekma_engine::Blocking<'_>,
    state_root: &Path,
    name: &str,
    env_var: &str,
    managed: &Path,
    dump_token: &str,
    poll_budget: Duration,
) -> Result<(), String> {
    // The dump path is resolved (and any STALE artifact from an earlier run
    // removed) BEFORE start: only THIS run's dump can satisfy the proof (the
    // 15-3 review) — a subject that never re-writes its dump cannot
    // false-pass on stale bytes.
    let home = engine_home(state_root, name)?;
    let dump = if std::path::Path::new(dump_token).is_absolute() {
        std::path::PathBuf::from(dump_token)
    } else {
        home.join(dump_token)
    };
    let _ = std::fs::remove_file(&dump);
    facade
        .start(name)
        .map_err(|e| format!("subject start failed: {e}"))?;
    // From here the subject is RUNNING with its declared linger; stop it on
    // EVERY exit path (the caller detaches after the terminal state).
    let outcome = (|| {
        wait_for_state(facade, name, LifecycleState::Running, poll_budget)?;
        let managed_str = managed.to_string_lossy().into_owned();
        if !dump_contains_env_for(&dump, env_var, &managed_str, poll_budget) {
            return Err(format!(
                "the manifest subject never received the managed memory dir through its own \
                 declared env var {env_var} (expected env={env_var}={managed_str}; dump at {} \
                 showed: {}) — manifest edited after registration? re-register the adapter",
                dump.display(),
                dump_env_lines(&dump)
            ));
        }
        Ok(())
    })();
    let _ = facade.stop(name, Some(Duration::from_secs(5)));
    outcome
}

/// Interaction, honest per the declared level:
/// * Guaranteed/BestEffort — a probe's send_input reaches the running agent
///   (the echo line appears in its captured log).
/// * Unsupported — fails fast `CapabilityUnsupported` (the channel does not
///   exist; the message names the declaration).
fn run_interaction(
    facade: &::hekma_engine::Blocking<'_>,
    level: SupportLevel,
    timeout: Duration,
) -> SectionResult {
    into_section_result(interaction_inner(facade, level, timeout))
}

/// [`run_interaction`]'s pipeline.
fn interaction_inner(
    facade: &::hekma_engine::Blocking<'_>,
    level: SupportLevel,
    timeout: Duration,
) -> Result<(), String> {
    let probe_kind = "tck-interaction-probe";
    let dir = tempfile::Builder::new()
        .prefix("ktesio-tck-interact-")
        .tempdir()
        .map_err(|e| format!("probe tempdir: {e}"))?;
    // The probe DECLARES interaction AT THE LEVEL UNDER TEST for the current
    // OS (mirroring the subject's declaration, like the pause probe does — a
    // BestEffort declaration is demonstrated AT best-effort), guaranteed
    // everywhere else so registration never blocks.
    write_probe_manifest(
        dir.path(),
        probe_kind,
        &["--echo-stdin", "--linger-ms", "600000"],
        None,
        interaction_probe_level(level),
        "self-reported",
        "1.0.0",
        None,
    )?;
    facade
        .register_with_adapter(probe_kind, &AdapterRef::Manifest(dir.path().to_path_buf()))
        .map_err(|e| format!("probe register failed: {e}"))?;
    facade
        .start(probe_kind)
        .map_err(|e| format!("probe start failed: {e}"))?;

    if level == SupportLevel::Unsupported {
        // Fail-fast proof: send_input must Err naming the declaration.
        let sent = facade.send_input(probe_kind, "tck");
        let _ = facade.stop(probe_kind, Some(Duration::from_secs(5)));
        let msg = sent
            .err()
            .map(|e| e.to_string())
            .ok_or("unsupported interaction must fail fast, but send_input succeeded")?;
        if !msg.contains("cannot send input") && !msg.contains("unsupported") {
            return Err(format!(
                "unsupported interaction must name the declaration, got: {msg}"
            ));
        }
        return Ok(());
    }

    // Guaranteed/BestEffort: delivery proof via the echo line.
    interaction_delivery(facade, probe_kind, timeout)
}

/// The delivery half of the interaction proof: `send_input` must reach the
/// running agent (the echo line lands in its captured log, read through the
/// PUBLIC `read_agent_log` seam — spec 15-3) within `budget`. Extracted so
/// the never-echoes arm is provable directly (a probe launched WITHOUT
/// `--echo-stdin`).
fn interaction_delivery(
    facade: &::hekma_engine::Blocking<'_>,
    probe_kind: &str,
    budget: Duration,
) -> Result<(), String> {
    // The proof is isolated so the STOP below runs on EVERY exit: the
    // never-echoes helper battery drives this to a TIMEOUT Err on purpose,
    // and that Err used to return PAST the stop and leak the probe for its
    // whole linger window (the helper engines are deliberately leaked and
    // never drop to clean up). The caller still sees the same Err.
    let outcome = (|| {
        facade
            .send_input(probe_kind, "tck-echo-line")
            .map_err(|e| format!("send_input failed: {e}"))?;
        wait_for_log_line(facade, probe_kind, "stdin: tck-echo-line", budget)
    })();
    let _ = facade.stop(probe_kind, Some(Duration::from_secs(5)));
    outcome
}

/// The manifest wire level an interaction probe declares for the level under
/// test — the pure mirror of the subject's declaration. Unit-tested.
fn interaction_probe_level(level: SupportLevel) -> &'static str {
    match level {
        SupportLevel::Guaranteed => "guaranteed",
        SupportLevel::BestEffort => "best-effort",
        SupportLevel::Unsupported => "unsupported",
    }
}

// ---------------------------------------------------------------------
// The loopback upstream stub (pure std, engine-test shape)
// ---------------------------------------------------------------------

/// A loopback TCP stub for the engine-observed section: accepts one
/// connection after another, reads a bounded HTTP request, and answers with a
/// fixed completion body whose usage is 30/70/100 — the known sentinels the
/// section asserts. Pure `std` (no runtime, no TLS): the engine's loopback
/// proxy talks plain HTTP.
struct UpstreamStub {
    /// The `http://127.0.0.1:PORT` base URL callers configure.
    base_url: String,
    /// How many requests were served (the forward-counter assertion).
    served: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The shutdown flag (set on drop).
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The listener thread handle.
    handle: Option<std::thread::JoinHandle<()>>,
}

impl UpstreamStub {
    /// Bind 127.0.0.1:0, spawn the accept loop, return the stub.
    fn start() -> Self {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback upstream");
        let port = listener.local_addr().expect("local addr").port();
        let served = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let served_t = served.clone();
        let stop_t = stop.clone();
        // Non-blocking accept + a stop-flag poll (retro #163, finding B11): a
        // BLOCKING `listener.incoming()` loop only re-checks `stop` AFTER it
        // accepts a stream, so a drop whose wake-up connect ever failed left
        // `join()` blocked forever. Here the loop re-checks the flag every
        // few milliseconds on its own — shutdown can no longer depend on the
        // wake-up connect succeeding.
        listener
            .set_nonblocking(true)
            .expect("nonblocking loopback upstream");
        let handle = std::thread::spawn(move || loop {
            if stop_t.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    served_t.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Self::serve_one(stream);
                }
                // Nothing pending: nap briefly and re-check the stop flag.
                // (The 5 ms nap only lives for the engine-observed section's
                // duration — a test-scoped listener, not a long-lived server.)
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // Transient accept errors (ECONNABORTED, EMFILE, …) must NOT
                // kill the stub: a silently-dead listener makes the section
                // fail far from the cause. Only the stop flag ends the loop;
                // anything else naps and retries.
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            served,
            stop,
            handle: Some(handle),
        }
    }

    /// Whether at least one call was served (the forward actually happened).
    fn served_sufficient(&self) -> bool {
        self.served.load(std::sync::atomic::Ordering::SeqCst) >= 1
    }

    /// One bounded request → one fixed response (with a correct
    /// Content-Length; agents and engines both read it).
    fn serve_one(mut stream: std::net::TcpStream) {
        use std::io::{Read, Write};
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = Vec::with_capacity(8 * 1024);
        let mut chunk = [0u8; 4096];
        // Read until the header terminator (or the 64KB cap — matching the
        // engine tests' bounded reader).
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if find_subsequence(&buf, b"\r\n\r\n").is_some() || buf.len() > 64 * 1024 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let body = format!(
            "{{\"id\":\"chatcmpl-stub\",\"object\":\"chat.completion\",\"model\":\"gpt-observed\",\
             \"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"ok\"}},\
             \"finish_reason\":\"stop\"}}],\
             \"usage\":{{\"prompt_tokens\":{OBSERVED_PROMPT_TOKENS},\
             \"completion_tokens\":{OBSERVED_COMPLETION_TOKENS},\"total_tokens\":100}}}}"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

impl Drop for UpstreamStub {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Wake the accept loop early (a connect from localhost breaks the
        // poll nap). Best-effort: the accept loop's own stop-flag poll is the
        // GUARANTEED shutdown path (retro #163, finding B11) — a failed
        // connect can no longer leave `join()` blocked forever, and an
        // in-flight `serve_one` is bounded by its 5s read timeout.
        let _ = std::net::TcpStream::connect(self.base_url.replacen("http://", "", 1));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The `needle`-in-`haystack` helper (the engine tests' shape).
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------
// Harness tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ::hekma_engine::{EffectiveCapabilities, TransitionCause, TransitionEvent};

    /// A all-pass twin manifest (launchable, guaranteed pause on the current
    /// Unix OS, self-reported metering) for the positive-path test. Declares
    /// the reserved `memory.dir` key so the memory section is APPLICABLE (its
    /// delivery is part of the demonstrated declaration).
    fn write_all_pass_manifest(dir: &Path) -> PathBuf {
        let level = if OsId::current() == OsId::Windows {
            "best-effort"
        } else {
            "guaranteed"
        };
        write_probe_manifest(
            dir,
            "tck-all-pass",
            &["--dump", "config-dump.txt", "--linger-ms", "600000"],
            Some(level),
            "guaranteed",
            "self-reported",
            "1.0.0",
            Some(
                "\n[config.model]\nenv = \"MODEL\"\n\n[config.\"memory.dir\"]\n\
                 env = \"TCK_SUBJECT_MEMORY\"\n",
            ),
        )
        .expect("write all-pass manifest")
    }

    /// The happy path: a conforming manifest adapter produces an all-pass
    /// report (not_applicable only for engine-observed, which it does not
    /// declare).
    #[test]
    fn conforming_manifest_adapter_is_conformant() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-test-")
            .tempdir()
            .expect("tempdir");
        write_all_pass_manifest(dir.path());
        let report = run_mock_conformance(dir.path());
        assert!(
            report.is_conformant(),
            "conforming adapter must pass: failures = {:?}",
            report.failures()
        );
        assert_eq!(
            report.section(section_ids::METERING_ENGINE_OBSERVED),
            Some(&SectionResult::NotApplicable {
                reason: "the declaration declares self-reported metering, so the engine-observed \
                     section does not apply"
                    .to_string()
            })
        );
    }

    /// A declared-but-failing adapter still yields a COMPLETE report: the
    /// failing sections carry reasons, the suite never aborts, and the
    /// verdict is false.
    #[test]
    fn failing_adapter_reports_every_section_without_aborting() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-test-")
            .tempdir()
            .expect("tempdir");
        // A declared-but-broken adapter: the launch exec DOES NOT EXIST (a
        // missing relative path fails to spawn on ALL THREE OSes — no
        // /usr/bin/false Unixism), the launch carries NO `--dump <path>` pair,
        // and pause is declared unsupported on the current OS.
        //
        // EXPECTED report shape (the point of this test — the fail path of
        // each section is real, not assumed):
        // * `lifecycle` FAILS — the start can never reach Running.
        // * `config_mapping` FAILS — the registered launch carries no
        //   `--dump <path>` pair, so delivered config is unprovable (this
        //   fires BEFORE any start; the dump poll is not involved).
        // * `interaction` PASSES — the section demonstrates delivery through
        //   its OWN healthy echo probe, not the broken subject.
        // * `pause` n/a (unsupported declared), `memory` n/a (no
        //   `memory.dir` mapping), `metering_engine_observed` n/a
        //   (self-reported); the metering/memory/interaction PROBES are the
        //   harness's own healthy fixtures and still pass their sections.
        // * The suite still completes every section — never aborts mid-suite.
        let manifest_body = format!(
            "\ncontract_version = \"1.0.0\"\n\n[adapter]\nkind = \"tck-fail-adapter\"\n\n             [lifecycle.start]\nexec = \"./tck-missing-agent\"\nargs = []\n\n             [capabilities.pause]\n{os} = \"unsupported\"\n\n             [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\n             windows = \"guaranteed\"\nother = \"guaranteed\"\n\n             [metering]\nsource = \"self-reported\"\n\n             [config.\"model\"]\nenv = \"MODEL\"\n",
            os = current_os_key()
        );
        std::fs::write(dir.path().join("adapter.toml"), manifest_body)
            .expect("write failing manifest");
        let report = run_mock_conformance(dir.path());
        assert!(!report.is_conformant());
        let failures = report.failures();

        // LIFECYCLE fails, naming the start that could never run.
        let lifecycle = report
            .section(section_ids::LIFECYCLE)
            .expect("lifecycle section present");
        let lifecycle_reason = match lifecycle {
            SectionResult::Fail { reason } => reason.clone(),
            other => panic!("lifecycle must FAIL for a non-spawnable adapter, got {other:?}"),
        };
        assert!(
            lifecycle_reason.contains("start failed")
                || lifecycle_reason.contains("terminal state"),
            "lifecycle reason must name the failed start: {lifecycle_reason}"
        );

        // CONFIG_MAPPING fails at the missing `--dump <path>` pair (the
        // unprovable-channel reason, not a dump-poll timeout).
        assert!(
            failures
                .iter()
                .any(|(section, reason)| *section == section_ids::CONFIG_MAPPING
                    && reason.contains("--dump")),
            "config_mapping must fail at the missing --dump pair: {failures:?}"
        );

        // The interaction section PASSES on its own healthy probe (reality
        // check — see the comment above).
        assert_eq!(
            report.section(section_ids::INTERACTION),
            Some(&SectionResult::Pass),
            "interaction demonstrates delivery via its own healthy probe"
        );

        // The suite still ran every section (complete report, 8 entries);
        // only the declaration-justified sections read not_applicable.
        assert_eq!(report.sections.len(), 8, "the report must be complete");
        for section in report.sections.iter() {
            assert!(
                !matches!(section.result, SectionResult::NotApplicable { .. })
                    || section.section == section_ids::METERING_ENGINE_OBSERVED
                    || section.section == section_ids::PAUSE
                    || section.section == section_ids::MEMORY,
                "unexpected not_applicable for {}: {:?}",
                section.section,
                section.result
            );
        }
    }

    /// The crash twin is detected as a lifecycle failure (Never policy, exit
    /// code 7, no restart) — the report's lifecycle section carries the crash
    /// proof.
    #[test]
    fn crash_lands_failed_with_exit_code() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-test-")
            .tempdir()
            .expect("tempdir");
        write_all_pass_manifest(dir.path());
        let report = run_mock_conformance(dir.path());
        let lifecycle = report
            .section(section_ids::LIFECYCLE)
            .expect("lifecycle section present");
        assert!(
            lifecycle.is_pass(),
            "the crash leg must pass on a healthy engine: {lifecycle:?}"
        );
    }

    /// The registration-failure report is complete and honest: 8 fail entries
    /// carrying the REGISTRATION stage's rendered prefix (the pre-15-3 string,
    /// kept verbatim for consumers that pinned it) INCLUDING the `register: `
    /// detail infix — the default-path failure renders byte-identical to
    /// pre-15-3 beyond the pinned stage prefix — and the kind label is
    /// derived without resolving anything.
    #[test]
    fn registration_failure_report_is_complete() {
        // A manifest dir that does not exist → registration fails.
        let report = run_conformance(&TckAdapter::Manifest(PathBuf::from(
            "/nonexistent/does-not-exist",
        )));
        assert!(!report.is_conformant());
        assert_eq!(report.adapter_kind, "does-not-exist");
        assert_eq!(report.sections.len(), 8);
        for entry in report.sections.iter() {
            match &entry.result {
                SectionResult::Fail { reason } => {
                    assert!(
                        reason.starts_with("registration failed: register: "),
                        "reason must be byte-identical to the pre-15-3 default-path render \
                         (stage prefix + register infix): {reason}"
                    );
                }
                other => panic!("expected Fail, got {other:?}"),
            }
        }
    }

    /// Every pipeline stage renders its OWN prefix (spec 15-3's pure render
    /// seam — the all-sections failure reason names WHERE the pipeline
    /// stopped, so an engine-open failure is no longer mislabeled
    /// "registration failed"). All five arms pinned; the Registration arm's
    /// string is the pre-15-3 prefix verbatim.
    #[test]
    fn stage_prefixes_render_per_stage() {
        assert_eq!(
            Stage::StateRoot.render("nope"),
            "state root preparation failed: nope"
        );
        assert_eq!(Stage::EngineOpen.render("nope"), "engine open failed: nope");
        assert_eq!(
            Stage::Registration.render("nope"),
            "registration failed: nope"
        );
        assert_eq!(
            Stage::CapabilitiesRead.render("nope"),
            "capability projection read failed: nope"
        );
        assert_eq!(
            Stage::SnapshotRead.render("nope"),
            "registered adapter read failed: nope"
        );
        // The compat pin: the registration arm is byte-identical to the
        // pre-15-3 reason prefix (the one string external consumers may
        // have matched on).
        assert!(Stage::Registration
            .render("detail")
            .starts_with("registration failed: "));
    }

    /// The StateRoot stage triggers FOR REAL on an unusable configured root
    /// (the I/O matrix's configured-invocation row): a root that exists but
    /// is a regular file fails every SELECTED section with the state-root
    /// prefix — proving both the stage arm and the selected-set failure
    /// report in one drive.
    #[test]
    fn unusable_state_root_fails_the_state_root_stage_for_the_selected_set() {
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-badroot-")
            .tempdir()
            .expect("tempdir");
        let not_a_dir = scratch.path().join("not-a-dir");
        std::fs::write(&not_a_dir, "a file, not a directory").expect("write blocker");
        let config = TckConfig {
            state_root: Some(not_a_dir.clone()),
            sections: SectionSelection::Only(vec![TckSection::Pause, TckSection::Memory]),
            ..TckConfig::default()
        };
        let report = run_conformance_with(&TckAdapter::Native("mock".to_string()), &config);
        assert!(!report.is_conformant());
        assert_eq!(
            report.sections.len(),
            2,
            "the failure report covers exactly the selected set"
        );
        let got: Vec<&str> = report.sections.iter().map(|s| s.section.as_str()).collect();
        assert_eq!(got, vec![section_ids::PAUSE, section_ids::MEMORY]);
        for entry in report.sections.iter() {
            match &entry.result {
                SectionResult::Fail { reason } => {
                    assert!(
                        reason.starts_with("state root preparation failed: "),
                        "reason must name the state-root stage: {reason}"
                    );
                    assert!(
                        reason.contains("not a directory"),
                        "reason must name the root's defect: {reason}"
                    );
                }
                other => panic!("expected Fail, got {other:?}"),
            }
        }
    }

    /// The FRESH-ROOT CONTRACT (the 15-3 review): a caller-supplied state
    /// root holding ANY pre-existing entry — a prior run's leftover file OR
    /// a stale garbage `state.db` — is a named StateRoot-stage failure for
    /// every selected section. The garbage-state.db shape is the residual
    /// state class named by the review: it is refused HERE, before the
    /// engine ever opens, so no prior-run artifact can false-pass this
    /// run's proofs.
    #[test]
    fn nonempty_caller_state_root_fails_the_state_root_stage_naming_the_fresh_contract() {
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-staleroot-")
            .tempdir()
            .expect("tempdir");

        for (label, populate) in [
            (
                "a leftover file",
                Box::new(|root: &Path| {
                    std::fs::write(root.join("stale-dump.txt"), "a previous run's dump")
                        .expect("write leftover");
                }) as Box<dyn Fn(&Path)>,
            ),
            (
                "a stale non-sqlite state.db",
                Box::new(|root: &Path| {
                    std::fs::write(root.join("state.db"), "not a sqlite file at all")
                        .expect("write stale db");
                }),
            ),
        ] {
            let root = scratch.path().join(format!("root-{label}"));
            std::fs::create_dir(&root).expect("mkdir");
            populate(&root);
            let config = TckConfig {
                state_root: Some(root.clone()),
                sections: SectionSelection::Only(vec![TckSection::Pause]),
                ..TckConfig::default()
            };
            let report = run_conformance_with(&TckAdapter::Native("mock".to_string()), &config);
            assert!(!report.is_conformant(), "{label}");
            assert_eq!(report.sections.len(), 1, "{label}");
            match &report.sections[0].result {
                SectionResult::Fail { reason } => {
                    assert!(
                        reason.starts_with("state root preparation failed: "),
                        "{label}: {reason}"
                    );
                    assert!(
                        reason.contains("not empty"),
                        "{label}: the reason names the fresh-root contract: {reason}"
                    );
                    assert!(
                        reason.contains("exclusively"),
                        "{label}: the reason names the exclusive-single-run contract: {reason}"
                    );
                }
                other => panic!("{label}: expected Fail, got {other:?}"),
            }
        }
    }

    /// The CREATABILITY probe (the 15-3 review): a caller root that cannot
    /// be created (its parent does not exist) fails at the STATE-ROOT stage
    /// with the root named — never as a mysterious engine-open failure. A
    /// root that does not yet exist but IS creatable is created and used.
    #[test]
    fn uncreatable_caller_state_root_fails_at_the_state_root_stage() {
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-noparent-")
            .tempdir()
            .expect("tempdir");
        // The root's PARENT is an existing FILE: the creatability probe
        // (create_dir_all) cannot succeed through it, on every OS.
        let blocker = scratch.path().join("blocked.txt");
        std::fs::write(&blocker, "a file, not a directory").expect("write blocker");
        let impossible = blocker.join("root");
        let config = TckConfig {
            state_root: Some(impossible.clone()),
            sections: SectionSelection::Only(vec![TckSection::CapabilityEdges]),
            ..TckConfig::default()
        };
        let report = run_conformance_with(&TckAdapter::Native("mock".to_string()), &config);
        assert!(!report.is_conformant());
        assert_eq!(report.sections.len(), 1);
        match &report.sections[0].result {
            SectionResult::Fail { reason } => {
                assert!(
                    reason.starts_with("state root preparation failed: "),
                    "the failure belongs to the StateRoot stage, not EngineOpen: {reason}"
                );
                assert!(
                    reason.contains("cannot be created"),
                    "the reason names the creatability probe: {reason}"
                );
            }
            other => panic!("expected Fail, got {other:?}"),
        }

        // The healthy counterpart: a NOT-YET-EXISTING root is created and
        // used (the engine's state lands there).
        let fresh = scratch.path().join("created-by-the-probe");
        let config = TckConfig {
            state_root: Some(fresh.clone()),
            sections: SectionSelection::Only(vec![TckSection::CapabilityEdges]),
            ..TckConfig::default()
        };
        let report = run_conformance_with(&TckAdapter::Native("mock".to_string()), &config);
        assert_eq!(
            report.section(section_ids::CAPABILITY_EDGES),
            Some(&SectionResult::Pass),
            "a creatable fresh root runs: {:?}",
            report.failures()
        );
        assert!(
            EnginePaths::new(Some(fresh))
                .expect("layout")
                .state_db()
                .is_file(),
            "the probe created the root and the engine used it"
        );
    }

    /// The EngineOpen stage gets a REAL pipeline drive (the 15-3 review):
    /// a caller root that is EMPTY and creatable but NOT WRITABLE passes
    /// every state-root check, and the engine's own open then fails
    /// creating its state DB — every selected section's reason starts with
    /// `engine open failed: `. (The review's original suggestion — a
    /// garbage non-SQLite `state.db` in the root — is unreachable here by
    /// design: the fresh-root contract refuses any pre-existing entry at
    /// the StateRoot stage FIRST, which is the stronger guarantee; the
    /// unwritable-empty-root shape drives the same stage arm.) Unix-only
    /// (directory write permissions — the one shape that passes the
    /// fresh-root check yet fails the engine open; boundary-allowlisted in
    /// ci.yml with this justification); skipped with a note when the runner
    /// ignores permissions (e.g. root).
    #[test]
    #[cfg(unix)] // boundary-allowlisted with justification in ci.yml: the
                 // EngineOpen drive needs an EMPTY-but-UNWRITABLE root — unix permission
                 // model only (the fresh-root contract refuses non-empty roots at StateRoot).
    fn unwritable_caller_state_root_fails_the_engine_open_stage_for_the_selected_set() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-readonly-")
            .tempdir()
            .expect("tempdir");
        let root = scratch.path().join("root");
        std::fs::create_dir(&root).expect("mkdir");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500))
            .expect("read-only perms");
        // Honesty guard: if the runner ignores directory permissions (root),
        // the engine would open fine and this drive would prove nothing —
        // skip with a visible note instead of false-passing.
        if std::fs::write(root.join("canary"), b"x").is_ok() {
            eprintln!("note: skipping unwritable-root engine-open drive (runner is root)");
            return;
        }
        let config = TckConfig {
            state_root: Some(root.clone()),
            sections: SectionSelection::Only(vec![TckSection::Pause, TckSection::Memory]),
            ..TckConfig::default()
        };
        let report = run_conformance_with(&TckAdapter::Native("mock".to_string()), &config);
        assert!(!report.is_conformant());
        assert_eq!(
            report.sections.len(),
            2,
            "the failure report covers exactly the selected set"
        );
        for entry in report.sections.iter() {
            match &entry.result {
                SectionResult::Fail { reason } => {
                    assert!(
                        reason.starts_with("engine open failed: "),
                        "reason must name the engine-open stage: {reason}"
                    );
                }
                other => panic!("expected Fail, got {other:?}"),
            }
        }
        // Restore writability so the TempDir teardown can clean up.
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    }

    /// An EMPTY `SectionSelection::Only` list is a named, complete failure —
    /// never a vacuously-conformant zero-entry report (the 15-3 review).
    #[test]
    fn empty_selection_is_a_named_complete_failure_never_a_vacuous_pass() {
        let config = TckConfig {
            sections: SectionSelection::Only(vec![]),
            ..TckConfig::default()
        };
        let report = run_conformance_with(&TckAdapter::Native("mock".to_string()), &config);
        assert!(
            !report.is_conformant(),
            "a run that selected nothing must not read conformant"
        );
        assert_eq!(report.sections.len(), 1, "exactly the one named entry");
        assert_eq!(report.sections[0].section, section_ids::SELECTION);
        match &report.sections[0].result {
            SectionResult::Fail { reason } => {
                assert!(reason.contains("selection is empty"), "{reason}");
                assert!(
                    reason.contains("All"),
                    "the reason names the remedy: {reason}"
                );
            }
            other => panic!("expected Fail, got {other:?}"),
        }
        // The defense-in-depth half: a hand-built EMPTY report also reads
        // non-conformant (`is_conformant` refuses the vacuous truth).
        let empty = ConformanceReport {
            schema_version: REPORT_SCHEMA_VERSION,
            contract_version: String::new(),
            adapter_kind: "hand-built".to_string(),
            sections: vec![],
        };
        assert!(!empty.is_conformant());
    }

    /// Stage raises strip a redundant leading restatement of the stage's OWN
    /// prefix (no "stage: stage: detail" double-stacking), while informative
    /// helper prefixes — the byte-parity `register: ` infix — survive (the
    /// 15-3 review).
    #[test]
    fn stage_raises_strip_own_prefix_but_keep_informative_infixes() {
        // A helper error that already restates the stage: the raise strips
        // the duplicate.
        assert_eq!(
            stage_err(
                Stage::EngineOpen,
                "engine open failed: the engine could not open".to_string()
            ),
            (Stage::EngineOpen, "the engine could not open".to_string())
        );
        // Every other stage's restatement is stripped by ITS raise only.
        assert_eq!(
            stage_err(
                Stage::StateRoot,
                "state root preparation failed: stale content".to_string()
            ),
            (Stage::StateRoot, "stale content".to_string())
        );
        // Informative prefixes are NOT the stage prefix and survive.
        assert_eq!(
            stage_err(
                Stage::Registration,
                "register: no such manifest".to_string()
            ),
            (
                Stage::Registration,
                "register: no such manifest".to_string()
            )
        );
        assert_eq!(
            stage_err(Stage::EngineOpen, "plain detail".to_string()),
            (Stage::EngineOpen, "plain detail".to_string())
        );
    }

    /// A real launchable manifest that declares UNSUPPORTED pause reads
    /// pause = NotApplicable (naming the declaration) — the frozen-spec rule.
    #[test]
    fn unsupported_pause_declaration_reads_not_applicable() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-test-")
            .tempdir()
            .expect("tempdir");
        // Declare pause SUPPORTED elsewhere but UNSUPPORTED here: the
        // projection on the current OS is Unsupported.
        let bin = crate::fake_agent_bin();
        let os = current_os_key();
        let other = other_os_key();
        let manifest = format!(
            r#"
contract_version = "1.0.0"

[adapter]
kind = "tck-no-pause-adapter"

[lifecycle.start]
exec = {exec:?}
args = ["--linger-ms", "600000"]

[capabilities.pause]
{other} = "guaranteed"
{os} = "unsupported"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#,
            exec = bin.to_string_lossy(),
        );
        std::fs::write(dir.path().join("adapter.toml"), manifest).expect("write manifest");
        let report = run_mock_conformance(dir.path());
        let pause = report.section(section_ids::PAUSE).expect("pause section");
        match pause {
            SectionResult::NotApplicable { reason } => {
                assert!(reason.contains("pause"), "reason names pause: {reason}");
                assert!(
                    reason.contains("unsupported"),
                    "reason names the level: {reason}"
                );
                assert!(
                    reason.contains(current_os_key()),
                    "reason names the current OS: {reason}"
                );
            }
            other => panic!("expected NotApplicable for unsupported pause, got {other:?}"),
        }
        // The suite still completed (8 sections, the rest of the sections ran).
        assert_eq!(report.sections.len(), 8);
    }

    /// An ENGINE-OBSERVED manifest adapter exercises the observed half of the
    /// metering coverage: the loopback section RUNS (its upstream stub + forward
    /// path commit exactly 3 × 30/70), the self-reported half reads
    /// not_applicable — the mirror image of the self-reported pass above. This
    /// is the proof that the TCK covers BOTH Metering Sources.
    #[test]
    fn engine_observed_manifest_adapter_passes_the_observed_section() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-test-")
            .tempdir()
            .expect("tempdir");
        // Best-effort pause keeps this arm OS-agnostic (the freeze proof stays
        // with the guaranteed all-pass test); the observed contract version is
        // the one that understands the `metering.base_url` mapping.
        write_probe_manifest(
            dir.path(),
            "tck-observed-adapter",
            &[
                "--dump",
                "config-dump.txt",
                "--observed-calls",
                "3",
                "--linger-ms",
                "600000",
            ],
            Some("best-effort"),
            "guaranteed",
            "engine-observed",
            "1.0.0",
            Some(
                "\n[config.model]\nenv = \"MODEL\"\n\n[config.\"metering.base_url\"]\n\
                 env = \"OPENAI_BASE_URL\"\n\n[config.\"memory.dir\"]\n\
                 env = \"TCK_SUBJECT_MEMORY\"\n",
            ),
        )
        .expect("write observed manifest");
        let report = run_mock_conformance(dir.path());
        assert!(
            report.is_conformant(),
            "a conforming engine-observed adapter must pass: failures = {:?}",
            report.failures()
        );
        assert_eq!(
            report.section(section_ids::METERING_ENGINE_OBSERVED),
            Some(&SectionResult::Pass),
            "the engine-observed section must RUN (loopback probe committed), not skip"
        );
        assert_eq!(
            report.section(section_ids::METERING_SELF_REPORTED),
            Some(&SectionResult::NotApplicable {
                reason: "the declaration declares engine-observed metering, so the \
                         self-reported section does not apply"
                    .to_string()
            })
        );
    }

    /// A native builtin with no launch command (the inert `mock` builtin) is
    /// honestly ALL-not-applicable for the live sections: nothing about it can
    /// be demonstrated by driving a process, and the harness says so per
    /// section instead of failing — and never panics.
    #[test]
    fn native_inert_mock_reports_not_applicable_sections() {
        let report = run_conformance(&TckAdapter::Native("mock".to_string()));
        assert_eq!(report.adapter_kind, "mock");
        assert_eq!(report.sections.len(), 8);
        assert!(
            report.is_conformant(),
            "an inert registration is conformant (nothing failed): failures = {:?}",
            report.failures()
        );
        // The declaration snapshot is still real: capability edges hold.
        assert_eq!(
            report.section(section_ids::CAPABILITY_EDGES),
            Some(&SectionResult::Pass)
        );
        // Every live section is not_applicable WITH a reason naming the gap.
        for id in [
            section_ids::LIFECYCLE,
            section_ids::PAUSE,
            section_ids::CONFIG_MAPPING,
            section_ids::METERING_SELF_REPORTED,
            section_ids::METERING_ENGINE_OBSERVED,
            section_ids::MEMORY,
            section_ids::INTERACTION,
        ] {
            match report.section(id) {
                Some(SectionResult::NotApplicable { reason }) => {
                    assert!(!reason.is_empty(), "{id} must justify its skip");
                }
                other => panic!("expected NotApplicable for {id}, got {other:?}"),
            }
        }
    }

    /// The report aggregation units: section lookup, failure extraction, and
    /// the conformant verdict treat NotApplicable as conformant.
    #[test]
    fn report_aggregation_units() {
        let report = ConformanceReport {
            schema_version: REPORT_SCHEMA_VERSION,
            contract_version: ::hekma_adapter_api::CONTRACT_VERSION.to_string(),
            adapter_kind: "unit".to_string(),
            sections: vec![
                SectionReport {
                    section: "a".to_string(),
                    result: SectionResult::Pass,
                },
                SectionReport {
                    section: "b".to_string(),
                    result: SectionResult::NotApplicable {
                        reason: "n/a".to_string(),
                    },
                },
                SectionReport {
                    section: "c".to_string(),
                    result: SectionResult::fail("boom"),
                },
            ],
        };
        assert!(!report.is_conformant());
        assert_eq!(report.failures(), vec![("c", "boom")]);
        assert_eq!(report.section("a"), Some(&SectionResult::Pass));
        assert_eq!(report.section("missing"), None);
        assert!(SectionResult::Pass.is_pass());
        assert!(!SectionResult::fail("x").is_pass());
    }

    // -----------------------------------------------------------------
    // Helper batteries: every diagnostic arm of the polling/parsing
    // helpers is proven directly (tiny budgets instead of the section
    // timeout), because a harness that reports failure reasons must have
    // its reasons themselves under test.
    // -----------------------------------------------------------------

    /// A real engine over a hermetic state root, for helper tests. The engine
    /// copies the state root at construction, so the facade is leaked to a
    /// `'static` lifetime (safe, no unsafe) and the caller keeps the TempDir
    /// alive for as long as it drives the engine.
    fn helper_engine() -> (tempfile::TempDir, ::hekma_engine::Blocking<'static>) {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-helper-")
            .tempdir()
            .expect("helper tempdir");
        let engine = Engine::open(Some(dir.path().to_path_buf())).expect("helper engine");
        let leaked: &'static Engine = Box::leak(Box::new(engine));
        (dir, leaked.blocking())
    }

    /// `read_registered_facts` (spec 15-3's seam read): the metering source
    /// comes from the public `fleet_entry` view and the launch boolean from
    /// the engine-written snapshot (routed through the public path layout).
    /// Real shapes are proven on REGISTERED probes; each corruption arm
    /// (unknown fleet name, missing/unparseable snapshot) reports a DISTINCT
    /// named reason. This is the SnapshotRead stage's helper-level corrupt
    /// trigger — through the full pipeline the engine (re)writes the
    /// snapshot at registration, so a post-registration corruption is not
    /// cheaply reachable there (the stage's prefix is pinned by the pure
    /// render test above).
    #[test]
    fn read_registered_facts_accepts_real_shapes_and_reports_each_corruption() {
        let (dir, facade) = helper_engine();

        // Unknown fleet name: the public view read fails, named.
        let err = read_registered_facts(&facade, dir.path(), "no-such-instance").unwrap_err();
        assert!(err.contains("fleet entry read failed"), "{err}");

        // A real self-reported launchable probe: both facts read through the
        // seams.
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-facts-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            scratch.path(),
            "tck-facts-probe",
            &["--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            None,
        )
        .expect("write facts manifest");
        facade
            .register_with_adapter(
                "tck-facts",
                &AdapterRef::Manifest(scratch.path().to_path_buf()),
            )
            .expect("register facts probe");
        let facts = read_registered_facts(&facade, dir.path(), "tck-facts").expect("real shape");
        assert_eq!(facts.metering_source, MeteringSource::SelfReported);
        assert!(
            facts.launch,
            "a manifest with [lifecycle.start] snapshotted a launch"
        );

        // The engine's home is the public layout's home (no hand-joined
        // literal). The launch-boolean read (the residual-coupled half)
        // reports each corruption with a named reason — never a fabricated
        // `false`. NOTE: corrupting the snapshot ALSO degrades the public
        // Fleet row's metering_source cell to "unknown" (the row composes
        // from the same snapshot), so these arms drive the launch read
        // directly — `read_registered_facts` itself then fails on EITHER
        // named arm, which its own asserts below pin.
        let snapshot = engine_home(dir.path(), "tck-facts")
            .expect("home")
            .join("adapter.json");
        std::fs::write(&snapshot, "not json at all").expect("corrupt snapshot");
        let err = registered_launch_present(dir.path(), "tck-facts").unwrap_err();
        assert!(err.contains("parse adapter snapshot"), "{err}");
        // Through the composed read, the same corruption surfaces as the
        // fleet arm's honest "unknown" — a NAMED error, never a fabricated
        // metering source.
        let err = read_registered_facts(&facade, dir.path(), "tck-facts").unwrap_err();
        assert!(err.contains("unknown metering_source"), "{err}");

        std::fs::remove_file(&snapshot).expect("remove snapshot");
        let err = registered_launch_present(dir.path(), "tck-facts").unwrap_err();
        assert!(err.contains("read adapter snapshot"), "{err}");

        // A parseable snapshot WITHOUT a launch key is an honest `false`
        // (the native-builtin shape; `#[serde(default)]` on the engine side).
        std::fs::write(
            &snapshot,
            r#"{"kind":"x","metering_source":"self-reported"}"#,
        )
        .expect("launchless snapshot");
        assert!(!(registered_launch_present(dir.path(), "tck-facts").expect("launchless")));
    }

    /// `dump_token_from_args` finds the token after `--dump` in a launch's
    /// args and reports `None` for every shape that lacks one (the pure
    /// half of the spec 15-3 --dump seam read).
    #[test]
    fn dump_token_from_args_finds_the_pair_or_none() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        // No args / no `--dump` / `--dump` as the LAST token.
        assert_eq!(dump_token_from_args(&args(&[])), None);
        assert_eq!(dump_token_from_args(&args(&["--linger-ms", "5"])), None);
        assert_eq!(dump_token_from_args(&args(&["--dump"])), None);
        // The hit: the token after `--dump`.
        assert_eq!(
            dump_token_from_args(&args(&["--linger-ms", "5", "--dump", "d.txt"])),
            Some("d.txt".to_string())
        );
    }

    /// `resolve_launch_dump_pair` resolves the pair FROM SOURCE through the
    /// public `adapter::resolve` seam: a real manifest's launch yields its
    /// `--dump` token, a dump-less launch yields `None`, and a resolve
    /// failure (the I/O matrix's named-fail-reason row) is a named `Err`.
    #[test]
    fn resolve_launch_dump_pair_resolves_from_source_with_named_errors() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-dumppair-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            dir.path(),
            "tck-dump-pair",
            &["--dump", "config-dump.txt", "--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            None,
        )
        .expect("write dump-pair manifest");
        assert_eq!(
            resolve_launch_dump_pair(&AdapterRef::Manifest(dir.path().to_path_buf())),
            Ok(Some("config-dump.txt".to_string()))
        );

        // A launch with no --dump pair resolves cleanly to None.
        let bare = tempfile::Builder::new()
            .prefix("ktesio-tck-barepair-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            bare.path(),
            "tck-bare-pair",
            &["--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            None,
        )
        .expect("write bare manifest");
        assert_eq!(
            resolve_launch_dump_pair(&AdapterRef::Manifest(bare.path().to_path_buf())),
            Ok(None)
        );

        // A nonexistent manifest dir: the resolve error is NAMED.
        let err = resolve_launch_dump_pair(&AdapterRef::Manifest(PathBuf::from(
            "/nonexistent/does-not-exist",
        )))
        .unwrap_err();
        assert!(err.contains("resolve the adapter's declaration"), "{err}");
    }

    /// The dump poll helpers report a hit, a miss, and a timeout as three
    /// DISTINCT outcomes (`Some(true)` / `Some(false)` / `Some(false)` for a
    /// missing artifact after the budget).
    #[test]
    fn dump_poll_helpers_distinguish_hit_miss_and_absent_artifact() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-dumppoll-")
            .tempdir()
            .expect("tempdir");
        let dump = dir.path().join("d.txt");

        // Artifact absent for the whole (tiny) budget → false ("not
        // delivered"), no panic.
        assert!(!dump_contains_env_for(
            &dump,
            "V",
            "1",
            Duration::from_millis(120)
        ));

        // Exact-match hit.
        std::fs::write(&dump, "env=A=1\nenv=V=1\n").unwrap();
        assert!(dump_contains_env_for(&dump, "V", "1", SECTION_TIMEOUT));
        assert!(!dump_contains_env_for(
            &dump,
            "V",
            "2",
            Duration::from_millis(60)
        ));
    }

    /// `wait_for_state` surfaces the public read's error for an unknown
    /// instance and reports a clean timeout when the state never arrives.
    #[test]
    fn wait_for_state_reports_read_errors_and_timeouts() {
        let (_dir, facade) = helper_engine();

        // Unknown instance: the read itself fails (a distinct reason from a
        // timeout).
        let err = wait_for_state(
            &facade,
            "no-such-instance",
            LifecycleState::Running,
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(err.contains("instance_status read failed"), "{err}");

        // A real registered-but-never-started instance never reaches
        // Running: the tiny budget expires into the timeout reason.
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-wait-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            dir.path(),
            "tck-wait-probe",
            &["--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            None,
        )
        .expect("write wait probe manifest");
        facade
            .register_with_adapter("tck-wait", &AdapterRef::Manifest(dir.path().to_path_buf()))
            .expect("register wait probe");
        let err = wait_for_state(
            &facade,
            "tck-wait",
            LifecycleState::Running,
            Duration::from_millis(250),
        )
        .unwrap_err();
        assert!(err.contains("timed out waiting for state"), "{err}");
    }

    /// The row-count helpers are error-honest (spec 15-3's INVERTED pin —
    /// the pre-15-3 test tolerated a missing/schemaless DB as `0`): an
    /// unopenable or corrupt state.db is `Err` with the error NAMED, and the
    /// wait helper propagates the FIRST read error IMMEDIATELY — never a 30s
    /// "have 0" timeout. Over a HEALTHY engine root the same helpers read
    /// `Ok(0)` and the wait helper times out into its honest reason. The
    /// busy-timeout knob (bounded by the section budget) rides along on
    /// every connection and changes none of these outcomes.
    #[test]
    fn usage_row_helpers_name_db_errors_and_propagate_them_immediately() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-rows-")
            .tempdir()
            .expect("tempdir");
        let busy = Duration::from_millis(250);

        // An UNOPENABLE state.db (the path is a directory): the OPEN itself
        // fails, named with the path.
        let dir_root = dir.path().join("root-unopenable");
        std::fs::create_dir(&dir_root).expect("mkdir");
        std::fs::create_dir(dir_root.join("state.db")).expect("state.db as a dir");
        let err = usage_row_count(&dir_root, "x", busy).unwrap_err();
        assert!(err.contains("open") && err.contains("state.db"), "{err}");
        assert!(observed_row_count(&dir_root, "x", busy).is_err());

        // A corrupt (non-SQLite) state.db: the QUERY fails, named.
        let corrupt_root = dir.path().join("root-corrupt");
        std::fs::create_dir(&corrupt_root).expect("mkdir");
        std::fs::write(corrupt_root.join("state.db"), "not a sqlite file at all")
            .expect("write corrupt db");
        let err = usage_row_count(&corrupt_root, "x", busy).unwrap_err();
        assert!(err.contains("count usage rows"), "{err}");
        let err = observed_row_count(&corrupt_root, "x", busy).unwrap_err();
        assert!(err.contains("count engine-observed rows"), "{err}");

        // IMMEDIATE propagation: with the FULL 30s budget the wait helper
        // returns the open/query error in well under a second — the inverted
        // pin (a "have 0" timeout here would take the whole budget; the
        // busy-timeout wait is bounded by the same budget and does not
        // rescue a genuinely corrupt DB).
        let started = std::time::Instant::now();
        let err = wait_for_usage_rows_for(&corrupt_root, "x", 1, SECTION_TIMEOUT).unwrap_err();
        assert!(err.contains("count usage rows"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "a DB error must propagate immediately, took {:?}",
            started.elapsed()
        );
        let err = wait_for_observed_rows_for(&corrupt_root, "x", 1, SECTION_TIMEOUT).unwrap_err();
        assert!(err.contains("count engine-observed rows"), "{err}");

        // Over a HEALTHY engine root (schema present, zero rows): the honest
        // timeout reason carries the count it saw — the REAL-behavior
        // timeout arm, distinct from every error arm above.
        let (engine_root, _facade) = helper_engine();
        assert_eq!(usage_row_count(engine_root.path(), "x", busy), Ok(0));
        assert_eq!(observed_row_count(engine_root.path(), "x", busy), Ok(0));
        let err = wait_for_usage_rows_for(engine_root.path(), "x", 1, Duration::from_millis(150))
            .unwrap_err();
        assert!(
            err.contains("timed out waiting for 1 committed usage rows (have 0)"),
            "{err}"
        );
        let err =
            wait_for_observed_rows_for(engine_root.path(), "x", 1, Duration::from_millis(150))
                .unwrap_err();
        assert!(
            err.contains("timed out waiting for 1 engine-observed rows (have 0)"),
            "{err}"
        );
    }

    /// The log helpers over the PUBLIC read seam (spec 15-3): a read of an
    /// UNREGISTERED name is a named error; a real heartbeat probe's log
    /// yields exact-line hits, whole-line-only matching, a timeout naming
    /// the wanted line, and the heartbeat counter.
    #[test]
    fn log_helpers_read_the_public_seam_and_report_each_outcome() {
        let (_dir, facade) = helper_engine();

        // An unregistered name: the public read itself fails (a distinct,
        // named reason — never an empty log pretending nothing was said).
        let err = read_log_texts(&facade, "no-such-instance").unwrap_err();
        assert!(
            err.contains("read_agent_log('no-such-instance') failed"),
            "{err}"
        );
        assert!(heartbeat_count(&facade, "no-such-instance").is_err());

        // A real probe with a live heartbeat: the echo-style exact-line wait
        // hits, substrings do NOT count as whole lines, a never-arriving
        // line times out naming it, and the counter counts heartbeat lines.
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-logseam-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            scratch.path(),
            "tck-logseam",
            &["--heartbeat-ms", "40", "--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            None,
        )
        .expect("write logseam manifest");
        facade
            .register_with_adapter(
                "tck-logseam",
                &AdapterRef::Manifest(scratch.path().to_path_buf()),
            )
            .expect("register logseam");
        facade.start("tck-logseam").expect("start logseam");

        wait_for_log_line(&facade, "tck-logseam", "heartbeat 0", SECTION_TIMEOUT)
            .expect("the heartbeat line arrives");
        // A whole line, not a substring: "heartbeat" alone never matches.
        assert!(wait_for_log_line(
            &facade,
            "tck-logseam",
            "heartbeat",
            Duration::from_millis(90)
        )
        .is_err());
        // A never-arriving line times out naming it (and the probe).
        let err = wait_for_log_line(
            &facade,
            "tck-logseam",
            "stdin: hi",
            Duration::from_millis(120),
        )
        .unwrap_err();
        assert!(
            err.contains("never observed") && err.contains("stdin: hi"),
            "{err}"
        );
        assert!(
            err.contains("tck-logseam"),
            "the reason names the instance: {err}"
        );
        // The counter grows with the live heartbeat.
        let first = heartbeat_count(&facade, "tck-logseam").expect("count");
        std::thread::sleep(Duration::from_millis(150));
        let second = heartbeat_count(&facade, "tck-logseam").expect("count again");
        assert!(second > first, "heartbeat lines grow: {first} → {second}");

        let _ = facade.stop("tck-logseam", Some(Duration::from_secs(5)));
    }

    /// The byte-sequence helper finds a present needle and reports None for
    /// an absent one (the upstream stub's bounded-reader shape).
    #[test]
    fn subsequence_helper_finds_and_misses() {
        assert_eq!(
            find_subsequence(b"GET /v1 HTTP/1.1\r\n\r\n", b"\r\n\r\n"),
            Some(16)
        );
        assert_eq!(find_subsequence(b"short", b"longer-needle"), None);
        assert_eq!(find_subsequence(b"", b"x"), None);
    }

    /// Both [`TckAdapter`] variants map to the matching [`AdapterRef`] and
    /// derive their report label without resolving anything.
    #[test]
    fn adapter_variants_map_to_refs_and_labels() {
        let dir = tempfile::Builder::new()
            .prefix("my-adapter-")
            .tempdir()
            .expect("tempdir");
        let manifest = TckAdapter::Manifest(dir.path().to_path_buf());
        assert!(matches!(manifest.adapter_ref(), AdapterRef::Manifest(_)));
        assert_eq!(
            manifest.kind_label(),
            dir.path().file_name().unwrap().to_string_lossy()
        );

        let native = TckAdapter::Native("hermes".to_string());
        assert!(matches!(native.adapter_ref(), AdapterRef::Native(_)));
        assert_eq!(native.kind_label(), "hermes");
    }

    /// The declaration-justified fast paths of the section functions report
    /// NotApplicable with a reason naming the gap — without driving any
    /// process (a launch-less registration, an unmapped memory declaration,
    /// an undeclared/unsupported pause capability, a native config subject).
    #[test]
    fn section_fast_paths_report_not_applicable_naming_the_gap() {
        let (_dir, facade) = helper_engine();
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-fast-")
            .tempdir()
            .expect("tempdir");

        // Config: a NATIVE subject has no `--dump` seam to observe.
        match run_config_mapping(
            &facade,
            scratch.path(),
            &TckAdapter::Native("mock".to_string()),
            SECTION_TIMEOUT,
        ) {
            SectionResult::NotApplicable { reason } => {
                assert!(
                    reason.contains("native") && reason.contains("--dump"),
                    "{reason}"
                );
            }
            other => panic!("expected NotApplicable for a native config subject, got {other:?}"),
        }

        // Memory: no launchable agent / no declared `memory.dir` mapping.
        // The gate under test is the THREADED target: `None` = undeclared.
        match run_memory(
            &facade,
            scratch.path(),
            &TckAdapter::Native("mock".to_string()),
            false,
            None,
            SECTION_TIMEOUT,
        ) {
            SectionResult::NotApplicable { reason } => {
                assert!(reason.contains("no launchable agent"), "{reason}");
            }
            other => panic!("expected NotApplicable for a launch-less memory probe, got {other:?}"),
        }
        match run_memory(
            &facade,
            scratch.path(),
            &TckAdapter::Native("mock".to_string()),
            true,
            None,
            SECTION_TIMEOUT,
        ) {
            SectionResult::NotApplicable { reason } => {
                assert!(reason.contains("memory.dir"), "{reason}");
            }
            other => panic!("expected NotApplicable for an unmapped declaration, got {other:?}"),
        }

        // Pause: no launch command / no declaration on this OS / an honest
        // `unsupported` declaration — each names the gap, none panics.
        for (launchable, level, needle) in [
            (false, Some(SupportLevel::Guaranteed), "no launch command"),
            (true, None, "no pause capability"),
            (true, Some(SupportLevel::Unsupported), "unsupported"),
        ] {
            match run_pause_section(&facade, "tck-fast", launchable, level, SECTION_TIMEOUT) {
                SectionResult::NotApplicable { reason } => {
                    assert!(reason.contains(needle), "needle {needle:?} in {reason}");
                }
                other => panic!(
                    "expected NotApplicable (launchable={launchable}, level={level:?}), got {other:?}"
                ),
            }
        }
    }

    /// An `unsupported` interaction declaration fails fast: the probe starts,
    /// `send_input` is refused naming the declaration, and the section PASSES
    /// (the declared behavior was demonstrated).
    #[test]
    fn unsupported_interaction_fails_fast_and_passes_the_section() {
        let (_dir, facade) = helper_engine();
        assert_eq!(
            run_interaction(&facade, SupportLevel::Unsupported, SECTION_TIMEOUT),
            SectionResult::Pass
        );
    }

    /// The freeze proof reports a probe whose heartbeat never starts (its
    /// heartbeat interval is far longer than the tiny budget) — a reason, not
    /// a harness hang.
    #[test]
    fn heartbeat_freeze_reports_a_probe_that_never_heartbeats() {
        let (_dir, facade) = helper_engine();
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-slowbeat-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            scratch.path(),
            "tck-slowbeat",
            &["--heartbeat-ms", "600000", "--linger-ms", "600000"],
            Some("guaranteed"),
            "guaranteed",
            "self-reported",
            "1.0.0",
            None,
        )
        .expect("write slowbeat manifest");
        facade
            .register_with_adapter(
                "tck-slowbeat",
                &AdapterRef::Manifest(scratch.path().to_path_buf()),
            )
            .expect("register slowbeat");
        facade.start("tck-slowbeat").expect("start slowbeat");
        let reason = prove_heartbeat_freeze(&facade, "tck-slowbeat", Duration::from_millis(400))
            .expect("a never-beating probe must be reported");
        assert!(reason.contains("heartbeat never started"), "{reason}");
        let _ = facade.stop("tck-slowbeat", Some(Duration::from_secs(5)));
    }

    /// A manifest adapter that declares NEITHER pause NOR interaction on the
    /// current OS (but guarantees both elsewhere) reads honest
    /// not_applicable entries for those two sections — and every other
    /// section still runs. The absolute `--dump` path also proves the
    /// config section's absolute-path arm.
    #[test]
    fn undeclared_current_os_capabilities_read_not_applicable() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-nodecl-")
            .tempdir()
            .expect("tempdir");
        let bin = crate::fake_agent_bin();
        let other = other_os_key();
        // Pause + interaction are declared ONLY for a NON-current OS; the
        // dump path is ABSOLUTE (the manifest's choice, honored by the
        // config section).
        let dump = dir.path().join("config-dump.txt");
        let manifest = format!(
            r#"
contract_version = "1.0.0"

[adapter]
kind = "tck-nodecl-adapter"

[lifecycle.start]
exec = {exec:?}
args = ["--dump", {dump:?}, "--linger-ms", "600000"]

[capabilities.pause]
{other} = "guaranteed"

[capabilities.interaction]
{other} = "guaranteed"

[metering]
source = "self-reported"

[config.model]
env = "MODEL"
"#,
            exec = bin.to_string_lossy(),
            dump = dump.to_string_lossy(),
            other = other,
        );
        std::fs::write(dir.path().join("adapter.toml"), manifest).expect("write manifest");
        let report = run_mock_conformance(dir.path());
        assert!(report.is_conformant(), "failures = {:?}", report.failures());
        // The engine's projection reports an UNDECLARED capability as
        // unsupported on this OS — an honest declaration, so the pause
        // section reads not_applicable NAMING the declaration.
        match report.section(section_ids::PAUSE) {
            Some(SectionResult::NotApplicable { reason }) => {
                assert!(reason.contains("pause"), "{reason}");
                assert!(reason.contains("unsupported"), "{reason}");
                assert!(reason.contains(current_os_key()), "{reason}");
            }
            other => panic!("expected NotApplicable for pause, got {other:?}"),
        }
        // Undeclared interaction is DEMONSTRATED as unsupported: the
        // section's probe fails fast `CapabilityUnsupported` and passes.
        assert_eq!(
            report.section(section_ids::INTERACTION),
            Some(&SectionResult::Pass),
            "undeclared interaction is demonstrated as unsupported fail-fast"
        );
    }

    /// A manifest subject whose `--dump` artifact can never appear (its
    /// parent path is an existing FILE) fails the config section with the
    /// not-delivered reason — and the suite would carry on.
    #[test]
    fn config_mapping_reports_a_dump_that_never_arrives() {
        let (dir, facade) = helper_engine();
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-baddump-")
            .tempdir()
            .expect("tempdir");
        let bin = crate::fake_agent_bin();
        // The dump's parent is an existing FILE: the best-effort dump write
        // can never succeed.
        let blocked = scratch.path().join("blocked.txt");
        std::fs::write(&blocked, "a file, not a directory").expect("write blocker");
        let dump = blocked.join("config-dump.txt");
        let manifest = format!(
            r#"
contract_version = "1.0.0"

[adapter]
kind = "tck-baddump-adapter"

[lifecycle.start]
exec = {exec:?}
args = ["--dump", {dump:?}, "--linger-ms", "600000"]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"

[config.model]
env = "MODEL"
"#,
            exec = bin.to_string_lossy(),
            dump = dump.to_string_lossy(),
        );
        std::fs::write(scratch.path().join("adapter.toml"), manifest).expect("write manifest");
        facade
            .register_with_adapter(
                "tck-subject",
                &AdapterRef::Manifest(scratch.path().to_path_buf()),
            )
            .expect("register baddump subject");
        let rules = match config_probe_scope(&scratch.path().join("adapter.toml")) {
            Ok(ConfigScope::Env(rules)) => rules,
            other => panic!("expected env rules, got {other:?}"),
        };
        let err = config_mapping_inner(
            &facade,
            dir.path(),
            &AdapterRef::Manifest(scratch.path().to_path_buf()),
            rules,
            Duration::from_millis(400),
        )
        .expect_err("the dump can never arrive");
        assert!(err.contains("never reached"), "{err}");
        let _ = facade.stop("tck-subject", Some(Duration::from_secs(5)));
    }

    /// A subject that delivers its DECLARED env mapping but swallows the
    /// `agent.*` pass-through fails the config section at the pass-through
    /// arm specifically — the reason names the swallowed `agent.*` key and
    /// the env lines the dump DID land, distinguishing it from the
    /// declared-mapping miss above (triage without a rerun).
    #[test]
    fn config_mapping_reports_a_swallowed_agent_passthrough() {
        let (dir, facade) = helper_engine();
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-nopass-")
            .tempdir()
            .expect("tempdir");
        let bin = crate::fake_agent_bin();
        // The dump path is WRITABLE (unlike the never-arrives twin above):
        // the declared `model` mapping lands in it, so the section gets PAST
        // the declared-rule loop and is failed by the pass-through check
        // alone — the arm under test.
        let dump = scratch.path().join("config-dump.txt");
        let manifest = format!(
            r#"
contract_version = "1.0.0"

[adapter]
kind = "tck-nopass-adapter"

[lifecycle.start]
exec = {exec:?}
args = ["--dump", {dump:?}, "--linger-ms", "600000", "--drop-env", "TCK_PROBE"]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"

[config.model]
env = "MODEL"
"#,
            exec = bin.to_string_lossy(),
            dump = dump.to_string_lossy(),
        );
        std::fs::write(scratch.path().join("adapter.toml"), manifest).expect("write manifest");
        facade
            .register_with_adapter(
                "tck-subject",
                &AdapterRef::Manifest(scratch.path().to_path_buf()),
            )
            .expect("register nopass subject");
        let rules = match config_probe_scope(&scratch.path().join("adapter.toml")) {
            Ok(ConfigScope::Env(rules)) => rules,
            other => panic!("expected env rules, got {other:?}"),
        };
        // The poll budget must give the DECLARED mapping's dump write time to
        // land on a loaded runner (unlike the never-arrives twin, this test
        // REQUIRES the model line to be found) while still bounding the
        // pass-through miss wait — 3s covers both.
        let err = config_mapping_inner(
            &facade,
            dir.path(),
            &AdapterRef::Manifest(scratch.path().to_path_buf()),
            rules,
            Duration::from_secs(3),
        )
        .expect_err("the pass-through can never arrive");
        assert!(
            err.contains("pass-through key was not delivered verbatim"),
            "{err}"
        );
        // The reason is TRIAGE-GRADE: it names the swallowed key's env var
        // and carries the dump sample (`dump_env_lines` samples the FIRST 5
        // env lines, so the sample content itself is ambient-env-dependent —
        // pinned only as non-empty here, the missing `env=TCK_PROBE=…` line
        // above is the assertion that matters).
        assert!(err.contains("env=TCK_PROBE=verbatim-1"), "{err}");
        assert!(err.contains("dump at"), "{err}");
        assert!(!err.contains("no env= lines"), "{err}");
        let _ = facade.stop("tck-subject", Some(Duration::from_secs(5)));
    }

    /// A memory probe whose dump artifact can never appear fails the
    /// delivery proof with its reason — and the probe is stopped and
    /// detached on the way out (no leaked attachment).
    #[test]
    fn memory_delivery_reports_a_dump_that_never_arrives() {
        let (_dir, facade) = helper_engine();
        // state_dir's LAST component is an existing FILE, so the derived
        // dump path `<file>/tck-memory-dump.txt` can never be written. The
        // engine's real state root is separate (`helper_engine`'s own).
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-badmem-")
            .tempdir()
            .expect("tempdir");
        let blocked = scratch.path().join("blocked.txt");
        std::fs::write(&blocked, "a file, not a directory").expect("write blocker");
        // The subject manifest dir deliberately holds NO adapter.toml: the
        // probe leg fails (the dump can never arrive) BEFORE the subject leg
        // could ever read a declaration, so a synthetic env target is
        // threaded — exactly what the pipeline's single read would supply.
        let target = ::hekma_adapter_api::ConfigTarget::Env {
            env: "TCK_SUBJECT_MEMORY".to_string(),
        };
        let err = memory_inner(
            &facade,
            &blocked,
            &TckAdapter::Manifest(scratch.path().to_path_buf()),
            Some(&target),
            Duration::from_millis(400),
        )
        .expect_err("the dump can never arrive");
        assert!(
            err.contains("never received the managed memory path"),
            "{err}"
        );
        // The failure path cleaned up after itself.
        assert!(facade.memory_status("tck-memory-probe").unwrap().is_none());
    }

    /// The default section budget is PLATFORM-SCALED at runtime (the
    /// per-OS-as-data rule, no compile-time selection): 90s where the
    /// engine-observed metering row commit has needed more than the flat
    /// 30s under CI contention (Windows, the #236 charter), the flat
    /// [`SECTION_TIMEOUT`] everywhere else. Pinned on every leg so a
    /// platform drift fails the platform's own lane by name.
    #[test]
    fn default_section_timeout_scales_with_the_running_os() {
        let expected = match ::hekma_engine::OsId::current() {
            ::hekma_engine::OsId::Windows => Duration::from_secs(90),
            _ => SECTION_TIMEOUT,
        };
        assert_eq!(default_section_timeout(), expected);
        assert_eq!(TckConfig::default().section_timeout, expected);
    }

    /// The `TckConfig` semantics (spec 15-3): the Default is exactly today's
    /// behavior; `TckSection::id` maps to the stable report ids; selection is
    /// a filter over the FIXED pipeline order (never the caller's order);
    /// an empty `Only` selects nothing (documented vacuity).
    #[test]
    fn tck_config_defaults_and_selection_semantics() {
        assert_eq!(
            TckConfig::default().section_timeout,
            default_section_timeout(),
            "the default rides the platform-scaled budget (#236)"
        );
        assert_eq!(TckConfig::default().state_root, None);
        assert_eq!(
            TckConfig::default().sections,
            SectionSelection::All,
            "the default selects every section (today's behavior)"
        );

        // The typed section maps onto the stable ids, and ALL carries the
        // fixed pipeline order.
        let all_ids: Vec<&str> = TckSection::ALL.iter().map(|s| s.id()).collect();
        assert_eq!(
            all_ids,
            vec![
                section_ids::CAPABILITY_EDGES,
                section_ids::LIFECYCLE,
                section_ids::PAUSE,
                section_ids::CONFIG_MAPPING,
                section_ids::METERING_SELF_REPORTED,
                section_ids::METERING_ENGINE_OBSERVED,
                section_ids::MEMORY,
                section_ids::INTERACTION,
            ]
        );

        // Selection FILTERS in the fixed order — a caller listing sections
        // backwards still gets the report in pipeline order.
        let backwards = SectionSelection::Only(vec![
            TckSection::Memory,
            TckSection::CapabilityEdges,
            TckSection::Pause,
        ]);
        assert_eq!(
            backwards.selected_ids(),
            vec![
                section_ids::CAPABILITY_EDGES,
                section_ids::PAUSE,
                section_ids::MEMORY
            ]
        );
        // Duplicates collapse; an empty Only selects nothing (vacuous).
        let dupes = SectionSelection::Only(vec![TckSection::Pause, TckSection::Pause]);
        assert_eq!(dupes.selected_ids(), vec![section_ids::PAUSE]);
        assert_eq!(
            SectionSelection::Only(vec![]).selected_ids(),
            Vec::<&str>::new()
        );
    }

    /// A CONFIGURED run (spec 15-3's configured-invocation row): a subset
    /// over a CALLER-OWNED fresh state root runs exactly the selected
    /// sections (fewer entries, fixed order — the report stays complete FOR
    /// THE SELECTED SET), the engine's state lands under the caller's root,
    /// and the configured section timeout is UNIQUELY identified: the
    /// budget is a small nonzero 2s, the subject's dump can never appear
    /// (its parent path is an existing file), so the config section's dump
    /// poll burns exactly the configured budget and fails — the elapsed
    /// bound (`>= 2s`, `< one default budget`) can ONLY hold if the 2s
    /// config was actually threaded (a run ignoring the config would burn
    /// the 30s default in the same poll). The lifecycle verdict is asserted
    /// alongside: the same subject is perfectly startable, so lifecycle
    /// PASSES under the configured budget.
    #[test]
    fn configured_run_selects_sections_uses_the_caller_root_and_timeout() {
        let manifest_dir = tempfile::Builder::new()
            .prefix("ktesio-tck-cfg-man-")
            .tempdir()
            .expect("manifest tempdir");
        // A launchable subject whose `--dump` parent is an existing FILE:
        // lifecycle start/stop/crash all work, but the config section's dump
        // artifact can NEVER appear — the one poll that consumes the whole
        // configured budget.
        let bin = crate::fake_agent_bin();
        let blocked = manifest_dir.path().join("blocked.txt");
        std::fs::write(&blocked, "a file, not a directory").expect("write blocker");
        let dump = blocked.join("config-dump.txt");
        let manifest = format!(
            r#"
contract_version = "1.0.0"

[adapter]
kind = "tck-cfg-adapter"

[lifecycle.start]
exec = {exec:?}
args = ["--dump", {dump:?}, "--linger-ms", "600000"]

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"

[config.model]
env = "MODEL"
"#,
            exec = bin.to_string_lossy(),
            dump = dump.to_string_lossy(),
        );
        std::fs::write(manifest_dir.path().join("adapter.toml"), manifest).expect("write manifest");
        let caller_root = tempfile::Builder::new()
            .prefix("ktesio-tck-cfg-root-")
            .tempdir()
            .expect("caller root");
        // The fresh-root contract: an EMPTY existing dir is a valid caller
        // root (this test doubles as its positive proof).
        let config = TckConfig {
            section_timeout: Duration::from_secs(2),
            state_root: Some(caller_root.path().to_path_buf()),
            sections: SectionSelection::Only(vec![
                TckSection::Lifecycle,
                TckSection::CapabilityEdges,
                TckSection::ConfigMapping,
            ]),
        };
        let started = std::time::Instant::now();
        let report = run_conformance_with(
            &TckAdapter::Manifest(manifest_dir.path().to_path_buf()),
            &config,
        );
        let elapsed = started.elapsed();
        // Fewer entries, fixed pipeline order (capability_edges before
        // lifecycle regardless of the listing order above).
        let got: Vec<&str> = report.sections.iter().map(|s| s.section.as_str()).collect();
        assert_eq!(
            got,
            vec![
                section_ids::CAPABILITY_EDGES,
                section_ids::LIFECYCLE,
                section_ids::CONFIG_MAPPING,
            ]
        );
        assert_eq!(
            report.section(section_ids::CAPABILITY_EDGES),
            Some(&SectionResult::Pass)
        );
        // The lifecycle verdict: a healthy subject PASSES under the 2s
        // budget (start/stop/crash each complete well inside it).
        assert_eq!(
            report.section(section_ids::LIFECYCLE),
            Some(&SectionResult::Pass),
            "lifecycle passes under the configured budget: {:?}",
            report.section(section_ids::LIFECYCLE)
        );
        // The config section fails at the never-arriving dump, having spent
        // the whole configured budget polling for it.
        match report.section(section_ids::CONFIG_MAPPING) {
            Some(SectionResult::Fail { reason }) => {
                assert!(
                    reason.contains("never reached") || reason.contains("not delivered verbatim"),
                    "the blocked dump fails the config section: {reason}"
                );
            }
            other => panic!("config_mapping must fail for a blocked dump, got {other:?}"),
        }
        // The elapsed bound UNIQUELY identifies the configured 2s: the poll
        // burned at least the whole budget, and the total stayed WELL UNDER
        // one default budget (30s) — a run that ignored the config would
        // burn ~30s in the same dump poll alone.
        assert!(
            elapsed >= Duration::from_secs(2),
            "the dump poll must consume the configured 2s budget, took {elapsed:?}"
        );
        assert!(
            elapsed < SECTION_TIMEOUT,
            "a 2s section timeout must not wait the 30s default, took {elapsed:?}"
        );
        // The caller-owned root was used AS-IS: the engine's state landed
        // there (and only there — the harness created no temp root of its
        // own for this run).
        assert!(
            EnginePaths::new(Some(caller_root.path().to_path_buf()))
                .expect("layout")
                .state_db()
                .is_file(),
            "the engine's state.db must live under the caller's root"
        );
    }

    /// The memory section's manifest-subject `--dump` leg (spec 15-3,
    /// Fibery 174): a manifest subject that declares `memory.dir` AND
    /// carries the `--dump` convention gets its OWN delivery proven — and a
    /// subject that SWALLOWS the delivered var (`fake_agent --drop-env`)
    /// fails the section naming the subject leg (the negative control that
    /// proves the leg actually runs; a silent skip would pass).
    #[test]
    fn memory_subject_dump_leg_proves_and_fails_the_subjects_own_delivery() {
        let (dir, facade) = helper_engine();

        // The positive shape: the subject's dump echoes the managed dir.
        let honest_dir = tempfile::Builder::new()
            .prefix("ktesio-tck-submem-ok-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            honest_dir.path(),
            "tck-submem-ok",
            &["--dump", "config-dump.txt", "--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            Some("\n[config.\"memory.dir\"]\nenv = \"TCK_SUBJECT_MEMORY\"\n"),
        )
        .expect("write honest subject manifest");
        facade
            .register_with_adapter(
                "tck-subject",
                &AdapterRef::Manifest(honest_dir.path().to_path_buf()),
            )
            .expect("register honest subject");
        let honest_target = TckAdapter::Manifest(honest_dir.path().to_path_buf())
            .memory_dir_target()
            .expect("the single declaration read");
        memory_inner(
            &facade,
            dir.path(),
            &TckAdapter::Manifest(honest_dir.path().to_path_buf()),
            honest_target.as_ref(),
            SECTION_TIMEOUT,
        )
        .expect("both memory legs pass for an honest subject");
        // The subject leg cleaned up: no attachment, terminal state.
        assert!(facade.memory_status("tck-subject").unwrap().is_none());

        // The negative control: same declaration, but the subject DROPS the
        // delivered env var — the subject leg must FAIL naming it. (A fresh
        // engine: `tck-subject` is a fleet-unique name.)
        let (dir, facade) = helper_engine();
        let lying_dir = tempfile::Builder::new()
            .prefix("ktesio-tck-submem-drop-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            lying_dir.path(),
            "tck-submem-drop",
            &[
                "--dump",
                "config-dump.txt",
                "--drop-env",
                "TCK_SUBJECT_MEMORY",
                "--linger-ms",
                "600000",
            ],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            Some("\n[config.\"memory.dir\"]\nenv = \"TCK_SUBJECT_MEMORY\"\n"),
        )
        .expect("write lying subject manifest");
        facade
            .register_with_adapter(
                "tck-subject",
                &AdapterRef::Manifest(lying_dir.path().to_path_buf()),
            )
            .expect("register lying subject");
        let lying_target = TckAdapter::Manifest(lying_dir.path().to_path_buf())
            .memory_dir_target()
            .expect("the single declaration read");
        let err = memory_inner(
            &facade,
            dir.path(),
            &TckAdapter::Manifest(lying_dir.path().to_path_buf()),
            lying_target.as_ref(),
            Duration::from_millis(400),
        )
        .expect_err("a subject that swallows the delivered dir must fail the section");
        assert!(
            err.contains("the manifest subject never received the managed memory dir")
                && err.contains("TCK_SUBJECT_MEMORY"),
            "{err}"
        );
        // The failure reason carries the resolve-from-source remedy hint
        // (the 15-3 review): a manifest edited after registration diverges
        // from the registration-time snapshot exactly here.
        assert!(
            err.contains("re-register the adapter"),
            "the timeout reason names the remedy: {err}"
        );
        // No leaked attachment on the failure path either.
        assert!(facade.memory_status("tck-subject").unwrap().is_none());
    }

    /// The subject leg's skip shapes are HONEST skips (the probe twin
    /// remains the delivery proof — never a failure, never silent), and the
    /// orphaned-RUNNING-subject case is TERMINALIZED before attach (the
    /// 15-3 review: an orphan left by an earlier section's `?` path must not
    /// misattribute the engine's by-design hot-swap refusal to the memory
    /// section) — the leg then proves the subject's delivery and detaches.
    #[test]
    fn memory_subject_leg_skips_honestly_and_terminalizes_an_orphaned_subject() {
        let (dir, facade) = helper_engine();

        // Native subject: a documented, surfaced skip (the reason names the
        // engine-side hermes pass as the subject-delivery proof).
        match subject_memory_delivery(
            &facade,
            dir.path(),
            &TckAdapter::Native("mock".to_string()),
            None,
            SECTION_TIMEOUT,
        )
        .expect("the native leg skips, never fails")
        {
            SubjectLegOutcome::Skipped { reason } => {
                assert!(reason.contains("native"), "{reason}");
                assert!(reason.contains("--dump"), "{reason}");
            }
            other => panic!("expected Skipped for a native subject, got {other:?}"),
        }

        // A manifest subject whose launch carries no --dump pair: skip, with
        // the probe twin named as the delivery proof.
        let bare_dir = tempfile::Builder::new()
            .prefix("ktesio-tck-submem-bare-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            bare_dir.path(),
            "tck-submem-bare",
            &["--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            Some("\n[config.\"memory.dir\"]\nenv = \"TCK_SUBJECT_MEMORY\"\n"),
        )
        .expect("write bare subject manifest");
        let bare_target = TckAdapter::Manifest(bare_dir.path().to_path_buf())
            .memory_dir_target()
            .expect("the single declaration read");
        match subject_memory_delivery(
            &facade,
            dir.path(),
            &TckAdapter::Manifest(bare_dir.path().to_path_buf()),
            bare_target.as_ref(),
            SECTION_TIMEOUT,
        )
        .expect("the dump-less leg skips, never fails")
        {
            SubjectLegOutcome::Skipped { reason } => {
                assert!(reason.contains("no --dump pair"), "{reason}");
                assert!(reason.contains("probe twin"), "{reason}");
            }
            other => panic!("expected Skipped for a dump-less subject, got {other:?}"),
        }

        // The ORPHANED-RUNNING subject (dump-carrying, env target): the leg
        // idempotently stops it, waits for the terminal state, attaches,
        // proves the subject's own delivery, and detaches — where the
        // pre-review code FAILED the leg with a misattributed hot-swap
        // refusal ("subject attach_memory failed").
        let dump_dir = tempfile::Builder::new()
            .prefix("ktesio-tck-submem-run-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            dump_dir.path(),
            "tck-submem-run",
            &["--dump", "config-dump.txt", "--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            Some("\n[config.\"memory.dir\"]\nenv = \"TCK_SUBJECT_MEMORY\"\n"),
        )
        .expect("write running-subject manifest");
        facade
            .register_with_adapter(
                "tck-subject",
                &AdapterRef::Manifest(dump_dir.path().to_path_buf()),
            )
            .expect("register subject");
        facade.start("tck-subject").expect("start subject");
        let dump_target = TckAdapter::Manifest(dump_dir.path().to_path_buf())
            .memory_dir_target()
            .expect("the single declaration read");
        let outcome = subject_memory_delivery(
            &facade,
            dir.path(),
            &TckAdapter::Manifest(dump_dir.path().to_path_buf()),
            dump_target.as_ref(),
            SECTION_TIMEOUT,
        )
        .expect("an orphaned running subject is terminalized, not failed");
        assert_eq!(
            outcome,
            SubjectLegOutcome::Proven,
            "the leg proves the subject's own delivery after terminalizing it"
        );
        // No leaked attachment; the subject is terminal again.
        assert!(facade.memory_status("tck-subject").unwrap().is_none());
        assert!(matches!(
            facade
                .instance_status("tck-subject")
                .unwrap()
                .instance
                .state,
            LifecycleState::Stopped | LifecycleState::Failed
        ));
    }

    /// `TckAdapter::memory_dir_env_target`: the manifest subject's own
    /// declared env var, the native builtin table's, and the named error
    /// for a declaration that cannot be re-read.
    #[test]
    fn memory_dir_env_target_reads_both_kinds_and_names_errors() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-envtarget-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            dir.path(),
            "tck-envtarget",
            &["--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            Some("\n[config.\"memory.dir\"]\nenv = \"SUBJECT_MEMORY\"\n"),
        )
        .expect("write manifest");
        assert_eq!(
            TckAdapter::Manifest(dir.path().to_path_buf()).memory_dir_env_target(),
            Ok(Some("SUBJECT_MEMORY".to_string()))
        );
        // The mock builtin declares the reserved key → its env var.
        assert_eq!(
            TckAdapter::Native("mock".to_string()).memory_dir_env_target(),
            Ok(Some("KTESIO_MEMORY_DIR".to_string()))
        );
        // A manifest that cannot be re-read: named error, never a collapsed
        // None (the same honesty rule as the single parsed read).
        let err = TckAdapter::Manifest(PathBuf::from("/nonexistent/does-not-exist"))
            .memory_dir_env_target()
            .unwrap_err();
        assert!(err.contains("could not re-read"), "{err}");
    }

    /// The subject leg's two remaining arms: a declaration that maps
    /// `memory.dir` to a NON-env target while carrying `--dump` SKIPS the
    /// leg with the named reason (a surfaced skip, never a fabricated
    /// failure — pre-15-3 this shape passed, and WHAT the section verifies
    /// is unchanged; the probe twin remains the proof), and a start that
    /// cannot spawn (the snapshot's exec binary vanished after attach)
    /// still fails the leg naming the start.
    #[test]
    fn memory_subject_leg_names_unprovable_channels_and_failed_starts() {
        let (dir, facade) = helper_engine();

        // `memory.dir` mapped to a FLAG target: delivery is declared but the
        // dump echo has no env var to observe — the leg SKIPS naming the
        // unprovable channel and the probe twin as the proof (the
        // section-level gate only proved A mapping exists, not an env one).
        let flag_dir = tempfile::Builder::new()
            .prefix("ktesio-tck-submem-flag-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            flag_dir.path(),
            "tck-submem-flag",
            &["--dump", "config-dump.txt", "--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            Some("\n[config.\"memory.dir\"]\nflag = \"--memory-dir\"\n"),
        )
        .expect("write flag-target manifest");
        let flag_target = TckAdapter::Manifest(flag_dir.path().to_path_buf())
            .memory_dir_target()
            .expect("the single declaration read");
        assert!(
            flag_target
                .as_ref()
                .is_some_and(|target| target.env_var().is_none()),
            "the fixture maps memory.dir to a non-env target"
        );
        match subject_memory_delivery(
            &facade,
            dir.path(),
            &TckAdapter::Manifest(flag_dir.path().to_path_buf()),
            flag_target.as_ref(),
            Duration::from_millis(400),
        )
        .expect("a non-env memory.dir target is a skip, never a failure")
        {
            SubjectLegOutcome::Skipped { reason } => {
                assert!(
                    reason.contains("non-env target") && reason.contains("probe twin"),
                    "the skip names the unprovable channel and the remaining proof: {reason}"
                );
            }
            other => panic!("expected Skipped for a non-env memory.dir target, got {other:?}"),
        }

        // A start that cannot spawn: the registered launch's exec binary is
        // gone by start time — the leg fails naming the subject start.
        let gone_dir = tempfile::Builder::new()
            .prefix("ktesio-tck-submem-gone-")
            .tempdir()
            .expect("tempdir");
        let bin = gone_dir.path().join("vanishing-agent");
        std::fs::copy(crate::fake_agent_bin(), &bin).expect("copy agent");
        let manifest = format!(
            r#"
contract_version = "1.0.0"

[adapter]
kind = "tck-submem-gone"

[lifecycle.start]
exec = {exec:?}
args = ["--dump", "config-dump.txt", "--linger-ms", "600000"]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"

[config."memory.dir"]
env = "TCK_SUBJECT_MEMORY"
"#,
            exec = bin.to_string_lossy(),
        );
        std::fs::write(gone_dir.path().join("adapter.toml"), manifest).expect("write manifest");
        facade
            .register_with_adapter(
                "tck-subject",
                &AdapterRef::Manifest(gone_dir.path().to_path_buf()),
            )
            .expect("register subject");
        std::fs::remove_file(&bin).expect("remove the exec binary");
        let gone_target = TckAdapter::Manifest(gone_dir.path().to_path_buf())
            .memory_dir_target()
            .expect("the single declaration read");
        let err = subject_memory_delivery(
            &facade,
            dir.path(),
            &TckAdapter::Manifest(gone_dir.path().to_path_buf()),
            gone_target.as_ref(),
            Duration::from_millis(400),
        )
        .expect_err("a spawnable-never start must fail the leg");
        assert!(err.contains("subject start failed"), "{err}");
        // The failed leg still detached (no leaked attachment).
        assert!(facade.memory_status("tck-subject").unwrap().is_none());
    }

    // -----------------------------------------------------------------
    // Pure validators: every section's detection logic is a unit-tested
    // decision over committed data, so a deleted check CANNOT go green.
    // -----------------------------------------------------------------

    /// A synthetic committed transition event.
    fn evt(prior: LifecycleState, new: LifecycleState, cause: TransitionCause) -> TransitionEvent {
        TransitionEvent {
            schema_version: 1,
            instance: "tck".to_string(),
            prior_state: prior,
            new_state: new,
            cause,
            at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    /// `capability_edge_defect`: the OS-mismatch arm, the instability arm,
    /// and the honest path are all pinned.
    #[test]
    fn capability_edge_defect_pins_each_arm() {
        let honest = EffectiveCapabilities {
            os: OsId::current(),
            entries: vec![(Capability::Pause, SupportLevel::Guaranteed)],
        };
        // Honest + stable.
        assert_eq!(capability_edge_defect(&honest, &honest), Ok(()));
        // Unstable across reads.
        let drifted = EffectiveCapabilities {
            os: OsId::current(),
            entries: vec![(Capability::Pause, SupportLevel::BestEffort)],
        };
        assert_eq!(
            capability_edge_defect(&honest, &drifted),
            Err("effective projection is not stable across reads".to_string())
        );
        // Wrong OS (the engine projected a different host).
        let foreign = EffectiveCapabilities {
            os: match OsId::current() {
                OsId::Linux => OsId::Macos,
                _ => OsId::Linux,
            },
            entries: vec![],
        };
        assert!(capability_edge_defect(&foreign, &foreign).is_err());
    }

    /// `crash_leg_defect`: each arm of the crash contract is pinned — the
    /// restart count, the preserved exit code, the `crashed` terminal cause,
    /// and the no-events case.
    #[test]
    fn crash_leg_defect_pins_each_arm() {
        let crashed_event = evt(
            LifecycleState::Running,
            LifecycleState::Failed,
            TransitionCause::crashed("exited with code 7"),
        );
        // Honest evidence.
        assert_eq!(
            crash_leg_defect(
                0,
                Some("exited with code 7"),
                std::slice::from_ref(&crashed_event)
            ),
            Ok(())
        );
        // A restart under Never policy.
        assert!(crash_leg_defect(
            1,
            Some("exited with code 7"),
            std::slice::from_ref(&crashed_event)
        )
        .is_err());
        // Lost exit code.
        assert!(crash_leg_defect(
            0,
            Some("exited with code 1"),
            std::slice::from_ref(&crashed_event)
        )
        .is_err());
        assert!(crash_leg_defect(0, None, std::slice::from_ref(&crashed_event)).is_err());
        // A terminal event WITHOUT the crashed cause.
        let lied = evt(
            LifecycleState::Running,
            LifecycleState::Failed,
            TransitionCause::StopGraceful,
        );
        assert!(crash_leg_defect(0, Some("exited with code 7"), &[lied]).is_err());
        // No events at all.
        assert!(crash_leg_defect(0, Some("exited with code 7"), &[]).is_err());
    }

    /// `best_effort_cause_defect`: missing events and wrong qualifier tags
    /// are FAILURES; the honest qualifier pair passes.
    #[test]
    fn best_effort_cause_defect_pins_missing_events_and_wrong_tags() {
        let honest = [
            evt(
                LifecycleState::Running,
                LifecycleState::Paused,
                TransitionCause::PauseBestEffort {
                    detail: "macos best-effort".to_string(),
                },
            ),
            evt(
                LifecycleState::Paused,
                LifecycleState::Running,
                TransitionCause::ResumeBestEffort {
                    detail: "macos best-effort".to_string(),
                },
            ),
        ];
        assert_eq!(best_effort_cause_defect(&honest), Ok(()));

        // The pause event never landed.
        let no_pause = [honest[1].clone()];
        assert!(best_effort_cause_defect(&no_pause)
            .unwrap_err()
            .contains("appended NO paused event"));

        // The pause carried a PLAIN command cause (a guaranteed-style lie).
        let plain_pause = [
            evt(
                LifecycleState::Running,
                LifecycleState::Paused,
                TransitionCause::Command {
                    command: "pause".to_string(),
                },
            ),
            honest[1].clone(),
        ];
        assert!(best_effort_cause_defect(&plain_pause)
            .unwrap_err()
            .contains("pause-best-effort cause tag"));

        // The resume event never landed.
        let no_resume = [honest[0].clone()];
        assert!(best_effort_cause_defect(&no_resume)
            .unwrap_err()
            .contains("appended NO resumed event"));

        // The resume carried the wrong qualifier.
        let wrong_resume = [
            honest[0].clone(),
            evt(
                LifecycleState::Paused,
                LifecycleState::Running,
                TransitionCause::Command {
                    command: "resume".to_string(),
                },
            ),
        ];
        assert!(best_effort_cause_defect(&wrong_resume)
            .unwrap_err()
            .contains("resume-best-effort cause tag"));
    }

    /// `guaranteed_cause_defect`: plain command causes pass; missing events
    /// and best-effort qualifiers on a GUARANTEED declaration fail.
    #[test]
    fn guaranteed_cause_defect_pins_missing_events_and_qualifiers() {
        let honest = [
            evt(
                LifecycleState::Running,
                LifecycleState::Paused,
                TransitionCause::Command {
                    command: "pause".to_string(),
                },
            ),
            evt(
                LifecycleState::Paused,
                LifecycleState::Running,
                TransitionCause::Command {
                    command: "resume".to_string(),
                },
            ),
        ];
        assert_eq!(guaranteed_cause_defect(&honest), Ok(()));

        // No paused event.
        assert!(guaranteed_cause_defect(&honest[1..])
            .unwrap_err()
            .contains("no paused event"));

        // A best-effort qualifier on a Guaranteed declaration: the engine
        // lied about the suspension.
        let qualified = [
            evt(
                LifecycleState::Running,
                LifecycleState::Paused,
                TransitionCause::PauseBestEffort {
                    detail: "unexpected".to_string(),
                },
            ),
            honest[1].clone(),
        ];
        // A COMMAND cause carrying the qualifier wording (the subtle lie the
        // arm defends against).
        let worded = [
            evt(
                LifecycleState::Running,
                LifecycleState::Paused,
                TransitionCause::Command {
                    command: "pause (best-effort)".to_string(),
                },
            ),
            honest[1].clone(),
        ];
        assert!(guaranteed_cause_defect(&worded)
            .unwrap_err()
            .contains("NO best-effort qualifier"));
        // The PauseBestEffort kind itself is ALSO a lie on a Guaranteed
        // declaration — caught by the plain-command arm.
        assert!(guaranteed_cause_defect(&qualified)
            .unwrap_err()
            .contains("plain command cause"));

        // No resumed event.
        assert!(guaranteed_cause_defect(&honest[..1])
            .unwrap_err()
            .contains("no paused\u{2192}running event"));
    }

    /// `replay_row_defect`: the exact-count arm passes; both an over-count
    /// (a replayed batch double-counted) and an under-count (rows lost) fail.
    #[test]
    fn replay_row_defect_pins_over_count_and_exact_count() {
        assert_eq!(replay_row_defect(EXPECTED_SELF_REPORTED_ROWS), Ok(()));
        let over = EXPECTED_SELF_REPORTED_ROWS + 1;
        assert!(replay_row_defect(over)
            .unwrap_err()
            .contains("must not add a row"));
        let under = EXPECTED_SELF_REPORTED_ROWS - 1;
        assert!(replay_row_defect(under)
            .unwrap_err()
            .contains(&format!("got {under}")));
    }

    /// `fleet_totals_defect`: the honest cells pass; inflated and dropped
    /// totals each fail naming both observed and wanted pairs; the
    /// mismatched-source arm (a healthy engine cannot exhibit it) fails
    /// naming the wire string it read.
    #[test]
    fn fleet_totals_defect_pins_all_three_arms() {
        let batches = EXPECTED_SELF_REPORTED_ROWS;
        let want_in = batches * USAGE_INPUT_TOKENS;
        let want_out = batches * USAGE_OUTPUT_TOKENS;

        // The honest cells.
        assert_eq!(
            fleet_totals_defect(want_in, want_out, batches, "self-reported"),
            Ok(())
        );

        // A subject that over-reports its input tokens (e.g. `--usage-input-tokens 11`).
        let err = fleet_totals_defect(want_in + 3, want_out, batches, "self-reported").unwrap_err();
        assert!(
            err.contains("fleet totals must equal the ledger exactly"),
            "{err}"
        );
        assert!(err.contains("got (33, 60)"), "{err}");
        assert!(err.contains("want (30, 60)"), "{err}");

        // A subject that drops its output tokens.
        let err = fleet_totals_defect(want_in, 0, batches, "self-reported").unwrap_err();
        assert!(err.contains("got (30, 0)"), "{err}");

        // The source cell: right totals, wrong wire string.
        let err = fleet_totals_defect(want_in, want_out, batches, "engine-observed").unwrap_err();
        assert!(
            err.contains("fleet metering_source must be 'self-reported', got \"engine-observed\""),
            "{err}"
        );
    }

    /// A LYING subject drives the real `self_reported_metering_inner`
    /// pipeline end-to-end: `--usage-input-tokens 11` emits 3 honest-shaped
    /// batches with off-sentinel counts, so the ledger lands exactly 3 rows
    /// (dedup passes) while the Fleet-equality arm — and ONLY that arm —
    /// fails with the triage-grade reason. Pins the previously unexecuted
    /// `fleet totals must equal the ledger exactly` failure path.
    #[test]
    fn self_reported_metering_fails_when_the_subject_lies_about_its_tokens() {
        let (dir, facade) = helper_engine();
        let err = self_reported_metering_inner_with(
            &facade,
            dir.path(),
            SECTION_TIMEOUT,
            &["--usage-input-tokens", "11"],
        )
        .expect_err("a lying subject must fail the fleet-equality check");
        assert!(
            err.contains("fleet totals must equal the ledger exactly"),
            "{err}"
        );
        // Triage-grade: both observed and wanted pairs in the reason — the
        // operator sees the 33-vs-30 inflation without a rerun.
        assert!(err.contains("got (33, 60)"), "{err}");
        assert!(err.contains("want (30, 60)"), "{err}");
    }

    /// `pause_demo`: Guaranteed is a REAL suspension on Unix and an honest
    /// not_applicable naming the engine ceiling on Windows (never a silent
    /// best-effort pass); BestEffort is demonstrated everywhere.
    #[test]
    fn pause_demo_honest_per_os() {
        // Windows + Guaranteed: the ceiling, named for the report.
        match pause_demo(SupportLevel::Guaranteed, OsId::Windows) {
            PauseDemo::NotApplicable { reason } => {
                assert!(reason.contains("guaranteed"), "{reason}");
                assert!(reason.contains("windows"), "{reason}");
                assert!(reason.contains("real suspension"), "{reason}");
            }
            other => panic!("windows+guaranteed must be NotApplicable, got {other:?}"),
        }
        // Unix + Guaranteed: the real-suspension proof.
        for os in [OsId::Linux, OsId::Macos] {
            assert!(matches!(
                pause_demo(SupportLevel::Guaranteed, os),
                PauseDemo::RealSuspension
            ));
        }
        // BestEffort is demonstrated on every OS (never skipped).
        for os in [OsId::Linux, OsId::Macos, OsId::Windows] {
            assert!(matches!(
                pause_demo(SupportLevel::BestEffort, os),
                PauseDemo::BestEffort
            ));
        }
    }

    /// `interaction_probe_level` mirrors the level under test — a BestEffort
    /// declaration is demonstrated AT best-effort, never upgraded.
    #[test]
    fn interaction_probe_level_mirrors_the_declaration() {
        assert_eq!(
            interaction_probe_level(SupportLevel::Guaranteed),
            "guaranteed"
        );
        assert_eq!(
            interaction_probe_level(SupportLevel::BestEffort),
            "best-effort"
        );
        assert_eq!(
            interaction_probe_level(SupportLevel::Unsupported),
            "unsupported"
        );
    }

    /// `config_probe_scope`: no `[config]` rules reads Undeclared, flag/file
    /// -only rules read NoEnvRules, and env rules (minus the reserved keys)
    /// read Env.
    #[test]
    fn config_probe_scope_pins_each_fast_path() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-scope-")
            .tempdir()
            .expect("tempdir");
        let write = |body: &str| {
            std::fs::write(dir.path().join("adapter.toml"), body).unwrap();
            dir.path().join("adapter.toml")
        };

        // No config rules at all.
        let undeclared = write(
            "contract_version = \"1.0.0\"\n\n[adapter]\nkind = \"k\"\n\n[lifecycle.start]\nexec = \"x\"\nargs = []\n\n[metering]\nsource = \"self-reported\"\n",
        );
        assert!(matches!(
            config_probe_scope(&undeclared),
            Ok(ConfigScope::Undeclared)
        ));

        // Only flag/file targets: env delivery is the only provable channel.
        let no_env = write(
            "contract_version = \"1.0.0\"\n\n[adapter]\nkind = \"k\"\n\n[lifecycle.start]\nexec = \"x\"\nargs = []\n\n[metering]\nsource = \"self-reported\"\n\n[config.temperature]\nflag = \"--temp\"\n\n[config.seed]\nfile = { path = \"config/agent.toml\", key = \"llm.seed\" }\n",
        );
        assert!(matches!(
            config_probe_scope(&no_env),
            Ok(ConfigScope::NoEnvRules)
        ));

        // Env rules (plus a reserved key that must be filtered out).
        let env = write(
            "contract_version = \"1.0.0\"\n\n[adapter]\nkind = \"k\"\n\n[lifecycle.start]\nexec = \"x\"\nargs = []\n\n[metering]\nsource = \"self-reported\"\n\n[config.model]\nenv = \"MODEL\"\n\n[config.\"memory.dir\"]\nenv = \"IGNORED\"\n",
        );
        match config_probe_scope(&env) {
            Ok(ConfigScope::Env(rules)) => {
                assert_eq!(rules, vec![("model".to_string(), "MODEL".to_string())]);
            }
            other => panic!("expected Env rules, got {other:?}"),
        }

        // A missing manifest is the section's failure reason.
        assert!(config_probe_scope(&dir.path().join("nope.toml"))
            .unwrap_err()
            .contains("read subject manifest"));
    }

    /// The config section's declaration-justified fast paths: zero rules and
    /// env-less rules read not_applicable naming the gap — no engine, no
    /// panic.
    #[test]
    fn config_mapping_fast_paths_read_not_applicable() {
        let (dir, facade) = helper_engine();
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-cfgfast-")
            .tempdir()
            .expect("tempdir");
        let write = |body: &str| {
            std::fs::write(scratch.path().join("adapter.toml"), body).unwrap();
            scratch.path().join("adapter.toml")
        };

        let undeclared = write(
            "contract_version = \"1.0.0\"\n\n[adapter]\nkind = \"k\"\n\n[lifecycle.start]\nexec = \"x\"\nargs = []\n\n[metering]\nsource = \"self-reported\"\n",
        );
        match run_config_mapping(
            &facade,
            dir.path(),
            &TckAdapter::Manifest(undeclared.parent().unwrap().to_path_buf()),
            SECTION_TIMEOUT,
        ) {
            SectionResult::NotApplicable { reason } => {
                assert!(reason.contains("no [config] rules"), "{reason}");
            }
            other => panic!("expected NotApplicable for an undeclared config, got {other:?}"),
        }

        let no_env = write(
            "contract_version = \"1.0.0\"\n\n[adapter]\nkind = \"k\"\n\n[lifecycle.start]\nexec = \"x\"\nargs = []\n\n[metering]\nsource = \"self-reported\"\n\n[config.temperature]\nflag = \"--temp\"\n",
        );
        match run_config_mapping(
            &facade,
            dir.path(),
            &TckAdapter::Manifest(no_env.parent().unwrap().to_path_buf()),
            SECTION_TIMEOUT,
        ) {
            SectionResult::NotApplicable { reason } => {
                assert!(reason.contains("ENV delivery"), "{reason}");
            }
            other => panic!("expected NotApplicable for an env-less config, got {other:?}"),
        }
    }

    /// An interaction probe launched WITHOUT `--echo-stdin` never echoes:
    /// the delivery proof must FAIL naming the line it waited for (pinned so
    /// the echo check cannot be deleted silently).
    #[test]
    fn interaction_delivery_fails_when_the_probe_never_echoes() {
        let (_dir, facade) = helper_engine();
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-noecho-")
            .tempdir()
            .expect("tempdir");
        // Deliberately NO --echo-stdin: the channel exists, input is
        // consumed, but the echo line can never appear.
        write_probe_manifest(
            scratch.path(),
            "tck-interaction-probe",
            &["--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            None,
        )
        .expect("write no-echo manifest");
        facade
            .register_with_adapter(
                "tck-interaction-probe",
                &AdapterRef::Manifest(scratch.path().to_path_buf()),
            )
            .expect("register no-echo probe");
        facade.start("tck-interaction-probe").expect("start probe");

        let err =
            interaction_delivery(&facade, "tck-interaction-probe", Duration::from_millis(400))
                .expect_err("a probe that never echoes must fail the delivery proof");
        assert!(err.contains("never observed"), "{err}");
    }

    /// `wait_for_state` fails FAST (with the actual state in the reason) when
    /// the instance reaches a terminal state other than the wanted one —
    /// instead of spinning out the whole budget.
    #[test]
    fn wait_for_state_fails_fast_on_a_terminal_state() {
        let (_dir, facade) = helper_engine();
        let scratch = tempfile::Builder::new()
            .prefix("ktesio-tck-terminal-")
            .tempdir()
            .expect("tempdir");
        write_probe_manifest(
            scratch.path(),
            "tck-terminal-probe",
            &["--linger-ms", "600000"],
            None,
            "guaranteed",
            "self-reported",
            "1.0.0",
            None,
        )
        .expect("write terminal probe manifest");
        // Point the launch exec at a path that does not exist on ANY OS: the
        // start lands `failed` (a terminal state that can never become
        // Running). Rewrite the `exec = ...` LINE rather than string-matching
        // the binary path — the manifest stores the path TOML-escaped
        // (`{:?}`), so on Windows the raw text has doubled backslashes and a
        // plain-path replace would silently no-op.
        let manifest = std::fs::read_to_string(scratch.path().join("adapter.toml")).unwrap();
        let broken = manifest
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("exec = ") {
                    "exec = \"./tck-missing-agent\""
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            broken.contains("./tck-missing-agent") && broken != manifest,
            "the exec rewrite must take effect (escaped-path no-op guard)"
        );
        std::fs::write(scratch.path().join("adapter.toml"), broken).unwrap();
        facade
            .register_with_adapter(
                "tck-terminal",
                &AdapterRef::Manifest(scratch.path().to_path_buf()),
            )
            .expect("register terminal probe");
        // A missing exec may surface as a start error OR as an instance that
        // landed `failed` — either way the state is (or becomes) terminal.
        let _ = facade.start("tck-terminal");

        let err = wait_for_state(
            &facade,
            "tck-terminal",
            LifecycleState::Running,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(
            err.contains("terminal state") && err.contains("Failed"),
            "{err}"
        );
    }

    /// The never-panic boundary: a harness panic is converted into the
    /// complete all-`fail` report naming the panic payload.
    #[test]
    fn a_panicking_harness_yields_a_complete_fail_report() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-panic-")
            .tempdir()
            .expect("tempdir");
        let adapter = TckAdapter::Manifest(dir.path().to_path_buf());
        let report = catch_report(&adapter, &TckConfig::default(), || {
            panic!("deliberate tck panic")
        });
        assert!(!report.is_conformant());
        assert_eq!(report.sections.len(), 8);
        for entry in report.sections.iter() {
            match &entry.result {
                SectionResult::Fail { reason } => {
                    assert!(
                        reason.contains("harness itself panicked")
                            && reason.contains("deliberate tck panic"),
                        "{reason}"
                    );
                }
                other => panic!("expected Fail, got {other:?}"),
            }
        }
    }

    /// The report round-trips through serde (the machine-readable contract is
    /// also consumable, not just producible) and pins the schema version.
    #[test]
    fn report_round_trips_through_serde_with_schema_version() {
        let report = ConformanceReport {
            schema_version: REPORT_SCHEMA_VERSION,
            contract_version: ::hekma_adapter_api::CONTRACT_VERSION.to_string(),
            adapter_kind: "round-trip".to_string(),
            sections: vec![
                SectionReport {
                    section: section_ids::PAUSE.to_string(),
                    result: SectionResult::Pass,
                },
                SectionReport {
                    section: section_ids::MEMORY.to_string(),
                    result: SectionResult::NotApplicable {
                        reason: "undeclared".to_string(),
                    },
                },
                SectionReport {
                    section: section_ids::LIFECYCLE.to_string(),
                    result: SectionResult::fail("boom"),
                },
            ],
        };
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.contains("\"schema_version\":1"), "{json}");
        // The negotiated contract version rides on every report (retro #163,
        // finding B8) — pinned as the verbatim wire value.
        assert!(json.contains("\"contract_version\":\"1.1.0\""), "{json}");
        let back: ConformanceReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, report);
        assert_eq!(back.schema_version, REPORT_SCHEMA_VERSION);
        assert_eq!(back.contract_version, ::hekma_adapter_api::CONTRACT_VERSION);
        assert_eq!(back.adapter_kind, "round-trip");
    }

    /// Archived reports from BEFORE the `contract_version` field landed carry
    /// the same `schema_version: 1` and no such key — `#[serde(default)]`
    /// keeps them deserializing (review finding: an additive field must not
    /// break the deserialization direction of the bump policy).
    #[test]
    fn archived_report_json_without_contract_version_still_deserializes() {
        let legacy = r#"{"schema_version":1,"adapter_kind":"old","sections":[]}"#;
        let back: ConformanceReport = serde_json::from_str(legacy).expect("deserialize");
        assert_eq!(back.schema_version, REPORT_SCHEMA_VERSION);
        assert_eq!(back.adapter_kind, "old");
        assert_eq!(back.contract_version, "", "empty = produced pre-field");
    }
}
