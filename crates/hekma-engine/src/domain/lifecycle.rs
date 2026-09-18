//! Lifecycle state — the ratified state set as *data* (spine AD-15).
//!
//! The full transition table (state × command → state) is deliberately NOT
//! built here; it lands with the supervision core in story 1.4. This story
//! only needs the state set to exist as data so:
//!   * a newly registered [`AgentInstance`] can hold `Registered`, and
//!   * `remove`'s running-guard can *name* the `Running` state (AC5).
//!
//! [`AgentInstance`]: crate::domain::AgentInstance

use std::fmt;

use serde::{Deserialize, Serialize};

/// The ratified Lifecycle State set (PRD Glossary / spine AD-15).
///
/// Only [`LifecycleState::Registered`] is *reachable* this story; the other
/// variants exist as data so the future transition table (1.4) and the
/// `remove` running-guard can refer to them by name.
///
/// The wire form is snake_case (`registered`, `running`, …), matching the
/// `state` column stored by the SQLite [`StateStore`](crate::ports::StateStore)
/// and the AD-14 event schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    /// Instance is known to the Fleet with an Agent Home; not yet started.
    Registered,
    /// Start requested; adapter launching (unreachable until 1.4).
    Starting,
    /// Adapter reported ready; process supervised (unreachable until 1.4).
    Running,
    /// Execution suspended (unreachable until 1.4).
    Paused,
    /// Stop requested; graceful shutdown in progress (unreachable until 1.4).
    Stopping,
    /// Cleanly exited (unreachable until 1.4).
    Stopped,
    /// Crashed or failed to launch (unreachable until 1.4).
    Failed,
}

impl LifecycleState {
    /// Snake_case wire form used in the DB `state` column and events.
    ///
    /// Kept in lockstep with the `#[serde(rename_all = "snake_case")]` form so
    /// the store can persist a plain string without pulling in serde_json.
    pub fn as_str(&self) -> &'static str {
        match self {
            LifecycleState::Registered => "registered",
            LifecycleState::Starting => "starting",
            LifecycleState::Running => "running",
            LifecycleState::Paused => "paused",
            LifecycleState::Stopping => "stopping",
            LifecycleState::Stopped => "stopped",
            LifecycleState::Failed => "failed",
        }
    }

    /// Parse the wire form back into a [`LifecycleState`].
    ///
    /// Returns `None` for an unrecognized string (e.g. a value written by a
    /// future schema version); callers decide how to treat that.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "registered" => Some(LifecycleState::Registered),
            "starting" => Some(LifecycleState::Starting),
            "running" => Some(LifecycleState::Running),
            "paused" => Some(LifecycleState::Paused),
            "stopping" => Some(LifecycleState::Stopping),
            "stopped" => Some(LifecycleState::Stopped),
            "failed" => Some(LifecycleState::Failed),
            _ => None,
        }
    }

    /// Minimal removal predicate for this story (AC5).
    ///
    /// `remove` refuses without `--force` only while the instance is
    /// `Running`. This is intentionally NOT the full transition table (1.4);
    /// it is the single guard the `remove` capability needs today. Every other
    /// state is freely removable (there is no real supervision yet, so nothing
    /// else can be "in flight").
    pub fn is_removable_without_force(&self) -> bool {
        !matches!(self, LifecycleState::Running)
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_form_round_trips_for_every_variant() {
        let all = [
            LifecycleState::Registered,
            LifecycleState::Starting,
            LifecycleState::Running,
            LifecycleState::Paused,
            LifecycleState::Stopping,
            LifecycleState::Stopped,
            LifecycleState::Failed,
        ];
        for state in all {
            let wire = state.as_str();
            assert_eq!(LifecycleState::from_wire(wire), Some(state), "{wire}");
            // Display must match the wire form exactly.
            assert_eq!(state.to_string(), wire);
        }
    }

    #[test]
    fn serde_round_trip_matches_wire_form() {
        // serde's snake_case form must equal as_str() so the DB string and the
        // event JSON never diverge.
        for state in [LifecycleState::Registered, LifecycleState::Running] {
            let json = serde_json::to_string(&state).expect("serialize");
            assert_eq!(json, format!("\"{}\"", state.as_str()));
            let back: LifecycleState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, state);
        }
    }

    #[test]
    fn from_wire_rejects_unknown() {
        assert_eq!(LifecycleState::from_wire("bogus"), None);
        assert_eq!(LifecycleState::from_wire(""), None);
        assert_eq!(LifecycleState::from_wire("Registered"), None);
    }

    #[test]
    fn only_running_requires_force() {
        assert!(LifecycleState::Registered.is_removable_without_force());
        assert!(LifecycleState::Starting.is_removable_without_force());
        assert!(LifecycleState::Paused.is_removable_without_force());
        assert!(LifecycleState::Stopping.is_removable_without_force());
        assert!(LifecycleState::Stopped.is_removable_without_force());
        assert!(LifecycleState::Failed.is_removable_without_force());
        assert!(!LifecycleState::Running.is_removable_without_force());
    }
}
