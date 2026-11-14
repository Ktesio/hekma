//! Story 24-3: the one summary event a Service Host commits after its
//! open-time reconcile — what the previous owner left and what became of
//! it. The per-Run truth rides the existing transition, Run and call
//! records; this event is the durable marker that a reconcile happened.

use serde::{Deserialize, Serialize};

/// The schema version of [`RecoveryEvent`].
pub const RECOVERY_SCHEMA_VERSION: u32 = 1;

/// What happened: today only a completed reconcile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryChange {
    /// The open-time reconcile finished.
    Recovered,
}

/// The Handoff Mode the previous owner recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPosture {
    /// Handoff Mode on: its agents were kept.
    On,
    /// Handoff Mode off: its agents were cleaned up.
    Off,
    /// Not recorded, or not chosen: its agents were kept.
    Unknown,
}

/// One open-time reconcile, summed up (story 24-3). Counts only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEvent {
    /// The event's schema version.
    pub schema_version: u32,
    /// What happened.
    pub change: RecoveryChange,
    /// The previous owner's Handoff Mode.
    pub posture: RecoveryPosture,
    /// Agents the previous owner left running that this host kept.
    pub runs_adopted: u64,
    /// Agents the previous owner left running that this host stopped
    /// (Handoff Mode off); their Runs end with cause `engine-crash`.
    pub runs_cleaned: u64,
    /// Proxied calls left open that closed as `crash` (usage unknown).
    pub calls_closed_crash: u64,
    /// Proxied calls left open whose usage the ledger held (`recovered`).
    pub calls_closed_recovered: u64,
    /// Agents whose recorded Shim was gone at adoption.
    pub shims_lost: u64,
    /// When (RFC 3339).
    pub at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recovery_event_serializes_its_documented_shape() {
        let event = crate::domain::bus::EngineEvent::Recovery(RecoveryEvent {
            schema_version: RECOVERY_SCHEMA_VERSION,
            change: RecoveryChange::Recovered,
            posture: RecoveryPosture::Unknown,
            runs_adopted: 2,
            runs_cleaned: 0,
            calls_closed_crash: 1,
            calls_closed_recovered: 3,
            shims_lost: 1,
            at: "2026-11-14T00:00:00Z".into(),
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "recovery", "schema_version": 1, "change": "recovered",
                "posture": "unknown", "runs_adopted": 2, "runs_cleaned": 0,
                "calls_closed_crash": 1, "calls_closed_recovered": 3,
                "shims_lost": 1, "at": "2026-11-14T00:00:00Z"
            })
        );
        let back: crate::domain::bus::EngineEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
        for (posture, wire) in [(RecoveryPosture::On, "on"), (RecoveryPosture::Off, "off")] {
            assert_eq!(serde_json::to_value(posture).unwrap(), wire);
        }
    }
}
