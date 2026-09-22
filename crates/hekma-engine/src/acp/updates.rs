//! The tolerant `session/update` parser (spine AD-19).
//!
//! ACP v1 delivers every turn event as a `session/update` notification whose
//! `update` object carries a `sessionUpdate` discriminator. This module maps
//! that discriminator set onto a small typed enum, TOLERANTLY: an unknown
//! discriminator (a future protocol addition) becomes a counted
//! [`SessionUpdate::Unhandled`] carrying the raw discriminator string —
//! surfaced-not-silent (AI-18), never a parse failure, never fatal to the
//! stream. Field shapes are best-effort: an update the engine recognizes but
//! whose fields it cannot read still classifies by discriminator (the raw
//! line remains the honest record in the agent log).
//!
//! METERING RULE (spine AD-19 / the epic-14 spec): `usage_update` is
//! CONTEXT-grain (`used`/`size` = the session context window, optional
//! `cost`). It is SURFACED (diagnostic + log) and NEVER minted into the
//! billing ledger — the billing shape comes only from the observed/sentinel
//! tiers (stories 14-3/14-6). This parser carries the figures; it never
//! touches metering modules.

use serde_json::Value;

/// One parsed `session/update`. Shapes are MINIMAL by design (the raw line
/// stays in the agent log; this enum drives surfacing + counting only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionUpdate {
    /// `agent_message_chunk` — a chunk of the agent's visible reply.
    AgentMessageChunk,
    /// `agent_thought_chunk` — a chunk of the agent's (extended-)thinking.
    AgentThoughtChunk,
    /// `tool_call` — the agent started a tool invocation.
    ToolCall,
    /// `tool_call_update` — progress/completion on a tool invocation.
    ToolCallUpdate,
    /// `plan` — the agent's task plan (todo list).
    Plan,
    /// `usage_update` — CONTEXT-grain usage: `used`/`size` context-window
    /// tokens and the optional `cost { amount, currency }`. SURFACED ONLY —
    /// never the billing ledger (the two grains must never mix, spine AD-19).
    UsageUpdate {
        /// Tokens of the context window currently used.
        used: Option<u64>,
        /// The context window size in tokens.
        size: Option<u64>,
        /// The optional cost block: `(amount as micros-ish float string,
        /// currency)`. Kept a raw display string — this engine never derives
        /// money from it (AD-8: no unlabeled/unreviewed money leaves this
        /// parser; the surfacing names it as agent-reported).
        cost: Option<(String, String)>,
    },
    /// `available_commands_update` — the agent's slash-command list changed.
    AvailableCommandsUpdate,
    /// `current_mode_update` — the agent's session mode changed.
    CurrentModeUpdate,
    /// An UNKNOWN discriminator (or a missing one) — counted + surfaced with
    /// the discriminator (or `<missing>`), then skipped. The stream lives on.
    Unhandled {
        /// The raw `sessionUpdate` discriminator string (or `<missing>`).
        discriminator: String,
    },
}

/// Parse the `params` of a `session/update` notification into a
/// [`SessionUpdate`]. Tolerant: a params shape without an `update` object or
/// without a `sessionUpdate` discriminator yields
/// [`SessionUpdate::Unhandled`] — never an error, never a panic.
pub fn parse_session_update(params: &Value) -> SessionUpdate {
    let discriminator = params
        .get("update")
        .and_then(|update| update.get("sessionUpdate"))
        .and_then(Value::as_str);
    let Some(discriminator) = discriminator else {
        return SessionUpdate::Unhandled {
            discriminator: "<missing>".to_string(),
        };
    };
    let update = params.get("update");
    match discriminator {
        "agent_message_chunk" => SessionUpdate::AgentMessageChunk,
        "agent_thought_chunk" => SessionUpdate::AgentThoughtChunk,
        "tool_call" => SessionUpdate::ToolCall,
        "tool_call_update" => SessionUpdate::ToolCallUpdate,
        "plan" => SessionUpdate::Plan,
        "usage_update" => {
            let used = update.and_then(|u| u.get("used")).and_then(Value::as_u64);
            let size = update.and_then(|u| u.get("size")).and_then(Value::as_u64);
            let cost = update.and_then(|u| u.get("cost")).and_then(|cost| {
                let amount = cost.get("amount")?;
                let currency = cost.get("currency").and_then(Value::as_str)?;
                Some((amount.to_string(), currency.to_string()))
            });
            SessionUpdate::UsageUpdate { used, size, cost }
        }
        "available_commands_update" => SessionUpdate::AvailableCommandsUpdate,
        "current_mode_update" => SessionUpdate::CurrentModeUpdate,
        other => SessionUpdate::Unhandled {
            discriminator: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_known_discriminator_parses_to_its_variant() {
        let cases = [
            ("agent_message_chunk", SessionUpdate::AgentMessageChunk),
            ("agent_thought_chunk", SessionUpdate::AgentThoughtChunk),
            ("tool_call", SessionUpdate::ToolCall),
            ("tool_call_update", SessionUpdate::ToolCallUpdate),
            ("plan", SessionUpdate::Plan),
            (
                "available_commands_update",
                SessionUpdate::AvailableCommandsUpdate,
            ),
            ("current_mode_update", SessionUpdate::CurrentModeUpdate),
        ];
        for (discriminator, expected) in cases {
            let params = json!({
                "sessionId": "s-1",
                "update": { "sessionUpdate": discriminator },
            });
            assert_eq!(parse_session_update(&params), expected, "{discriminator}");
        }
    }

    #[test]
    fn usage_update_carries_used_size_and_the_optional_cost() {
        // With the cost block.
        let with_cost = json!({
            "update": {
                "sessionUpdate": "usage_update",
                "used": 1200,
                "size": 200000,
                "cost": { "amount": 0.0034, "currency": "USD" },
            },
        });
        match parse_session_update(&with_cost) {
            SessionUpdate::UsageUpdate { used, size, cost } => {
                assert_eq!(used, Some(1200));
                assert_eq!(size, Some(200000));
                assert_eq!(cost, Some(("0.0034".to_string(), "USD".to_string())));
            }
            other => panic!("expected UsageUpdate, got {other:?}"),
        }
        // Without the cost block (it is optional).
        let without_cost = json!({
            "update": { "sessionUpdate": "usage_update", "used": 5, "size": 100 },
        });
        match parse_session_update(&without_cost) {
            SessionUpdate::UsageUpdate { used, size, cost } => {
                assert_eq!(used, Some(5));
                assert_eq!(size, Some(100));
                assert_eq!(cost, None);
            }
            other => panic!("expected UsageUpdate, got {other:?}"),
        }
        // A malformed usage_update still classifies (fields best-effort).
        let broken = json!({ "update": { "sessionUpdate": "usage_update" } });
        assert!(matches!(
            parse_session_update(&broken),
            SessionUpdate::UsageUpdate {
                used: None,
                size: None,
                cost: None
            }
        ));
    }

    #[test]
    fn unknown_and_missing_discriminators_are_counted_unhandled_never_fatal() {
        // A future protocol variant is a counted Unhandled, not an error.
        let future = json!({
            "update": { "sessionUpdate": "usage_update_v2", "used": 1 },
        });
        assert_eq!(
            parse_session_update(&future),
            SessionUpdate::Unhandled {
                discriminator: "usage_update_v2".to_string()
            }
        );
        // A missing update object / discriminator likewise.
        assert_eq!(
            parse_session_update(&json!({ "sessionId": "s" })),
            SessionUpdate::Unhandled {
                discriminator: "<missing>".to_string()
            }
        );
        assert_eq!(
            parse_session_update(&json!({ "update": {} })),
            SessionUpdate::Unhandled {
                discriminator: "<missing>".to_string()
            }
        );
    }

    #[test]
    fn extra_fields_are_tolerated_and_discriminators_round_trip() {
        // Extra members (a richer future shape) never break classification.
        let rich = json!({
            "sessionId": "s-1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "t-1",
                "title": "read file",
                "futureField": { "opaque": true },
            },
        });
        assert_eq!(parse_session_update(&rich), SessionUpdate::ToolCall);
    }
}
