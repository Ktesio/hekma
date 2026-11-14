//! SM-7's auditor (story 24-3, T6): "no Run is stopped, paused or blocked
//! by anything NFR-9 does not list". It reads the durable record — the
//! export outbox in `state.db` — and maps every stop-, pause- or
//! block-class action onto one of NFR-9's five clauses; anything that maps
//! to none is reported, naming the event, so a test fails on it.
//!
//! Only the INITIATING edges are actions (`→ stopping`, `running → paused`,
//! a readiness-timeout teardown, a hold or reject disposition, a non-`warn`
//! breach); the edges they lead to (`→ stopped`, a crash's `→ failed`, a
//! Run's `ended`) are consequences.

use std::path::Path;

/// NFR-9's five clauses: the only reasons Hekma may stop, pause or block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clause {
    /// 1. An Operator or Host verb (`stop`, `pause`, `drain`, `hold`,
    ///    `reject`) — or, indistinguishably on the wire, 4. a Service Host
    ///    shutdown with Handoff Mode off, which stops through the same
    ///    `stop` command.
    VerbOrShutdown,
    /// 2. A configured non-`warn` Breach Action (or Scent-Check `enforce`).
    BreachAction,
    /// 3. A failed start or a declared probe's failure (readiness, liveness)
    ///    under the Restart Policy.
    StartOrProbeFailure,
    /// 5. Cleanup of a crashed owner's agents with Handoff Mode off.
    OrphanCleanup,
}

/// One stop-, pause- or block-class action and the clause it maps to
/// (`None`: unauthorized).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    /// The outbox sequence number.
    pub seq: i64,
    /// What happened, in words (the instance or Run, the edge, the cause).
    pub what: String,
    /// The clause that authorizes it, if any.
    pub clause: Option<Clause>,
}

/// Every action in `events` (`(seq, event JSON)`), mapped.
pub fn audit(events: &[(i64, serde_json::Value)]) -> Vec<Action> {
    events
        .iter()
        .filter_map(|(seq, event)| action(*seq, event))
        .collect()
}

/// The actions that map to no clause.
pub fn unauthorized(actions: &[Action]) -> Vec<&Action> {
    actions.iter().filter(|a| a.clause.is_none()).collect()
}

/// Every action in a state directory's outbox.
pub fn audit_state_dir(state: &Path) -> Vec<Action> {
    let conn = rusqlite::Connection::open_with_flags(
        state.join("state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("the state directory's store");
    let mut stmt = conn
        .prepare("SELECT seq, event FROM outbox ORDER BY seq")
        .expect("the outbox");
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .expect("the outbox rows");
    let events: Vec<(i64, serde_json::Value)> = rows
        .filter_map(Result::ok)
        .filter_map(|(seq, text)| serde_json::from_str(&text).ok().map(|v| (seq, v)))
        .collect();
    audit(&events)
}

fn action(seq: i64, event: &serde_json::Value) -> Option<Action> {
    let kind = event["kind"].as_str()?;
    match kind {
        "transition" => {
            let instance = event["instance"].as_str().unwrap_or("?");
            let prior = event["prior_state"].as_str()?;
            let new = event["new_state"].as_str()?;
            let cause = event["cause"]["kind"].as_str().unwrap_or("?");
            let initiating = match (prior, new) {
                (_, "stopping") => true,
                ("running", "paused") => true,
                ("starting", "failed") => cause == "readiness-timeout",
                _ => false,
            };
            if !initiating {
                return None;
            }
            let clause = match (new, cause) {
                (_, "command") => Some(Clause::VerbOrShutdown),
                ("paused", "pause-best-effort" | "pause-cooperative") => {
                    Some(Clause::VerbOrShutdown)
                }
                (_, "budget-exceeded") => Some(Clause::BreachAction),
                (_, "unresponsive" | "readiness-timeout") => Some(Clause::StartOrProbeFailure),
                (_, "engine-crash") => Some(Clause::OrphanCleanup),
                _ => None,
            };
            Some(Action {
                seq,
                what: format!("{instance}: {prior} → {new} ({cause})"),
                clause,
            })
        }
        "run" if event["change"] == "disposition-changed" => {
            let cause = event["cause"].as_str().unwrap_or("");
            // Only setting a hold or a reject acts (`hold by <who>`,
            // `reject by <who>`); a release or a reroute does not.
            let acts = cause.starts_with("hold ") || cause.starts_with("reject ");
            acts.then(|| Action {
                seq,
                what: format!(
                    "Run {}: disposition {cause}",
                    event["run_id"].as_str().unwrap_or("?")
                ),
                clause: Some(Clause::VerbOrShutdown),
            })
        }
        "budget_breach" => {
            let action = event["action"].as_str().unwrap_or("warn");
            (action != "warn").then(|| Action {
                seq,
                what: format!(
                    "{}: breach action {action}",
                    event["instance"].as_str().unwrap_or("?")
                ),
                clause: Some(Clause::BreachAction),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transition(prior: &str, new: &str, cause: serde_json::Value) -> serde_json::Value {
        json!({"kind": "transition", "instance": "a", "prior_state": prior,
               "new_state": new, "cause": cause})
    }

    #[test]
    fn every_initiating_action_maps_to_a_clause_and_consequences_are_ignored() {
        let events = vec![
            (
                1,
                transition(
                    "running",
                    "stopping",
                    json!({"kind": "command", "command": "stop"}),
                ),
            ),
            (
                2,
                transition("stopping", "stopped", json!({"kind": "stop-graceful"})),
            ),
            (
                3,
                transition(
                    "running",
                    "paused",
                    json!({"kind": "command", "command": "pause"}),
                ),
            ),
            (
                4,
                transition("running", "stopping", json!({"kind": "budget-exceeded"})),
            ),
            (
                5,
                transition("running", "stopping", json!({"kind": "unresponsive"})),
            ),
            (
                6,
                transition("starting", "failed", json!({"kind": "readiness-timeout"})),
            ),
            (
                7,
                transition("running", "stopping", json!({"kind": "engine-crash"})),
            ),
            (
                8,
                transition("running", "failed", json!({"kind": "crashed"})),
            ),
            (
                9,
                transition(
                    "starting",
                    "running",
                    json!({"kind": "adopted-unconfirmed"}),
                ),
            ),
            (
                10,
                json!({"kind": "run", "change": "disposition-changed", "run_id": "r",
                        "cause": "hold by operator"}),
            ),
            (
                11,
                json!({"kind": "run", "change": "disposition-changed", "run_id": "r",
                        "cause": "release by operator"}),
            ),
            (
                12,
                json!({"kind": "budget_breach", "instance": "a", "action": "block"}),
            ),
            (
                13,
                json!({"kind": "budget_breach", "instance": "a", "action": "warn"}),
            ),
            (
                14,
                transition("running", "paused", json!({"kind": "pause-cooperative"})),
            ),
        ];
        let actions = audit(&events);
        let seqs: Vec<i64> = actions.iter().map(|a| a.seq).collect();
        assert_eq!(seqs, vec![1, 3, 4, 5, 6, 7, 10, 12, 14]);
        assert!(unauthorized(&actions).is_empty(), "{actions:?}");
        assert_eq!(actions[5].clause, Some(Clause::OrphanCleanup));
    }

    #[test]
    fn an_unlisted_reason_to_stop_is_reported_by_name() {
        let events = vec![(
            42,
            transition("running", "stopping", json!({"kind": "something-new"})),
        )];
        let actions = audit(&events);
        let bad = unauthorized(&actions);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].seq, 42);
        assert!(bad[0].what.contains("something-new"));
    }
}
