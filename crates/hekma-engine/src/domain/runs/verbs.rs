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
