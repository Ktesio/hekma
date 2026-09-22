//! The ACP wire codec (spine AD-19): newline-delimited JSON-RPC 2.0 over the
//! child's stdio.
//!
//! Framing is the ACP v1 pin: UTF-8, ONE JSON-RPC message per `\n`, no
//! embedded newlines (a `serde_json::to_string` encoding can never emit a raw
//! newline inside a string — control characters are escaped — so encoding is
//! framing-safe by construction; unit-tested). OS-uniform (AD-4: no per-OS
//! cfg — ndJSON framing is identical on every platform). Hand-rolled over
//! `serde_json` ONLY (NO new crates — the official ACP SDK was rejected for
//! v1 under NFR-8, spine AD-19).
//!
//! Parsing is TOLERANT (surfaced-not-silent, AI-18): a line that is not a
//! recognizable JSON-RPC envelope is a typed [`CodecError`] naming the
//! problem — the CALLER (the connection reader) surfaces it, skips the line,
//! counts it, and keeps the stream alive; a malformed line is never fatal to
//! the transport. Protocol errors may name method names and line numbers;
//! they NEVER echo prompt text or file contents (no-leak discipline).
//!
//! Method names pinned by the ACP v1 docs (agentclientprotocol.com
//! `/protocol/v1/*`): `initialize`, `session/new`, `session/prompt`,
//! `session/update`, `session/request_permission`, `session/cancel`.

use serde_json::{json, Value};

/// The single ACP protocol version this engine tolerates (spine AD-19: the
/// pinned set `{1}`; a counter outside it closes the connection with a
/// surfaced, traffic-free refusal). A slice, not a bare bool, so a future
/// tolerated set is a one-line edit plus a test update.
pub const TOLERATED_PROTOCOL_VERSIONS: &[i64] = &[1];

/// The `clientCapabilities` the engine advertises at `initialize`: the
/// PROTOCOL DEFAULT — advertise-nothing (D3; spine AD-19 "no fs, no terminal,
/// no elicitation"). No sandbox claims (NFR-6). A function (not a `const`):
/// `json!` is not const-evaluable.
pub fn client_capabilities() -> Value {
    json!({
        "fs": { "readFile": false, "writeFile": false },
        "terminal": false,
    })
}

/// The `clientInfo` name the engine presents at `initialize`.
pub const CLIENT_INFO_NAME: &str = "hekma";

/// One INBOUND message parsed off the child's stdout stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    /// A successful response to one of our requests (`result` present).
    Response {
        /// The correlated request id.
        id: u64,
        /// The response `result` payload (may be `Null` for a `null` result).
        result: Value,
    },
    /// An ERROR response to one of our requests (`error` object present).
    ResponseError {
        /// The correlated request id.
        id: u64,
        /// The JSON-RPC error `code`.
        code: i64,
        /// The JSON-RPC error `message` (agent-authored — the caller decides
        /// what may be surfaced; the no-leak discipline applies).
        message: String,
    },
    /// A notification (no id): `session/update` or anything else (unknown
    /// methods are surfaced + counted by the connection, never fatal).
    Notification {
        /// The method name.
        method: String,
        /// The `params` payload (`Null` when absent).
        params: Value,
    },
    /// An agent→client REQUEST (method + id): the agent is CALLING us and
    /// expects our response — `session/request_permission` is the v1 case.
    /// The connection answers every agent request (denied or
    /// method-not-found) so the agent never hangs on its own call.
    AgentRequest {
        /// The agent's request id (our response echoes it).
        id: u64,
        /// The method name.
        method: String,
        /// The `params` payload (`Null` when absent).
        params: Value,
    },
}

/// Why one inbound line could not be decoded (traffic-free: names the shape
/// of the problem, never echoes the offending line's CONTENT — the raw line
/// is already in the instance's honest agent log if the operator wants it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// The line was not valid JSON at all.
    NotJson,
    /// Valid JSON but not a JSON object.
    NotAnObject,
    /// An object that is neither a response (id + result/error) nor a
    /// notification (method).
    Unclassifiable,
    /// A response whose `id` is present but not an unsigned integer.
    UnusableId,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::NotJson => write!(f, "line is not valid JSON"),
            CodecError::NotAnObject => write!(f, "line is valid JSON but not an object"),
            CodecError::Unclassifiable => {
                write!(f, "line is neither a JSON-RPC response nor a notification")
            }
            CodecError::UnusableId => write!(f, "response id is not an unsigned integer"),
        }
    }
}

/// Parse ONE inbound ndJSON line (the caller strips the trailing newline).
///
/// Tolerant by contract: an EMPTY/whitespace-only line is `Ok(None)` (a
/// framing hiccup, not a message — skip silently); anything else either
/// parses into an [`Inbound`] or is a typed [`CodecError`] the caller
/// surfaces + counts.
pub fn parse_line(line: &str) -> Result<Option<Inbound>, CodecError> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(line).map_err(|_| CodecError::NotJson)?;
    let Some(object) = value.as_object() else {
        return Err(CodecError::NotAnObject);
    };
    // A `method` member makes it an outbound message from the agent: WITH an
    // id it is a request the agent expects us to answer (the
    // `session/request_permission` case); WITHOUT one it is a notification.
    // `params` defaults to Null when absent.
    if let Some(method) = object.get("method") {
        let Some(method) = method.as_str() else {
            return Err(CodecError::Unclassifiable);
        };
        if let Some(id_value) = object.get("id") {
            let Some(id) = id_value.as_u64() else {
                return Err(CodecError::UnusableId);
            };
            return Ok(Some(Inbound::AgentRequest {
                id,
                method: method.to_string(),
                params: object.get("params").cloned().unwrap_or(Value::Null),
            }));
        }
        return Ok(Some(Inbound::Notification {
            method: method.to_string(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }));
    }
    // Otherwise it must be a response: an id + (result | error).
    let Some(id_value) = object.get("id") else {
        return Err(CodecError::Unclassifiable);
    };
    let Some(id) = id_value.as_u64() else {
        return Err(CodecError::UnusableId);
    };
    if let Some(error) = object.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Ok(Some(Inbound::ResponseError { id, code, message }));
    }
    Ok(Some(Inbound::Response {
        id,
        result: object.get("result").cloned().unwrap_or(Value::Null),
    }))
}

/// Encode ONE outbound JSON-RPC REQUEST as a single ndJSON line (no trailing
/// newline — the writer appends it; see [`encode_line_terminator`]).
///
/// `id` is the correlation id; `params` is embedded verbatim. The encoding
/// can never contain a raw newline (serde_json escapes control characters),
/// so one call is exactly one wire line — the framing purity invariant.
pub fn encode_request(id: u64, method: &str, params: Value) -> String {
    encode_envelope(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
}

/// Encode ONE outbound JSON-RPC RESPONSE (our answer to an agent request,
/// e.g. the denial reply to `session/request_permission`) as a single line.
pub fn encode_response(id: u64, result: Value) -> String {
    encode_envelope(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
}

/// Encode ONE outbound JSON-RPC NOTIFICATION (no id — e.g. `session/cancel`)
/// as a single line.
pub fn encode_notification(method: &str, params: Value) -> String {
    encode_envelope(json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    }))
}

/// The line terminator the writer appends after every encoded envelope.
pub const LINE_TERMINATOR: &str = "\n";

fn encode_envelope(envelope: Value) -> String {
    // serde_json::to_string of an object built from `json!` cannot fail, and
    // can never emit a raw newline (control characters are escaped) — the
    // ndJSON framing invariant holds by construction. The expect is a
    // contract pin, not an error path; the unit tests prove it.
    serde_json::to_string(&envelope).expect("a json!-built envelope always serializes")
}

/// Build the `initialize` request params: protocol version 1, the DEFAULT
/// client capabilities (advertise-nothing — D3), and the clientInfo.
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": client_capabilities(),
        "clientInfo": {
            "name": CLIENT_INFO_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Build the `session/new` request params: the Agent Home as cwd, no MCP
/// servers (spine AD-19: the handshake shape).
pub fn session_new_params(cwd: &std::path::Path) -> Value {
    json!({
        "cwd": cwd.to_string_lossy(),
        "mcpServers": [],
    })
}

/// Build the `session/load` request params (story 14-2, D4): the persisted
/// session id plus the same cwd/mcpServers shape `session/new` carries (the
/// ACP v1 load shape). Sent by the handshake ONLY when the agent advertised
/// `loadSession` and a session id is persisted from a previous Run.
pub fn session_load_params(session_id: &str, cwd: &std::path::Path) -> Value {
    json!({
        "sessionId": session_id,
        "cwd": cwd.to_string_lossy(),
        "mcpServers": [],
    })
}

/// Build the `session/prompt` request params: the session id + ONE text
/// ContentBlock (spine AD-19: `send` = one `session/prompt`). `text` rides
/// INSIDE the params — never in any diagnostic (no-leak discipline).
pub fn session_prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [ { "type": "text", "text": text } ],
    })
}

/// Build the `session/cancel` notification params (the stop-path turn
/// cancellation).
pub fn session_cancel_params(session_id: &str) -> Value {
    json!({ "sessionId": session_id })
}

/// Whether an agent-counter `protocolVersion` is tolerated (spine AD-19: the
/// pinned set {1}). Pure — unit-tested; the handshake's refusal decision.
pub fn protocol_version_is_tolerated(version: i64) -> bool {
    TOLERATED_PROTOCOL_VERSIONS.contains(&version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_response_notification_round_trip_through_the_codec() {
        // A request encodes to one line and parses back to what the agent
        // would see; a response and a notification parse into the right
        // inbound variants.
        let request = encode_request(7, "session/prompt", session_prompt_params("s-1", "hi"));
        assert!(!request.contains('\n'), "no embedded newline: {request}");
        // The agent's reply to that request parses back with the same id.
        let reply = encode_response(7, json!({"stopReason": "end_turn"}));
        match parse_line(&reply).unwrap().unwrap() {
            Inbound::Response { id, result } => {
                assert_eq!(id, 7);
                assert_eq!(result["stopReason"], "end_turn");
            }
            other => panic!("expected Response, got {other:?}"),
        }
        // An agent notification parses with its method + params.
        let update = encode_notification(
            "session/update",
            json!({"sessionId": "s-1", "update": {"sessionUpdate": "agent_message_chunk"}}),
        );
        match parse_line(&update).unwrap().unwrap() {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["update"]["sessionUpdate"], "agent_message_chunk");
            }
            other => panic!("expected Notification, got {other:?}"),
        }
        // An agent→client REQUEST (method + id) parses as AgentRequest — the
        // session/request_permission shape.
        let permission = format!(
            r#"{{"jsonrpc":"2.0","id":42,"method":"session/request_permission","params":{}}}"#,
            r#"{"sessionId":"s-1","options":[{"kind":"reject_once","name":"Deny","id":"d"}]}"#
        );
        match parse_line(&permission).unwrap().unwrap() {
            Inbound::AgentRequest { id, method, params } => {
                assert_eq!(id, 42);
                assert_eq!(method, "session/request_permission");
                assert_eq!(params["options"][0]["kind"], "reject_once");
            }
            other => panic!("expected AgentRequest, got {other:?}"),
        }
        // Requests and notifications are distinguishable by the id member on
        // the wire (the agent's parser relies on the same rule).
        assert!(request.contains("\"id\":7"), "{request}");
        assert!(!update.contains("\"id\""), "{update}");
    }

    #[test]
    fn every_encoded_envelope_is_exactly_one_newline_free_line() {
        // The framing purity invariant: a params payload carrying a newline
        // INSIDE a string must be escaped, not emitted raw.
        let line = encode_request(1, "session/prompt", session_prompt_params("s", "a\nb"));
        assert!(!line.contains('\n'), "newline must be escaped: {line}");
        assert!(line.contains("a\\nb"), "escaped form present: {line}");
        let notification = encode_notification("session/cancel", session_cancel_params("s"));
        assert!(!notification.contains('\n'));
        assert!(notification.ends_with('}'));
        assert_eq!(LINE_TERMINATOR, "\n");
    }

    #[test]
    fn malformed_lines_surface_typed_errors_and_blank_lines_are_skipped() {
        assert_eq!(parse_line("   ").unwrap(), None);
        assert_eq!(parse_line("").unwrap(), None);
        assert_eq!(
            parse_line("not json at all").unwrap_err(),
            CodecError::NotJson
        );
        assert_eq!(parse_line("[1,2,3]").unwrap_err(), CodecError::NotAnObject);
        // A JSON object with neither method nor id is unclassifiable.
        assert_eq!(
            parse_line("{\"x\":1}").unwrap_err(),
            CodecError::Unclassifiable
        );
        // A response with a non-integer id is unusable (surfaced, skipped).
        assert_eq!(
            parse_line("{\"id\":\"abc\",\"result\":null}").unwrap_err(),
            CodecError::UnusableId
        );
    }

    #[test]
    fn error_responses_parse_with_code_and_message() {
        let line =
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"method not found"}}"#;
        match parse_line(line).unwrap().unwrap() {
            Inbound::ResponseError { id, code, message } => {
                assert_eq!(id, 3);
                assert_eq!(code, -32601);
                assert_eq!(message, "method not found");
            }
            other => panic!("expected ResponseError, got {other:?}"),
        }
        // A null result still parses as a successful response (JSON-RPC null).
        match parse_line(r#"{"id":4,"result":null}"#).unwrap().unwrap() {
            Inbound::Response { id, result } => {
                assert_eq!(id, 4);
                assert!(result.is_null());
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn protocol_version_tolerance_is_the_pinned_set_one() {
        // Spine AD-19: the tolerated set is {1}; anything else refuses.
        assert!(protocol_version_is_tolerated(1));
        assert!(!protocol_version_is_tolerated(0));
        assert!(!protocol_version_is_tolerated(2));
        assert!(!protocol_version_is_tolerated(-1));
    }

    #[test]
    fn handshake_and_session_param_builders_carry_the_default_shape() {
        // The initialize params advertise NOTHING (D3): fs reads/writes false,
        // no terminal, and the hekma clientInfo.
        let params = initialize_params();
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientCapabilities"]["fs"]["readFile"], false);
        assert_eq!(params["clientCapabilities"]["fs"]["writeFile"], false);
        assert_eq!(params["clientCapabilities"]["terminal"], false);
        assert_eq!(params["clientInfo"]["name"], CLIENT_INFO_NAME);
        // session/new carries the cwd + an EMPTY mcpServers array.
        let new_params = session_new_params(std::path::Path::new("/home/agent"));
        assert_eq!(new_params["cwd"], "/home/agent");
        assert_eq!(new_params["mcpServers"], json!([]));
        // session/prompt carries ONE text ContentBlock.
        let prompt = session_prompt_params("s-9", "hello");
        assert_eq!(prompt["sessionId"], "s-9");
        assert_eq!(prompt["prompt"][0]["type"], "text");
        assert_eq!(prompt["prompt"][0]["text"], "hello");
        // session/cancel names the session and nothing else.
        assert_eq!(session_cancel_params("s-9"), json!({"sessionId": "s-9"}));
    }
}
