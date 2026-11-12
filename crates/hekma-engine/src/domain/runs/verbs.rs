//! The verb × Control Depth table (PRD FR-49, story 21-2).
//!
//! Each verb behaves per Control Depth as FR-49's table states: a cell that
//! *fails fast* returns an error naming the depth, a *qualified* cell
//! succeeds with a visible qualifier (the FR-7 honesty rule). [`verb_cell`]
//! transcribes the table once, in the domain core, so every verb consults the
//! same data. The `supervised (no wire)` column is FR-49's ruling: wire-only
//! cells fail fast naming the Metering Source.
//!
//! [`VerbCell::available`] says whether the engine delivers the cell today.
//! Later stories flip cells as they build each verb at each depth; the
//! conformance fixture (`hekma-conformance/fixtures/verb-depth.json`) pins
//! the table and probes every available cell against a live engine.

use serde::{Deserialize, Serialize};

use super::ControlDepth;
use crate::domain::budget::BreachAction;

/// Whether a Run at `(depth, wire_path)` can EXECUTE `action` today
/// (story 21-3, one source of truth with the verb × depth table):
///
/// * `warn` always executes (it only signals);
/// * `pause` / `stop` are process actions — the supervised depth only
///   (below it they need wire hold/reject, story 23.4);
/// * `block` fences the metering listener — supervised with a wire path
///   only (`supervised (no wire)` cannot; FR-49).
///
/// A resolved action that cannot execute falls back to `warn` with the
/// `depth-fallback` Anomaly (FR-60); it takes effect when the Run's depth
/// allows it again.
pub(crate) fn action_executable(
    depth: ControlDepth,
    wire_path: bool,
    action: BreachAction,
) -> bool {
    match action {
        BreachAction::Warn => true,
        BreachAction::Pause | BreachAction::Stop => depth == ControlDepth::Supervised,
        BreachAction::Block => depth == ControlDepth::Supervised && wire_path,
    }
}

/// A verb row of the FR-49 table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunVerb {
    /// Listing and usage.
    ListUsage,
    /// `logs`.
    Logs,
    /// Budgets and the `warn` Breach Action.
    BudgetsWarn,
    /// `pause` / `resume`.
    PauseResume,
    /// `stop`.
    Stop,
    /// `send` and `attach`.
    SendAttach,
    /// hold / release / reroute / reject.
    HoldReleaseRerouteReject,
    /// The `pause` / `stop` / `block` Breach Actions.
    BreachActionPauseStopBlock,
}

/// Every verb row, in table order.
pub const RUN_VERBS: [RunVerb; 8] = [
    RunVerb::ListUsage,
    RunVerb::Logs,
    RunVerb::BudgetsWarn,
    RunVerb::PauseResume,
    RunVerb::Stop,
    RunVerb::SendAttach,
    RunVerb::HoldReleaseRerouteReject,
    RunVerb::BreachActionPauseStopBlock,
];

impl RunVerb {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunVerb::ListUsage => "list-usage",
            RunVerb::Logs => "logs",
            RunVerb::BudgetsWarn => "budgets-warn",
            RunVerb::PauseResume => "pause-resume",
            RunVerb::Stop => "stop",
            RunVerb::SendAttach => "send-attach",
            RunVerb::HoldReleaseRerouteReject => "hold-release-reroute-reject",
            RunVerb::BreachActionPauseStopBlock => "breach-action-pause-stop-block",
        }
    }
}

/// How a verb behaves at a depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CellBehavior {
    /// Does what it says.
    Honored,
    /// Succeeds with a visible qualifier.
    Qualified,
    /// Returns an error naming the depth (or the Metering Source).
    FailsFast,
    /// Refused at configuration (a Breach Action set directly).
    Refused,
}

impl CellBehavior {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            CellBehavior::Honored => "honored",
            CellBehavior::Qualified => "qualified",
            CellBehavior::FailsFast => "fails-fast",
            CellBehavior::Refused => "refused",
        }
    }
}

/// One cell of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerbCell {
    /// What FR-49 says the verb does here.
    pub behavior: CellBehavior,
    /// Whether the engine delivers the cell today.
    pub available: bool,
    /// FR-49's wording for the cell.
    pub note: &'static str,
}

const fn cell(behavior: CellBehavior, available: bool, note: &'static str) -> VerbCell {
    VerbCell {
        behavior,
        available,
        note,
    }
}

/// The cell for `verb` at `depth` (`wire_path` matters only at
/// `supervised`, where `false` is the `supervised (no wire)` column).
pub fn verb_cell(verb: RunVerb, depth: ControlDepth, wire_path: bool) -> VerbCell {
    use CellBehavior::{FailsFast, Honored, Qualified, Refused};
    let no_wire = depth == ControlDepth::Supervised && !wire_path;
    match (verb, depth) {
        (RunVerb::ListUsage, ControlDepth::Observed) => {
            cell(Honored, false, "from its telemetry, estimated")
        }
        (RunVerb::ListUsage, ControlDepth::Wire) => cell(Honored, false, "per-call records"),
        (RunVerb::ListUsage, ControlDepth::Announced) => cell(Honored, true, "per-call records"),
        (RunVerb::ListUsage, ControlDepth::Supervised) => {
            cell(Honored, true, "per-call records and process state")
        }

        (RunVerb::Logs, ControlDepth::Observed) => cell(FailsFast, false, "fails fast"),
        (RunVerb::Logs, ControlDepth::Wire | ControlDepth::Announced) => {
            cell(Honored, false, "per-call records")
        }
        (RunVerb::Logs, ControlDepth::Supervised) => {
            cell(Honored, true, "process output and per-call records")
        }

        (RunVerb::BudgetsWarn, ControlDepth::Observed) => {
            cell(Honored, false, "from its telemetry")
        }
        (RunVerb::BudgetsWarn, ControlDepth::Wire) => cell(Honored, false, "honored"),
        (RunVerb::BudgetsWarn, ControlDepth::Announced | ControlDepth::Supervised) => {
            cell(Honored, true, "honored")
        }

        (RunVerb::PauseResume, ControlDepth::Observed) => cell(FailsFast, false, "fails fast"),
        (RunVerb::PauseResume, ControlDepth::Wire | ControlDepth::Announced) => cell(
            Qualified,
            false,
            "qualified: holds / releases the Run's calls",
        ),
        (RunVerb::PauseResume, ControlDepth::Supervised) => cell(
            Honored,
            true,
            "Adapter pause; a wire hold only as the FR-7 amendment allows",
        ),

        (RunVerb::Stop, ControlDepth::Observed) => cell(FailsFast, false, "fails fast"),
        (RunVerb::Stop, ControlDepth::Wire) => cell(
            Qualified,
            false,
            "qualified: rejects further calls, the process keeps running",
        ),
        (RunVerb::Stop, ControlDepth::Announced) => cell(
            Qualified,
            false,
            "qualified: rejects further calls and ends the Lease",
        ),
        (RunVerb::Stop, ControlDepth::Supervised) => cell(Honored, true, "process stop"),

        (RunVerb::SendAttach, ControlDepth::Supervised) => cell(
            Honored,
            true,
            "per the Adapter's Capability Declaration; a child Run fails fast",
        ),
        (RunVerb::SendAttach, _) => cell(FailsFast, false, "fails fast"),

        (RunVerb::HoldReleaseRerouteReject, ControlDepth::Observed) => {
            cell(FailsFast, false, "fails fast")
        }
        (RunVerb::HoldReleaseRerouteReject, _) if no_wire => {
            cell(FailsFast, false, "fails fast, naming the Metering Source")
        }
        (RunVerb::HoldReleaseRerouteReject, _) => cell(Honored, false, "honored"),

        (RunVerb::BreachActionPauseStopBlock, ControlDepth::Observed) => cell(
            Refused,
            false,
            "set directly: refused at configuration; from a Policy: the fallback to warn",
        ),
        (RunVerb::BreachActionPauseStopBlock, ControlDepth::Supervised) if no_wire => cell(
            Honored,
            true,
            "pause and stop as the verbs above, latched; block refused at configuration",
        ),
        (RunVerb::BreachActionPauseStopBlock, ControlDepth::Supervised) => {
            cell(Honored, true, "as the verbs above, latched")
        }
        (RunVerb::BreachActionPauseStopBlock, ControlDepth::Announced) if !wire_path => cell(
            Refused,
            true,
            "set directly with no wire path: refused at configuration, naming the Metering \
             Source",
        ),
        (RunVerb::BreachActionPauseStopBlock, _) => cell(
            Honored,
            false,
            "as the verbs above (block as reject), latched until the ceiling is raised or reset",
        ),
    }
}

/// A lifecycle verb addressed to a Run (story 21-6, PRD FR-50): mapped to
/// the instance verb on a process-owning Run, answered from the verb ×
/// depth table on any other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunLifecycleVerb {
    /// `stop`.
    Stop,
    /// `pause`.
    Pause,
    /// `resume`.
    Resume,
    /// `drain` (pause, a bounded wait, then stop).
    Drain,
}

impl RunLifecycleVerb {
    /// Every verb.
    pub const ALL: [RunLifecycleVerb; 4] = [
        RunLifecycleVerb::Stop,
        RunLifecycleVerb::Pause,
        RunLifecycleVerb::Resume,
        RunLifecycleVerb::Drain,
    ];

    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunLifecycleVerb::Stop => "stop",
            RunLifecycleVerb::Pause => "pause",
            RunLifecycleVerb::Resume => "resume",
            RunLifecycleVerb::Drain => "drain",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|verb| verb.as_str() == raw)
    }

    /// The FR-49 table row the verb is answered from (a drain ends in a
    /// stop, so it is served where `stop` is).
    pub fn table_row(&self) -> RunVerb {
        match self {
            RunLifecycleVerb::Stop | RunLifecycleVerb::Drain => RunVerb::Stop,
            RunLifecycleVerb::Pause | RunLifecycleVerb::Resume => RunVerb::PauseResume,
        }
    }
}

/// Whether `verb` acts on `record`, and on which instance (story 21-6):
/// `Ok(instance)` when the Run owns its instance's supervised process and
/// the table serves the cell (the instance verb then runs with today's
/// semantics); otherwise the per-Run refusal — an ended Run, a child held by
/// its parent (`resume`), or the cell's own refusal naming the depth (or
/// the Metering Source). No cell is flipped here: a child Run's lifecycle
/// verbs act on its calls, which arrive with wire hold.
pub(crate) fn run_verb_target(
    verb: RunLifecycleVerb,
    record: &super::RunRecord,
) -> Result<String, super::RunError> {
    let run_id = record.run_id.as_str().to_string();
    if record.is_ended() {
        return Err(super::RunError::RunEnded { run_id });
    }
    let depth = record.control_depth.display_with_wire(record.wire_path);
    let cell = verb_cell(verb.table_row(), record.control_depth, record.wire_path);
    if record.process_owner && cell.available {
        return record
            .instance
            .clone()
            .ok_or_else(|| super::RunError::Invalid {
                field: "run".to_string(),
                reason: format!("Run `{run_id}` owns a process but names no agent"),
            });
    }
    let host = record
        .instance
        .as_deref()
        .map(|instance| {
            format!(
                "the agent that hosts it (`hekma agent {} {instance}`)",
                verb.as_str()
            )
        })
        .unwrap_or_else(|| "the agent that hosts it".to_string());
    let reason = if verb == RunLifecycleVerb::Resume
        && record.status == super::RunStatus::Held
        && !record.process_owner
    {
        let parent = record
            .parent_run_id
            .as_ref()
            .map(|parent| format!("`{parent}`"))
            .unwrap_or_else(|| "Run".to_string());
        format!(
            "it is held because its parent Run {parent} is paused (parent-paused) — resume {}",
            record
                .instance
                .as_deref()
                .map(|instance| format!(
                    "the agent that hosts it (`hekma agent resume {instance}`)"
                ))
                .unwrap_or_else(|| "its parent".to_string())
        )
    } else {
        format!(
            "on a {depth} Run, {} acts on its model calls ({}), which is not available until \
             wire hold lands — {} {host}, or end the Run with `hekma run close {run_id}`",
            verb.as_str(),
            cell.note,
            match verb {
                RunLifecycleVerb::Stop | RunLifecycleVerb::Drain => "stop",
                RunLifecycleVerb::Pause => "pause",
                RunLifecycleVerb::Resume => "resume",
            },
        )
    };
    Err(super::RunError::VerbUnavailable {
        run_id,
        verb: verb.as_str().to_string(),
        depth,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cell_has_a_note_and_the_supervised_column_is_delivered() {
        for verb in RUN_VERBS {
            for depth in ControlDepth::ALL {
                for wire in [true, false] {
                    assert!(!verb_cell(verb, depth, wire).note.is_empty());
                }
            }
            if verb != RunVerb::HoldReleaseRerouteReject {
                assert!(
                    verb_cell(verb, ControlDepth::Supervised, true).available,
                    "{}",
                    verb.as_str()
                );
            }
        }
    }

    #[test]
    fn wire_only_cells_fail_fast_without_a_wire_path() {
        let cell = verb_cell(
            RunVerb::HoldReleaseRerouteReject,
            ControlDepth::Supervised,
            false,
        );
        assert_eq!(cell.behavior, CellBehavior::FailsFast);
        assert!(cell.note.contains("Metering Source"));
        assert_eq!(
            verb_cell(
                RunVerb::BreachActionPauseStopBlock,
                ControlDepth::Announced,
                false
            )
            .behavior,
            CellBehavior::Refused
        );
        assert!(
            !verb_cell(
                RunVerb::BreachActionPauseStopBlock,
                ControlDepth::Announced,
                true
            )
            .available
        );
    }

    fn record(process_owner: bool, status: super::super::RunStatus) -> super::super::RunRecord {
        super::super::RunRecord {
            run_id: crate::domain::usage::RunId::from_wire("run_X"),
            control_depth: if process_owner {
                ControlDepth::Supervised
            } else {
                ControlDepth::Announced
            },
            wire_path: true,
            status,
            anomalies: Vec::new(),
            parent_run_id: (!process_owner)
                .then(|| crate::domain::usage::RunId::from_wire("run_P")),
            instance: Some("agent".to_string()),
            process_owner,
            end_cause: None,
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
            ended_at: None,
        }
    }

    #[test]
    fn a_run_verb_maps_to_its_instance_or_answers_its_cell_per_run() {
        use super::super::RunStatus;
        for verb in RunLifecycleVerb::ALL {
            assert_eq!(RunLifecycleVerb::from_wire(verb.as_str()), Some(verb));
            // The process-owning Run: the instance's own verb.
            assert_eq!(
                run_verb_target(verb, &record(true, RunStatus::Active)).unwrap(),
                "agent"
            );
            // An ended Run: `run-ended`.
            assert_eq!(
                run_verb_target(verb, &record(true, RunStatus::Ended))
                    .unwrap_err()
                    .code(),
                "run-ended"
            );
            // A child: refused per Run, naming its depth and the calls.
            let err = run_verb_target(verb, &record(false, RunStatus::Active)).unwrap_err();
            assert_eq!(err.code(), "verb-unavailable");
            let text = err.to_string();
            assert!(text.contains("announced") && text.contains("model calls"), "{text}");
            assert!(text.contains("hekma run close run_X"), "{text}");
        }
        assert_eq!(RunLifecycleVerb::from_wire("start"), None);
        assert_eq!(RunLifecycleVerb::Drain.table_row(), RunVerb::Stop);
        // A held child's resume names the parent that holds it.
        let err = run_verb_target(RunLifecycleVerb::Resume, &record(false, RunStatus::Held))
            .unwrap_err()
            .to_string();
        assert!(err.contains("parent Run `run_P`") && err.contains("parent-paused"), "{err}");
        assert!(err.contains("hekma agent resume agent"), "{err}");
        // A process-owning Run without an agent is a typed refusal.
        let mut orphan = record(true, RunStatus::Active);
        orphan.instance = None;
        assert_eq!(
            run_verb_target(RunLifecycleVerb::Stop, &orphan)
                .unwrap_err()
                .code(),
            "invalid-request"
        );
        let mut hostless = record(false, RunStatus::Held);
        hostless.instance = None;
        hostless.parent_run_id = None;
        let err = run_verb_target(RunLifecycleVerb::Resume, &hostless)
            .unwrap_err()
            .to_string();
        assert!(err.contains("resume its parent"), "{err}");
        assert!(run_verb_target(RunLifecycleVerb::Stop, &{
            let mut child = record(false, RunStatus::Active);
            child.instance = None;
            child
        })
        .unwrap_err()
        .to_string()
        .contains("the agent that hosts it"));
    }

    #[test]
    fn wire_forms_are_kebab_case() {
        assert_eq!(
            serde_json::to_string(&RunVerb::BreachActionPauseStopBlock).unwrap(),
            "\"breach-action-pause-stop-block\""
        );
        for verb in RUN_VERBS {
            assert_eq!(
                serde_json::to_string(&verb).unwrap(),
                format!("\"{}\"", verb.as_str())
            );
        }
        for behavior in [
            CellBehavior::Honored,
            CellBehavior::Qualified,
            CellBehavior::FailsFast,
            CellBehavior::Refused,
        ] {
            assert_eq!(
                serde_json::to_string(&behavior).unwrap(),
                format!("\"{}\"", behavior.as_str())
            );
        }
    }
}
