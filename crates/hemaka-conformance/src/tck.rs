//! # The Conformance Test Kit (TCK) — story 6-4, FR-27
//!
//! A **compliance report**, not a test framework: the harness registers a
//! caller-provided adapter with a FRESH engine over a hermetic temp state root,
//! drives every section a live adapter can be held to, and returns a
//! machine-readable [`ConformanceReport`] whose per-section entries are
//! `pass` / `fail(reason)` / `not_applicable(reason)`. The caller's `#[test]`
//! asserts on the report — this keeps the TCK usable from any third-party
//! adapter crate's dev-tests against `hemaka-conformance` alone.
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
//!   sequence-0 batch does not double-count.
//! * `metering_engine_observed` — for EngineObserved adapters only: a
//!   loopback upstream stub (Content-Length responses, the fixed 30/70/100
//!   usage body), the operator sets the real upstream, and 3 forwarded calls
//!   commit exactly.
//! * `memory` — attach → status reports the attachment AND the declared
//!   delivery fact → a probe with the same declaration receives the managed
//!   dir through its declared env var → detach clears it. Adapters that
//!   declare no `memory.dir` mapping read `not_applicable` (delivery is
//!   offered, not imposed); a declaration that cannot be re-read FAILS the
//!   section naming the cause (never a fabricated "maps nothing"). The
//!   SUBJECT's own declared delivery is proven for the shipping hermes
//!   builtin by the hermes pass (`hermes_tck.rs`: the subject's process
//!   receives the managed dir as `HERMES_HOME`).
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

use hemaka_adapter_api::{Capability, MeteringSource, OsId, SupportLevel};
use hemaka_engine::{AdapterRef, Engine, LifecycleState, MemoryBackingKind, RestartPolicy};

/// How long any single section's poll may run before it fails the section
/// (never the harness) with a timeout reason. Generous: this covers a loaded
/// CI runner plus the engine reaper's ~250ms cadence.
const SECTION_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// negotiated [`hemaka_adapter_api::CONTRACT_VERSION`] (retro #163,
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
    /// verdict a third-party `#[test]` asserts.
    pub fn is_conformant(&self) -> bool {
        self.sections
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

    /// Whether the adapter's declaration maps the reserved `memory.dir` key.
    /// For a MANIFEST adapter this re-reads the `adapter.toml` file the caller
    /// supplied (that file IS the declaration the engine registered — the
    /// engine has no separate persisted copy of the mapping), so the source is
    /// caller-controlled by nature. For a NATIVE adapter the declaration is
    /// the engine's compiled-in builtin table (no I/O — cannot fail).
    ///
    /// A manifest re-read/parse FAILURE is `Err` (retro #163, finding B11):
    /// it must FAIL the memory section, never collapse to `false` — a
    /// collapsed `false` would report `not_applicable` ("maps no reserved
    /// key"), a lie about a declaration the harness could not read.
    fn declares_memory_dir(&self) -> Result<bool, String> {
        match self {
            TckAdapter::Manifest(dir) => {
                let path = dir.join("adapter.toml");
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("could not re-read {}: {e}", path.display()))?;
                let manifest = ::hemaka_adapter_api::Manifest::from_toml_str(&text)
                    .map_err(|e| format!("could not re-parse {}: {e}", path.display()))?;
                Ok(manifest
                    .config_mapping()
                    .target(::hemaka_engine::domain::MEMORY_DIR_KEY)
                    .is_some())
            }
            TckAdapter::Native(kind) => Ok(::hemaka_engine::adapter::native_config_mapping(kind)
                .is_some_and(|mapping| {
                    mapping
                        .target(::hemaka_engine::domain::MEMORY_DIR_KEY)
                        .is_some()
                })),
        }
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
}

/// The full mock/manifest conformance pass: run every section against a
/// manifest adapter and return the report. A thin wrapper over
/// [`run_conformance`].
pub fn run_mock_conformance(manifest_dir: &Path) -> ConformanceReport {
    run_conformance(&TckAdapter::Manifest(manifest_dir.to_path_buf()))
}

/// The public harness entry point: register the caller's adapter with a fresh
/// engine over a hermetic temp state root, run every applicable section, and
/// return the report. A registration failure (a contract violation before any
/// section can run) yields the complete all-`fail` report — never a panic.
pub fn run_conformance(adapter: &TckAdapter) -> ConformanceReport {
    catch_report(adapter, || {
        // A registration failure IS a complete all-`fail` report, not a
        // separate error surface: the detail string just fills every
        // section's reason.
        conformance_inner(adapter)
            .map_err(|detail| registration_failure_report(adapter, detail))
            .unwrap_or_else(|report| report)
    })
}

/// The never-panic boundary: run `op` and, if it panics, return the complete
/// all-`fail` report naming the panic (the payload's message when it is a
/// string, a generic note otherwise). One place — the entry point flows
/// through here.
fn catch_report<F>(adapter: &TckAdapter, op: F) -> ConformanceReport
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
            registration_failure_report(adapter, format!("the harness itself panicked: {message}"))
        }
    }
}

/// [`run_conformance`]'s pipeline; `Err` carries the registration-failure
/// detail that fills every section's reason.
fn conformance_inner(adapter: &TckAdapter) -> Result<ConformanceReport, String> {
    let state = tempfile::Builder::new()
        .prefix("ktesio-tck-")
        .tempdir()
        .map_err(|e| format!("state tempdir: {e}"))?;
    let engine =
        Engine::open(Some(state.path().to_path_buf())).map_err(|e| format!("engine open: {e}"))?;
    let facade = engine.blocking();

    let registered = facade
        .register_with_adapter("tck-subject", &adapter.adapter_ref())
        .map_err(|e| format!("register: {e}"))?;

    // The declaration under test, read from the engine's own projection (the
    // adapter as REGISTERED — the source of every applicability decision).
    let capabilities = facade
        .effective_capabilities("tck-subject")
        .map_err(|e| format!("effective capabilities: {e}"))?;

    // The subject manifest path (manifest adapters only) — the config section
    // reads the DECLARED mapping rules from it.
    let subject_manifest_path = adapter.manifest_path();

    // The subject's declared `memory.dir` delivery fact (the memory section's
    // applicability input): manifest subjects read their own adapter.toml;
    // native subjects consult the engine's builtin table — both are the
    // REGISTERED declaration, never a caller-supplied copy. A re-read failure
    // is carried as `Err` and FAILS the memory section (retro #163, finding
    // B11) — it must never collapse to "declares nothing".
    let declares_memory_dir = adapter.declares_memory_dir();

    // The registered adapter snapshot (the launchability + metering truth).
    let snapshot = read_snapshot(state.path(), "tck-subject")?;

    // Launchable ⇔ the snapshot carries a resolved `launch` (present AND
    // non-null): a manifest adapter that declares `[lifecycle.start]`. A
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

    let mut sections = Vec::new();

    sections.push(SectionReport {
        section: section_ids::CAPABILITY_EDGES.to_string(),
        result: run_capability_edges(&facade, "tck-subject", &capabilities),
    });

    sections.push(SectionReport {
        section: section_ids::LIFECYCLE.to_string(),
        result: if launchable {
            run_lifecycle(&facade, "tck-subject")
        } else {
            SectionResult::NotApplicable {
                reason: "the registered adapter declares no launchable [lifecycle.start] \
                         template, so no live lifecycle can be driven"
                    .to_string(),
            }
        },
    });

    sections.push(SectionReport {
        section: section_ids::PAUSE.to_string(),
        result: run_pause_section(
            &facade,
            state.path(),
            "tck-subject",
            launchable,
            pause_level,
        ),
    });

    sections.push(SectionReport {
        section: section_ids::CONFIG_MAPPING.to_string(),
        result: if launchable {
            run_config_mapping(&facade, state.path(), subject_manifest_path.as_deref())
        } else {
            SectionResult::NotApplicable {
                reason: "no launchable agent to observe the delivered config".to_string(),
            }
        },
    });

    sections.push(SectionReport {
        section: section_ids::METERING_SELF_REPORTED.to_string(),
        result: match (&snapshot.metering_source, launchable) {
            (MeteringSource::SelfReported, true) => {
                run_self_reported_metering(&facade, state.path())
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

    sections.push(SectionReport {
        section: section_ids::METERING_ENGINE_OBSERVED.to_string(),
        result: match (&snapshot.metering_source, launchable) {
            (MeteringSource::EngineObserved, true) => {
                run_engine_observed_metering(&facade, state.path())
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

    sections.push(SectionReport {
        section: section_ids::MEMORY.to_string(),
        result: match &declares_memory_dir {
            // A declaration we cannot re-read is a FAIL naming the cause —
            // never a `not_applicable` derived from a fabricated "maps
            // nothing" (retro #163, finding B11).
            Err(detail) => SectionResult::fail(format!(
                "the declaration could not be re-read to decide the memory section's \
                 applicability: {detail}"
            )),
            Ok(declares) => run_memory(&facade, state.path(), launchable, *declares),
        },
    });

    sections.push(SectionReport {
        section: section_ids::INTERACTION.to_string(),
        result: match (launchable, interaction_level) {
            (true, Some(level)) => run_interaction(&facade, state.path(), level),
            (false, _) => SectionResult::NotApplicable {
                reason: "no launchable agent to interact with".to_string(),
            },
            (true, None) => SectionResult::NotApplicable {
                reason: "the declaration declares no interaction capability on this OS".to_string(),
            },
        },
    });

    // Finalize: stop everything the harness left running — the subject and
    // any probe twin a failed section orphaned via `?` on its way out.
    stop_leftovers(&facade);

    Ok(ConformanceReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: ::hemaka_adapter_api::CONTRACT_VERSION.to_string(),
        adapter_kind: registered.kind.clone(),
        sections,
    })
}

/// Stop every instance still live in the harness engine (Running, Starting,
/// Paused, or mid-Stopping). Sections stop their own probes on success; this
/// is the orphan sweep for the `?` paths and the crash twin.
fn stop_leftovers(facade: &::hemaka_engine::Blocking<'_>) {
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

/// The report shape when registration itself failed (every section reports the
/// registration failure — the caller sees a complete, machine-readable report
/// rather than an error type).
fn registration_failure_report(adapter: &TckAdapter, detail: String) -> ConformanceReport {
    let kind = adapter.kind_label();
    let fail = || SectionResult::Fail {
        reason: format!("registration failed: {detail}"),
    };
    ConformanceReport {
        schema_version: REPORT_SCHEMA_VERSION,
        contract_version: ::hemaka_adapter_api::CONTRACT_VERSION.to_string(),
        adapter_kind: kind,
        sections: vec![
            SectionReport {
                section: section_ids::CAPABILITY_EDGES.to_string(),
                result: fail(),
            },
            SectionReport {
                section: section_ids::LIFECYCLE.to_string(),
                result: fail(),
            },
            SectionReport {
                section: section_ids::PAUSE.to_string(),
                result: fail(),
            },
            SectionReport {
                section: section_ids::CONFIG_MAPPING.to_string(),
                result: fail(),
            },
            SectionReport {
                section: section_ids::METERING_SELF_REPORTED.to_string(),
                result: fail(),
            },
            SectionReport {
                section: section_ids::METERING_ENGINE_OBSERVED.to_string(),
                result: fail(),
            },
            SectionReport {
                section: section_ids::MEMORY.to_string(),
                result: fail(),
            },
            SectionReport {
                section: section_ids::INTERACTION.to_string(),
                result: fail(),
            },
        ],
    }
}

/// The registered adapter snapshot: the parts the TCK's applicability decisions
/// are derived from (metering source + launch presence), read from the
/// engine-written `adapter.json` — the registered truth, not a caller-supplied
/// copy.
#[derive(Debug)]
struct AdapterSnapshot {
    /// The declared Metering Source, as its wire string.
    metering_source: MeteringSource,
    /// Whether the snapshot carries a resolved launch (present AND non-null).
    launch: bool,
}

/// Read + minimally parse the engine's adapter snapshot for `name`.
fn read_snapshot(state_dir: &Path, name: &str) -> Result<AdapterSnapshot, String> {
    let home = state_dir.join("agents").join(name);
    let text = std::fs::read_to_string(home.join("adapter.json"))
        .map_err(|e| format!("read adapter snapshot: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse adapter snapshot: {e}"))?;
    let metering_source = match value
        .get("metering_source")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "adapter snapshot carries no metering_source".to_string())?
    {
        "self-reported" => MeteringSource::SelfReported,
        "engine-observed" => MeteringSource::EngineObserved,
        other => return Err(format!("unknown metering_source {other:?} in the snapshot")),
    };
    // `launch` present AND non-null ⇔ the adapter carries a resolved
    // registration-time launch (manifest + `[lifecycle.start]`).
    let launch = value.get("launch").map(|l| !l.is_null()).unwrap_or(false);
    Ok(AdapterSnapshot {
        metering_source,
        launch,
    })
}

// ---------------------------------------------------------------------
// Shared polling helpers (all on COMMITTED artifacts / public reads)
// ---------------------------------------------------------------------

/// Poll the committed lifecycle state (via the public read) until it reaches
/// `want`, or report timeout.
fn wait_for_state(
    facade: &::hemaka_engine::Blocking<'_>,
    name: &str,
    want: LifecycleState,
) -> Result<(), String> {
    wait_for_state_for(facade, name, want, SECTION_TIMEOUT)
}

/// [`wait_for_state`] with an explicit deadline budget (the section timeout by
/// default; the harness tests pass a tiny budget to prove the timeout arm).
fn wait_for_state_for(
    facade: &::hemaka_engine::Blocking<'_>,
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

/// Count committed usage rows for `name` via a direct read-only connection to
/// the same state DB the engine commits to (committed STATE, never a guess).
fn usage_row_count(state_dir: &Path, name: &str) -> u64 {
    rusqlite::Connection::open(state_dir.join("state.db"))
        .map(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM usage_events e \
                 JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n.max(0) as u64)
            .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Poll the committed usage-row count until it reaches `expected`.
fn wait_for_usage_rows(state_dir: &Path, name: &str, expected: u64) -> Result<(), String> {
    wait_for_usage_rows_for(state_dir, name, expected, SECTION_TIMEOUT)
}

/// [`wait_for_usage_rows`] with an explicit deadline budget.
fn wait_for_usage_rows_for(
    state_dir: &Path,
    name: &str,
    expected: u64,
    budget: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let count = usage_row_count(state_dir, name);
        if count >= expected {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for {expected} committed usage rows (have {count})"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Count committed ENGINE-OBSERVED usage rows for `name`.
fn observed_row_count(state_dir: &Path, name: &str) -> u64 {
    rusqlite::Connection::open(state_dir.join("state.db"))
        .map(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM usage_events e \
                 JOIN agent_instances i ON i.id = e.instance_id \
                 WHERE i.name = ?1 AND e.metering_source = 'engine-observed'",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n.max(0) as u64)
            .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// The fleet entry for `name`, or a failure reason.
fn fleet_entry(
    facade: &::hemaka_engine::Blocking<'_>,
    name: &str,
) -> Result<::hemaka_engine::FleetEntry, String> {
    let fleet = facade
        .fleet()
        .map_err(|e| format!("fleet read failed: {e}"))?;
    fleet
        .into_iter()
        .find(|e| e.name.as_str() == name)
        .ok_or_else(|| format!("fleet entry '{name}' missing"))
}

/// The agent's captured-output log path (the engine-owned layout leaf tests
/// may observe).
fn agent_log_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir
        .join("agents")
        .join(name)
        .join("logs")
        .join("agent.log")
}

/// [`wait_for_log_line`] with an explicit deadline budget.
fn wait_for_log_line_for(log: &Path, wanted: &str, budget: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Ok(contents) = std::fs::read_to_string(log) {
            if contents.lines().any(|line| line == wanted) {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("never observed {wanted:?} in {}", log.display()));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Count `heartbeat <n>` lines in an agent's captured output log.
fn heartbeat_lines(agent_log: &Path) -> usize {
    std::fs::read_to_string(agent_log)
        .map(|c| c.lines().filter(|l| l.starts_with("heartbeat ")).count())
        .unwrap_or(0)
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
    facade: &::hemaka_engine::Blocking<'_>,
    name: &str,
    effective: &::hemaka_engine::EffectiveCapabilities,
) -> SectionResult {
    into_section_result(capability_edges_inner(facade, name, effective))
}

/// [`run_capability_edges`]'s pipeline: `Ok(())` ⇔ the projection is honest.
fn capability_edges_inner(
    facade: &::hemaka_engine::Blocking<'_>,
    name: &str,
    effective: &::hemaka_engine::EffectiveCapabilities,
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
    effective: &::hemaka_engine::EffectiveCapabilities,
    again: &::hemaka_engine::EffectiveCapabilities,
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
fn run_lifecycle(facade: &::hemaka_engine::Blocking<'_>, name: &str) -> SectionResult {
    into_section_result(lifecycle_inner(facade, name))
}

/// [`run_lifecycle`]'s pipeline.
fn lifecycle_inner(facade: &::hemaka_engine::Blocking<'_>, name: &str) -> Result<(), String> {
    // Start → running.
    facade
        .start(name)
        .map_err(|e| format!("start failed: {e}"))?;
    wait_for_state(facade, name, LifecycleState::Running)?;

    // Stop → stopped (bounded window; the fixture agents linger until told).
    facade
        .stop(name, Some(Duration::from_secs(5)))
        .map_err(|e| format!("stop failed: {e}"))?;
    wait_for_state(facade, name, LifecycleState::Stopped)?;

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
    run_crash_leg(facade).map_or(Ok(()), Err)
}

/// The crash leg. The harness registers a second instance from its OWN crash
/// twin manifest (`fake_agent --crash-after-ms 450 --crash-with 7` — past the
/// readiness window, exit code 7), arms a Never policy, starts it, and waits
/// for the reaper to land `failed`. Returns `None` on success, `Some(reason)`
/// on failure.
fn run_crash_leg(facade: &::hemaka_engine::Blocking<'_>) -> Option<String> {
    crash_leg_inner(facade).err()
}

/// [`run_crash_leg`]'s pipeline.
fn crash_leg_inner(facade: &::hemaka_engine::Blocking<'_>) -> Result<(), String> {
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
    wait_for_state(facade, "tck-crash", LifecycleState::Failed)
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
    events: &[::hemaka_engine::TransitionEvent],
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
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    name: &str,
    launchable: bool,
    level: Option<SupportLevel>,
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

    into_section_result(pause_applicable_inner(facade, state_dir, name, level))
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
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    name: &str,
    level: SupportLevel,
) -> Result<(), String> {
    facade
        .start(name)
        .map_err(|e| format!("start failed: {e}"))?;
    wait_for_state(facade, name, LifecycleState::Running)?;

    // Guaranteed on Windows never reaches here (the section gate reports it
    // not_applicable via `pause_demo`); everything else is applicable.
    let result = if matches!(
        pause_demo(level, OsId::current()),
        PauseDemo::RealSuspension
    ) {
        pause_guaranteed_inner(facade, state_dir, name)
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
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    name: &str,
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
    wait_for_state(facade, name, LifecycleState::Running)?;

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
    wait_for_state(facade, probe_kind, LifecycleState::Running)?;

    let freeze_failure = prove_heartbeat_freeze(facade, state_dir, probe_kind, SECTION_TIMEOUT);

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
fn guaranteed_cause_defect(events: &[::hemaka_engine::TransitionEvent]) -> Result<(), String> {
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
/// first-heartbeat startup wait and the post-SIGCONT growth wait) — the
/// section timeout by default. Returns `None` on success.
fn prove_heartbeat_freeze(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    probe: &str,
    budget: Duration,
) -> Option<String> {
    heartbeat_freeze_inner(facade, state_dir, probe, budget).err()
}

/// [`prove_heartbeat_freeze`]'s pipeline.
fn heartbeat_freeze_inner(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    probe: &str,
    budget: Duration,
) -> Result<(), String> {
    let log = agent_log_path(state_dir, probe);
    let deadline = std::time::Instant::now() + budget;
    loop {
        if heartbeat_lines(&log) >= 2 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "the pause probe's heartbeat never started at {}",
                log.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    facade
        .pause(probe)
        .map_err(|e| format!("probe pause failed: {e}"))?;

    std::thread::sleep(Duration::from_millis(200)); // let SIGSTOP + in-flight lines settle
    let baseline = heartbeat_lines(&log);
    let watch_until = std::time::Instant::now() + Duration::from_millis(1000);
    while std::time::Instant::now() < watch_until {
        let now = heartbeat_lines(&log);
        if now > baseline {
            return Err(format!(
                "heartbeat must NOT grow while paused (real SIGSTOP): baseline {baseline}, saw {now}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let after = heartbeat_lines(&log);
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
        if heartbeat_lines(&log) > after {
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
    facade: &::hemaka_engine::Blocking<'_>,
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
fn best_effort_cause_defect(events: &[::hemaka_engine::TransitionEvent]) -> Result<(), String> {
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
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    subject_manifest: Option<&Path>,
) -> SectionResult {
    let Some(path) = subject_manifest else {
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
    match config_probe_scope(path) {
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
            state_dir,
            rules,
            SECTION_TIMEOUT,
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
    let manifest = ::hemaka_adapter_api::Manifest::from_toml_str(&text)
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
            ::hemaka_adapter_api::ConfigTarget::Env { env } => Some((key.clone(), env.clone())),
            _ => None,
        })
        .filter(|(key, _)| {
            key != ::hemaka_engine::domain::MEMORY_DIR_KEY
                && key != ::hemaka_engine::domain::METERING_BASE_URL_KEY
        })
        .collect();
    if env_rules.is_empty() {
        Ok(ConfigScope::NoEnvRules)
    } else {
        Ok(ConfigScope::Env(env_rules))
    }
}

/// [`run_config_mapping`]'s pipeline for a MANIFEST subject with the given
/// env-mapped rules. `poll_budget` bounds the dump-artifact poll (the section
/// timeout by default; the harness tests pass a tiny budget to prove the
/// not-delivered arm).
fn config_mapping_inner(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
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

    // The subject's own `--dump <path>` pair (from its committed launch
    // snapshot) is the delivery-evidence artifact. A manifest that accepts no
    // `--dump <path>` cannot prove delivered config — the section reports the
    // gap (the TCK does not invent config delivery).
    let dump_token = read_launch_dump_path(state_dir, name).ok_or(
        "the registered launch carries no `--dump <path>` pair; the TCK requires \
         manifest subjects to accept `--dump <path>` so delivered config is provable",
    )?;
    // The dump path is the MANIFEST'S choice: absolute, or relative to the
    // subject's Agent Home (the process CWD the engine execs it with).
    let home = state_dir.join("agents").join(name);
    let dump = if std::path::Path::new(&dump_token).is_absolute() {
        std::path::PathBuf::from(dump_token)
    } else {
        home.join(&dump_token)
    };

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
    wait_for_state(facade, name, LifecycleState::Running)?;

    // Every DECLARED env rule must receive the set value under its OWN env
    // var; a miss names the dump lines that DID land (triage without a rerun).
    for (key, env) in &declared {
        if !dump_contains_env_for(&dump, env, probe_value, poll_budget) {
            return Err(format!(
                "the mapped value for '{key}' never reached the agent's environment \
                 as env={env}={probe_value} (dump at {} showed: {})",
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
             env=TCK_PROBE=verbatim-1 line; dump at {} showed: {})",
            dump.display(),
            dump_env_lines(&dump)
        ));
    }

    facade
        .stop(name, Some(Duration::from_secs(5)))
        .map_err(|e| format!("stop failed: {e}"))?;
    Ok(())
}

/// The token following `--dump` in the subject's committed launch snapshot
/// (`adapter.json` → launch.args), when present.
fn read_launch_dump_path(state_dir: &Path, name: &str) -> Option<String> {
    let home = state_dir.join("agents").join(name);
    let text = std::fs::read_to_string(home.join("adapter.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let args = value.get("launch")?.get("args")?.as_array()?;
    let mut args = args.iter().filter_map(|a| a.as_str());
    while let Some(arg) = args.next() {
        if arg == "--dump" {
            return args.next().map(|p| p.to_string());
        }
    }
    None
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

/// Self-reported metering: a probe emits 3 sentinel batches (10 in / 20 out);
/// the ledger gains EXACTLY 3 rows; a replayed sequence-0 batch does not
/// double-count; Fleet totals equal the ledger exactly.
fn run_self_reported_metering(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
) -> SectionResult {
    into_section_result(self_reported_metering_inner(facade, state_dir))
}

/// [`run_self_reported_metering`]'s pipeline.
fn self_reported_metering_inner(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
) -> Result<(), String> {
    let probe_kind = "tck-meter-probe";
    let dir = tempfile::Builder::new()
        .prefix("ktesio-tck-meter-")
        .tempdir()
        .map_err(|e| format!("probe tempdir: {e}"))?;
    // ONE probe proves BOTH halves (landing + dedup): `--emit-usage 3` emits
    // the 3 distinct events, `--replay-usage` re-sends sequence 0 afterward.
    write_probe_manifest(
        dir.path(),
        probe_kind,
        &[
            "--emit-usage",
            "3",
            "--replay-usage",
            "--linger-ms",
            "600000",
        ],
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

    // The 3 DISTINCT events commit; the replayed sequence-0 line is then
    // drained as a no-op. Wait for 3 rows, settle past the replay drain, and
    // require the count to STAY exactly 3 (an absolute count — a floor-poll
    // would hide a genuine over-count).
    wait_for_usage_rows(state_dir, probe_kind, EXPECTED_SELF_REPORTED_ROWS)?;
    std::thread::sleep(Duration::from_millis(800));
    let rows = usage_row_count(state_dir, probe_kind);
    replay_row_defect(rows)?;

    // Fleet totals EQUAL the ledger exactly: 3 × (10 in, 20 out).
    let entry = fleet_entry(facade, probe_kind)?;
    let want_in = 3 * USAGE_INPUT_TOKENS;
    let want_out = 3 * USAGE_OUTPUT_TOKENS;
    if entry.usage.cumulative_input_tokens != want_in
        || entry.usage.cumulative_output_tokens != want_out
    {
        return Err(format!(
            "fleet totals must equal the ledger exactly: got ({}, {}), want \
             ({want_in}, {want_out})",
            entry.usage.cumulative_input_tokens, entry.usage.cumulative_output_tokens
        ));
    }
    if entry.metering_source != "self-reported" {
        return Err(format!(
            "fleet metering_source must be 'self-reported', got {:?}",
            entry.metering_source
        ));
    }

    let _ = facade.stop(probe_kind, Some(Duration::from_secs(5)));
    Ok(())
}

/// Engine-observed metering: the operator sets the real upstream
/// (`metering.upstream_base_url`); the engine's loopback proxy forwards the
/// agent's calls and commits observed rows. The upstream is a loopback TCP
/// stub (pure std) serving the known 30/70/100 usage with a proper
/// Content-Length.
fn run_engine_observed_metering(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
) -> SectionResult {
    into_section_result(engine_observed_metering_inner(facade, state_dir))
}

/// [`run_engine_observed_metering`]'s pipeline.
fn engine_observed_metering_inner(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
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

    // Wait for the observed rows to commit (the reaper drains the listener).
    let deadline = std::time::Instant::now() + SECTION_TIMEOUT;
    loop {
        let count = observed_row_count(state_dir, probe_kind);
        if count >= OBSERVED_CALLS {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for {OBSERVED_CALLS} engine-observed rows (have {count})"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }

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
/// delivery fact → a probe twin with the same declaration RECEIVES the managed
/// dir through ITS declared env var → detach clears it. NotApplicable when the
/// adapter declares no `memory.dir` mapping (delivery is offered, not imposed
/// — the honesty rule) or registers nothing launchable.
fn run_memory(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    launchable: bool,
    declares_memory_dir: bool,
) -> SectionResult {
    if !launchable {
        return SectionResult::NotApplicable {
            reason: "no launchable agent to observe the delivered memory path".to_string(),
        };
    }
    // The declaration is the input: an adapter that maps no `memory.dir` key
    // declares NO Memory Backing delivery, so there is nothing to demonstrate
    // (the engine must not impose delivery on it — DC-10 / the honesty rule).
    if !declares_memory_dir {
        return SectionResult::NotApplicable {
            reason: "the declaration maps no reserved 'memory.dir' key, so Memory Backing \
                     delivery is not declared (delivery is offered, not imposed)"
                .to_string(),
        };
    }
    into_section_result(memory_inner(facade, state_dir, SECTION_TIMEOUT))
}

/// [`run_memory`]'s pipeline (the declaration gates already passed).
/// `poll_budget` bounds the dump-artifact poll (the section timeout by
/// default; the harness tests pass a tiny budget to prove the not-delivered
/// arm).
fn memory_inner(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
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
    let dump = state_dir.join("tck-memory-dump.txt");
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
    Ok(())
}

/// Interaction, honest per the declared level:
/// * Guaranteed/BestEffort — a probe's send_input reaches the running agent
///   (the echo line appears in its captured log).
/// * Unsupported — fails fast `CapabilityUnsupported` (the channel does not
///   exist; the message names the declaration).
fn run_interaction(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    level: SupportLevel,
) -> SectionResult {
    into_section_result(interaction_inner(facade, state_dir, level))
}

/// [`run_interaction`]'s pipeline.
fn interaction_inner(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    level: SupportLevel,
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
    interaction_delivery(facade, state_dir, probe_kind, SECTION_TIMEOUT)
}

/// The delivery half of the interaction proof: `send_input` must reach the
/// running agent (the echo line lands in its captured log) within `budget`.
/// Extracted so the never-echoes arm is provable directly (a probe launched
/// WITHOUT `--echo-stdin`).
fn interaction_delivery(
    facade: &::hemaka_engine::Blocking<'_>,
    state_dir: &Path,
    probe_kind: &str,
    budget: Duration,
) -> Result<(), String> {
    facade
        .send_input(probe_kind, "tck-echo-line")
        .map_err(|e| format!("send_input failed: {e}"))?;
    let log = agent_log_path(state_dir, probe_kind);
    wait_for_log_line_for(&log, "stdin: tck-echo-line", budget)?;
    let _ = facade.stop(probe_kind, Some(Duration::from_secs(5)));
    Ok(())
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
    use ::hemaka_engine::{EffectiveCapabilities, TransitionCause, TransitionEvent};

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
    /// with the detail string, and the kind label is derived without
    /// resolving anything.
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
                        reason.starts_with("registration failed: "),
                        "reason must name registration: {reason}"
                    );
                }
                other => panic!("expected Fail, got {other:?}"),
            }
        }
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
            contract_version: ::hemaka_adapter_api::CONTRACT_VERSION.to_string(),
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
    fn helper_engine() -> (tempfile::TempDir, ::hemaka_engine::Blocking<'static>) {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-helper-")
            .tempdir()
            .expect("helper tempdir");
        let engine = Engine::open(Some(dir.path().to_path_buf())).expect("helper engine");
        let leaked: &'static Engine = Box::leak(Box::new(engine));
        (dir, leaked.blocking())
    }

    /// `read_snapshot` accepts the two real snapshot shapes and reports a
    /// DISTINCT reason for each corrupt shape (missing file, bad JSON, no
    /// metering_source, unknown metering_source).
    #[test]
    fn read_snapshot_accepts_real_shapes_and_reports_each_corruption() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-snap-")
            .tempdir()
            .expect("tempdir");
        let home = dir.path().join("agents").join("snap");
        std::fs::create_dir_all(&home).expect("mkdir");

        // Missing file.
        let err = read_snapshot(dir.path(), "snap").unwrap_err();
        assert!(err.contains("read adapter snapshot"), "{err}");

        // Unparseable JSON.
        std::fs::write(home.join("adapter.json"), "not json at all").unwrap();
        let err = read_snapshot(dir.path(), "snap").unwrap_err();
        assert!(err.contains("parse adapter snapshot"), "{err}");

        // No metering_source field.
        std::fs::write(home.join("adapter.json"), "{}").unwrap();
        let err = read_snapshot(dir.path(), "snap").unwrap_err();
        assert!(err.contains("no metering_source"), "{err}");

        // Unknown metering_source value.
        std::fs::write(home.join("adapter.json"), r#"{"metering_source":"bogus"}"#).unwrap();
        let err = read_snapshot(dir.path(), "snap").unwrap_err();
        assert!(err.contains("unknown metering_source"), "{err}");

        // The two real shapes: self-reported with no launch, engine-observed
        // with a resolved launch.
        std::fs::write(
            home.join("adapter.json"),
            r#"{"metering_source":"self-reported","launch":null}"#,
        )
        .unwrap();
        let snap = read_snapshot(dir.path(), "snap").expect("self-reported snapshot");
        assert_eq!(snap.metering_source, MeteringSource::SelfReported);
        assert!(!snap.launch);

        std::fs::write(
            home.join("adapter.json"),
            r#"{"metering_source":"engine-observed","launch":{"args":["a"]}}"#,
        )
        .unwrap();
        let snap = read_snapshot(dir.path(), "snap").expect("engine-observed snapshot");
        assert_eq!(snap.metering_source, MeteringSource::EngineObserved);
        assert!(snap.launch);
    }

    /// `read_launch_dump_path` finds the token after `--dump` in the launch
    /// snapshot and reports None for every shape that lacks one.
    #[test]
    fn read_launch_dump_path_finds_the_pair_or_none() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-dump-")
            .tempdir()
            .expect("tempdir");
        let home = dir.path().join("agents").join("dump");
        std::fs::create_dir_all(&home).expect("mkdir");

        // No snapshot at all.
        assert!(read_launch_dump_path(dir.path(), "dump").is_none());

        let write = |body: &str| std::fs::write(home.join("adapter.json"), body).unwrap();
        // No launch / no args / `--dump` as the LAST token.
        write("{}");
        assert!(read_launch_dump_path(dir.path(), "dump").is_none());
        write(r#"{"launch":{}}"#);
        assert!(read_launch_dump_path(dir.path(), "dump").is_none());
        write(r#"{"launch":{"args":["--dump"]}}"#);
        assert!(read_launch_dump_path(dir.path(), "dump").is_none());
        // The hit: the token after `--dump`.
        write(r#"{"launch":{"args":["--linger-ms","5","--dump","d.txt"]}}"#);
        assert_eq!(
            read_launch_dump_path(dir.path(), "dump"),
            Some("d.txt".to_string())
        );
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
        let err = wait_for_state_for(
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
        let err = wait_for_state_for(
            &facade,
            "tck-wait",
            LifecycleState::Running,
            Duration::from_millis(250),
        )
        .unwrap_err();
        assert!(err.contains("timed out waiting for state"), "{err}");
    }

    /// The usage-row helpers tolerate a missing database and a database
    /// without the schema (both read 0, never panic), and the row-wait
    /// helper times out into its reason.
    #[test]
    fn usage_row_helpers_tolerate_missing_and_schemaless_databases() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-rows-")
            .tempdir()
            .expect("tempdir");

        // No state.db at all.
        assert_eq!(usage_row_count(dir.path(), "x"), 0);
        assert_eq!(observed_row_count(dir.path(), "x"), 0);

        // A database without the usage schema: the query errors → 0.
        let conn = rusqlite::Connection::open(dir.path().join("state.db")).expect("open");
        drop(conn);
        assert_eq!(usage_row_count(dir.path(), "x"), 0);
        assert_eq!(observed_row_count(dir.path(), "x"), 0);

        // The wait helper reports the timeout (with the count it saw).
        let err =
            wait_for_usage_rows_for(dir.path(), "x", 1, Duration::from_millis(200)).unwrap_err();
        assert!(
            err.contains("timed out waiting for 1 committed usage rows"),
            "{err}"
        );
    }

    /// The log poller reports a hit on a present line and a timeout naming
    /// the missing line; the heartbeat counter counts only heartbeat lines.
    #[test]
    fn log_helpers_report_hits_timeouts_and_heartbeat_counts() {
        let dir = tempfile::Builder::new()
            .prefix("ktesio-tck-log-")
            .tempdir()
            .expect("tempdir");
        let log = dir.path().join("agent.log");

        // Timeout on an absent artifact names the wanted line.
        let err = wait_for_log_line_for(&log, "stdin: hi", Duration::from_millis(150)).unwrap_err();
        assert!(
            err.contains("never observed") && err.contains("stdin: hi"),
            "{err}"
        );

        // Hit: the line lands (a full line, not a substring).
        std::fs::write(&log, "heartbeat 0\nstdin: hi\nheartbeat 1\n").unwrap();
        wait_for_log_line_for(&log, "stdin: hi", SECTION_TIMEOUT).expect("line present");
        assert!(wait_for_log_line_for(&log, "stdin: bye", Duration::from_millis(90)).is_err());
        // Substrings do not count as whole-line hits.
        assert!(wait_for_log_line_for(&log, "stdin", Duration::from_millis(90)).is_err());

        // Heartbeat counting: only lines starting with `heartbeat `.
        assert_eq!(heartbeat_lines(&log), 2);
        let missing = dir.path().join("nope.log");
        assert_eq!(heartbeat_lines(&missing), 0);
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
        match run_config_mapping(&facade, scratch.path(), None) {
            SectionResult::NotApplicable { reason } => {
                assert!(
                    reason.contains("native") && reason.contains("--dump"),
                    "{reason}"
                );
            }
            other => panic!("expected NotApplicable for a native config subject, got {other:?}"),
        }

        // Memory: no launchable agent / no declared `memory.dir` mapping.
        match run_memory(&facade, scratch.path(), false, true) {
            SectionResult::NotApplicable { reason } => {
                assert!(reason.contains("no launchable agent"), "{reason}");
            }
            other => panic!("expected NotApplicable for a launch-less memory probe, got {other:?}"),
        }
        match run_memory(&facade, scratch.path(), true, false) {
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
            match run_pause_section(&facade, scratch.path(), "tck-fast", launchable, level) {
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
        let (dir, facade) = helper_engine();
        assert_eq!(
            run_interaction(&facade, dir.path(), SupportLevel::Unsupported),
            SectionResult::Pass
        );
    }

    /// The freeze proof reports a probe whose heartbeat never starts (its
    /// heartbeat interval is far longer than the tiny budget) — a reason, not
    /// a harness hang.
    #[test]
    fn heartbeat_freeze_reports_a_probe_that_never_heartbeats() {
        let (dir, facade) = helper_engine();
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
        let reason = prove_heartbeat_freeze(
            &facade,
            dir.path(),
            "tck-slowbeat",
            Duration::from_millis(400),
        )
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
        let err = config_mapping_inner(&facade, dir.path(), rules, Duration::from_millis(400))
            .expect_err("the dump can never arrive");
        assert!(err.contains("never reached"), "{err}");
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
        let err = memory_inner(&facade, &blocked, Duration::from_millis(400))
            .expect_err("the dump can never arrive");
        assert!(
            err.contains("never received the managed memory path"),
            "{err}"
        );
        // The failure path cleaned up after itself.
        assert!(facade.memory_status("tck-memory-probe").unwrap().is_none());
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
        match run_config_mapping(&facade, dir.path(), Some(&undeclared)) {
            SectionResult::NotApplicable { reason } => {
                assert!(reason.contains("no [config] rules"), "{reason}");
            }
            other => panic!("expected NotApplicable for an undeclared config, got {other:?}"),
        }

        let no_env = write(
            "contract_version = \"1.0.0\"\n\n[adapter]\nkind = \"k\"\n\n[lifecycle.start]\nexec = \"x\"\nargs = []\n\n[metering]\nsource = \"self-reported\"\n\n[config.temperature]\nflag = \"--temp\"\n",
        );
        match run_config_mapping(&facade, dir.path(), Some(&no_env)) {
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
        let (dir, facade) = helper_engine();
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

        let err = interaction_delivery(
            &facade,
            dir.path(),
            "tck-interaction-probe",
            Duration::from_millis(400),
        )
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

        let err = wait_for_state_for(
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
        let report = catch_report(&adapter, || panic!("deliberate tck panic"));
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
            contract_version: ::hemaka_adapter_api::CONTRACT_VERSION.to_string(),
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
        assert!(json.contains("\"contract_version\":\"1.0.0\""), "{json}");
        let back: ConformanceReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, report);
        assert_eq!(back.schema_version, REPORT_SCHEMA_VERSION);
        assert_eq!(
            back.contract_version,
            ::hemaka_adapter_api::CONTRACT_VERSION
        );
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
