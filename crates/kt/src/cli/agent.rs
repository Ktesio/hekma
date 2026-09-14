//! `kt agent register | remove | list` — thin CLI over the engine's
//! synchronous registration API (spine AD-2, CLI-first gate).
//!
//! This module holds NO domain logic and constructs NO paths: the engine is
//! the sole path authority (it computes and returns the Agent Home path, which
//! we merely display). Every capability is reachable here (register, remove
//! with an explicit retain/delete disposition, the running-guard via `--force`,
//! and a list to observe results), satisfying the CLI-first gate.
//!
//! Errors: the engine returns `thiserror` [`RegistryError`]; we translate them
//! into `miette` diagnostics with remediation hints (miette lives in `kt`
//! only — conventions). Output discipline (AD-12): command results to stdout,
//! diagnostics/notices to stderr.

use std::time::{Duration, Instant};

use ktesio_engine::{
    render_dollars, render_dollars_bare, AdapterRef, BudgetView, Capability, ConfigError,
    ConfigLayer, EffectiveCapabilities, EffectiveConfig, Engine, EngineError, EstimateLabel,
    FleetEntry, FleetListing, FleetTotals, GuaranteeLevel, LifecycleState, LogLine,
    MemoryBackingKind, Micros, RegistryError, RemoveDisposition, SupportLevel, UsageView,
    FLEET_SCHEMA_VERSION,
};
use serde::Serialize;

use crate::error::{
    AgentCapabilityUnsupported, AgentConfig, AgentContractIncompatible, AgentDuplicateName,
    AgentInteractionTimedOut, AgentInteractionUnavailable, AgentInvalidName,
    AgentInvalidTransition, AgentIo, AgentLaunchFailed, AgentManifestInvalid,
    AgentManifestNotFound, AgentManifestUnreadable, AgentMemoryHotSwap, AgentMemoryKindConflict,
    AgentNoCapabilities, AgentNoMeteringSource, AgentNotFound, AgentNotRunning,
    AgentResumeUnsupported, AgentRunningRequiresForce, AgentStopUnconfirmed, AgentStore,
    AgentUnknownConfigKey, AgentUnknownKind,
};
use crate::ui;

/// Retain/delete choice as parsed from the CLI flags.
///
/// `[ASSUMPTION]` when neither `--delete` nor `--retain` is given we default to
/// **retain** — the safer choice, since it never destroys data silently. The
/// two flags are mutually exclusive at the clap layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispositionArg {
    /// Neither flag given → default to retain.
    Unspecified,
    /// `--retain`.
    Retain,
    /// `--delete`.
    Delete,
}

impl DispositionArg {
    /// Resolve clap booleans into a [`DispositionArg`].
    ///
    /// clap marks `--delete` and `--retain` mutually exclusive
    /// (`conflicts_with`), so both-true cannot happen through the CLI. As
    /// defense-in-depth we still fail **closed** to `Retain` if both are
    /// somehow set — retain is the safe default and must never lose to delete
    /// on an ambiguous input (it would silently destroy data).
    pub fn from_flags(delete: bool, retain: bool) -> Self {
        match (delete, retain) {
            // Both set (should be unreachable via clap): fail closed to Retain.
            (true, true) => DispositionArg::Retain,
            (true, false) => DispositionArg::Delete,
            (false, true) => DispositionArg::Retain,
            (false, false) => DispositionArg::Unspecified,
        }
    }

    /// Map to the engine's [`RemoveDisposition`], defaulting Unspecified to
    /// Retain (the safe default).
    fn resolve(self) -> RemoveDisposition {
        match self {
            DispositionArg::Delete => RemoveDisposition::Delete,
            DispositionArg::Retain | DispositionArg::Unspecified => RemoveDisposition::Retain,
        }
    }
}

/// How the operator selected the adapter on the command line.
///
/// `--kind` and `--manifest` are mutually exclusive at the clap layer, and at
/// least one is required; this enum resolves the parsed flags into the engine's
/// [`AdapterRef`]. `[ASSUMPTION]` on the mutually-exclusive-required shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterArg {
    /// `--kind <kind>` — a native builtin adapter by kind.
    Kind(String),
    /// `--manifest <path>` — a manifest adapter loaded from a dir or file.
    Manifest(String),
}

impl AdapterArg {
    /// Resolve clap's `Option`s into an [`AdapterArg`].
    ///
    /// clap enforces "exactly one of --kind/--manifest"; as defense-in-depth we
    /// prefer `--kind` if both somehow arrive and error if neither does.
    pub fn from_flags(
        kind: Option<String>,
        manifest: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        match (kind, manifest) {
            (Some(k), _) => Ok(AdapterArg::Kind(k)),
            (None, Some(m)) => Ok(AdapterArg::Manifest(m)),
            (None, None) => Err(AgentInvalidName {
                message: "one of --kind <kind> or --manifest <path> is required".to_string(),
            }
            .into()),
        }
    }

    /// Translate into the engine's [`AdapterRef`].
    fn to_ref(&self) -> AdapterRef {
        match self {
            AdapterArg::Kind(k) => AdapterRef::Native(k.clone()),
            AdapterArg::Manifest(p) => AdapterRef::Manifest(std::path::PathBuf::from(p)),
        }
    }
}

/// `kt agent register <name> (--kind <kind> | --manifest <path>)`.
///
/// Opens the engine (default state dir, or `KTESIO_STATE_DIR`), resolves +
/// validates the adapter, registers the instance, and prints the engine-computed
/// Agent Home path plus the effective (current-OS) Capability Declaration to
/// stdout. On an adapter/validation failure, nothing is written and a miette
/// diagnostic naming the problem goes to stderr.
pub fn register(name: &str, adapter: &AdapterArg) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let engine = engine.blocking();
    match engine.register_with_adapter(name, &adapter.to_ref()) {
        Ok(instance) => {
            ui::success(format!(
                "Registered Agent Instance {} ({})",
                ui::skill_name(instance.name.as_str()),
                instance.kind
            ));
            // Command result to stdout: the created Agent Home path.
            println!("{}", instance.agent_home);

            // Surface the effective per-OS Capability Declaration (AC1). Read it
            // back from the just-persisted snapshot so what we print is exactly
            // what `kt agent show` will render.
            match engine.effective_capabilities(instance.name.as_str()) {
                Ok(caps) => render_capabilities(instance.name.as_str(), &caps),
                // A render read-back failure must not fail a successful
                // registration; note it to stderr and move on.
                Err(err) => ui::warning(format!(
                    "Registered, but could not read back the Capability Declaration: {err}"
                )),
            }
            Ok(())
        }
        Err(err) => Err(map_error(err)),
    }
}

/// The `kt agent show <name> --json` document (story 1-7, AD-14).
///
/// A versioned wrapper carrying the SAME [`FLEET_SCHEMA_VERSION`] as the
/// `list --json` [`FleetListing`] (so `kt --json` speaks ONE schema, AD-14) plus
/// the single instance's [`FleetEntry`]. Presentation-only — the engine owns the
/// domain types; this wraps one entry with the shared schema version for the
/// `show` surface.
#[derive(Serialize)]
struct ShowDocument {
    /// The Fleet document schema version ([`FLEET_SCHEMA_VERSION`]).
    schema_version: u32,
    /// The single instance's Fleet entry (runtime fields + honest metering seed).
    instance: FleetEntry,
}

impl ShowDocument {
    /// Wrap one [`FleetEntry`], stamping the current [`FLEET_SCHEMA_VERSION`].
    fn new(instance: FleetEntry) -> Self {
        Self {
            schema_version: FLEET_SCHEMA_VERSION,
            instance,
        }
    }
}

/// The `kt agent usage <name> --json` document (story 4-3, FR-22/FR-26, AD-14).
///
/// A versioned wrapper mirroring [`ShowDocument`] exactly, carrying the named
/// instance's [`UsageView`] — the SAME snapshot type already embedded at
/// `FleetEntry.usage`, NOT a parallel usage type and NOT the event-stream
/// `UsageUpdateEvent` delta (a different type, routed to subscribers by story
/// 7-2). A SINGLE document, never NDJSON: usage is a snapshot, not a stream.
///
/// `schema_version` REUSES [`FLEET_SCHEMA_VERSION`] rather than minting a
/// `usage`-specific constant, because the governing rule across the CLI is that
/// `schema_version` tracks the serialized CONTENT-TYPE FAMILY, not the command:
/// the fleet-content commands (`list`/`show`/`usage`) all ride the fleet version,
/// `config get` has its own, and `logs` uses the engine's `LOG_SCHEMA_VERSION`.
#[derive(Serialize)]
struct UsageDocument {
    /// The Fleet document schema version ([`FLEET_SCHEMA_VERSION`]).
    schema_version: u32,
    /// The Agent Instance this usage snapshot belongs to.
    instance: String,
    /// The instance's Usage Ledger snapshot (tokens by scope + derived dollars).
    usage: UsageView,
}

impl UsageDocument {
    /// Wrap one instance's [`UsageView`], stamping [`FLEET_SCHEMA_VERSION`].
    fn new(instance: String, usage: UsageView) -> Self {
        Self {
            schema_version: FLEET_SCHEMA_VERSION,
            instance,
            usage,
        }
    }
}

/// The Fleet-wide `kt agent usage --json` document (no name — story 4-3, FR-22).
///
/// The no-name counterpart of [`UsageDocument`], carrying the engine-computed
/// [`FleetTotals`] aggregate — the SAME type already embedded at
/// `FleetListing.totals` (reused, never forked), so the Fleet-wide usage scope is
/// a first-class scriptable surface without the full `list` payload. Rides
/// [`FLEET_SCHEMA_VERSION`] for the same content-type-family reason.
#[derive(Serialize)]
struct FleetUsageDocument {
    /// The Fleet document schema version ([`FLEET_SCHEMA_VERSION`]).
    schema_version: u32,
    /// The Fleet-WIDE usage + cost aggregate over every instance.
    totals: FleetTotals,
}

impl FleetUsageDocument {
    /// Wrap the Fleet-wide [`FleetTotals`], stamping [`FLEET_SCHEMA_VERSION`].
    fn new(totals: FleetTotals) -> Self {
        Self {
            schema_version: FLEET_SCHEMA_VERSION,
            totals,
        }
    }
}

/// The memory-document schema version (story 6-6 — the deferred Epic-5 wire
/// surface, frozen at the Adapter Contract v1 freeze).
///
/// A NEW content-family constant (value `1`), minted per the CLI's documented
/// rule that `schema_version` tracks the serialized CONTENT-TYPE FAMILY, not
/// the command: the fleet-content commands (`list`/`show`/`usage`) ride
/// [`FLEET_SCHEMA_VERSION`], `config get` has [`CONFIG_GET_SCHEMA_VERSION`],
/// `logs` uses the engine's `LOG_SCHEMA_VERSION` — and the memory commands now
/// own this one. The `memory attach|detach --json` documents are a v1
/// compatibility surface (PRD §7): any key change is the ONE intentional,
/// announced edit the freeze policy allows, never silent drift.
const MEMORY_SCHEMA_VERSION: u32 = 1;

/// The `kt agent memory attach --json` document (story 6-6, AD-14).
///
/// Carries the attached backing's full typed read: the kind and guarantee
/// level in their snake_case wire forms (`filesystem`/`native`,
/// `managed_dir_byte_durable`/`home_persistence_only` — the stable strings
/// reserved for exactly this freeze and adopted VERBATIM), the engine-computed
/// managed directory (DC-1), and the DC-10 delivery fact (`declared`: whether
/// the adapter's declared config mapping targets the reserved `memory.dir`
/// key — always `false` for a `native` backing, which delivers nothing).
#[derive(Serialize)]
struct MemoryAttachDocument {
    /// The memory-document schema version ([`MEMORY_SCHEMA_VERSION`]).
    schema_version: u32,
    /// The Agent Instance the backing was attached to.
    instance: String,
    /// The backing kind (snake_case wire form, verbatim).
    kind: String,
    /// The NFR-7 guarantee level (snake_case wire form, verbatim).
    guarantee: String,
    /// The engine-computed managed directory (created for `filesystem`; the
    /// computed location only for `native`).
    dir: String,
    /// Whether the adapter's declared config mapping targets the reserved
    /// `memory.dir` key (the DC-10 delivery fact; always `false` for `native`).
    declared: bool,
}

/// The `kt agent memory detach --json` document (story 6-6, AD-14). The
/// detachment is metadata-only, so the document is intentionally minimal: the
/// versioned confirmation that NOTHING is attached anymore. It deliberately
/// carries NO path — `kt` never constructs the managed-directory name itself
/// (DC-1), and after a detach the engine reports no attachment to quote.
#[derive(Serialize)]
struct MemoryDetachDocument {
    /// The memory-document schema version ([`MEMORY_SCHEMA_VERSION`]).
    schema_version: u32,
    /// The Agent Instance the backing was detached from.
    instance: String,
}

/// Serialize the attach result into the pretty `memory attach --json` document
/// (a versioned [`MemoryAttachDocument`]). Pure (no engine, no I/O) so it is
/// unit-testable in-process; the CLI just prints the returned string.
fn memory_attach_json(
    instance: String,
    kind: MemoryBackingKind,
    guarantee: GuaranteeLevel,
    dir: String,
    declared: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let document = MemoryAttachDocument {
        schema_version: MEMORY_SCHEMA_VERSION,
        instance,
        kind: kind.as_str().to_string(),
        guarantee: guarantee.as_str().to_string(),
        dir,
        declared,
    };
    serde_json::to_string_pretty(&document).map_err(|e| serialize_error("memory attachment", e))
}

/// Serialize the detach result into the pretty `memory detach --json` document
/// (a versioned [`MemoryDetachDocument`]). Pure, as above.
fn memory_detach_json(instance: String) -> Result<String, Box<dyn std::error::Error>> {
    let document = MemoryDetachDocument {
        schema_version: MEMORY_SCHEMA_VERSION,
        instance,
    };
    serde_json::to_string_pretty(&document).map_err(|e| serialize_error("memory detachment", e))
}

/// Serialize the composed [`FleetListing`] into the pretty `list --json` document (a
/// versioned wrapper carrying the rows AND the Fleet-WIDE `totals`, story 3-5). Pure
/// (no engine, no I/O) so it is unit-testable in-process; the CLI just prints the
/// returned string to stdout. The listing is composed by the caller
/// ([`FleetListing::new`], which computes the aggregate from the rows), so this stays
/// a thin serialize. A serialize failure (not reachable for these plain serde structs)
/// becomes an [`AgentIo`] diagnostic rather than a panic.
fn fleet_json(listing: &FleetListing) -> Result<String, Box<dyn std::error::Error>> {
    serde_json::to_string_pretty(listing).map_err(|e| serialize_error("Fleet", e))
}

/// Serialize one entry into the pretty `show --json` document (a versioned
/// [`ShowDocument`]). Pure, for the same reason as [`fleet_json`].
fn show_json(entry: FleetEntry) -> Result<String, Box<dyn std::error::Error>> {
    let document = ShowDocument::new(entry);
    serde_json::to_string_pretty(&document).map_err(|e| serialize_error("instance", e))
}

/// Serialize one instance's [`UsageView`] into the pretty `usage <name> --json`
/// document (a versioned [`UsageDocument`]). Pure, for the same reason as
/// [`fleet_json`] — a SINGLE document (usage is a snapshot, not a stream).
fn usage_json(instance: String, usage: UsageView) -> Result<String, Box<dyn std::error::Error>> {
    let document = UsageDocument::new(instance, usage);
    serde_json::to_string_pretty(&document).map_err(|e| serialize_error("usage", e))
}

/// Serialize the Fleet-wide [`FleetTotals`] into the pretty `usage --json`
/// document (a versioned [`FleetUsageDocument`]). Pure, as above.
fn fleet_usage_json(totals: FleetTotals) -> Result<String, Box<dyn std::error::Error>> {
    let document = FleetUsageDocument::new(totals);
    serde_json::to_string_pretty(&document).map_err(|e| serialize_error("Fleet usage", e))
}

/// Reject a MALFORMED Agent Instance name before a command that resolves the
/// instance by scanning the Fleet ever looks it up (fix pass, M2).
///
/// Most `kt` commands (`logs`, `stop`, `pause`, the human `show`, …) pass the raw
/// name into an engine call that validates it internally — `InstanceName::new`
/// inside `Supervisor::read_agent_log` / `Registry::effective_capabilities` — so a
/// name like `"Bad Name"` surfaces as [`RegistryError::InvalidName`] → exit `2`
/// (usage). `show --json` and `usage <name>`, however, resolve the instance with a
/// linear `find` over `fleet()` and then SYNTHESIZE [`RegistryError::NotFound`],
/// which reported the same malformed input as exit `3`. Validating here — through
/// the engine's PUBLIC [`ktesio_engine::InstanceName`] newtype, the SAME rule the
/// engine applies internally, so `kt` re-derives nothing (AD-2) — makes the code
/// uniformly `2` for every command.
fn validate_instance_name(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    ktesio_engine::InstanceName::new(name)
        .map(|_| ())
        .map_err(|reason| {
            map_error(RegistryError::InvalidName {
                name: name.to_string(),
                reason,
            })
        })
}

/// Wrap a `serde_json` serialization failure into an [`AgentIo`] diagnostic. Not
/// reachable for the plain serde structs `--json` emits (serialization of a
/// derive-only struct cannot fail), so this is defense-in-depth, never a panic.
fn serialize_error(what: &str, err: serde_json::Error) -> Box<dyn std::error::Error> {
    AgentIo {
        message: format!("Failed to serialize the {what}: {err}"),
    }
    .into()
}

/// `kt agent show <name> [--json]` — render an instance's effective Capability
/// Declaration (AC1 "visible for the instance") plus its runtime status (story
/// 1-6, AC9): the current Lifecycle State, the active Restart Policy, the restart
/// count, the REAL story-3-1 Usage token totals + the REAL story-3-2 Token Budget
/// (ceilings + remaining + Breach Action, or `—`/`null` when un-budgeted), and —
/// for a `failed` instance — the failed cause.
///
/// `--json` mode (story 1-7) writes a single versioned document to STDOUT and
/// nothing else there: `{ schema_version, instance: <FleetEntry> }` — the SAME
/// [`FleetEntry`] shape `list --json` emits (RUNTIME fields only; the effective
/// Capability Declaration stays a human-`show` concern — decision recorded in the
/// Dev Agent Record). Output discipline (AD-12): result → stdout; the Epic-3
/// metering note + any read-back diagnostic → stderr.
pub fn show(name: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let facade = engine.blocking();

    if json {
        // Fix pass (M2): VALIDATE the name before the Fleet lookup, so a
        // MALFORMED name is a usage error (exit 2) rather than being reported as
        // "not found" (exit 3) merely because no Fleet row could ever match it.
        // Without this, `show --json` and the human `show` — which validates
        // inside `effective_capabilities` — disagreed on the same input.
        validate_instance_name(name)?;
        // AI-15: the SINGLE-INSTANCE registry lookup (`Engine::fleet_entry`) —
        // an O(1) keyed lookup whose errors surface directly, instead of reading
        // the whole Fleet and scanning it here (which also inherited a full
        // Fleet read's degradations just to find one row). The entry is still
        // composed by the same `fleet_entry_for` the list rows use, so the
        // `show` object remains byte-identical to that instance's `list` row; a
        // missing name surfaces the engine's own `RegistryError::NotFound` —
        // the SAME uniform not-found diagnostic (to stderr) the old path
        // synthesized.
        let entry = facade.fleet_entry(name).map_err(map_error)?;
        let json = show_json(entry)?;
        println!("{json}");
        // The metering note rides on stderr (AD-12), keeping stdout pure JSON.
        ui::note(METERING_NOTE);
        return Ok(());
    }

    let caps = facade.effective_capabilities(name).map_err(map_error)?;
    render_capabilities(name, &caps);
    // Runtime status (story 1-6, AC9): state + policy + restart count + failed
    // cause + the story-3-1 usage totals + Metering Source. A status read-back
    // failure must not fail `show` (the capabilities already printed); note it and
    // continue. The usage/metering rows come from the Fleet entry (the same read
    // `list` uses), so `show` and `list` agree exactly.
    match facade.instance_status(name) {
        Ok(status) => {
            let entry = facade
                .fleet()
                .ok()
                .and_then(|f| f.into_iter().find(|e| e.name.as_str() == name));
            render_runtime_status(&status, entry.as_ref());
            // One stderr note (AD-12): usage is real tokens; budget/dollars are later.
            ui::note(METERING_NOTE);
        }
        Err(err) => ui::warning(format!("Could not read runtime status for '{name}': {err}")),
    }
    Ok(())
}

/// Render the per-instance runtime status (story 1-6, AC9) as a small table.
///
/// Rows: State, Restart Policy, Restart count, the REAL story-3-2 Token Budget
/// (ceilings, remaining, Breach Action — or `—` when un-budgeted), the REAL
/// story-3-1 Usage token totals, the active Metering Source, and — for a `failed`
/// instance — the failed cause below (result to stdout, AD-12). The caller prints
/// the metering note to stderr. `entry` is the instance's Fleet entry (the same
/// read `list` uses), or `None` if that read degraded — in which case the
/// usage/metering/budget rows fall back to zero/unknown/absent so the table still
/// renders (mirroring the runtime-field degradation).
fn render_runtime_status(status: &ktesio_engine::InstanceStatus, entry: Option<&FleetEntry>) {
    let title = format!("Runtime status for {}", status.instance.name.as_str());
    let columns = [
        ui::TableColumn::new("Field", 14, 20),
        ui::TableColumn::new("Value", 14, 48),
    ];
    // Usage + Metering Source from the Fleet entry (story 3-1); a degraded read
    // falls back to zero usage / "unknown" source so `show` never fails on it.
    let usage_value = entry
        .map(|e| usage_cell_show(&e.usage))
        .unwrap_or_else(|| "in 0 / out 0".to_string());
    let metering_value = entry
        .map(|e| e.metering_source.clone())
        .unwrap_or_else(|| "unknown".to_string());
    // Budget from the Fleet entry (story 3-2 tokens + story 3-3 dollar cap): the
    // token ceiling(s) + remaining, the dollar Cost Cap + remaining (WHEN a Rate is
    // configured), and the Breach Action — or the honest `—` absence when nothing is
    // configured (or the read degraded). The `show` Value column is wide (no
    // truncation), so the estimate qualifier is labeled INLINE in the cell.
    let budget_value = budget_cell(entry.and_then(|e| e.budget.as_ref()), DollarLabel::Inline);
    // Cost from the Fleet entry (story 3-3): the DERIVED cumulative dollar cost,
    // rendered THROUGH the single currency module + labeled (AD-8), or the honest
    // inert note when no Rate is configured (AC-B: dollar features inert and SAY SO).
    let cost_value =
        cost_row_value(entry.and_then(|e| e.usage.cumulative_dollars.zip(e.usage.estimate_label)));
    let rows = vec![
        vec![
            ui::TableCell::plain("State"),
            ui::TableCell::status(status.instance.state.as_str()),
        ],
        vec![
            ui::TableCell::plain("Restart policy"),
            ui::TableCell::plain(status.restart_policy.as_str()),
        ],
        vec![
            ui::TableCell::plain("Restart count"),
            ui::TableCell::plain(status.restart_count.to_string()),
        ],
        // Budget is REAL for TOKENS (story 3-2) and DOLLARS (story 3-3, when a Rate
        // is configured): the ceiling(s) + remaining + Breach Action, or `—`.
        vec![
            ui::TableCell::plain("Budget"),
            ui::TableCell::plain(budget_value),
        ],
        // Usage is REAL now (story 3-1): the cumulative token totals (+ dollars when
        // a Rate is configured, via usage_cell through the currency module).
        vec![
            ui::TableCell::plain("Usage (tokens)"),
            ui::TableCell::plain(usage_value),
        ],
        // Cost is REAL for a Rate'd instance (story 3-3): the labeled derived dollar
        // cost; an honest inert note when no Rate is configured (AC-B).
        vec![
            ui::TableCell::plain("Cost (estimated)"),
            ui::TableCell::plain(cost_value),
        ],
        // The active Metering Source (AC-C).
        vec![
            ui::TableCell::plain("Metering source"),
            ui::TableCell::plain(metering_value),
        ],
    ];
    ui::print_table(&title, &columns, &rows);
    // For a failed instance, surface the last-known cause (the crash / crash-loop
    // detail) so the operator sees WHY it failed and the active policy (AC9).
    if status.instance.state == ktesio_engine::LifecycleState::Failed {
        if let Some(cause) = &status.failed_cause {
            ui::info(format!("Failed cause: {cause}"));
        }
    }
}

/// Render the effective (current-OS) Capability Declaration as a small table.
///
/// Command output → stdout (AD-12), reusing `ui.rs`. Each row is a capability
/// and its support level on the current OS.
fn render_capabilities(name: &str, caps: &EffectiveCapabilities) {
    let title = format!("Capabilities for {name} (OS: {})", caps.os);
    if caps.is_empty() {
        ui::info(format!("{title}: none declared"));
        return;
    }
    let columns = [
        ui::TableColumn::new("Capability", 12, 24),
        ui::TableColumn::new("Support (current OS)", 14, 16),
    ];
    let rows: Vec<Vec<ui::TableCell>> = caps
        .entries
        .iter()
        .map(|(capability, level)| {
            vec![
                ui::TableCell::skill(capability.as_str()),
                ui::TableCell::status(level.as_str()),
            ]
        })
        .collect();
    ui::print_table(&title, &columns, &rows);
}

/// `kt agent remove <name> [--delete|--retain] [--force]`.
pub fn remove(
    name: &str,
    disposition: DispositionArg,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // `--force` is only meaningful for a running instance. If the caller did
    // not choose a disposition we default to retain (safe — never destroys data
    // silently); see DispositionArg docs.
    let engine = open_engine()?;
    match engine.blocking().remove(name, disposition.resolve(), force) {
        Ok(()) => {
            let verb = match disposition.resolve() {
                RemoveDisposition::Delete => "removed (Agent Home deleted)",
                RemoveDisposition::Retain => "removed (Agent Home retained)",
            };
            ui::success(format!("Agent Instance {} {}", ui::skill_name(name), verb));
            Ok(())
        }
        Err(err) => Err(map_error(err)),
    }
}

/// The one-line stderr NOTE (AD-12: notices → stderr) about the honest metering
/// boundary now that stories 3-1/3-2/3-3 ship: `usage`/`budget` show REAL token
/// counts, and — WHEN a Rate is configured — the DERIVED dollar cost + Cost Cap +
/// remaining, every dollar figure LABELED an estimate (AD-8/FR-23). With no Rate,
/// dollar features are honestly INERT (no dollar figure — AC-B). Shared by `list`
/// and `show` so both surfaces state it identically.
const METERING_NOTE: &str =
    "usage + budget are real TOKEN counts from the Usage Ledger (budget '—' means \
     no budget configured); dollar figures appear only when a Rate is configured \
     (cost.rate.input/output) and are labeled estimates — with no Rate, dollar \
     features are inert.";

/// The guidance printed when the Fleet is EMPTY — shared by `list` and the
/// Fleet-wide `usage` (fix pass, L3) so both surfaces say the same thing rather
/// than `usage` silently reporting all-zero totals that look like real, consumed
/// nothing. `list` prints it to stdout in human mode (`ui::info`) and to stderr
/// under `--json`; `usage` routes it to stderr in BOTH modes (AD-12).
const EMPTY_FLEET_HINT: &str =
    "No Agent Instances registered yet. Register one with: kt agent register <name> --kind <kind>";

/// The `kt agent list` Budget column HEADER (story 3-3, FR-23/AD-8).
///
/// Honestly names BOTH dimensions the column now shows — a token budget AND an
/// ESTIMATED dollar Cost Cap — and carries the estimate qualifier ("est. $") in the
/// HEADER. The narrow Budget cell truncates with `…`; because the estimate label
/// lives here in the header (never in the truncatable cell), truncation can NEVER
/// strip the mandated estimate qualifier off a real dollar figure and leave a bare,
/// unlabeled dollar. Replaces the stale "Budget (tokens)" header, which mislabeled a
/// column that now also renders a dollar cap. (The `show` human view + `--json` are
/// fully labeled already; this is the `list`-surface fix.)
const BUDGET_LIST_HEADER: &str = "Budget (tok, est. $)";

/// The `kt agent list` Usage column HEADER (story 11-4, AI-45 — the Usage-side
/// twin of [`BUDGET_LIST_HEADER`]).
///
/// With a Rate configured the Usage column renders a derived dollar cost next to
/// the token totals — an ESTIMATE. The narrow Usage column truncates with `…`, so
/// exactly like the Budget column the estimate qualifier ("est. $") lives HERE in
/// the header (never in the truncatable cell): truncation can NEVER strip the
/// mandated estimate qualifier off a real dollar figure and leave a bare,
/// unlabeled dollar. The cell renders the dollar BARE ([`DollarLabel::InHeader`]);
/// the wide `show` cell keeps its inline label ([`usage_cell_show`]).
const USAGE_LIST_HEADER: &str = "Usage (tok, est. $)";

/// Render a [`UsageView`]'s CUMULATIVE totals as a compact human cell (story 3-1
/// tokens + story 3-3 dollars), e.g. `in 120 / out 340` or, with a Rate,
/// `in 120 / out 340 · $0.30 (estimated)`. The dollar figure is rendered THROUGH
/// the single currency module (AD-8) and appears ONLY when a Rate is configured
/// (`cumulative_dollars`/`estimate_label` present); with no Rate the cell is
/// tokens-only (honest inert dollar view — AC-B). Kept here so `list` and `show`
/// render the cumulative scope identically. The narrow `list` column shows
/// the CUMULATIVE scope only; the wide `show` Value column additionally surfaces the
/// current-Run scope via [`usage_cell_show`] (AC8 — tokens by BOTH scopes are legible
/// where the column can hold them; `--json` carries both on every surface).
///
/// `dollar_label` chooses WHERE the estimate qualifier lives — the same
/// parameterization [`budget_cell`] carries (story 11-4, AI-45: the `list` Usage
/// column is narrow and TRUNCATES, so it renders the dollar BARE with the
/// qualifier in the column header — [`USAGE_LIST_HEADER`] — FR-23), while the
/// wide `show` cell keeps the label INLINE. The CELL itself therefore never
/// carries a strippable label; the HEADER carries the qualifier and keeps it
/// for every width that can still render the header (the column's min_width
/// equals the header length). Examples: Inline → `in 120 / out 340 · $0.30
/// (estimated)`; InHeader → `in 120 / out 340 · $0.30`.
fn usage_cell(usage: &UsageView, dollar_label: DollarLabel) -> String {
    let tokens = format!(
        "in {} / out {}",
        usage.cumulative_input_tokens, usage.cumulative_output_tokens
    );
    match (usage.cumulative_dollars, usage.estimate_label) {
        (Some(dollars), label) => {
            // The dollar renders ONE way or the other — a Some-dollars-with-
            // no-label state (possible only in a hand-built entry: the engine
            // derives the label from the same Rate that prices the dollars)
            // falls back to the INLINE-labeled form rather than silently
            // dropping a real dollar figure (surfaced-not-silent).
            let dollar = match (label, dollar_label) {
                (Some(_label), DollarLabel::InHeader) => render_dollars_bare(dollars),
                // The default label is Estimated (v1's only produced kind), so
                // the no-label fallback renders the honest inline estimate.
                _ => render_dollars(dollars, label.unwrap_or_default()),
            };
            format!("{tokens} · {dollar}")
        }
        // No Rate ⇒ tokens only (dollar view honestly inert — AC-B).
        (None, _) => tokens,
    }
}

/// The `show` "Usage (tokens)" cell (story 3-5, AC8 — tokens BY SCOPE on the wide
/// detail surface). Renders the CUMULATIVE scope (via [`usage_cell`], including the
/// LABELED dollar cost when a Rate exists — the wide `show` column never truncates,
/// so the inline estimate label is safe here, [`DollarLabel::Inline`]) AND — when
/// the instance has a current Run with usage — the CURRENT-RUN token scope, so
/// BOTH scopes the AC names are legible in human `show` detail (the `--json`
/// `FleetEntry` already carries all four token fields). A non-running /
/// zero-current-run instance shows only the cumulative scope (the current-Run
/// scope is an honest absence, not a fabricated `run: in 0 / out 0`).
fn usage_cell_show(usage: &UsageView) -> String {
    let cumulative = usage_cell(usage, DollarLabel::Inline);
    let run_total = usage
        .current_run_input_tokens
        .saturating_add(usage.current_run_output_tokens);
    if run_total > 0 {
        format!(
            "cumulative {cumulative}; this run: in {} / out {}",
            usage.current_run_input_tokens, usage.current_run_output_tokens
        )
    } else {
        cumulative
    }
}

/// Where the estimate qualifier (`(estimated)`) for a rendered dollar figure lives
/// on a given surface (FR-23/AD-8 — every rendered dollar MUST stay labeled).
///
/// The `show` "Value" column is WIDE (no truncation), so it carries the label
/// INLINE in the cell (`… $0.20/$0.50 (estimated) …`). The `list` "Budget" column
/// is NARROW and TRUNCATES with `…`, which could CHOP an inline `(estimated)` off a
/// real dollar figure and leave a bare, unlabeled dollar (the FR-23 violation this
/// fixes). So on `list` the qualifier lives in the COLUMN HEADER ([`BUDGET_LIST_HEADER`])
/// and the cell renders the dollar value BARE — the label can never be truncated
/// because it is not in the cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DollarLabel {
    /// Append `(estimated)` inline in the cell (the wide `show` Value column).
    Inline,
    /// Omit the inline label; the qualifier lives in the column HEADER (the narrow,
    /// truncatable `list` Budget column).
    InHeader,
}

/// Render a [`BudgetView`] as a compact human `budget` cell (story 3-2 tokens +
/// story 3-3 dollars, AC9/AC10): the configured token ceiling(s) + remaining, the
/// dollar Cost Cap + remaining (WHEN a Rate is configured, rendered THROUGH the
/// single currency module — AD-8), and the Breach Action. E.g.
/// `cum 380/500 tok (pause)` or `cum 380/500 tok, cum $0.20/$0.50 (estimated) (pause)`.
/// An instance with NEITHER a budget nor an enforceable cap (`None`) renders the
/// honest absence token `—`. A cap with no Rate is inert (no dollar figure — AC-B).
///
/// `dollar_label` chooses WHERE the estimate qualifier lives: [`DollarLabel::Inline`]
/// for the wide `show` Value column (labeled in-cell), [`DollarLabel::InHeader`] for
/// the narrow, truncatable `list` Budget column (the qualifier is in the header —
/// [`BUDGET_LIST_HEADER`] — so truncation can never strip it, FR-23). Shared by both
/// surfaces so tokens + action render identically; only the dollar label placement
/// differs.
fn budget_cell(budget: Option<&BudgetView>, dollar_label: DollarLabel) -> String {
    let Some(b) = budget else {
        return FleetEntry::METERING_SEED_CELL.to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    if let (Some(limit), Some(remaining)) = (b.per_run_limit, b.per_run_remaining) {
        parts.push(format!("run {remaining}/{limit} tok"));
    }
    if let (Some(limit), Some(remaining)) = (b.cumulative_limit, b.cumulative_remaining) {
        parts.push(format!("cum {remaining}/{limit} tok"));
    }
    // DOLLAR cap/remaining THROUGH the single currency module (AD-8) — present only
    // when a Rate is configured (the label carries the dimension's honesty).
    let label = b.estimate_label.unwrap_or_default();
    if let (Some(cap), Some(remaining)) = (b.per_run_cost_cap, b.per_run_dollars_remaining) {
        parts.push(format!(
            "run {}",
            dollar_cap_cell(remaining, cap, label, dollar_label)
        ));
    }
    if let (Some(cap), Some(remaining)) = (b.cumulative_cost_cap, b.cumulative_dollars_remaining) {
        parts.push(format!(
            "cum {}",
            dollar_cap_cell(remaining, cap, label, dollar_label)
        ));
    }
    // A budget with an action but somehow no scope (defensive) still shows the
    // action honestly rather than an empty cell.
    if parts.is_empty() {
        return format!("({})", b.breach_action.as_str());
    }
    format!("{} ({})", parts.join(", "), b.breach_action.as_str())
}

/// Render a dollar `remaining/cap` pair THROUGH the single currency module (AD-8)
/// — e.g. `$0.20/$0.50 (estimated)`. The SOLE currency formatting in `kt` routes
/// through the currency module: the `remaining` value ALWAYS uses the module's
/// bare-value form ([`render_dollars_bare`]); the `cap` uses [`render_dollars`]
/// (inline label) or [`render_dollars_bare`] (label in the header) per
/// `dollar_label`. Either way the `$X.XX` digits ORIGINATE in that one module —
/// there is NO string-surgery on a labeled string (primary L1). On the
/// [`DollarLabel::InHeader`] surface the qualifier is carried by the column header
/// ([`BUDGET_LIST_HEADER`]) instead of the cell, so a truncated cell can never drop
/// the label.
fn dollar_cap_cell(
    remaining: Micros,
    cap: Micros,
    label: EstimateLabel,
    dollar_label: DollarLabel,
) -> String {
    let cap_str = match dollar_label {
        DollarLabel::Inline => render_dollars(cap, label),
        DollarLabel::InHeader => render_dollars_bare(cap),
    };
    format!("{}/{}", render_dollars_bare(remaining), cap_str)
}

/// The `show` "Cost (estimated)" row value (story 3-3, AC-B): the DERIVED
/// cumulative dollar cost rendered THROUGH the single currency module + labeled
/// (AD-8) when a Rate is configured, or the honest INERT note when no Rate exists
/// (dollar features inert and SAY SO — never a fabricated `$0.00`). `dollars` is
/// `Some((cost, label))` iff a Rate is configured.
fn cost_row_value(dollars: Option<(Micros, EstimateLabel)>) -> String {
    match dollars {
        Some((cost, label)) => render_dollars(cost, label),
        None => format!(
            "{} (no Rate configured — dollar features inert)",
            FleetEntry::METERING_SEED_CELL
        ),
    }
}

/// Render the Fleet-WIDE total footer for the human `kt agent list` (story 3-5,
/// AC-A/AC-B/AC5/AC7 — FR-22/FR-23). Summarizes the [`FleetTotals`] the engine
/// composed over the rows: total input/output tokens (always present — zero-not-
/// absent), and the total derived dollars THROUGH the single currency module
/// (AD-8), carrying the honesty of the aggregate:
///
/// * A COMPLETE dollar total (every metered instance priced): `≈ $X.XX (estimated)`.
/// * A PARTIAL total (a metered instance has no Rate): `≈ $X.XX (estimated; N
///   instances unpriced)` — the honest lower-bound note (AC5, SM-C3), NAMING how many
///   metered-but-unpriced rows the dollar sum omits so the reader knows the total's
///   basis (AC7). `N` is [`FleetTotals::unpriced_count`], computed by the engine
///   `domain` aggregate (`kt` only renders it, with the singular "1 instance unpriced").
/// * NO instance has a Rate: the token total + an honest `—` dollar marker (AC4/AC5),
///   NEVER a fabricated `$0.00`.
///
/// The `≈` marks the figure a labeled estimate; the dollar DIGITS come ONLY from the
/// currency module ([`render_dollars_bare`]) and the estimate label is composed by
/// this caller — the narrow-column pattern 3-3 established (the label survives, FR-23,
/// and the `$` originates in the one module so the AD-8 grep-lint stays green; the CLI
/// never formats a `$` string itself). Pure (no engine, no I/O) so it is unit-testable
/// in-process; `list` prints the returned line to stdout as command output (AD-12).
fn fleet_total_footer(totals: &FleetTotals) -> String {
    let tokens = format!(
        "in {} / out {}",
        totals.total_input_tokens, totals.total_output_tokens
    );
    // The count of metered-but-unpriced rows that make the dollar total a lower bound
    // is carried on `totals.unpriced_count` (the engine `domain` computed it alongside
    // the sum — AD-2); the footer only NAMES it (AC5/AC7) on the partial path below.
    let dollars = match (totals.total_dollars, totals.estimate_label) {
        // A priced total: the bare dollar value through the ONE currency module, then
        // the estimate label (+ the honest lower-bound note when partial). `≈` marks it
        // an aggregate estimate. The label is ALWAYS present (FR-23 — no unlabeled
        // dollar); on a partial total it is folded together with the unpriced count.
        (Some(cost), Some(label)) => {
            let bare = render_dollars_bare(cost);
            if totals.dollars_partial {
                // A lower bound — say so, and NAME how many rows are unpriced (AC5/AC7):
                // "1 instance unpriced" / "N instances unpriced" (correct singular).
                let n = totals.unpriced_count;
                let unit = if n == 1 { "instance" } else { "instances" };
                format!("≈ {bare} ({label}; {n} {unit} unpriced)")
            } else {
                format!("≈ {bare} ({label})")
            }
        }
        // NO Rate anywhere ⇒ the dollar total is honestly absent (never $0.00).
        _ => format!(
            "{} (no Rate configured — dollars not derived)",
            FleetEntry::METERING_SEED_CELL
        ),
    };
    format!("Fleet total: {tokens} · {dollars}")
}

/// `kt agent list [--json]` — render the Fleet (FR-4).
///
/// Human mode prints a table: Name, Kind, State, Restarts (story 1-6), the REAL
/// story-3-2 Token Budget column (ceilings + remaining + Breach Action, or `—`
/// when un-budgeted) + the REAL story-3-1 Usage token totals, and the Agent Home;
/// one stderr note explains the
/// metering boundary (AD-12: result → stdout, note → stderr). `--json` mode writes a single versioned
/// [`FleetListing`] document to STDOUT and nothing else there (AD-14: `kt --json`
/// serializes the same struct the Host event stream will publish). Freshness
/// (≤2s, AC6) is structural: each invocation opens the engine and reads live
/// persisted state via [`ktesio_engine::Engine::fleet`] — there is no cache, so
/// any committed transition is reflected on the next listing (a single DB read).
pub fn list(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let facade = engine.blocking();
    let entries = facade.fleet().map_err(map_error)?;
    // Compose the versioned document ONCE (story 1-7 + 3-5): it carries the rows AND
    // the Fleet-WIDE `totals`, computed PURELY from those rows by the engine `domain`
    // (`FleetListing::new` → `FleetTotals::from_entries`). Both the `--json` document
    // and the human footer read the SAME computed aggregate — one read pass, no second
    // ledger query, `kt` never sums the ledger itself (AD-2).
    let listing = FleetListing::new(entries);

    if json {
        // AC5/AC9: the whole result is ONE JSON document to stdout (an empty Fleet
        // is a valid empty `instances` array + an all-zero/absent-dollars `totals`).
        // Any guidance/notes go to stderr so stdout is always parseable JSON.
        let empty = listing.instances.is_empty();
        let document = fleet_json(&listing)?;
        println!("{document}");
        if empty {
            ui::note(EMPTY_FLEET_HINT);
        }
        // The metering note still rides on stderr (AD-12), keeping stdout pure JSON.
        ui::note(METERING_NOTE);
        return Ok(());
    }

    if listing.instances.is_empty() {
        ui::info(EMPTY_FLEET_HINT);
        return Ok(());
    }
    let entries = &listing.instances;

    // METERING-SOURCE SPLIT (AI-43, story 11-4 — stated plainly, WHERE the value
    // appears): the active Metering Source is surfaced on `kt agent show` (the
    // detail row), `kt agent list --json` (the `metering_source` field), and
    // `kt agent usage` (named + JSON forms). It is NOT a human-`list` column: the
    // compact table is the ratified 80-col design, and a Metering column there
    // overflows the default width and truncates cells. AC-C's "visible in Fleet
    // listing detail" is the DETAIL surface — `show` — which is exactly where the
    // value lives; human `list` surfaces the real Usage token totals instead.
    //
    // Both money-bearing headers carry their "est. $" qualifier in the HEADER, not
    // the cell: the Budget column via [`BUDGET_LIST_HEADER`], the Usage column via
    // [`USAGE_LIST_HEADER`] (AI-45 — the same FR-23 treatment). The narrow cells
    // truncate with `…`, so putting the qualifier in the header means truncation
    // can NEVER strip the estimate label off a real dollar figure (FR-23/AD-8 —
    // every rendered dollar stays labeled). The cells therefore render the dollar
    // value BARE (DollarLabel::InHeader).
    let columns = [
        ui::TableColumn::new("Name", 12, 32),
        ui::TableColumn::new("Kind", 8, 24),
        ui::TableColumn::new("State", 10, 12),
        ui::TableColumn::new("Restarts", 8, 10),
        ui::TableColumn::new(BUDGET_LIST_HEADER, 15, 34),
        // min_width = the header's own length: the qualifier survives any
        // shrink that still renders the header at all (review loop 1).
        ui::TableColumn::new(USAGE_LIST_HEADER, 19, 26),
        ui::TableColumn::new("Agent Home", 20, 64),
    ];
    let rows: Vec<Vec<ui::TableCell>> = entries
        .iter()
        .map(|entry| {
            // Budget is REAL for TOKENS (story 3-2) and DOLLARS (story 3-3): ceiling(s)
            // + remaining + action, or `—` when un-budgeted. The estimate qualifier for
            // any dollar figure lives in the column HEADER (DollarLabel::InHeader), so a
            // truncated cell never drops the label. A budgeted cell is `plain`, an
            // absent one stays `muted` (the honest `—`).
            let budget = entry.budget.as_ref();
            let budget_text = budget_cell(budget, DollarLabel::InHeader);
            let budget_cell = if budget.is_some() {
                ui::TableCell::plain(budget_text)
            } else {
                ui::TableCell::muted(budget_text)
            };
            vec![
                ui::TableCell::skill(entry.name.as_str()),
                ui::TableCell::plain(entry.kind.clone()),
                ui::TableCell::status(entry.state.as_str()),
                ui::TableCell::plain(entry.restart_count.to_string()),
                budget_cell,
                // Usage is REAL now (story 3-1): the cumulative token totals —
                // plus, with a Rate, the derived dollar rendered BARE (AI-45): the
                // estimate qualifier lives in the Usage column HEADER
                // ([`USAGE_LIST_HEADER`]), whose min_width keeps it for every
                // width that can still render the header at all.
                ui::TableCell::plain(usage_cell(&entry.usage, DollarLabel::InHeader)),
                ui::TableCell::muted(entry.agent_home.clone()),
            ]
        })
        .collect();
    ui::print_table("Fleet", &columns, &rows);
    // Story 3-5 (AC-A/AC-B): the Fleet-WIDE total footer — total tokens + the labeled
    // total dollars THROUGH the single currency module (AD-8), an honest lower-bound
    // note when partial, and a `—` (never $0.00) when no instance is Rate'd. It is
    // command output (a summary of the table above it), so it rides STDOUT (AD-12).
    println!("{}", fleet_total_footer(&listing.totals));
    // One stderr note (AD-12): usage is real tokens; dollars appear with a Rate.
    ui::note(METERING_NOTE);
    Ok(())
}

/// `kt agent usage [<name>] [--json]` — a focused, scriptable read of the Usage
/// Ledger (story 4-3, FR-22/FR-26).
///
/// The two scopes FR-22 names, mirroring `show` (named) / `list` (Fleet-wide):
/// with `<name>` → that instance's [`UsageView`] snapshot; with NO name → the
/// Fleet-wide [`FleetTotals`] aggregate. Both are composed from the SAME facade
/// read (`fleet()` → [`FleetListing::new`]) that `list`/`show` use, so the token
/// totals equal the Usage Ledger — and each other — EXACTLY (the FR-22 honesty
/// discipline). This command re-reads nothing and sums nothing itself: the engine
/// `domain` owns the aggregate (AD-2).
///
/// `--json` writes ONE versioned document to stdout and nothing else there (a
/// [`UsageDocument`] / [`FleetUsageDocument`], both riding [`FLEET_SCHEMA_VERSION`])
/// — NOT NDJSON; that shape is `logs`-only, for its unbounded stream. The human
/// form renders a focused table whose dollar figures route through the SINGLE
/// currency module and stay labeled (AD-8/FR-23); on the wire, dollars are integer
/// micros + the label, NEVER a `$` string (AD-14). Output discipline (AD-12): the
/// result → stdout, the metering note → stderr. No secret reaches either surface
/// (AD-10 — this reads ledger counters only).
pub fn usage(name: Option<&str>, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let facade = engine.blocking();
    let entries = facade.fleet().map_err(map_error)?;

    match name {
        // Named: the instance's own UsageView snapshot — the SAME object
        // `list`/`show --json` already carry at `FleetEntry.usage`. An unknown
        // name is the uniform not-found diagnostic (stderr, exit 3), exactly as
        // `show --json` resolves a missing instance.
        Some(name) => {
            // Fix pass (M2): VALIDATE first — see `validate_instance_name`. A
            // malformed name is exit 2 here, uniform with `logs`/`stop`/`show`,
            // instead of the exit 3 the bare `find` below would otherwise yield.
            validate_instance_name(name)?;
            let entry = entries
                .into_iter()
                .find(|e| e.name.as_str() == name)
                .ok_or_else(|| {
                    map_error(RegistryError::NotFound {
                        name: name.to_string(),
                    })
                })?;
            if json {
                let document = usage_json(entry.name.as_str().to_string(), entry.usage)?;
                println!("{document}");
            } else {
                render_usage_instance(&entry);
            }
        }
        // Fleet-wide: the engine-computed aggregate over the SAME rows `list`
        // sums, via `FleetListing::new` — so this total can never drift from the
        // one `list` shows (it is literally the same computation).
        None => {
            let listing = FleetListing::new(entries);
            // Fix pass (L3): an EMPTY Fleet gets the same honest guidance `list`
            // prints, so a Fleet-wide `usage` of all zeros is never mistaken for
            // "registered instances that consumed nothing". It rides STDERR in
            // BOTH modes (AD-12), so `--json` stdout stays a pure document.
            if listing.instances.is_empty() {
                ui::note(EMPTY_FLEET_HINT);
            }
            let totals = listing.totals;
            if json {
                let document = fleet_usage_json(totals)?;
                println!("{document}");
            } else {
                // Reuse the SAME honest footer `list` prints (labeled dollars via
                // the one currency module, an explicit lower-bound note when the
                // total is partial, `—` rather than a fabricated $0.00).
                println!("{}", fleet_total_footer(&totals));
            }
        }
    }
    // The metering note rides on stderr (AD-12) in BOTH modes, so stdout stays a
    // pure, parseable document under `--json`.
    ui::note(METERING_NOTE);
    Ok(())
}

/// Render one instance's [`UsageView`] as the focused human `usage` table (story
/// 4-3): tokens by BOTH scopes (cumulative + current-run), the DERIVED dollar cost
/// per scope, and the active Metering Source.
///
/// Every dollar figure routes through the SINGLE currency module via the shared
/// [`cost_row_value`] (AD-8) and therefore always carries its
/// `estimated`/`reconciled` label (FR-23); with no Rate configured it renders the
/// honest inert note instead of a fabricated `$0.00` (AC-B). The `Value` column is
/// wide (no truncation), so — as in `show` — the label rides INLINE in the cell.
/// Result → stdout (AD-12).
fn render_usage_instance(entry: &FleetEntry) {
    let usage = &entry.usage;
    let title = format!("Usage for {}", entry.name.as_str());
    let columns = [
        ui::TableColumn::new("Field", 14, 22),
        ui::TableColumn::new("Value", 14, 48),
    ];
    let rows = vec![
        vec![
            ui::TableCell::plain("Cumulative tokens"),
            ui::TableCell::plain(format!(
                "in {} / out {}",
                usage.cumulative_input_tokens, usage.cumulative_output_tokens
            )),
        ],
        vec![
            ui::TableCell::plain("Cumulative cost"),
            ui::TableCell::plain(cost_row_value(
                usage.cumulative_dollars.zip(usage.estimate_label),
            )),
        ],
        vec![
            ui::TableCell::plain("Current-run tokens"),
            ui::TableCell::plain(format!(
                "in {} / out {}",
                usage.current_run_input_tokens, usage.current_run_output_tokens
            )),
        ],
        vec![
            ui::TableCell::plain("Current-run cost"),
            ui::TableCell::plain(cost_row_value(
                usage.current_run_dollars.zip(usage.estimate_label),
            )),
        ],
        vec![
            ui::TableCell::plain("Metering source"),
            ui::TableCell::plain(entry.metering_source.clone()),
        ],
    ];
    ui::print_table(&title, &columns, &rows);
}

/// `kt agent start <name>` — start a registered Agent Instance (AC1/AC2).
///
/// Opens the engine, drives `start` through the blocking facade, and prints the
/// new Lifecycle State (`running`) to stdout on success. On a launch failure the
/// instance lands in `failed`, and a miette diagnostic preserving the adapter's
/// diagnostic goes to stderr (AC2). Output discipline (AD-12): result → stdout,
/// diagnostics/notices → stderr.
///
/// SINGLE-LIFETIME SUPERVISION BOUNDARY (honest notice, AD-5): the engine
/// supervises the started process only for the lifetime of THIS engine session.
/// Because the backend kills the process group / job on handle drop, a
/// standalone `kt agent start <name>` stops the agent when this CLI process
/// exits cleanly — the persisted `running` row then outlives the live process.
/// Story 1-6 delivers CRASH recovery: if the engine CRASHES (no clean drop), a
/// surviving process is re-adopted on the next `Engine::open` (by pid +
/// start-time fingerprint) and crashes are detected + handled by the Restart
/// Policy. It does NOT make a cleanly-exited standalone `kt agent start` leave a
/// durably-supervised process across separate CLI invocations — that remains
/// future work. To keep the operator honest at the point of pain, the success
/// path prints a one-line notice to STDERR (never stdout — existing tests assert
/// the stdout result line).
pub fn start(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    match engine.blocking().start(name) {
        Ok(instance) => {
            ui::success(format!(
                "Started Agent Instance {}",
                ui::skill_name(instance.name.as_str())
            ));
            // Command result to stdout: the new Lifecycle State.
            println!("{}", instance.state);
            // Honest single-lifetime notice to STDERR (AD-12: notices → stderr,
            // never stdout). A clean CLI exit stops the agent; durable supervision
            // across separate CLI invocations is future work.
            ui::note(
                "the started process is supervised only for this engine session \
                 and stops when this command exits; durable supervision across \
                 separate CLI invocations is future work.",
            );
            Ok(())
        }
        Err(err) => Err(map_engine_error(err)),
    }
}

/// `kt agent stop <name> [--timeout <secs>]` — stop a running Agent Instance
/// (AC3/AC4).
///
/// Opens the engine and drives `stop` through the blocking facade with the
/// graceful window (default 30s, or `--timeout <secs>`). Prints the final state
/// (`stopped`) to stdout, or the uniform invalid-transition diagnostic (e.g.
/// stop on `stopped`, AC4) to stderr.
pub fn stop(name: &str, timeout_secs: Option<u64>) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let window = timeout_secs.map(Duration::from_secs);
    match engine.blocking().stop(name, window) {
        Ok(instance) => {
            ui::success(format!(
                "Stopped Agent Instance {}",
                ui::skill_name(instance.name.as_str())
            ));
            // Command result to stdout: the final Lifecycle State.
            println!("{}", instance.state);
            Ok(())
        }
        Err(err) => Err(map_engine_error(err)),
    }
}

/// `kt agent pause <name>` — pause a running Agent Instance with honest, per-OS
/// semantics (AC2/AC3/AC6 — "surfaced not silent").
///
/// Drives `pause` through the blocking facade. On success prints the new state
/// (`paused`) to stdout; then — per the AC2 honesty contract — re-reads the
/// effective Capability Declaration and, if pause is `BestEffort` on this OS,
/// emits a VISIBLE qualifier NOTE to STDERR (never a silent success; the
/// machine-readable half rides in the transition event's `pause-best-effort`
/// cause). On `unsupported` the engine fails fast and we render
/// [`AgentCapabilityUnsupported`] quoting the declaration to stderr with a
/// non-zero exit (AC3). Output discipline (AD-12): result → stdout, the
/// qualifier note/diagnostics → stderr.
pub fn pause(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let facade = engine.blocking();
    match facade.pause(name) {
        Ok(instance) => {
            ui::success(format!(
                "Paused Agent Instance {}",
                ui::skill_name(instance.name.as_str())
            ));
            // Command result to stdout: the new Lifecycle State.
            println!("{}", instance.state);
            // AC2 honesty: if pause is best-effort on this OS, surface the
            // qualifier to STDERR (the human half of "surfaced not silent").
            note_if_best_effort(&facade, name, "pause");
            Ok(())
        }
        Err(err) => Err(map_engine_error(err)),
    }
}

/// `kt agent resume <name>` — resume a paused Agent Instance (AC2/AC6).
///
/// The symmetric counterpart of [`pause`]: prints the new state (`running`) to
/// stdout, and if pause is best-effort on this OS emits the resume qualifier note
/// to stderr.
pub fn resume(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let facade = engine.blocking();
    match facade.resume(name) {
        Ok(instance) => {
            ui::success(format!(
                "Resumed Agent Instance {}",
                ui::skill_name(instance.name.as_str())
            ));
            // Command result to stdout: the new Lifecycle State.
            println!("{}", instance.state);
            note_if_best_effort(&facade, name, "resume");
            Ok(())
        }
        Err(err) => Err(map_engine_error(err)),
    }
}

/// `kt agent send <name> <text>` — send text input to a running Agent
/// Instance's native input channel (story 4.1, FR-24).
///
/// Drives `send_input` through the blocking facade. Unlike `pause`/`resume`,
/// `send` is not a lifecycle transition, so there is no new Lifecycle State to
/// print — on success only a confirmation goes to stdout. Failure modes,
/// mapped by [`map_engine_error`]: the instance is not `running`
/// ([`AgentNotRunning`]); interaction is `unsupported` on this OS, quoting the
/// Capability Declaration ([`AgentCapabilityUnsupported`]); or the instance is
/// running but this engine session holds no live stdin pipe for it, e.g. an
/// ADOPTED instance ([`AgentInteractionUnavailable`]).
pub fn send(name: &str, text: &str) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let facade = engine.blocking();
    match facade.send_input(name, text) {
        Ok(()) => {
            ui::success(format!(
                "Sent input to Agent Instance {}",
                ui::skill_name(name)
            ));
            Ok(())
        }
        Err(err) => Err(map_engine_error(err)),
    }
}

/// How often `kt agent logs --follow` polls for new output (story 4-2,
/// Assumption 7) — a reasonable default left unspecified by any source
/// document, mirroring `STOP_POLL_INTERVAL`'s existing precedent
/// (`backends/unix/mod.rs`) of a hardcoded short interval rather than a
/// configurable one.
const LOGS_FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// How long AC-C's final drain (below) may retry after a non-`running`
/// transition is observed, before giving up and printing the exit note
/// (fix pass, M2, review of #80). `Supervisor::transition_with_log_capture`
/// writes `output.log`'s engine line SYNCHRONOUSLY now (fix pass, H1 — no
/// background writer thread/channel is involved anymore), but it does so
/// AFTER first persisting the new state to the DB (`registry.set_state`) —
/// two separate steps within the SAME call, not one atomic unit. `kt agent
/// logs --follow` runs in a SEPARATE process from whatever transitions the
/// instance (e.g. a concurrent `kt agent stop`), so it can observe the NEW
/// state via `instance_status` (step one) a moment before that OTHER
/// process has finished the engine-line write (step two) — a real,
/// cross-process race, distinct from (and independent of) the reader/tailer
/// mechanics. Retrying briefly here closes it. Mirrors
/// `crates/ktesio-engine/tests/logs.rs`'s
/// `follow_drains_and_exits_cleanly_on_stop`'s own accommodation for this
/// exact race (there, a 3s bound); kept short here since the remaining gap
/// between the two steps is normally sub-millisecond.
const FOLLOW_FINAL_DRAIN_BOUND: Duration = Duration::from_secs(2);

/// How often the final-drain retry (above) re-polls while waiting.
const FOLLOW_FINAL_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// `kt agent logs <name> [--follow]` — read retained logs, optionally
/// following live output (story 4-2, FR-25, spine AD-12).
///
/// One-shot: dumps every currently-retained [`LogLine`] to stdout (this IS
/// the command's result — AD-12's "stdout of kt is command output"
/// convention) and returns. `--follow` (AC-B/AC-C): after that initial dump
/// (mirroring `docker logs -f`'s "show existing, then keep streaming"
/// convention), loops on [`LOGS_FOLLOW_POLL_INTERVAL`] calling
/// `read_agent_log_since`, printing new lines as they arrive, and
/// periodically checking `instance_status`. On a detected rotation (the
/// engine's returned cursor snaps BELOW the one just passed in), prints one
/// honest notice rather than silently claiming completeness. On a non-
/// `running` state, performs a bounded final drain (fix pass, M2 — see
/// [`FOLLOW_FINAL_DRAIN_BOUND`]; so no line emitted before the transition is
/// lost) and exits cleanly with a state-appropriate note — a `paused`
/// instance is not "gone", so it gets an honest "paused" note, not an
/// error. No special Ctrl-C handling is needed: this is a read-only command
/// holding no supervising handle, so a SIGINT exit is always safe.
///
/// `--json` (story 4-3, AD-14/DC-2) emits **newline-delimited JSON**: one
/// serialized engine [`LogLine`] per stdout line (compact, not pretty), in the
/// SAME append order as the human form (never timestamp-sorted — story 4-2
/// AC-G). NDJSON — not a single wrapping document — because `--follow` is an
/// unbounded stream a wrapper could never close; each [`LogLine`] already carries
/// its own `schema_version` ([`ktesio_engine::LOG_SCHEMA_VERSION`]), so per-line
/// versioning is intrinsic. The shape is identical for one-shot and `--follow`.
/// Output discipline holds in BOTH modes (DC-3): stdout stays pure NDJSON while
/// the rotation notice and the follow-exit note ride on stderr (AD-12).
///
/// A downstream consumer that STOPS READING (`kt agent logs --json | head -5`)
/// ends the command CLEANLY with exit `0` in every mode — one-shot, `--follow`,
/// human, or JSON (fix pass, M1: see [`emit_log_lines`]). It is not an error; the
/// consumer got what it asked for, and a `--follow` loop stops rather than
/// spinning against a dead pipe.
pub fn logs(name: &str, follow: bool, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let facade = engine.blocking();

    // Fix pass (M1, review of #80): `read_agent_log` returns the byte-cursor
    // it reached ALONGSIDE its one-shot dump — this primes the follow loop's
    // cursor directly from the SAME read, with no separate
    // `read_agent_log_since(name, 0)` priming call whose own lines used to
    // be silently discarded (a T1→T2 window between the two reads during
    // which any emitted output was lost before `--follow` ever started
    // polling).
    let (lines, mut cursor) = facade.read_agent_log(name).map_err(map_engine_error)?;
    // Fix pass (M1): a consumer that stopped reading (`| head -5`) ends the command
    // CLEANLY (exit 0), never a `println!` panic (exit 101) — see `emit_log_lines`.
    if emit_log_lines(&lines, json)? == EmitOutcome::PipeClosed || !follow {
        return Ok(());
    }

    loop {
        std::thread::sleep(LOGS_FOLLOW_POLL_INTERVAL);
        let (new_lines, next_cursor) = facade
            .read_agent_log_since(name, cursor)
            .map_err(map_engine_error)?;
        note_if_rotated(next_cursor, cursor, name);
        if emit_log_lines(&new_lines, json)? == EmitOutcome::PipeClosed {
            return Ok(());
        }
        cursor = next_cursor;

        let status = facade.instance_status(name).map_err(map_engine_error)?;
        if status.instance.state != LifecycleState::Running {
            // AC-C: a BOUNDED final drain (fix pass, M2) so nothing emitted
            // right up to the transition is lost — retries briefly rather
            // than a single unretried read (see FOLLOW_FINAL_DRAIN_BOUND's
            // docs for why); RETRIES while a poll comes back empty (the
            // writer thread has not caught up yet) and stops as soon as
            // EITHER a poll finds new content or the bound elapses —
            // never the reverse (stopping on the FIRST empty read would
            // defeat the retry's whole purpose).
            let final_deadline = Instant::now() + FOLLOW_FINAL_DRAIN_BOUND;
            loop {
                let (more, next_cursor) = facade
                    .read_agent_log_since(name, cursor)
                    .map_err(map_engine_error)?;
                note_if_rotated(next_cursor, cursor, name);
                if emit_log_lines(&more, json)? == EmitOutcome::PipeClosed {
                    return Ok(());
                }
                cursor = next_cursor;
                if !more.is_empty() || Instant::now() >= final_deadline {
                    break;
                }
                std::thread::sleep(FOLLOW_FINAL_DRAIN_POLL_INTERVAL);
            }
            ui::note(follow_exit_note(name, status.instance.state));
            return Ok(());
        }
    }
}

/// Render one [`LogLine`] in the human `<at> [<stream>] <text>` form (AC-G: the
/// caller is responsible for never re-sorting). Pure — the caller writes it.
fn human_log_line(line: &LogLine) -> String {
    format!("{} [{}] {}", line.at, line.stream, line.text)
}

/// Whether the stdout consumer is still reading, as observed by [`emit_log_lines`]
/// (fix pass, M1).
///
/// A closed downstream pipe (`kt agent logs --json | head -5`) is NOT an error: the
/// consumer got what it asked for. It IS, however, a reason to stop — a `--follow`
/// loop that kept polling and writing into a dead pipe would spin forever with
/// nowhere to put its output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmitOutcome {
    /// Everything was written; keep going.
    Continue,
    /// The downstream consumer closed the pipe — stop cleanly (exit `0`).
    PipeClosed,
}

/// Emit a batch of [`LogLine`]s to stdout in the append order given (AC-G — never
/// re-sorted), choosing the surface by `json` (story 4-3): `true` → one compact
/// NDJSON object per line via [`log_line_json`]; `false` → the human
/// `<at> [<stream>] <text>` form via [`human_log_line`]. Shared by the one-shot
/// dump and every `--follow` poll/drain so both modes stay byte-consistent. An
/// empty batch emits nothing (an empty log is zero stdout lines, not `[]`).
///
/// **Broken-pipe discipline (fix pass, M1).** Writes go through a held
/// [`std::io::StdoutLock`] rather than `println!`, because `println!` PANICS on a
/// write error and Rust ignores `SIGPIPE` — so `kt agent logs --json | head -5`
/// used to abort with exit `101`, a code outside the frozen table `docs/commands.md`
/// documents. An [`std::io::ErrorKind::BrokenPipe`] is therefore reported as
/// [`EmitOutcome::PipeClosed`], which the caller turns into a clean `Ok(())` (exit
/// `0`); any OTHER write failure is still a real [`AgentIo`] diagnostic. This is
/// scoped to the log-streaming path deliberately — no process-wide `SIGPIPE`
/// disposition change, which would alter behavior for every other command.
fn emit_log_lines(
    lines: &[LogLine],
    json: bool,
) -> Result<EmitOutcome, Box<dyn std::error::Error>> {
    use std::io::Write;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in lines {
        let rendered = if json {
            log_line_json(line)?
        } else {
            human_log_line(line)
        };
        if let Err(err) = writeln!(out, "{rendered}") {
            return classify_stdout_write(err);
        }
    }
    // `Stdout` is line-buffered, so each `writeln!` above already surfaced its own
    // failure; flush anyway so a batch is never left half-visible to the consumer.
    match out.flush() {
        Ok(()) => Ok(EmitOutcome::Continue),
        Err(err) => classify_stdout_write(err),
    }
}

/// Map a stdout write failure to either a clean [`EmitOutcome::PipeClosed`] (the
/// consumer went away — `| head`, a closed terminal) or a real [`AgentIo`]
/// diagnostic (a genuine I/O failure, e.g. a full disk when stdout is a file).
fn classify_stdout_write(err: std::io::Error) -> Result<EmitOutcome, Box<dyn std::error::Error>> {
    if err.kind() == std::io::ErrorKind::BrokenPipe {
        Ok(EmitOutcome::PipeClosed)
    } else {
        Err(AgentIo {
            message: format!("Failed to write the log output to stdout: {err}"),
        }
        .into())
    }
}

/// Serialize one [`LogLine`] to a SINGLE compact JSON line (NDJSON — story 4-3):
/// `serde_json::to_string` (never `to_string_pretty`, which would break the
/// one-object-per-line invariant). Reuses the engine's already-versioned
/// [`LogLine`] (its `schema_version` is [`ktesio_engine::LOG_SCHEMA_VERSION`]) — no
/// new schema constant. Pure and unit-testable; a serialize failure (unreachable
/// for this plain serde struct) becomes an [`AgentIo`] diagnostic, never a panic.
fn log_line_json(line: &LogLine) -> Result<String, Box<dyn std::error::Error>> {
    serde_json::to_string(line).map_err(|e| serialize_error("log line", e))
}

/// If `next_cursor` snapped BELOW `prior_cursor`, a rotation happened since
/// the last poll (story 4-2, AC-D/AC-H) — print the one honest notice rather
/// than silently claim completeness. This is how the CLI detects the
/// signal `Supervisor::read_agent_log_since` returns via the plain
/// `(Vec<LogLine>, u64)` shape (no separate boolean field).
fn note_if_rotated(next_cursor: u64, prior_cursor: u64, name: &str) {
    if next_cursor < prior_cursor {
        ui::note(format!(
            "output rotated; a small window may be missing from the live tail — `kt agent logs {name}` \
             (without --follow) re-reads everything currently retained"
        ));
    }
}

/// The honest `--follow` exit note for a non-`running` state (AC-C) — a
/// `paused` instance is not "gone" (so it gets its own wording, never an
/// error), while every other non-running state gets a generic "no further
/// output will arrive" note.
fn follow_exit_note(name: &str, state: LifecycleState) -> String {
    if state == LifecycleState::Paused {
        format!("Agent Instance '{name}' is paused; no further output until it is resumed.")
    } else {
        format!("Agent Instance '{name}' is {state}; no further output will arrive.")
    }
}

/// After a successful best-effort-eligible pause/resume, re-read the effective
/// Capability Declaration and, if pause is [`SupportLevel::BestEffort`] on the
/// current OS, print a one-line qualifier NOTE to STDERR (AD-12: notices →
/// stderr, never stdout). This is the RECOMMENDED best-effort detection (a cheap
/// extra read via the same `effective_capabilities` mechanism `kt agent show`
/// uses — no `Engine::pause` signature change). A read-back failure is swallowed:
/// it must never turn a successful pause into a CLI error (the state already
/// changed and the machine-readable qualifier is already in the event log).
fn note_if_best_effort(facade: &ktesio_engine::Blocking<'_>, name: &str, op: &str) {
    let Ok(caps) = facade.effective_capabilities(name) else {
        return;
    };
    let pause_level = caps
        .entries
        .iter()
        .find(|(c, _)| *c == Capability::Pause)
        .map(|(_, level)| *level);
    if pause_level == Some(SupportLevel::BestEffort) {
        ui::note(format!(
            "{op} for '{name}' is best-effort on {os} (adapter-cooperative); \
             the process may keep running.",
            os = caps.os,
        ));
    }
}

/// `kt agent config set <name> <key> <value>` — write one key to the Agent
/// Instance config layer (story 2-1, AC-B/AC10, AD-12).
///
/// Validated at WRITE time by the engine: a known unified key or an `agent.*`
/// pass-through key is accepted and persisted (ATOMICALLY since story 11-2 —
/// temp file + rename, so a crash mid-write cannot truncate the config) to the
/// instance `config.toml` through path authority; an unknown key OUTSIDE
/// `agent.*` is REJECTED before anything is written (the on-disk config is
/// byte-unchanged) and a miette diagnostic naming the offending key + the
/// nearest valid key goes to STDERR with a non-zero exit. On success `ui::success`
/// confirms to stdout — and any WARN-ONLY steering warnings the engine attached
/// (story 11-2, AI-33: today, a `secret:NAME` value on a FLAG-targeted key) are
/// printed to STDERR first (AD-12: results → stdout, warnings → stderr); the
/// command still exits 0 (warn-only, never a rejection). The value is stored
/// verbatim — a `secret:NAME` REFERENCE is what is persisted here (story 2-4
/// resolves + masks it at start/read, FR-14; this write neither resolves nor
/// echoes a secret).
pub fn config_set(name: &str, key: &str, value: &str) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    match engine.blocking().set_config(name, key, value) {
        Ok(warnings) => {
            // Story 11-2 (AI-33): the WARN-ONLY secret→flag steering rides the
            // engine's return value (the registry has no diagnostic sink, and
            // this synchronous CLI path prints it directly). stderr; exit 0.
            for warning in warnings {
                ui::warning(warning);
            }
            ui::success(format!(
                "Set {} = {} on Agent Instance {} (instance layer)",
                key,
                value,
                ui::skill_name(name)
            ));
            Ok(())
        }
        Err(err) => Err(map_config_error(err)),
    }
}

/// `kt agent config get <name> [<key>] [--json]` — read the EFFECTIVE (resolved)
/// config WITH per-value provenance (story 2-1 read + story 2-3 provenance,
/// AC10/AC-A/AC3/AC4, AD-12/AD-9).
///
/// With `<key>`, prints that key's effective VALUE to stdout (a not-set key is a
/// diagnostic on stderr + non-zero exit); `--json` emits that one leaf as
/// `{ key, value, source, unvalidated }`. Without a key, prints the whole
/// effective config as a Key/Value/Validated/**Source** table to stdout, or (with
/// `--json`) a single versioned document to stdout and nothing else there. Each
/// value NAMES its source layer (`engine-default` / `kind-default` / `instance` /
/// `invocation-override`), read from the [`ktesio_engine::SourceLayer`] tag the
/// engine records per leaf (AD-2: `kt` never re-derives it). Deep-resolved via the
/// engine (engine defaults < kind defaults < instance < invocation overrides); a
/// key set at the instance layer overrides the same key at a lower layer, every
/// time (FR-11). No invocation overrides are supplied here (a plain read).
///
/// Output discipline (AD-12): result → stdout; `--json` is pure JSON on stdout,
/// with any note on stderr. The 2-1/2-2 "provenance arrives in Epic 2.3" stderr
/// note is RETIRED here (Decision 3) — the "Source" column now IS the provenance,
/// so a residual deferral note would be false.
///
/// SECRETS (story 2-4, AC-C/AC11): `secret:NAME` values are MASKED by default (the
/// engine's [`ktesio_engine::ResolvedValue::display`] masks them — `kt` renders
/// whatever the engine hands it, AD-2). `--reveal` (`reveal == true`) is the SOLE
/// un-mask: it asks the engine to re-resolve the secret leaves LIVE and overlays
/// their cleartext into BOTH the human table and `--json` (Assumption 11 —
/// symmetric). A
/// reveal resolution failure is a stderr diagnostic (mapped from
/// [`ConfigError::SecretReveal`]), never a crash; `--reveal` NEVER touches the
/// snapshot/logs/events.
pub fn config_get(
    name: &str,
    key: Option<&str>,
    json: bool,
    reveal: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_engine()?;
    let blocking = engine.blocking();
    let effective = blocking
        .effective_config(name, ConfigLayer::empty())
        .map_err(map_config_error)?;

    // With --reveal, ask the ENGINE for the resolved cleartext of the secret leaves
    // (kt never resolves secrets itself, AD-2). A live-resolution failure surfaces
    // as a stderr diagnostic (never a crash). Without --reveal, an empty overlay
    // leaves every secret masked via the engine's display().
    let revealed = if reveal {
        blocking
            .reveal_secrets(name, ConfigLayer::empty())
            .map_err(map_config_error)?
    } else {
        std::collections::BTreeMap::new()
    };

    match key {
        Some(key) => {
            // A syntactically fine key that has no effective value (unset at every
            // layer) is the honest not-found diagnostic (stderr, non-zero exit) —
            // in BOTH human and --json mode (stdout stays clean of a partial doc).
            if effective.get(key).is_none() {
                // AI-25 (story 11-4): a valueless key that IS a prefix of effective
                // leaves (`budget` when `budget.tokens.*` exist) is not simply
                // unknown — it names a config TABLE. The rejection stays honest
                // (same stderr + non-zero exit), but it now NAMES the child leaves
                // (first [`TABLE_PREFIX_MAX_CHILDREN`], then an ellipsis) so the
                // operator is steered to the right leaf in one step. Surfaced, not
                // silent: the shape mismatch is a diagnostic, not a bare "no value".
                if let Some(message) = table_prefix_diagnostic(name, key, &effective) {
                    return Err(AgentUnknownConfigKey { message }.into());
                }
                return Err(AgentUnknownConfigKey {
                    message: format!(
                        "Agent Instance '{name}' has no effective value for config key '{key}'. \
                         List the effective config with: kt agent config get {name}"
                    ),
                }
                .into());
            }
            if json {
                // Emit just that one leaf as the same per-leaf object shape.
                let document = config_json(&effective, Some(key), &revealed)?;
                println!("{document}");
            } else {
                // Command result to stdout: the effective value (revealed cleartext
                // for a secret leaf under --reveal, else the masked display).
                println!("{}", leaf_display(&effective, key, &revealed));
            }
            Ok(())
        }
        None => {
            if json {
                // AC4/AD-12: the whole result is ONE JSON document to stdout.
                let document = config_json(&effective, None, &revealed)?;
                println!("{document}");
            } else {
                render_effective_config(name, &effective, &revealed);
            }
            Ok(())
        }
    }
}

/// How many effective child leaves the AI-25 table-prefix diagnostic names before
/// it elides the rest (the known key families have at most a handful of leaves; a
/// long `agent.*` subtree must not flood stderr).
const TABLE_PREFIX_MAX_CHILDREN: usize = 8;

/// The AI-25 refinement of the not-found diagnostic: when `<key>` has no value of
/// its own but IS a dotted prefix of effective leaves (`budget` →
/// `budget.tokens.per_run`, …), build a message that says so and NAMES the child
/// leaves (sorted — the engine's effective map iterates deterministically; capped
/// at [`TABLE_PREFIX_MAX_CHILDREN`] with an ellipsis). `None` when the key is not
/// a prefix either — the caller then emits the plain unknown-key diagnostic. Pure
/// (no engine, no I/O) so it is unit-testable in-process.
fn table_prefix_diagnostic(name: &str, key: &str, effective: &EffectiveConfig) -> Option<String> {
    let prefix = format!("{key}.");
    let children: Vec<&str> = effective
        .iter()
        .map(|(child, _)| child.as_str())
        .filter(|child| child.starts_with(&prefix))
        .collect();
    if children.is_empty() {
        return None;
    }
    let named: Vec<&str> = children
        .iter()
        .copied()
        .take(TABLE_PREFIX_MAX_CHILDREN)
        .collect();
    let elided = children.len() - named.len();
    let suffix = if elided > 0 {
        format!(", … (+{elided} more)")
    } else {
        String::new()
    };
    Some(format!(
        "Agent Instance '{name}' has no effective value for config key '{key}' — it is a \
         config table prefix. Its effective child leaves are: {}{suffix}. Get one of them, \
         e.g.: kt agent config get {name} {}",
        named.join(", "),
        named[0]
    ))
}

/// The display string for one leaf, honoring a `--reveal` overlay (story 2-4). If
/// `revealed` holds this key (a secret leaf under `--reveal`), its CLEARTEXT is
/// shown; otherwise the engine's masked/plain `value_display` is used. The overlay
/// only ever contains secret leaves the engine resolved, so a non-secret key is
/// always the plain display.
fn leaf_display(
    effective: &EffectiveConfig,
    key: &str,
    revealed: &std::collections::BTreeMap<String, String>,
) -> String {
    match revealed.get(key) {
        Some(cleartext) => cleartext.clone(),
        None => effective.value_display(key).unwrap_or_default(),
    }
}

/// The `kt agent config get --json` document (story 2-3, AC4 / AD-12).
///
/// A versioned wrapper — its own `schema_version` (this surface had NO prior
/// `--json`; recorded Decision 4) — carrying each resolved leaf as
/// `{ key, value, source, unvalidated }`: `value` is the rendered display string
/// (the ONE display path shared with the human table + the persisted snapshot, so
/// story 2-4 masks a `secret:` value at this single choke point — AC8), OVERLAID
/// with the engine-resolved cleartext for a secret leaf when `--reveal` is passed
/// (AC-C — the sole un-mask of machine-readable output); `source` is the kebab-case
/// [`ktesio_engine::SourceLayer`] wire label; `unvalidated` is the story-2-2
/// pass-through marker (derived via the engine accessor, AD-2). When `only` is
/// `Some(key)` the document carries just that one leaf (the single-key
/// `config get <name> <key> --json` form).
///
/// Presentation-only: the engine owns the domain types; this wraps the rendered
/// leaves for the `config get` surface.
const CONFIG_GET_SCHEMA_VERSION: u32 = 1;

/// One leaf in the `config get --json` document (story 2-3).
#[derive(Serialize)]
struct ConfigLeaf {
    /// The dotted leaf key.
    key: String,
    /// The rendered winning value (via the single display path — AC8).
    value: String,
    /// The winning source layer's kebab-case label (AC4).
    source: String,
    /// Whether the leaf skipped known-key validation (`agent.*` — story 2-2).
    unvalidated: bool,
}

/// The versioned `config get --json` document (story 2-3, AC4).
#[derive(Serialize)]
struct ConfigDocument {
    /// The config-get document schema version ([`CONFIG_GET_SCHEMA_VERSION`]).
    schema_version: u32,
    /// The resolved leaves (all, or just the single requested key).
    entries: Vec<ConfigLeaf>,
}

/// Serialize the effective config into the pretty `config get --json` document
/// (a versioned [`ConfigDocument`]). Pure (no engine, no I/O) so it is
/// unit-testable in-process; the CLI just prints the returned string to stdout.
/// `only` selects a single leaf (the single-key form) or `None` for the whole
/// config. Every value renders via the engine's ONE display path — the resolved
/// leaf's [`ktesio_engine::ResolvedValue::display`] — and every source via the
/// winning layer tag's `as_str` ([`ktesio_engine::SourceLayer::as_str`]) — `kt`
/// never re-derives either (AD-2). A serialize failure (not reachable for these
/// plain serde structs) becomes an [`AgentIo`] diagnostic, never a panic.
fn config_json(
    effective: &EffectiveConfig,
    only: Option<&str>,
    revealed: &std::collections::BTreeMap<String, String>,
) -> Result<String, Box<dyn std::error::Error>> {
    let entries: Vec<ConfigLeaf> = effective
        .iter()
        .filter(|(key, _)| only.is_none_or(|k| k == key.as_str()))
        .map(|(key, resolved)| ConfigLeaf {
            key: key.clone(),
            // The engine renders the value (the ONE display path — AC8), which
            // MASKS a secret by default, so `kt` needs no `toml` dep and cannot leak
            // (AD-2/AD-10). `--reveal` overlays the engine-resolved cleartext for a
            // secret leaf (AC-C) — the SOLE way machine-readable output carries an
            // unmasked secret; a non-secret leaf is never in the overlay.
            value: match revealed.get(key.as_str()) {
                Some(cleartext) => cleartext.clone(),
                None => resolved.display(),
            },
            // The source layer is READ from the engine tag, never re-derived.
            source: resolved.source.as_str().to_string(),
            unvalidated: effective.is_unvalidated(key),
        })
        .collect();
    let document = ConfigDocument {
        schema_version: CONFIG_GET_SCHEMA_VERSION,
        entries,
    };
    serde_json::to_string_pretty(&document).map_err(|e| serialize_error("effective config", e))
}

/// The per-row marker (story 2-2, AC-B/AC7) shown in the `config get` table's
/// "Validated" column for a leaf that skipped known-key validation — i.e. a leaf
/// under the `agent.*` pass-through namespace. A validated (known) key shows the
/// affirmative marker. The marker is DERIVED from the pass-through prefix via the
/// engine's [`EffectiveConfig::is_unvalidated`] accessor (so `kt` owns no config
/// internals — AD-2), NOT from a new persisted field; the full per-value source
/// layer stays Epic 2.3.
const UNVALIDATED_MARKER: &str = "unvalidated";
/// The affirmative counterpart shown for a validated (known) key.
const VALIDATED_MARKER: &str = "validated";

/// Render the whole effective config as a table (result → stdout, AD-12). VALUES,
/// the story-2-2 "Validated" marker column, and the story-2-3 **"Source"** column
/// naming each value's winning layer (FR-13). A leaf under `agent.*` is marked
/// **unvalidated** (it bypassed known-key validation, AC-B/AC7); a known key is
/// marked validated. The "Source" column shows the winning [`ktesio_engine::SourceLayer`]
/// label (`engine-default` / `kind-default` / `instance` / `invocation-override`),
/// read per leaf from the engine's `source` tag (AD-2: `kt` never re-derives it).
/// An empty effective config prints a plain info line rather than an empty table.
///
/// `revealed` (story 2-4, AC-C) overlays the engine-resolved cleartext for a secret
/// leaf under `--reveal`; without it (empty map) every `secret:` value stays masked
/// via the engine's `display()`.
fn render_effective_config(
    name: &str,
    effective: &EffectiveConfig,
    revealed: &std::collections::BTreeMap<String, String>,
) {
    let title = format!("Effective config for {name}");
    if effective.is_empty() {
        ui::info(format!("{title}: no config keys set"));
        return;
    }
    let columns = [
        ui::TableColumn::new("Key", 12, 40),
        ui::TableColumn::new("Value", 12, 48),
        ui::TableColumn::new("Validated", 9, 12),
        ui::TableColumn::new("Source", 12, 20),
    ];
    let rows: Vec<Vec<ui::TableCell>> = effective
        .iter()
        .map(|(key, resolved)| {
            // The marker is derived from the `agent.*` pass-through prefix via the
            // engine accessor (AD-2: `kt` never re-implements the boundary). A
            // pass-through leaf is "unvalidated"; a known key is "validated".
            let marker = if effective.is_unvalidated(key) {
                ui::TableCell::muted(UNVALIDATED_MARKER)
            } else {
                ui::TableCell::plain(VALIDATED_MARKER)
            };
            // The engine renders the value (no `toml::Value` in `kt` — AD-2),
            // masking a secret by default; --reveal overlays the resolved cleartext.
            let value = match revealed.get(key.as_str()) {
                Some(cleartext) => cleartext.clone(),
                None => resolved.display(),
            };
            vec![
                ui::TableCell::skill(key.clone()),
                ui::TableCell::plain(value),
                marker,
                // The source layer is READ from the engine tag (story 2-3, FR-13);
                // `kt` never re-derives it (AD-2).
                ui::TableCell::muted(resolved.source.as_str()),
            ]
        })
        .collect();
    ui::print_table(&title, &columns, &rows);
}

/// `kt agent memory attach <name> --kind <kind> [--json]` — attach a Memory
/// Backing to an Agent Instance (story 5-1, FR-15, spine AD-11; `native`
/// behavior is story 5-2).
///
/// The instance name is validated FIRST (the shipped 4-3 M2 convention: a
/// malformed name is a usage error, exit `2`, uniformly), then the `--kind`
/// token against what THIS release implements (both vocabulary kinds —
/// `filesystem` since 5-1, `native` since 5-2; an unrecognized token is the
/// [`AgentUnknownKind`]-shaped usage error naming the accepted values). The
/// engine does everything else through path authority: it creates the managed
/// directory inside the Agent Home for `filesystem` (for `native` nothing is
/// created — the delegation is metadata) and persists the attachment; this
/// command only displays the path the ENGINE returned (DC-1 — `kt` never joins
/// "memory" itself). Human mode (A-3/DC-6): a confirmation naming the
/// instance, the kind, its NFR-7 guarantee sentence, and the managed path on
/// stdout; diagnostics on stderr (AD-12).
///
/// `--json` (story 6-6: Epic 5's deferred wire surface, frozen at the contract
/// v1 freeze with the typed snake_case [`MemoryBackingKind`]/[`GuaranteeLevel`]
/// strings adopted VERBATIM) writes a single versioned
/// [`MemoryAttachDocument`] to STDOUT and nothing else there (AD-14). Only
/// THIS mode performs the extra `memory_status` read-back that fills
/// `declared` (the DC-10 delivery fact: whether the injected path will actually
/// reach the agent — `true` only for a `filesystem` backing whose adapter maps
/// `memory.dir` (the `hermes` builtin does, via `HERMES_HOME`); `false` for a
/// mapping-less adapter and also for a `native` backing, where it means "no
/// delivery is offered", not "the adapter declined")
/// and `guarantee`; the human path stays byte-identical to its pre-freeze
/// behavior (one attach call, the guarantee computed locally from the kind),
/// so it can never fail where it succeeded before the wire existed.
pub fn memory_attach(name: &str, kind: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Validate the name BEFORE anything else so a malformed name exits 2
    // uniformly (never 3 via a lookup miss), per the test-pinned convention.
    validate_instance_name(name)?;
    let backing_kind = match kind {
        "filesystem" => MemoryBackingKind::Filesystem,
        "native" => MemoryBackingKind::Native,
        other => {
            return Err(AgentUnknownKind {
                message: format!(
                "Unknown Memory Backing kind '{other}'. This release accepts: --kind filesystem, \
                 --kind native."
            ),
            }
            .into())
        }
    };
    let engine = open_engine()?;
    let dir = match engine.blocking().attach_memory(name, backing_kind) {
        Ok(dir) => dir,
        Err(err) => return Err(map_error(err)),
    };
    if json {
        // The DC-10 delivery fact + the typed guarantee, read back from the
        // engine (kt re-derives neither — AD-2). JSON-only: the human path
        // below must not gain an extra engine round-trip (or a new failure
        // mode) just because the wire surface exists. Right after a
        // successful attach the backing exists, so `None` is unreachable in
        // practice; it is an error, not a panic.
        let status = match engine.blocking().memory_status(name) {
            Ok(Some(status)) => status,
            Ok(None) => {
                return Err(AgentIo {
                    message: format!(
                        "Attached the Memory Backing to '{name}', but the engine reports no \
                         attachment; re-run kt agent memory attach {name} --kind {kind} --json."
                    ),
                }
                .into())
            }
            // Retro #166 (finding B10): by this point the attach PERSISTED —
            // the backing REMAINS attached — so an errored read-back must say
            // so, never surface as a generic failure an operator could read
            // as "the attach failed". Name the partial state + the remediation
            // (the same-kind re-attach is an idempotent success, and
            // `memory detach` undoes it).
            Err(err) => return Err(attach_readback_failed(name, kind, &err).into()),
        };
        // A machine-consumable document must never carry U+FFFD replacement
        // characters: a managed path that is not valid UTF-8 is a NAMED error,
        // not a lossy silent rewrite.
        let dir = dir.to_str().ok_or_else(|| -> Box<dyn std::error::Error> {
            AgentIo {
                message: format!(
                    "The managed Memory Backing directory for '{name}' is not valid UTF-8 \
                     ('{}'); the --json document cannot represent it. Re-register the instance \
                     at a UTF-8 path.",
                    dir.display()
                ),
            }
            .into()
        })?;
        let document = memory_attach_json(
            name.to_string(),
            status.kind,
            status.guarantee,
            dir.to_string(),
            status.declared,
        )?;
        println!("{document}");
        return Ok(());
    }
    ui::success(format!(
        "Attached {} Memory Backing to Agent Instance {}",
        kind,
        ui::skill_name(name)
    ));
    // NFR-7 boundary statement (DC-6): one sentence naming what is
    // guaranteed versus delegated, worded once in the engine so every
    // surface agrees.
    println!(
        "{}",
        GuaranteeLevel::for_kind(backing_kind).boundary_statement()
    );
    // Command result to stdout: the managed path, received from the
    // engine (DC-1) — never constructed here. For `native` this is the
    // COMPUTED location only (nothing was created).
    println!("{}", dir.display());
    Ok(())
}

/// Compose the `memory attach --json` read-back-failure diagnostic (retro
/// #166, finding B10). The caller only reaches here AFTER the attach
/// persisted, so the message must name the partial state — the backing
/// REMAINS attached, nothing was rolled back — plus the underlying cause and
/// both remediations, never a generic failure an operator could read as "the
/// attach failed".
fn attach_readback_failed(name: &str, kind: &str, err: &RegistryError) -> AgentIo {
    AgentIo {
        message: format!(
            "Attached the Memory Backing to '{name}', but reading the attachment back failed: \
             {err}. The attachment REMAINS attached (nothing was rolled back) — re-run \
             `kt agent memory attach {name} --kind {kind} --json` (a same-kind re-attach is an \
             idempotent success) or undo it with `kt agent memory detach {name}`."
        ),
    }
}

/// `kt agent memory detach <name> [--json]` — detach an Agent Instance's Memory
/// Backing (story 5-1). METADATA ONLY: the managed directory and its contents
/// remain on disk (operator data is never silently deleted); a later re-attach
/// re-adopts them. Same terminal-state guard as attach — no hot-swap, no force
/// escape. `--json` (story 6-6's deferred wire surface) emits a versioned
/// [`MemoryDetachDocument`] and nothing else on stdout (AD-14).
pub fn memory_detach(name: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    validate_instance_name(name)?;
    let engine = open_engine()?;
    match engine.blocking().detach_memory(name) {
        Ok(()) => {
            if json {
                let document = memory_detach_json(name.to_string())?;
                println!("{document}");
                return Ok(());
            }
            ui::success(format!(
                "Detached the Memory Backing from Agent Instance {} (the managed directory \
                 and its contents remain on disk)",
                ui::skill_name(name)
            ));
            Ok(())
        }
        Err(err) => Err(map_error(err)),
    }
}

/// Translate a [`ConfigError`] (story 2-1) into a `miette` diagnostic with a
/// remediation hint (NFR-1). The unknown-key class (AC-B) carries the offending
/// key + the nearest-key suggestion the engine computed; the shared name/store
/// classes reuse the existing agent diagnostics for a consistent surface;
/// malformed-layer names the layer + path (AC8).
fn map_config_error(err: ConfigError) -> Box<dyn std::error::Error> {
    match err {
        // AC-B: an unknown key outside `agent.*` — the engine already computed the
        // nearest valid key (or "no close match"); surface the whole message
        // (which names the key + the suggestion) with a pass-through remediation.
        ConfigError::UnknownKey { .. } => AgentUnknownConfigKey {
            message: format!(
                "{err}. Set a known unified key, or use the agent.* pass-through namespace for \
                 agent-native extras (e.g. kt agent config set <name> agent.<key> <value>)."
            ),
        }
        .into(),
        // Patch #3: a write that would nest a child under an existing scalar is
        // rejected (nothing persisted) — the message names the conflicting
        // ancestor; add the remediation.
        ConfigError::WriteShapeConflict { .. } => AgentConfig {
            message: format!(
                "{err}. Nothing was changed. Unset or rename the conflicting key first, then \
                 set the nested key."
            ),
        }
        .into(),
        ConfigError::InvalidName { name, reason } => AgentInvalidName {
            message: format!(
                "Invalid Agent Instance name '{name}': {reason}. Names must match \
                 ^[a-z0-9][a-z0-9_-]*$ (lowercase letters, digits, '_' or '-', not starting \
                 with '_' or '-')."
            ),
        }
        .into(),
        ConfigError::NotFound { name } => AgentNotFound {
            message: format!(
                "No Agent Instance named '{name}' is registered. List the Fleet with: kt agent list"
            ),
        }
        .into(),
        ConfigError::MalformedLayer {
            layer,
            path,
            detail,
        } => AgentConfig {
            message: format!(
                "The {layer} config layer at '{path}' could not be read/parsed: {detail}. \
                 Fix the TOML (or restore the file) and try again."
            ),
        }
        .into(),
        ConfigError::Store { name, detail } => AgentStore {
            message: format!(
                "State store error for Agent Instance '{name}': {detail}. The state database may \
                 be inaccessible."
            ),
        }
        .into(),
        // Story 2-4 (AC-C/AC11): `--reveal` re-resolved a secret and it failed
        // (unset env var, ill-permissioned/absent secrets file). A read-surface
        // DIAGNOSTIC (stderr, non-zero exit) — the detail names the NAME + the
        // resolvers tried + a remediation, never a value.
        ConfigError::SecretReveal { detail } => AgentConfig {
            message: format!(
                "{detail}. Set the environment variable, or add it to the engine secrets file \
                 (chmod 600), then try --reveal again."
            ),
        }
        .into(),
        // Story 3-2 (AC-C): a KNOWN budget key with a malformed VALUE — a
        // `budget.tokens.*` that is not a whole number, or a `budget.breach_action`
        // that is not pause/stop/warn. Rejected before any persistence (nothing
        // changed); the message names the key + value + the accepted form.
        ConfigError::InvalidValue { .. } => AgentConfig {
            message: format!("{err}. Nothing was changed. Fix the value and try again."),
        }
        .into(),
    }
}

/// Open the engine using the default (or env-overridden) state dir.
///
/// Passing `None` lets the engine resolve the base via `KTESIO_STATE_DIR` then
/// the platform data dir — the engine remains the sole path authority. The
/// engine owns its tokio runtime; `kt` drives it through the blocking facade.
fn open_engine() -> Result<Engine, Box<dyn std::error::Error>> {
    Engine::open(None).map_err(map_error)
}

/// Translate a [`RegistryError`] into a `miette` diagnostic carrying a
/// remediation hint (NFR-1: name the instance + reason + remediation).
fn map_error(err: RegistryError) -> Box<dyn std::error::Error> {
    match err {
        RegistryError::DuplicateName { name } => AgentDuplicateName {
            message: format!(
                "An Agent Instance named '{name}' already exists. Choose a different name, \
                 or remove the existing instance with: kt agent remove {name}"
            ),
        }
        .into(),
        RegistryError::InvalidName { name, reason } => AgentInvalidName {
            message: format!(
                "Invalid Agent Instance name '{name}': {reason}. Names must match \
                 ^[a-z0-9][a-z0-9_-]*$ (lowercase letters, digits, '_' or '-', not starting \
                 with '_' or '-')."
            ),
        }
        .into(),
        RegistryError::NotFound { name } => AgentNotFound {
            message: format!(
                "No Agent Instance named '{name}' is registered. List the Fleet with: kt agent list"
            ),
        }
        .into(),
        RegistryError::RunningRequiresForce { name } => AgentRunningRequiresForce {
            message: format!(
                "Agent Instance '{name}' is running. Stop it first, or pass --force to remove \
                 it anyway: kt agent remove {name} --delete --force"
            ),
        }
        .into(),
        RegistryError::Io { name, path, source } => AgentIo {
            message: format!(
                "Filesystem error for Agent Instance '{name}' at '{path}': {source}. Check \
                 directory permissions and available disk space."
            ),
        }
        .into(),
        RegistryError::RemoveLeftoverHome { name, path, detail } => AgentIo {
            message: format!(
                "Agent Instance '{name}' was removed from the Fleet, but its Agent Home at \
                 '{path}' could not be deleted: {detail}. Remove the directory manually."
            ),
        }
        .into(),
        // Story 2-3: the effective-config snapshot could not be written (AD-9/AD-6).
        // Surfaced through the lifecycle path as EngineError::Snapshot; this arm
        // keeps the RegistryError mapper exhaustive with a matching diagnostic.
        RegistryError::SnapshotWrite { name, path, detail } => AgentIo {
            message: format!(
                "Could not write the effective-config snapshot for '{name}' at '{path}': {detail}. \
                 Check directory permissions and available disk space, then start it again."
            ),
        }
        .into(),
        RegistryError::RegisterOrphanRow {
            name,
            home_error,
            rollback_error,
        } => AgentIo {
            message: format!(
                "Agent Instance '{name}' left an orphaned registry row: its Agent Home could not \
                 be created ({home_error}) and the automatic rollback also failed \
                 ({rollback_error}). Remove the stale entry with: kt agent remove {name} --force"
            ),
        }
        .into(),
        RegistryError::UnknownAdapterKind { kind } => AgentUnknownKind {
            message: format!(
                "Unknown adapter kind '{kind}'. Register a native adapter with a known kind \
                 (e.g. --kind mock), or supply a manifest adapter with --manifest <path>."
            ),
        }
        .into(),
        RegistryError::ManifestNotFound { path } => AgentManifestNotFound {
            message: format!(
                "No adapter.toml found at '{path}'. Point --manifest at a directory containing \
                 an adapter.toml, or at the file itself."
            ),
        }
        .into(),
        RegistryError::ManifestUnreadable { path, detail } => AgentManifestUnreadable {
            message: format!(
                "Could not read the adapter manifest at '{path}': {detail}. Check that it exists \
                 and is readable (a regular file, with read permission)."
            ),
        }
        .into(),
        RegistryError::ManifestInvalid { path, detail } => AgentManifestInvalid {
            message: format!(
                "The adapter manifest at '{path}' is invalid: {detail}. Fix the named section \
                 and try again."
            ),
        }
        .into(),
        // Story 6-6 (FR-30, the v1 freeze): the manifest is well-formed but its
        // contract MAJOR does not negotiate with this engine. `detail` already
        // names BOTH versions and quotes the compatibility rule (rendered once
        // in ktesio-adapter-api), so this arm adds only the remediation.
        RegistryError::ContractIncompatible { path, detail } => AgentContractIncompatible {
            message: format!(
                "The adapter manifest at '{path}' cannot load: {detail}. Ask the adapter author \
                 to retarget the manifest's contract_version to this engine's contract major — \
                 see docs/adapter-contract.md."
            ),
        }
        .into(),
        RegistryError::NoMeteringSource { adapter } => AgentNoMeteringSource {
            message: format!(
                "Adapter '{adapter}' declares no viable Metering Source. Add a `[metering]` \
                 section with source = \"self-reported\" or \"engine-observed\" — Ktesio rejects \
                 adapters with no metering source."
            ),
        }
        .into(),
        RegistryError::NoCapabilities { adapter } => AgentNoCapabilities {
            message: format!(
                "Adapter '{adapter}' declares no capabilities. Add a `[capabilities]` section \
                 declaring at least one capability."
            ),
        }
        .into(),
        // Story 5-1: the two Memory Backing guards. Both are invalid-STATE
        // rejections (exit `4`, DC-4) — no new exit-code number was minted — and
        // neither has a force escape (AD-11 forbids hot-swap outright, unlike
        // remove's RunningRequiresForce).
        RegistryError::MemoryBackingHotSwap { name, state } => AgentMemoryHotSwap {
            message: format!(
                "Agent Instance '{name}' is '{state}'; a Memory Backing cannot be hot-swapped. \
                 Attach/detach need a terminal state (registered, stopped, or failed). Bring it \
                 to a terminal state first: kt agent stop {name} from running or paused"
            ),
        }
        .into(),
        RegistryError::MemoryBackingKindConflict {
            name,
            attached,
            requested,
        } => AgentMemoryKindConflict {
            message: format!(
                "Agent Instance '{name}' already has a '{attached}' Memory Backing attached; \
                 detach it before attaching '{requested}': kt agent memory detach {name}"
            ),
        }
        .into(),
        RegistryError::Store(inner) => AgentStore {
            message: format!("State store error: {inner}. The state database may be inaccessible."),
        }
        .into(),
    }
}

/// Translate an [`EngineError`] (lifecycle: start / stop) into a `miette`
/// diagnostic with a remediation hint (NFR-1). The invalid-transition class
/// (AC4) and the launch-failed diagnostic (AC2) get their own codes; the shared
/// registry-shaped variants (NotFound / InvalidName / Store) reuse the existing
/// agent diagnostics for a consistent surface.
fn map_engine_error(err: EngineError) -> Box<dyn std::error::Error> {
    match err {
        EngineError::NotFound { name } => AgentNotFound {
            message: format!(
                "No Agent Instance named '{name}' is registered. List the Fleet with: kt agent list"
            ),
        }
        .into(),
        EngineError::InvalidName { name, reason } => AgentInvalidName {
            message: format!(
                "Invalid Agent Instance name '{name}': {reason}. Names must match \
                 ^[a-z0-9][a-z0-9_-]*$ (lowercase letters, digits, '_' or '-', not starting \
                 with '_' or '-')."
            ),
        }
        .into(),
        // AC4: the ONE uniform invalid-transition class, identical for every
        // adapter (it comes from the shared transition table).
        EngineError::InvalidTransition(inner) => AgentInvalidTransition {
            message: format!("{inner}. Check the instance's current state with: kt agent list"),
        }
        .into(),
        // AC2: the adapter/process diagnostic is preserved verbatim; the instance
        // is left in `failed`.
        EngineError::LaunchFailed { name, detail } => AgentLaunchFailed {
            message: format!(
                "Agent Instance '{name}' failed to launch: {detail}. The instance is now in the \
                 'failed' state; fix the adapter's launch command and try starting it again."
            ),
        }
        .into(),
        // AC3: pause is UNSUPPORTED on this OS — fail fast QUOTING the declaration
        // (the level + OS) and pointing at `kt agent show`. No state changed.
        EngineError::CapabilityUnsupported {
            name,
            capability,
            os,
            level,
        } => AgentCapabilityUnsupported {
            message: format!(
                "Agent Instance '{name}' cannot {capability}: this agent declares {capability} \
                 '{level}' on {os}. Inspect its Capability Declaration with: kt agent show {name}"
            ),
        }
        .into(),
        // AI-7: `resume` targeted a PAUSED instance whose adapter declares PAUSE
        // unsupported on this OS — the engine cannot signal the suspension awake,
        // and the bare pause-unsupported diagnostic would strand an operator whose
        // instance is ALREADY `paused`. Name the state + the declaration and give
        // the real escape hatch (stop works without pause support) instead.
        EngineError::ResumeUnsupported { name, os, level } => AgentResumeUnsupported {
            message: format!(
                "Agent Instance '{name}' is paused, but this agent declares pause '{level}' \
                 on {os}, so resume cannot signal the suspension awake. Recovery: \
                 kt agent stop {name} && kt agent start {name}. The same resume does work on \
                 hosts where the adapter declares pause support (informational — this CLI \
                 cannot change this host's OS). Inspect the Capability Declaration with: \
                 kt agent show {name}"
            ),
        }
        .into(),
        EngineError::AdapterUnresolved { name, detail } => AgentLaunchFailed {
            message: format!(
                "Could not resolve the adapter to start Agent Instance '{name}': {detail}."
            ),
        }
        .into(),
        EngineError::Log { name, path, detail } => AgentIo {
            message: format!(
                "Could not write the instance log for '{name}' at '{path}': {detail}. Check \
                 directory permissions and available disk space."
            ),
        }
        .into(),
        // Story 2-3: the effective-config snapshot could not be written at start
        // (AD-9/AD-6). It lands before the `starting` transition, so the instance
        // stays in its prior state; name the snapshot path + a disk/permissions
        // remediation (NFR-1).
        EngineError::Snapshot { name, path, detail } => AgentIo {
            message: format!(
                "Could not write the effective-config snapshot for '{name}' at '{path}': {detail}. \
                 Check directory permissions and available disk space, then start it again."
            ),
        }
        .into(),
        // Story 2-4 (AC-A/AC9): a `secret:NAME` reference could not be resolved at
        // start (unset env var, ill-permissioned/absent secrets file). Resolution
        // runs before the `starting` transition, so the instance stays in its prior
        // state; the detail names the NAME + resolvers + a remediation, never a
        // value (NFR-6).
        EngineError::Secret { name, detail } => AgentConfig {
            message: format!(
                "Agent Instance '{name}' could not start: {detail}. Nothing was changed; set the \
                 secret and start it again."
            ),
        }
        .into(),
        // Story 3-4 (AC-A): the engine-observed loopback forward listener could not
        // start (no configured upstream URL, a non-http upstream, or a bind failure).
        // It starts before the `starting` transition, so the instance stays in its
        // prior state; the detail is traffic-free (never a body/header/key — NFR-6).
        EngineError::ObservedMetering { name, detail } => AgentConfig {
            message: format!(
                "Agent Instance '{name}' could not start engine-observed metering: {detail}. \
                 Nothing was changed; fix the metering configuration and start it again."
            ),
        }
        .into(),
        EngineError::Backend { name, source } => AgentIo {
            message: format!("Process control failed for Agent Instance '{name}': {source}."),
        }
        .into(),
        // Story 4.1 AC-C: `send` targeted an instance that is not `running`.
        // Not a transition error (there is no transition table entry for
        // `send`), so it gets its own remediation naming the current state.
        //
        // M2 fix (review of #79): the remediation VERB must match the
        // instance's ACTUAL state. A `paused` instance's correct remediation
        // is `kt agent resume` — suggesting `kt agent start` there hits a
        // SECOND, confusing `InvalidTransition` error (start only accepts
        // registered/stopped/failed), not a helpful fix. Every other
        // NOT-running state (registered/starting/stopping/stopped/failed)
        // keeps the original `start` remediation.
        EngineError::NotRunning { name, state } => {
            let remediation = if state == "paused" {
                format!("resume it with: kt agent resume {name}")
            } else {
                format!("start it first with: kt agent start {name}")
            };
            AgentNotRunning {
                message: format!(
                    "Agent Instance '{name}' is not running (current state: {state}); \
                     {remediation}. List the Fleet with: kt agent list"
                ),
            }
            .into()
        }
        // Story 4.1 AC-D: the instance IS running, but this engine session
        // holds no live stdin pipe for it (most commonly an instance ADOPTED
        // from a prior engine session). Distinct from CapabilityUnsupported —
        // the adapter's declaration may truthfully say `interaction:
        // guaranteed`; it is this session's reach that is limited. State the
        // single-lifetime boundary honestly rather than implying a bug.
        EngineError::InteractionUnavailable { name, detail } => AgentInteractionUnavailable {
            message: format!(
                "Agent Instance '{name}' cannot receive input right now: {detail}. Durable \
                 cross-invocation interaction needs a persistent engine session (planned for a \
                 future release); within a single `kt` process/embedding session this works."
            ),
        }
        .into(),
        // Story 4.1 fix pass (CRITICAL finding, review of #79): `send`'s
        // write to this instance's stdin did not complete within the bounded
        // timeout — the agent may be stuck (not draining its input).
        // Distinct from InteractionUnavailable: this engine session DID hold
        // a live pipe and attempted the write; it simply never came back.
        // The instance's interaction channel is now permanently unusable for
        // the rest of this session, so the remediation is a fresh start.
        EngineError::InteractionTimedOut { name, timeout_secs } => AgentInteractionTimedOut {
            message: format!(
                "Agent Instance '{name}' is not draining its input within {timeout_secs}s and \
                 may be stuck. Its interaction channel is now unavailable for the rest of this \
                 session; restart it for a fresh one: kt agent stop {name} && kt agent start \
                 {name}"
            ),
        }
        .into(),
        // Fix pass (review of #80 follow-up — the CRITICAL finding): `stop`
        // sent SIGKILL but could not confirm the process's death within the
        // bound — most likely because it is stuck in an OS-level
        // uninterruptible I/O wait (e.g. disk pressure caused by a fast
        // writer with no reader-side backpressure). The instance is NOT
        // `stopped` — it stays `stopping`; point at retrying `stop` (which
        // will not re-block for the same duration if it is still stuck) and
        // name the likely cause honestly rather than implying a bug.
        EngineError::StopUnconfirmed { name, timeout_secs } => AgentStopUnconfirmed {
            message: format!(
                "Agent Instance '{name}' was sent SIGKILL but has not been confirmed dead \
                 within {timeout_secs}s; it may be stuck in an OS-level I/O wait (for example, \
                 disk pressure). It remains in the 'stopping' state. Try again with: \
                 kt agent stop {name} — this will not re-block if it is still stuck, and will \
                 succeed once the process actually exits (which may require relieving disk \
                 pressure or waiting for the stuck I/O to resolve)."
            ),
        }
        .into(),
        EngineError::Store(inner) => AgentStore {
            message: format!("State store error: {inner}. The state database may be inaccessible."),
        }
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the in-process engine-driver tests that mutate the shared
    /// `KTESIO_STATE_DIR` process env var (`config_get_*` + `list_and_show_*`), so
    /// they never race each other under the multi-threaded test runner (one test
    /// clearing the var mid-run would break another's `open_engine`). A poisoned
    /// lock is fine — the guard is only for env-var mutual exclusion.
    static STATE_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn disposition_from_flags_resolves_each_combination() {
        assert_eq!(
            DispositionArg::from_flags(true, false),
            DispositionArg::Delete
        );
        assert_eq!(
            DispositionArg::from_flags(false, true),
            DispositionArg::Retain
        );
        assert_eq!(
            DispositionArg::from_flags(false, false),
            DispositionArg::Unspecified
        );
    }

    #[test]
    fn disposition_from_flags_fails_closed_to_retain_when_both_set() {
        // F10: clap makes these mutually exclusive, but as defense-in-depth an
        // ambiguous both-set input must fail CLOSED to Retain (the safe
        // default), never silently prefer Delete.
        assert_eq!(
            DispositionArg::from_flags(true, true),
            DispositionArg::Retain
        );
    }

    #[test]
    fn unspecified_resolves_to_retain_the_safe_default() {
        assert_eq!(
            DispositionArg::Unspecified.resolve(),
            RemoveDisposition::Retain
        );
        assert_eq!(DispositionArg::Retain.resolve(), RemoveDisposition::Retain);
        assert_eq!(DispositionArg::Delete.resolve(), RemoveDisposition::Delete);
    }

    #[test]
    fn map_error_includes_remediation_hints() {
        let dup = map_error(RegistryError::DuplicateName {
            name: "demo".into(),
        });
        assert!(dup.to_string().contains("kt agent remove demo"));

        let running = map_error(RegistryError::RunningRequiresForce {
            name: "live".into(),
        });
        assert!(running.to_string().contains("--force"));

        let invalid = map_error(RegistryError::InvalidName {
            name: "Bad".into(),
            reason: ktesio_engine::NameError::BadChar,
        });
        assert!(invalid.to_string().contains("^[a-z0-9]"));

        let missing = map_error(RegistryError::NotFound {
            name: "ghost".into(),
        });
        assert!(missing.to_string().contains("kt agent list"));

        let io = map_error(RegistryError::Io {
            name: "demo".into(),
            path: "/x/agents/demo".into(),
            source: std::io::Error::other("boom"),
        });
        assert!(io.to_string().contains("/x/agents/demo"));

        let leftover = map_error(RegistryError::RemoveLeftoverHome {
            name: "demo".into(),
            path: "/x/agents/demo".into(),
            detail: "still there".into(),
        });
        assert!(leftover.to_string().contains("removed from the Fleet"));

        // F2: the orphan-row partial failure renders the --force remediation.
        let orphan = map_error(RegistryError::RegisterOrphanRow {
            name: "demo".into(),
            home_error: "mkdir failed".into(),
            rollback_error: "delete blocked".into(),
        });
        let orphan_msg = orphan.to_string();
        assert!(orphan_msg.contains("orphaned registry row"));
        assert!(orphan_msg.contains("kt agent remove demo --force"));

        // Store errors surface as a state-store diagnostic.
        let store = map_error(RegistryError::Store(
            ktesio_engine::ports::StoreError::Backend("db gone".into()),
        ));
        assert!(store.to_string().contains("State store error"));
    }

    #[test]
    fn map_error_renders_the_contract_incompatible_rejection() {
        // Story 6-6 (FR-30): the negotiation rejection keeps BOTH versions +
        // the rule from the engine's message verbatim, and adds the
        // retarget-the-manifest remediation. The rule literal below is pinned
        // against the REAL message in the engine's
        // `incompatible_major_fails_naming_both_versions_and_the_rule` (kt
        // depends on the engine's surface, not the adapter-api crate
        // directly), so text drift fails there first.
        let detail = "incompatible adapter contract: manifest declares 2.1.0, engine speaks \
                      1.0.0 — compatible iff the major versions match (contract v1 policy, \
                      docs/adapter-contract.md#versioning)";
        let err = map_error(RegistryError::ContractIncompatible {
            path: "/x/adapter.toml".into(),
            detail: detail.to_string(),
        });
        let msg = err.to_string();
        assert!(msg.contains("/x/adapter.toml"), "{msg}");
        assert!(msg.contains("2.1.0"), "names the manifest version: {msg}");
        assert!(msg.contains("1.0.0"), "names the engine version: {msg}");
        assert!(
            msg.contains("compatible iff the major versions match"),
            "quotes the rule: {msg}"
        );
        assert!(
            msg.contains("docs/adapter-contract.md"),
            "points at the policy page: {msg}"
        );
    }

    #[test]
    fn attach_readback_failure_names_the_persisted_attachment() {
        // Retro #166 (finding B10): after a persisted attach, an errored
        // --json read-back must name the partial state — the backing REMAINS
        // attached — plus the underlying cause and both remediations, never a
        // generic failure an operator could read as "the attach failed".
        // (The read-back cannot be faulted end-to-end without a store-fault
        // harness, so the composed diagnostic is pinned at this seam.)
        let err = attach_readback_failed(
            "demo",
            "filesystem",
            &RegistryError::Io {
                name: "demo".into(),
                path: "/x/agents/demo".into(),
                source: std::io::Error::other("boom"),
            },
        );
        let msg = err.to_string();
        assert!(
            msg.contains("Attached the Memory Backing to 'demo'"),
            "names the completed attach: {msg}"
        );
        assert!(
            msg.contains("REMAINS attached"),
            "names the partial state: {msg}"
        );
        assert!(msg.contains("boom"), "carries the underlying cause: {msg}");
        assert!(
            msg.contains("kt agent memory attach demo --kind filesystem --json"),
            "gives the re-run remediation: {msg}"
        );
        assert!(
            msg.contains("kt agent memory detach demo"),
            "gives the undo remediation: {msg}"
        );
    }

    #[test]
    fn memory_json_serializers_pin_the_frozen_documents() {
        // In-process half of the freeze fixtures (the end-to-end key-set
        // assertions live in agent_cli.rs): the documents' key-sets, the
        // MEMORY_SCHEMA_VERSION literal, and the verbatim snake_case wire
        // strings are pinned here too, so a wire change cannot slip past the
        // binary-spawning suite alone.
        let attach: serde_json::Value = serde_json::from_str(
            &memory_attach_json(
                "demo".to_string(),
                MemoryBackingKind::Filesystem,
                GuaranteeLevel::ManagedDirByteDurable,
                "/home/agents/demo/memory".to_string(),
                true,
            )
            .expect("serialize"),
        )
        .expect("parse");
        assert_eq!(
            sorted(attach.as_object().expect("object")),
            [
                "declared",
                "dir",
                "guarantee",
                "instance",
                "kind",
                "schema_version"
            ]
        );
        assert_eq!(attach["schema_version"], serde_json::json!(1));
        assert_eq!(attach["kind"], serde_json::json!("filesystem"));
        assert_eq!(
            attach["guarantee"],
            serde_json::json!("managed_dir_byte_durable")
        );
        assert_eq!(attach["declared"], serde_json::json!(true));

        let native: serde_json::Value = serde_json::from_str(
            &memory_attach_json(
                "nat".to_string(),
                MemoryBackingKind::Native,
                GuaranteeLevel::HomePersistenceOnly,
                "/home/agents/nat/memory".to_string(),
                false,
            )
            .expect("serialize"),
        )
        .expect("parse");
        assert_eq!(native["kind"], serde_json::json!("native"));
        assert_eq!(
            native["guarantee"],
            serde_json::json!("home_persistence_only")
        );
        assert_eq!(native["declared"], serde_json::json!(false));

        let detach: serde_json::Value =
            serde_json::from_str(&memory_detach_json("demo".to_string()).expect("serialize"))
                .expect("parse");
        assert_eq!(
            sorted(detach.as_object().expect("object")),
            ["instance", "schema_version"]
        );
        assert_eq!(detach["schema_version"], serde_json::json!(1));
        assert_eq!(detach["instance"], serde_json::json!("demo"));
    }

    /// Sorted JSON object keys, for the in-process frozen key-set pins.
    fn sorted(keys: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
        let mut keys: Vec<String> = keys.keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn map_engine_error_renders_the_interaction_timed_out_diagnostic() {
        // Fix pass (CRITICAL finding, review of #79) coverage-closer: unlike
        // every OTHER `map_engine_error` arm (all reached only indirectly
        // through full CLI-process-spawning tests in `agent_cli.rs`),
        // `EngineError::InteractionTimedOut`'s CLI rendering CANNOT be
        // exercised that way at all — a genuinely stuck write needs a LIVE
        // pipe, which only exists within the SAME engine session that
        // spawned it, but a single `kt agent send` invocation opens its OWN
        // fresh `Engine` and exits after one call; a SEPARATE `kt agent
        // send` reaching an instance a prior invocation started can only do
        // so via `adopt_orphans`, which NEVER carries a live pipe (mirrors
        // the story's OWN Task 8 Deviation 1 finding for
        // `InteractionUnavailable`'s happy path — the same structural
        // constraint, one variant further). So this diagnostic's rendering
        // is unit-tested directly here, mirroring `map_error_includes_
        // remediation_hints`'s existing direct-call pattern for `map_error`.
        let err = map_engine_error(EngineError::InteractionTimedOut {
            name: "stuck".to_string(),
            timeout_secs: 5,
        });
        let msg = err.to_string();
        assert!(msg.contains("stuck"), "names the instance: {msg}");
        assert!(msg.contains('5'), "names the bound: {msg}");
        assert!(
            msg.contains("kt agent stop stuck") && msg.contains("kt agent start stuck"),
            "points at a restart for a fresh channel: {msg}"
        );
        assert!(
            !msg.contains("unsupported") && !msg.contains("declares"),
            "must never be misattributed to CapabilityUnsupported: {msg}"
        );
    }

    #[test]
    fn every_engine_error_mapper_arm_preserves_its_documented_exit_code() {
        // Story 4-3 fix pass (H1) — closing a PROVEN gate hole. `exit_code.rs`
        // pins `classify(AgentInteractionTimedOut) == 6`, and `agent_cli.rs` pins
        // that `main` wires `classify` to the process status. NOTHING pinned the
        // middle link: that the timeout PATH actually PRODUCES an
        // `AgentInteractionTimedOut`. An adversarial pass changed the
        // `EngineError::InteractionTimedOut` arm of `map_engine_error` to build an
        // `AgentIo` instead — silently demoting the documented `6` to `1` — and
        // the whole suite still passed.
        //
        // This test closes that seam for EVERY engine-error class that carries a
        // non-`1` code: it drives the REAL mapper and then the REAL classifier, so
        // severing any arm changes an asserted number. It is deterministic and
        // runs on ALL THREE OSes (no process spawn), which matters because codes
        // `5` and `6` have no cross-OS end-to-end path (`send`/`pause` both need a
        // genuinely running child, and the surviving-engine harness is Unix-only).
        use crate::exit_code::{classify, ExitCode};

        let cases: Vec<(&str, EngineError, ExitCode)> = vec![
            (
                // 6 — the hole the adversarial pass proved.
                "InteractionTimedOut",
                EngineError::InteractionTimedOut {
                    name: "stuck".to_string(),
                    timeout_secs: 5,
                },
                ExitCode::TimedOut,
            ),
            (
                "InteractionUnavailable",
                EngineError::InteractionUnavailable {
                    name: "svc".to_string(),
                    detail: "no live process handle".to_string(),
                },
                ExitCode::Unsupported,
            ),
            (
                "CapabilityUnsupported",
                EngineError::CapabilityUnsupported {
                    name: "un".to_string(),
                    capability: "pause".to_string(),
                    os: "linux".to_string(),
                    level: "unsupported".to_string(),
                },
                ExitCode::Unsupported,
            ),
            (
                // AI-7: the dedicated resume-under-unsupported-pause diagnostic —
                // the same capability-unsupported class (5), never a demotion.
                "ResumeUnsupported",
                EngineError::ResumeUnsupported {
                    name: "pz".to_string(),
                    os: "linux".to_string(),
                    level: "unsupported".to_string(),
                },
                ExitCode::Unsupported,
            ),
            (
                "NotRunning",
                EngineError::NotRunning {
                    name: "svc".to_string(),
                    state: "stopped".to_string(),
                },
                ExitCode::InvalidState,
            ),
            (
                "StopUnconfirmed",
                EngineError::StopUnconfirmed {
                    name: "svc".to_string(),
                    timeout_secs: 5,
                },
                ExitCode::InvalidState,
            ),
            (
                "InvalidName",
                EngineError::InvalidName {
                    name: "Bad Name".to_string(),
                    reason: ktesio_engine::NameError::BadFirstChar,
                },
                ExitCode::Usage,
            ),
            (
                "NotFound",
                EngineError::NotFound {
                    name: "ghost".to_string(),
                },
                ExitCode::NotFound,
            ),
            (
                "LaunchFailed",
                EngineError::LaunchFailed {
                    name: "svc".to_string(),
                    detail: "exec failed".to_string(),
                },
                ExitCode::General,
            ),
        ];

        for (what, engine_error, expected) in cases {
            let mapped = map_engine_error(engine_error);
            assert_eq!(
                classify(mapped.as_ref()),
                expected,
                "`EngineError::{what}` must still map to a diagnostic that classifies \
                 as {expected:?} — the exit code is a FROZEN v1 compatibility surface \
                 (PRD §7). Rendered diagnostic: {mapped}",
            );
        }
    }

    #[test]
    fn registry_error_mapper_arms_preserve_their_documented_exit_codes() {
        // The `map_error` half of the same seam (fix pass, H1): the registry-shaped
        // diagnostics `register`/`remove`/`show --json`/`usage` raise.
        use crate::exit_code::{classify, ExitCode};

        let cases: Vec<(&str, RegistryError, ExitCode)> = vec![
            (
                "InvalidName",
                RegistryError::InvalidName {
                    name: "Bad Name".to_string(),
                    reason: ktesio_engine::NameError::BadFirstChar,
                },
                ExitCode::Usage,
            ),
            (
                "DuplicateName",
                RegistryError::DuplicateName {
                    name: "alpha".to_string(),
                },
                ExitCode::Usage,
            ),
            (
                "NotFound",
                RegistryError::NotFound {
                    name: "ghost".to_string(),
                },
                ExitCode::NotFound,
            ),
            (
                "RunningRequiresForce",
                RegistryError::RunningRequiresForce {
                    name: "svc".to_string(),
                },
                ExitCode::InvalidState,
            ),
            (
                // Story 5-1 (DC-4): the hot-swap guard joins code 4 — no new
                // exit-code number was minted.
                "MemoryBackingHotSwap",
                RegistryError::MemoryBackingHotSwap {
                    name: "svc".to_string(),
                    state: "running".to_string(),
                },
                ExitCode::InvalidState,
            ),
            (
                // Story 5-1 (DC-4): a kind conflict is likewise invalid state.
                "MemoryBackingKindConflict",
                RegistryError::MemoryBackingKindConflict {
                    name: "svc".to_string(),
                    attached: "filesystem".to_string(),
                    requested: "native".to_string(),
                },
                ExitCode::InvalidState,
            ),
        ];

        for (what, registry_error, expected) in cases {
            let mapped = map_error(registry_error);
            assert_eq!(
                classify(mapped.as_ref()),
                expected,
                "`RegistryError::{what}` must still classify as {expected:?} \
                 (frozen exit-code surface, PRD §7). Rendered: {mapped}",
            );
        }
    }

    #[test]
    fn a_malformed_name_is_a_usage_error_not_a_not_found() {
        // Fix pass (M2): `usage <name>` and `show --json` resolve the instance by
        // scanning `fleet()`, so a malformed name used to be reported as NotFound
        // (3) while `logs`/`stop`/`show` reported the same input as a usage error
        // (2). `validate_instance_name` is what makes it uniformly 2.
        use crate::exit_code::{classify, ExitCode};

        for bad in ["Bad Name", "", "UPPER", "-leading-dash", "has space"] {
            let err = validate_instance_name(bad).expect_err("malformed name must be rejected");
            assert_eq!(
                classify(err.as_ref()),
                ExitCode::Usage,
                "`{bad}` must be a usage error (2); rendered: {err}",
            );
        }
        // A well-formed name passes through untouched (no existence check here —
        // that stays the caller's Fleet lookup).
        for good in ["alpha", "a", "a-b_c9", "0start"] {
            assert!(validate_instance_name(good).is_ok(), "`{good}` is valid");
        }
    }

    #[test]
    fn map_engine_error_renders_the_stop_unconfirmed_diagnostic() {
        // Fix pass (review of #80 follow-up — the CRITICAL finding)
        // coverage-closer: mirrors `map_engine_error_renders_the_
        // interaction_timed_out_diagnostic`'s reasoning exactly —
        // `EngineError::StopUnconfirmed` needs a process GENUINELY stuck in
        // an OS-level uninterruptible I/O wait (disk exhaustion), which a
        // full CLI-process-spawning test cannot safely or deterministically
        // induce (and must never attempt against the real host disk — see
        // the fix pass's mandatory ramdisk-only empirical proof). So this
        // diagnostic's rendering is unit-tested directly here instead.
        let err = map_engine_error(EngineError::StopUnconfirmed {
            name: "stuck".to_string(),
            timeout_secs: 5,
        });
        let msg = err.to_string();
        assert!(msg.contains("stuck"), "names the instance: {msg}");
        assert!(msg.contains('5'), "names the bound: {msg}");
        assert!(
            msg.contains("SIGKILL") && msg.contains("I/O wait"),
            "names the honest likely cause, not a generic failure: {msg}"
        );
        assert!(
            msg.contains("'stopping'"),
            "states the instance's actual (non-terminal) state honestly: {msg}"
        );
        assert!(
            msg.contains("kt agent stop stuck"),
            "points at retrying stop rather than a generic remediation: {msg}"
        );
    }

    #[test]
    fn map_error_covers_story_1_3_adapter_variants() {
        let unknown = map_error(RegistryError::UnknownAdapterKind {
            kind: "nope".into(),
        });
        assert!(unknown.to_string().contains("Unknown adapter kind 'nope'"));
        assert!(unknown.to_string().contains("--manifest"));

        let not_found = map_error(RegistryError::ManifestNotFound {
            path: "/x/adapter.toml".into(),
        });
        assert!(not_found.to_string().contains("/x/adapter.toml"));

        // F4: unreadable is its own diagnostic with an existence/readability
        // remediation, NOT the "fix the section" message.
        let unreadable = map_error(RegistryError::ManifestUnreadable {
            path: "/x/adapter.toml".into(),
            detail: "permission denied".into(),
        });
        let unreadable_msg = unreadable.to_string();
        assert!(unreadable_msg.contains("Could not read"));
        assert!(unreadable_msg.contains("permission denied"));
        assert!(unreadable_msg.contains("readable"));
        assert!(
            !unreadable_msg.contains("Fix the named section"),
            "unreadable must not claim a section fix"
        );

        let invalid = map_error(RegistryError::ManifestInvalid {
            path: "/x/adapter.toml".into(),
            detail: "missing the required `[metering]` section".into(),
        });
        assert!(invalid.to_string().contains("[metering]"));

        let no_metering = map_error(RegistryError::NoMeteringSource {
            adapter: "demo".into(),
        });
        assert!(no_metering.to_string().contains("[metering]"));
        assert!(no_metering.to_string().contains("self-reported"));

        let no_caps = map_error(RegistryError::NoCapabilities {
            adapter: "demo".into(),
        });
        assert!(no_caps.to_string().contains("[capabilities]"));
    }

    #[test]
    fn adapter_arg_from_flags_resolves_and_requires_one() {
        assert_eq!(
            AdapterArg::from_flags(Some("mock".into()), None).unwrap(),
            AdapterArg::Kind("mock".into())
        );
        assert_eq!(
            AdapterArg::from_flags(None, Some("/x".into())).unwrap(),
            AdapterArg::Manifest("/x".into())
        );
        // --kind wins if both somehow arrive (clap prevents this normally).
        assert_eq!(
            AdapterArg::from_flags(Some("mock".into()), Some("/x".into())).unwrap(),
            AdapterArg::Kind("mock".into())
        );
        // Neither is an error.
        assert!(AdapterArg::from_flags(None, None).is_err());
    }

    #[test]
    fn adapter_arg_to_ref_maps_both_kinds() {
        assert_eq!(
            AdapterArg::Kind("mock".into()).to_ref(),
            AdapterRef::Native("mock".into())
        );
        assert_eq!(
            AdapterArg::Manifest("/x/dir".into()).to_ref(),
            AdapterRef::Manifest(std::path::PathBuf::from("/x/dir"))
        );
    }

    fn sample_fleet_entry(name: &str) -> FleetEntry {
        FleetEntry {
            name: ktesio_engine::InstanceName::new(name).unwrap(),
            kind: "mock".to_string(),
            state: ktesio_engine::LifecycleState::Registered,
            restart_count: 0,
            restart_policy: ktesio_engine::RestartPolicy::OnFailure,
            failed_cause: None,
            budget: None,
            usage: UsageView::new(
                ktesio_engine::UsageTotals::zero(),
                ktesio_engine::UsageTotals::zero(),
            ),
            metering_source: "self-reported".to_string(),
            agent_home: format!("/x/agents/{name}"),
        }
    }

    #[test]
    fn fleet_json_emits_versioned_document_with_budget_seed_and_real_usage() {
        // The `list --json` document is a versioned FleetListing whose per-entry
        // `budget` is the honest JSON null seed (never 0) while `usage` is a real
        // token-totals object (story 3-1). Pure — no engine.
        let doc = fleet_json(&FleetListing::new(vec![sample_fleet_entry("alpha")])).unwrap();
        let value: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(
            value["schema_version"],
            serde_json::json!(FLEET_SCHEMA_VERSION)
        );
        let entry = &value["instances"][0];
        assert_eq!(entry["name"], serde_json::json!("alpha"));
        assert_eq!(entry["budget"], serde_json::Value::Null);
        // usage is a real object with zero token totals (not null).
        assert!(entry["usage"].is_object(), "{entry}");
        assert_eq!(
            entry["usage"]["cumulative_input_tokens"],
            serde_json::json!(0)
        );
        assert_eq!(entry["metering_source"], serde_json::json!("self-reported"));
        // Story 3-5: the document carries a top-level `totals` object (an all-zero
        // never-metered Fleet → zero tokens, dollars absent).
        assert!(value["totals"].is_object(), "{value}");
        assert_eq!(value["totals"]["total_input_tokens"], serde_json::json!(0));
    }

    #[test]
    fn fleet_json_on_empty_is_a_valid_empty_array() {
        // AC9: an empty Fleet serializes as a valid empty `instances` array + a
        // zero/absent-dollars `totals` (story 3-5).
        let doc = fleet_json(&FleetListing::new(vec![])).unwrap();
        let value: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(value["instances"], serde_json::json!([]));
        assert_eq!(value["totals"]["total_output_tokens"], serde_json::json!(0));
    }

    #[test]
    fn show_json_wraps_one_entry_with_the_shared_schema_version() {
        // `show --json` is { schema_version, instance: <FleetEntry> } — the SAME
        // schema version as list --json (AD-14: one schema). `budget` is the null
        // seed; `usage` is a real token-totals object (story 3-1).
        let doc = show_json(sample_fleet_entry("web-1")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(
            value["schema_version"],
            serde_json::json!(FLEET_SCHEMA_VERSION)
        );
        assert_eq!(value["instance"]["name"], serde_json::json!("web-1"));
        assert_eq!(value["instance"]["budget"], serde_json::Value::Null);
        assert!(value["instance"]["usage"].is_object(), "{value}");
        assert_eq!(
            value["instance"]["metering_source"],
            serde_json::json!("self-reported")
        );
    }

    #[test]
    fn serialize_error_wraps_into_an_agent_io_diagnostic() {
        // The defense-in-depth serialization-failure wrapper names what failed.
        let err = serde_json::from_str::<i32>("not-json").unwrap_err();
        let wrapped = serialize_error("Fleet", err);
        assert!(wrapped
            .to_string()
            .contains("Failed to serialize the Fleet"));
    }

    // ---- Story 3-5: the Fleet-wide total footer + the per-instance scope render ----

    /// A metered entry with the given cumulative tokens and, when `dollars` is `Some`,
    /// a derived cost + `estimated` label (a Rate); `None` = a no-Rate instance.
    fn metered_fleet_entry(
        name: &str,
        input: u64,
        output: u64,
        dollars: Option<Micros>,
    ) -> FleetEntry {
        let mut entry = sample_fleet_entry(name);
        let base = UsageView::new(
            ktesio_engine::UsageTotals {
                input_tokens: input,
                output_tokens: output,
            },
            ktesio_engine::UsageTotals::zero(),
        );
        entry.usage = match dollars {
            Some(cost) => base.with_dollars(cost, Micros::ZERO, EstimateLabel::Estimated),
            None => base,
        };
        entry
    }

    #[test]
    fn fleet_footer_all_rated_shows_a_labeled_complete_total() {
        // A complete dollar total (every metered instance priced): `≈ $X.XX
        // (estimated)`, tokens summed, NO partial note.
        let totals = FleetTotals::from_entries(&[
            metered_fleet_entry("a", 1_000_000, 0, Some(Micros(3_000_000))),
            metered_fleet_entry("b", 0, 1_000_000, Some(Micros(15_000_000))),
        ]);
        let footer = fleet_total_footer(&totals);
        assert!(footer.contains("in 1000000 / out 1000000"), "{footer}");
        // $18.00 total, labeled estimated, an aggregate `≈`, no partial note.
        assert!(footer.contains("≈ $18.00 (estimated)"), "{footer}");
        assert!(!footer.contains("unpriced"), "complete total: {footer}");
    }

    #[test]
    fn fleet_footer_partial_total_says_so_with_the_estimate_label() {
        // A metered-but-unpriced instance makes the dollar total a LOWER BOUND — the
        // footer says so (AC5/SM-C3) AND keeps the estimate label (FR-23 — no unlabeled
        // dollar; the label lives here, not in a truncatable cell). With exactly ONE
        // unpriced row the note NAMES the count in the singular (AC7).
        let totals = FleetTotals::from_entries(&[
            metered_fleet_entry("rated", 1_000_000, 0, Some(Micros(3_000_000))),
            metered_fleet_entry("unpriced", 500, 0, None),
        ]);
        assert_eq!(totals.unpriced_count, 1, "one metered-unpriced row");
        let footer = fleet_total_footer(&totals);
        assert!(
            footer.contains("≈ $3.00"),
            "the lower-bound value: {footer}"
        );
        assert!(footer.contains("estimated"), "label survives: {footer}");
        // The count is NAMED, singular for one instance (AC7 nicety).
        assert!(
            footer.contains("1 instance unpriced"),
            "singular count named: {footer}"
        );
        assert!(
            !footer.contains("instances unpriced"),
            "singular, not plural, for one: {footer}"
        );
    }

    #[test]
    fn fleet_footer_partial_total_names_the_plural_count_of_unpriced_instances() {
        // TWO metered-but-unpriced instances (plus a Rate'd one that makes the total a
        // priced lower bound): the footer NAMES the exact count in the plural — "2
        // instances unpriced" — so the reader knows the basis of the lower bound (AC7).
        let totals = FleetTotals::from_entries(&[
            metered_fleet_entry("rated", 1_000_000, 0, Some(Micros(3_000_000))),
            metered_fleet_entry("unpriced-a", 500, 0, None),
            metered_fleet_entry("unpriced-b", 250, 0, None),
        ]);
        assert_eq!(totals.unpriced_count, 2, "two metered-unpriced rows");
        let footer = fleet_total_footer(&totals);
        assert!(
            footer.contains("2 instances unpriced"),
            "plural count named: {footer}"
        );
    }

    #[test]
    fn fleet_footer_no_rate_shows_tokens_and_an_honest_dash_never_zero_dollars() {
        // No instance has a Rate ⇒ the token total + an honest `—` marker, NEVER a
        // fabricated `$0.00` (AC4/AC5). The tokens still sum (zero-not-absent).
        let totals = FleetTotals::from_entries(&[
            metered_fleet_entry("a", 100, 200, None),
            metered_fleet_entry("idle", 0, 0, None),
        ]);
        let footer = fleet_total_footer(&totals);
        assert!(footer.contains("in 100 / out 200"), "{footer}");
        assert!(
            footer.contains('—'),
            "honest absent-dollars marker: {footer}"
        );
        assert!(
            !footer.contains("$0.00"),
            "never a fabricated zero: {footer}"
        );
        assert!(!footer.contains('$'), "no dollar figure at all: {footer}");
    }

    #[test]
    fn fleet_footer_empty_fleet_is_zeros_and_no_dollars() {
        // An empty Fleet: zero tokens + the honest no-dollars marker.
        let footer = fleet_total_footer(&FleetTotals::from_entries(&[]));
        assert!(footer.contains("in 0 / out 0"), "{footer}");
        assert!(footer.contains('—'), "{footer}");
    }

    #[test]
    fn usage_cell_show_surfaces_both_token_scopes_when_a_run_is_active() {
        // AC8: the wide `show` Usage cell shows BOTH scopes — cumulative AND current-Run
        // — when a Run has usage; a non-running/zero-run instance shows only cumulative
        // (no fabricated `run: in 0 / out 0`).
        let running = UsageView::new(
            ktesio_engine::UsageTotals {
                input_tokens: 100,
                output_tokens: 250,
            },
            ktesio_engine::UsageTotals {
                input_tokens: 40,
                output_tokens: 60,
            },
        );
        let shown = usage_cell_show(&running);
        assert!(shown.contains("in 100 / out 250"), "cumulative: {shown}");
        assert!(
            shown.contains("this run: in 40 / out 60"),
            "run scope: {shown}"
        );

        // No current run → cumulative only.
        let idle = UsageView::new(
            ktesio_engine::UsageTotals {
                input_tokens: 100,
                output_tokens: 250,
            },
            ktesio_engine::UsageTotals::zero(),
        );
        let shown = usage_cell_show(&idle);
        assert!(shown.contains("in 100 / out 250"), "{shown}");
        assert!(
            !shown.contains("this run"),
            "no fabricated run scope: {shown}"
        );
    }

    #[test]
    fn list_and_show_drive_the_engine_in_process_json_and_human() {
        // Cover the list()/show() success paths in-process (both --json and
        // human) against a real temp state dir. `open_engine()` reads
        // KTESIO_STATE_DIR; set it, seed one instance via the engine, then drive
        // each surface. They print to stdout (test noise, harmless) and must all
        // return Ok — proving the full CLI read path, not just the pure helpers.
        // Hold the shared env lock so this and the config_get driver test never
        // race on the process-global KTESIO_STATE_DIR.
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: guarded by STATE_DIR_ENV_LOCK; set the state dir the CLI resolves.
        unsafe {
            std::env::set_var("KTESIO_STATE_DIR", tmp.path());
        }
        {
            let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
            engine.blocking().register("demo", "mock").unwrap();
        }
        // Human + JSON list, human + JSON show — every success path.
        list(false).unwrap();
        list(true).unwrap();
        show("demo", false).unwrap();
        show("demo", true).unwrap();
        // Empty-Fleet JSON + human paths (a different state dir, no instances).
        let empty = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("KTESIO_STATE_DIR", empty.path());
        }
        list(true).unwrap();
        list(false).unwrap();
        unsafe {
            std::env::remove_var("KTESIO_STATE_DIR");
        }
    }

    #[test]
    fn show_json_uses_single_instance_lookup() {
        // AI-15: `show --json` resolves through the single-instance facade
        // method (`Engine::fleet_entry`) — an O(1) keyed registry lookup whose
        // errors surface directly — not a scan over the whole `fleet()`. A known
        // name yields the SAME versioned document (the row `list` would emit);
        // an unknown name surfaces the ENGINE's `RegistryError::NotFound`
        // (mapped to the exit-3 diagnostic), exactly like the human path — not a
        // silent Ok and not a usage error. Drives the real engine in-process,
        // mirroring the list/show cover test above (holds the shared env lock).
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: guarded by STATE_DIR_ENV_LOCK; set the state dir the CLI resolves.
        unsafe {
            std::env::set_var("KTESIO_STATE_DIR", tmp.path());
        }
        {
            let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
            engine.blocking().register("solo", "mock").unwrap();
        }
        // Known name: the full CLI --json path succeeds.
        show("solo", true).unwrap();
        // The facade method itself: one row for the known name, the engine's
        // typed NotFound for an unknown one (the exit-3 contract's source).
        {
            let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
            let facade = engine.blocking();
            let entry = facade.fleet_entry("solo").unwrap();
            assert_eq!(entry.name.as_str(), "solo", "one entry, the requested one");
            assert_eq!(entry.kind, "mock");
            let err = facade.fleet_entry("ghost").unwrap_err();
            assert!(
                matches!(err, RegistryError::NotFound { ref name } if name == "ghost"),
                "an unknown name must surface the engine's NotFound, got {err:?}"
            );
            // A malformed name is the registry's InvalidName (the exit-2 path),
            // NOT a synthesized NotFound — the M2 contract, now from the engine.
            let err = facade.fleet_entry("Bad Name").unwrap_err();
            assert!(
                matches!(err, RegistryError::InvalidName { .. }),
                "a malformed name must surface InvalidName, got {err:?}"
            );
        }
        unsafe {
            std::env::remove_var("KTESIO_STATE_DIR");
        }
    }

    /// Build a small effective config in-process for the config_json unit tests
    /// (a known key from the instance layer + an agent.* pass-through leaf from a
    /// weaker layer), reusing the engine's public resolver.
    fn sample_effective() -> EffectiveConfig {
        use ktesio_engine::{resolve, SourceLayer};
        let layers = [
            ConfigLayer::parse(
                SourceLayer::EngineDefault,
                "<e>",
                "agent = { legacy = \"on\" }\n",
            )
            .unwrap(),
            ConfigLayer::empty(),
            ConfigLayer::parse(SourceLayer::Instance, "<i>", "model = \"gpt-4\"\n").unwrap(),
            ConfigLayer::empty(),
        ];
        resolve(layers)
    }

    #[test]
    fn config_json_emits_versioned_document_with_source_and_unvalidated_per_leaf() {
        // Story 2-3 (AC4): the pure serializer emits a versioned document with
        // { key, value, source, unvalidated } per leaf. The known `model` key is
        // instance-sourced + validated; the agent.* leaf is engine-sourced +
        // unvalidated. Values render via the ONE display path (bare strings).
        let eff = sample_effective();
        let doc = config_json(&eff, None, &std::collections::BTreeMap::new()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(value["schema_version"], serde_json::json!(1));
        let entries = value["entries"].as_array().unwrap();

        let model = entries.iter().find(|e| e["key"] == "model").unwrap();
        assert_eq!(model["value"], serde_json::json!("gpt-4"));
        assert_eq!(model["source"], serde_json::json!("instance"));
        assert_eq!(model["unvalidated"], serde_json::json!(false));

        let legacy = entries.iter().find(|e| e["key"] == "agent.legacy").unwrap();
        assert_eq!(legacy["value"], serde_json::json!("on"));
        assert_eq!(legacy["source"], serde_json::json!("engine-default"));
        assert_eq!(legacy["unvalidated"], serde_json::json!(true));
    }

    #[test]
    fn config_json_single_key_emits_just_that_leaf() {
        // The single-key `config get <name> <key> --json` form emits exactly one
        // leaf, sourced + rendered identically to the whole-config form.
        let eff = sample_effective();
        let doc = config_json(&eff, Some("model"), &std::collections::BTreeMap::new()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&doc).unwrap();
        let entries = value["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["key"], serde_json::json!("model"));
        assert_eq!(entries[0]["source"], serde_json::json!("instance"));
    }

    #[test]
    fn config_json_value_matches_the_human_display_form() {
        // AC8: the --json value and the human value both render via the ONE display
        // path — a non-string scalar renders in the same inline form in both.
        use ktesio_engine::{resolve, SourceLayer};
        let eff = resolve([
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::parse(SourceLayer::Instance, "<i>", "n = 42\narr = [1, 2]\n").unwrap(),
            ConfigLayer::empty(),
        ]);
        let doc = config_json(&eff, None, &std::collections::BTreeMap::new()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&doc).unwrap();
        let entries = value["entries"].as_array().unwrap();
        let n = entries.iter().find(|e| e["key"] == "n").unwrap();
        // Same inline rendering as effective.value_display / the human table.
        assert_eq!(
            n["value"],
            serde_json::json!(eff.value_display("n").unwrap())
        );
        let arr = entries.iter().find(|e| e["key"] == "arr").unwrap();
        assert_eq!(
            arr["value"],
            serde_json::json!(eff.value_display("arr").unwrap())
        );
    }

    #[test]
    fn table_prefix_diagnostic_names_the_child_leaves_sorted_and_capped() {
        // AI-25 (story 11-4): a valueless key that IS a prefix of effective leaves
        // yields a diagnostic that NAMES the children (sorted — the engine map
        // iterates deterministically), capped at [`TABLE_PREFIX_MAX_CHILDREN`] with
        // an elision count. A key that is neither a value nor a prefix yields None
        // (the plain unknown-key diagnostic). Built through the engine's public
        // resolver, like the config_json unit tests.
        use ktesio_engine::{resolve, SourceLayer};
        let resolve_instance = |toml_text: &str| {
            resolve([
                ConfigLayer::empty(),
                ConfigLayer::empty(),
                ConfigLayer::parse(SourceLayer::Instance, "<i>", toml_text).unwrap(),
                ConfigLayer::empty(),
            ])
        };

        // Full list when the family fits under the cap: the child leaf is named and
        // the remediation suggests a concrete `get`.
        let eff_budget = resolve_instance("budget = { breach_action = \"pause\" }\n");
        let message =
            table_prefix_diagnostic("demo", "budget", &eff_budget).expect("budget is a prefix");
        assert!(
            message.contains("budget.breach_action"),
            "the child leaf is named: {message}"
        );
        assert!(
            message.contains("kt agent config get demo budget.breach_action"),
            "the remediation suggests a concrete get: {message}"
        );
        assert!(
            !message.contains('…'),
            "nothing elided under the cap: {message}"
        );

        // An INTERMEDIATE prefix (a depth-2 key over a NESTED table — the
        // `budget.tokens` table makes `budget.tokens.per_run` a leaf) takes
        // the same path (review loop 1): the leaves under `budget.tokens.`
        // are named.
        let eff_deep = resolve_instance("[budget.tokens]\nper_run = 1000\n");
        let message = table_prefix_diagnostic("demo", "budget.tokens", &eff_deep)
            .expect("budget.tokens is an intermediate prefix");
        assert!(
            message.contains("budget.tokens.per_run"),
            "the depth-2 prefix names its leaf: {message}"
        );

        // Over the cap: exactly TABLE_PREFIX_MAX_CHILDREN named, then the elision.
        let nine_agent_leaves: String = (1..=9)
            .map(|i| format!("k{i} = \"v\"\n"))
            .collect::<Vec<_>>()
            .join("");
        let eff_agent = resolve_instance(&format!("[agent]\n{nine_agent_leaves}"));
        let message =
            table_prefix_diagnostic("demo", "agent", &eff_agent).expect("agent is a prefix");
        assert!(
            message.contains("… (+1 more)"),
            "9 leaves cap at 8 named + the elision count: {message}"
        );
        // The rendered list holds EXACTLY the capped number of children (k9 is
        // elided), then the ellipsis; the trailing suggestion names only k1.
        assert!(
            message.contains(
                "agent.k1, agent.k2, agent.k3, agent.k4, agent.k5, agent.k6, \
                 agent.k7, agent.k8, … (+1 more)"
            ),
            "exactly the capped number of children named, in sorted order: {message}"
        );
        assert!(
            !message.contains("agent.k9"),
            "the ninth leaf is elided, never named: {message}"
        );

        // Not a prefix at all → the caller's plain unknown-key diagnostic.
        assert_eq!(table_prefix_diagnostic("demo", "modle", &eff_budget), None);
    }

    #[test]
    fn config_get_drives_the_engine_in_process_human_and_json() {
        // Cover the config_get() success paths in-process (human + --json, whole +
        // single-key) against a real temp state dir, mirroring the list/show cover
        // test. Prints to stdout (harmless test noise) and must all return Ok.
        // Hold the shared env lock: this test mutates KTESIO_STATE_DIR, which other
        // in-process engine-driver tests also touch.
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: guarded by STATE_DIR_ENV_LOCK; set the state dir the CLI resolves.
        unsafe {
            std::env::set_var("KTESIO_STATE_DIR", tmp.path());
        }
        {
            let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
            let blocking = engine.blocking();
            blocking.register("demo", "mock").unwrap();
            blocking.set_config("demo", "model", "gpt-4").unwrap();
            blocking.set_config("demo", "agent.flag", "on").unwrap();
        }
        // Whole-config: human + JSON. Single-key: human + JSON.
        config_get("demo", None, false, false).unwrap();
        config_get("demo", None, true, false).unwrap();
        config_get("demo", Some("model"), false, false).unwrap();
        config_get("demo", Some("model"), true, false).unwrap();
        // A not-set key is a non-zero (Err) diagnostic in both modes.
        assert!(config_get("demo", Some("missing"), false, false).is_err());
        assert!(config_get("demo", Some("missing"), true, false).is_err());
        unsafe {
            std::env::remove_var("KTESIO_STATE_DIR");
        }
    }

    #[test]
    fn config_get_reveal_overlays_resolved_cleartext_in_process() {
        // Story 2-4 (AC-C): cover the `--reveal` paths in-process — the reveal
        // overlay (`leaf_display`, `config_json` + `render_effective_config` with a
        // non-empty overlay) and the `leaf_display` fallback. Sets a secret env var
        // so the engine resolves it. Holds the shared env lock (mutates
        // KTESIO_STATE_DIR + the secret env var).
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let sentinel = "s3cr3t-inproc-reveal";
        let secret_key = "KTESIO_INPROC_REVEAL_KEY";
        // SAFETY: guarded by STATE_DIR_ENV_LOCK.
        unsafe {
            std::env::set_var("KTESIO_STATE_DIR", tmp.path());
            std::env::set_var(secret_key, sentinel);
        }
        {
            let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
            let blocking = engine.blocking();
            blocking.register("sec", "mock").unwrap();
            blocking
                .set_config("sec", "model", &format!("secret:{secret_key}"))
                .unwrap();
            blocking
                .set_config("sec", "agent.plain", "visible")
                .unwrap();

            // reveal_secrets returns the resolved cleartext for the secret leaf only.
            let revealed = blocking
                .reveal_secrets("sec", ktesio_engine::ConfigLayer::empty())
                .unwrap();
            assert_eq!(revealed.get("model").map(String::as_str), Some(sentinel));
            assert!(!revealed.contains_key("agent.plain"), "only secret leaves");
        }
        // The reveal render paths run without error (whole + single-key, human +
        // JSON), and the default (masked) paths too.
        config_get("sec", None, true, true).unwrap(); // --json --reveal (whole)
        config_get("sec", None, false, true).unwrap(); // human --reveal (whole)
        config_get("sec", Some("model"), true, true).unwrap(); // single-key reveal
        config_get("sec", Some("model"), false, true).unwrap(); // single-key human reveal
        config_get("sec", None, true, false).unwrap(); // default masked --json

        // leaf_display: revealed overlay wins; absent key falls back to display().
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let eff = engine
            .blocking()
            .effective_config("sec", ktesio_engine::ConfigLayer::empty())
            .unwrap();
        let mut overlay = std::collections::BTreeMap::new();
        overlay.insert("model".to_string(), sentinel.to_string());
        assert_eq!(leaf_display(&eff, "model", &overlay), sentinel);
        // A non-overlaid secret leaf falls back to the masked display().
        assert_eq!(
            leaf_display(&eff, "model", &std::collections::BTreeMap::new()),
            ktesio_engine::SECRET_MASK
        );

        unsafe {
            std::env::remove_var("KTESIO_STATE_DIR");
            std::env::remove_var(secret_key);
        }
    }

    #[test]
    fn render_capabilities_handles_empty_and_nonempty() {
        use ktesio_engine::{Capability, OsId, SupportLevel};
        // Empty projection: prints the "none declared" info line without panic.
        let empty = EffectiveCapabilities {
            os: OsId::current(),
            entries: vec![],
        };
        render_capabilities("demo", &empty);

        // Non-empty: renders a table without panic.
        let full = EffectiveCapabilities {
            os: OsId::current(),
            entries: vec![
                (Capability::Pause, SupportLevel::Guaranteed),
                (Capability::Interaction, SupportLevel::BestEffort),
            ],
        };
        render_capabilities("demo", &full);
    }

    // ---- `budget_cell`: the FR-23 / AD-8 budget rendering contract ----

    /// A [`BudgetView`] with every scope explicitly chosen, so each test states
    /// exactly which dimensions it is asserting about (no hidden defaults).
    #[allow(clippy::too_many_arguments)]
    fn budget_view(
        per_run: Option<(u64, u64)>,
        cumulative: Option<(u64, u64)>,
        per_run_dollars: Option<(Micros, Micros)>,
        cumulative_dollars: Option<(Micros, Micros)>,
        estimate_label: Option<EstimateLabel>,
        breach_action: ktesio_engine::BreachAction,
    ) -> BudgetView {
        BudgetView {
            per_run_limit: per_run.map(|(limit, _)| limit),
            per_run_remaining: per_run.map(|(_, remaining)| remaining),
            cumulative_limit: cumulative.map(|(limit, _)| limit),
            cumulative_remaining: cumulative.map(|(_, remaining)| remaining),
            per_run_cost_cap: per_run_dollars.map(|(cap, _)| cap),
            per_run_dollars_remaining: per_run_dollars.map(|(_, remaining)| remaining),
            cumulative_cost_cap: cumulative_dollars.map(|(cap, _)| cap),
            cumulative_dollars_remaining: cumulative_dollars.map(|(_, remaining)| remaining),
            estimate_label,
            breach_action,
        }
    }

    #[test]
    fn budget_cell_renders_the_per_run_scope_before_the_cumulative_scope() {
        // AC9: both token scopes render as `remaining/ceiling tok`, per-run FIRST
        // (the narrower guardrail leads), comma-joined, with the Breach Action in
        // trailing parens. ORDER is part of the surface: `list` truncates from the
        // right, so a reordering would silently drop the per-run scope — the one a
        // user is most likely watching mid-run — instead of the cumulative one.
        let budget = budget_view(
            Some((100, 80)),
            Some((500, 380)),
            None,
            None,
            None,
            ktesio_engine::BreachAction::Pause,
        );

        assert_eq!(
            budget_cell(Some(&budget), DollarLabel::Inline),
            "run 80/100 tok, cum 380/500 tok (pause)"
        );
    }

    #[test]
    fn budget_cell_keeps_the_estimate_label_out_of_the_truncatable_list_cell() {
        // FR-23 / AD-8: no unlabeled dollar figure reaches a human, and the label
        // must survive truncation. `show` has a wide Value column, so the
        // qualifier rides INLINE in the cell; `list`'s Budget column is narrow and
        // truncatable, so the SAME cell must render the cap BARE and the qualifier
        // lives in the column header instead. Asserting both surfaces together is
        // what makes this a contract test: dropping the label from `Inline` OR
        // leaking it into `InHeader` (where truncation could strip it) both fail.
        let budget = budget_view(
            None,
            None,
            Some((Micros(500_000), Micros(200_000))),
            Some((Micros(2_000_000), Micros(1_250_000))),
            Some(EstimateLabel::Estimated),
            ktesio_engine::BreachAction::Stop,
        );

        assert_eq!(
            budget_cell(Some(&budget), DollarLabel::Inline),
            "run $0.20/$0.50 (estimated), cum $1.25/$2.00 (estimated) (stop)"
        );
        assert_eq!(
            budget_cell(Some(&budget), DollarLabel::InHeader),
            "run $0.20/$0.50, cum $1.25/$2.00 (stop)"
        );
        // …and the qualifier the InHeader cell omits really is carried by the
        // header that surface uses, so it is displaced, never lost.
        assert!(
            BUDGET_LIST_HEADER.contains("est. $"),
            "{BUDGET_LIST_HEADER}"
        );
    }

    #[test]
    fn usage_cell_keeps_the_estimate_label_out_of_the_truncatable_list_cell() {
        // AI-45 (story 11-4): the Usage column gets the SAME FR-23/AD-8 treatment
        // the Budget column already has. The narrow `list` Usage column truncates
        // with `…`, so it renders the derived dollar BARE (the estimate qualifier
        // lives in the column header); the wide `show` cell keeps the inline label.
        // Asserting both forms together is the contract test: dropping the header
        // qualifier ([`USAGE_LIST_HEADER`]) OR leaking the label into the InHeader
        // cell (where truncation could strip it) both fail.
        let usage = UsageView::new(
            ktesio_engine::UsageTotals {
                input_tokens: 120,
                output_tokens: 340,
            },
            ktesio_engine::UsageTotals::zero(),
        )
        .with_dollars(Micros(300_000), Micros::ZERO, EstimateLabel::Estimated);

        assert_eq!(
            usage_cell(&usage, DollarLabel::Inline),
            "in 120 / out 340 · $0.30 (estimated)"
        );
        assert_eq!(
            usage_cell(&usage, DollarLabel::InHeader),
            "in 120 / out 340 · $0.30"
        );
        // …and the qualifier the InHeader cell omits really is carried by the
        // header that surface uses, so it is displaced, never lost.
        assert!(USAGE_LIST_HEADER.contains("est. $"), "{USAGE_LIST_HEADER}");
    }

    #[test]
    fn budget_cell_omits_the_dollar_dimension_when_no_rate_is_configured() {
        // AC-B: a Cost Cap with no Rate is INERT — the engine leaves the dollar
        // fields absent, and the cell must then show tokens only. It must NEVER
        // fabricate a `$0.00` cap out of the missing Rate.
        let budget = budget_view(
            None,
            Some((500, 380)),
            None,
            None,
            None,
            ktesio_engine::BreachAction::Pause,
        );

        let cell = budget_cell(Some(&budget), DollarLabel::Inline);
        assert_eq!(cell, "cum 380/500 tok (pause)");
        assert!(!cell.contains('$'), "no Rate must mean no dollar figure");
    }

    #[test]
    fn budget_cell_with_no_configured_scope_still_names_the_breach_action() {
        // Defensive arm: a BudgetView that reached the renderer with an action but
        // no scope must still say what would happen on breach, rather than render
        // an empty cell that reads as "no guardrail". An absent budget is a
        // DIFFERENT thing and keeps the honest `—` seed — assert both so the two
        // cannot be conflated.
        let actionless = budget_view(
            None,
            None,
            None,
            None,
            None,
            ktesio_engine::BreachAction::Warn,
        );

        assert_eq!(
            budget_cell(Some(&actionless), DollarLabel::Inline),
            "(warn)"
        );
        assert_eq!(
            budget_cell(None, DollarLabel::Inline),
            FleetEntry::METERING_SEED_CELL
        );
    }

    // ---- Log streaming: broken-pipe discipline + the follow exit note ----

    #[test]
    fn a_broken_stdout_pipe_stops_cleanly_while_a_real_io_failure_still_diagnoses() {
        // Fix-pass M1 contract: `kt agent logs --json | head -5` must exit 0. Rust
        // ignores SIGPIPE and `println!` PANICS on a write error, which produced
        // exit 101 — a code outside the FROZEN table in docs/commands.md. The
        // BrokenPipe arm is what keeps that promise, and it must NOT swallow other
        // write failures (a full disk when stdout is redirected to a file is a
        // real error the user needs to see), so both halves are asserted here.
        use crate::exit_code::{classify, ExitCode};

        let closed = classify_stdout_write(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "consumer went away",
        ))
        .expect("a closed consumer is a clean stop, not an error");
        assert_eq!(closed, EmitOutcome::PipeClosed);

        let failed = classify_stdout_write(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "no space left on device",
        ))
        .expect_err("a genuine write failure must stay an error");
        assert!(
            failed.downcast_ref::<AgentIo>().is_some(),
            "a real stdout write failure must surface as AgentIo: {failed}"
        );
        assert!(failed.to_string().contains("no space left on device"));
        assert_eq!(classify(failed.as_ref()), ExitCode::General);
    }

    #[test]
    fn the_follow_exit_note_tells_a_paused_instance_apart_from_a_terminal_one() {
        // AC-C: `--follow` ends with a NOTE, never an error, when the instance is
        // not running — but the two cases mean different things to the reader. A
        // paused instance can still produce output later, so its note must point at
        // resuming; every terminal state must say the stream is over. Collapsing
        // the two would tell a user watching a paused agent that nothing more will
        // ever arrive, which is false.
        let paused = follow_exit_note("demo", LifecycleState::Paused);
        assert_eq!(
            paused,
            "Agent Instance 'demo' is paused; no further output until it is resumed."
        );

        for terminal in [
            LifecycleState::Stopped,
            LifecycleState::Failed,
            LifecycleState::Registered,
        ] {
            let note = follow_exit_note("demo", terminal);
            assert_eq!(
                note,
                format!("Agent Instance 'demo' is {terminal}; no further output will arrive.")
            );
            assert!(
                note.contains("no further output will arrive"),
                "{terminal}: {note}"
            );
        }
    }

    // ---- The remaining mapper arms (the "middle link" of the exit-code chain) ----

    #[test]
    fn config_error_mapper_arms_preserve_their_documented_exit_codes() {
        // The `map_config_error` half of the seam that
        // `every_engine_error_mapper_arm_preserves_its_documented_exit_code`
        // closes for `map_engine_error`: nothing pinned that a config-surface
        // condition PRODUCES the diagnostic its exit code is documented against.
        // A bad name is a USAGE error (2) — scripts branch on it to tell "you
        // typed it wrong" from "it isn't there" — while a store failure is a
        // general/internal error (1). Retargeting either arm changes an asserted
        // number here.
        use crate::exit_code::{classify, ExitCode};

        let invalid = map_config_error(ConfigError::InvalidName {
            name: "Bad Name".to_string(),
            reason: "must not contain spaces".to_string(),
        });
        assert_eq!(classify(invalid.as_ref()), ExitCode::Usage);
        assert!(invalid.to_string().contains("Bad Name"));
        // The remediation must state the rule, not just that it was broken.
        assert!(invalid.to_string().contains("^[a-z0-9][a-z0-9_-]*$"));

        let store = map_config_error(ConfigError::Store {
            name: "demo".to_string(),
            detail: "database is locked".to_string(),
        });
        assert_eq!(classify(store.as_ref()), ExitCode::General);
        assert!(
            store.downcast_ref::<AgentStore>().is_some(),
            "a store failure must surface as AgentStore: {store}"
        );
        assert!(store.to_string().contains("database is locked"));
    }

    #[test]
    fn a_failed_secret_reveal_diagnoses_without_leaking_the_secret_value() {
        // FR-14 (story 2-4, AC-C/AC11): `config get --reveal` re-resolves secrets
        // LIVE, and a resolution failure is a stderr diagnostic — never a crash and
        // never a leak. The engine's detail is contracted to name the NAME and the
        // resolvers tried but no value; `kt` appends the remediation. This test
        // pins the leak half explicitly: a value that happened to be in scope must
        // not appear in the rendered diagnostic, and the message must tell the user
        // the two places a secret can come from.
        let mapped = map_config_error(ConfigError::SecretReveal {
            detail: "secret 'OPENAI_API_KEY' not found (tried: environment, secrets file)"
                .to_string(),
        });

        let rendered = mapped.to_string();
        assert!(rendered.contains("OPENAI_API_KEY"), "{rendered}");
        assert!(
            !rendered.contains("sk-live-"),
            "a reveal failure must never echo a secret value: {rendered}"
        );
        // Both documented resolvers are named, so the user knows where to put it.
        assert!(rendered.contains("environment variable"), "{rendered}");
        assert!(rendered.contains("secrets file"), "{rendered}");
        assert!(rendered.contains("--reveal"), "{rendered}");
    }

    #[test]
    fn a_snapshot_write_failure_names_the_path_and_says_nothing_started() {
        // NFR-1: an I/O diagnostic must name WHAT failed, WHERE, and what to do.
        // The effective-config snapshot is written on the way into `start`, so a
        // failure leaves the instance in its prior state — the message must say so
        // ("start it again"), otherwise a user cannot tell whether a half-started
        // instance is now running. Both mappers raise this condition, so both are
        // pinned.
        use crate::exit_code::{classify, ExitCode};

        for mapped in [
            map_error(RegistryError::SnapshotWrite {
                name: "demo".to_string(),
                path: "/x/agents/demo/config.snapshot.toml".to_string(),
                detail: "permission denied".to_string(),
            }),
            map_engine_error(EngineError::Snapshot {
                name: "demo".to_string(),
                path: "/x/agents/demo/config.snapshot.toml".to_string(),
                detail: "permission denied".to_string(),
            }),
        ] {
            let rendered = mapped.to_string();
            assert!(rendered.contains("demo"), "{rendered}");
            assert!(
                rendered.contains("/x/agents/demo/config.snapshot.toml"),
                "{rendered}"
            );
            assert!(rendered.contains("permission denied"), "{rendered}");
            assert!(rendered.contains("start it again"), "{rendered}");
            assert!(
                mapped.downcast_ref::<AgentIo>().is_some(),
                "a snapshot write failure must surface as AgentIo: {rendered}"
            );
            assert_eq!(classify(mapped.as_ref()), ExitCode::General);
        }
    }

    #[test]
    fn the_remaining_engine_error_arms_map_to_their_documented_diagnostic_types() {
        // Completes `every_engine_error_mapper_arm_preserves_its_documented_exit_code`
        // over the arms it left out. These all classify as `1` today, so the exit
        // code alone would NOT catch a severed arm (the catch-all also returns 1) —
        // the DIAGNOSTIC TYPE is therefore the assertion that bites. The type is
        // the contract surface: `exit_code.rs`'s frozen table is written in terms
        // of these diagnostics, so re-pointing an arm at a different one silently
        // moves that condition to a different row of the table (and, the moment any
        // of them is given a non-1 code, to a different exit status).
        use crate::exit_code::{classify, ExitCode};

        let unresolved = map_engine_error(EngineError::AdapterUnresolved {
            name: "demo".to_string(),
            detail: "no adapter registered for kind 'ghost'".to_string(),
        });
        assert!(
            unresolved.downcast_ref::<AgentLaunchFailed>().is_some(),
            "an unresolved adapter is a LAUNCH failure: {unresolved}"
        );
        assert!(unresolved.to_string().contains("ghost"));

        let log = map_engine_error(EngineError::Log {
            name: "demo".to_string(),
            path: "/x/agents/demo/agent.log".to_string(),
            detail: "no space left on device".to_string(),
        });
        assert!(log.downcast_ref::<AgentIo>().is_some(), "{log}");
        assert!(log.to_string().contains("/x/agents/demo/agent.log"));
        assert!(log.to_string().contains("disk space"));

        // NFR-6: the metering detail is traffic-free, and the arm promises the
        // start was a no-op so the user knows nothing was half-applied.
        let metering = map_engine_error(EngineError::ObservedMetering {
            name: "demo".to_string(),
            detail: "could not bind the loopback listener".to_string(),
        });
        assert!(
            metering.downcast_ref::<AgentConfig>().is_some(),
            "{metering}"
        );
        assert!(metering.to_string().contains("Nothing was changed"));

        let backend = map_engine_error(EngineError::Backend {
            name: "demo".to_string(),
            source: ktesio_engine::ports::BackendError::Spawn {
                exec: "/usr/bin/ghost".to_string(),
                detail: "No such file or directory".to_string(),
            },
        });
        assert!(backend.downcast_ref::<AgentIo>().is_some(), "{backend}");
        assert!(backend.to_string().contains("/usr/bin/ghost"));

        let store = map_engine_error(EngineError::Store(
            ktesio_engine::ports::StoreError::CorruptRow {
                name: "demo".to_string(),
                detail: "unrecognized lifecycle state 'quantum'".to_string(),
            },
        ));
        assert!(store.downcast_ref::<AgentStore>().is_some(), "{store}");
        assert!(store.to_string().contains("quantum"));

        for mapped in [unresolved, log, metering, backend, store] {
            assert_eq!(classify(mapped.as_ref()), ExitCode::General);
        }
    }
}
