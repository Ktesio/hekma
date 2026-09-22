//! The ACP client-side protocol driver (spine AD-19): the `initialize` →
//! `session/new` handshake and the `session/request_permission` denial
//! policy.
//!
//! The client is deliberately a SMALL state machine, not a framework (the
//! epic-14 Design Notes): request ids from a counter, request-response
//! correlation via the connection's pending map, and one synchronous
//! BOUNDED handshake driven by the start path (AD-17's bounded-work rule —
//! the handshake waits on the child, i.e. on external state, so it carries
//! an explicit deadline; a handshake that outlives the bound refuses the
//! start, surfaced and traffic-free).
//!
//! Traffic-free discipline: every error names the ACP method and, where a
//! version counter is involved, the integer values — NEVER prompt text,
//! file contents, or other params payloads.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::codec;
use super::connection::AcpConnection;

/// How long the WHOLE handshake (`initialize` round trip + `session/new`
/// round trip) may take before the start refuses (spine AD-19 "version
/// counter outside the tolerated set → close + surfaced refusal"; the
/// timeout refusal is the same close-and-surface shape). A bound, not a
/// target: a compliant agent answers in milliseconds. Chosen generously
/// over the readiness window's scale (a cold binary may page in) while
/// keeping a hung agent from stalling a bounded start — the same
/// AI-59-style deliberate, ACCEPTED, BOUNDED trade the stdin write carries.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why the handshake failed (traffic-free — method names + integers only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// The child did not answer a method within [`HANDSHAKE_TIMEOUT`].
    Timeout {
        /// The ACP method that went unanswered.
        method: &'static str,
    },
    /// The request could not be WRITTEN to the child's stdin (bounded-write
    /// timeout or a broken pipe).
    WriteFailed {
        /// The ACP method that could not be sent.
        method: &'static str,
        /// The bounded-write failure detail (backend-authored, traffic-free).
        detail: String,
    },
    /// The agent countered with a protocol version outside the tolerated
    /// set (spine AD-19: `{1}`). Names both integers.
    VersionMismatch {
        /// The version the agent countered with.
        countered: i64,
        /// The versions this engine tolerates (rendered, e.g. `"1"`).
        tolerated: String,
    },
    /// The agent's response lacked a usable `protocolVersion`.
    MissingProtocolVersion,
    /// The agent's `initialize` response lacked a parseable capabilities
    /// object (recorded as no-capabilities; a hard failure only because the
    /// response shape is unusable for the session step).
    MalformedResponse {
        /// The ACP method whose response was unusable.
        method: &'static str,
        /// The shape problem (traffic-free).
        detail: String,
    },
    /// The `session/new` response carried no usable `sessionId`.
    MissingSessionId,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Timeout { method } => write!(
                f,
                "the agent did not answer '{method}' within {}s",
                HANDSHAKE_TIMEOUT.as_secs()
            ),
            HandshakeError::WriteFailed { method, detail } => {
                write!(f, "could not send '{method}' to the agent: {detail}")
            }
            HandshakeError::VersionMismatch {
                countered,
                tolerated,
            } => write!(
                f,
                "the agent countered ACP protocol version {countered}, which is outside the \
                 tolerated set {{{tolerated}}}; refusing to run under an unpinned protocol"
            ),
            HandshakeError::MissingProtocolVersion => {
                write!(
                    f,
                    "the agent's initialize response carried no usable protocolVersion"
                )
            }
            HandshakeError::MalformedResponse { method, detail } => {
                write!(f, "the agent's '{method}' response was unusable: {detail}")
            }
            HandshakeError::MissingSessionId => {
                write!(f, "the agent's session/new response carried no sessionId")
            }
        }
    }
}

/// What the handshake established (recorded in memory for the Run; the session
/// id is ALSO persisted to the spawn record by the caller — story 14-2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    /// The ACP session id the agent minted for `session/new`, or the resumed
    /// id accepted by `session/load`.
    pub session_id: String,
    /// Whether the agent advertised the `loadSession` capability.
    pub load_session: bool,
    /// Story 14-2 (D4): `true` when the session was RESUMED — a persisted id
    /// was offered via `session/load` and the agent accepted it.
    pub resumed: bool,
    /// Story 14-2: WHY a persisted session id was NOT resumed, when one was
    /// offered — `"the agent does not support resuming"` (no `loadSession`
    /// capability) or `"session/load failed (...)"` (the agent refused / the
    /// round trip failed). `None` when there was no persisted id (a first
    /// start — nothing was expected, so no note) or when the load succeeded.
    /// The caller surfaces this as the one honest stderr note (AI-18).
    pub resume_declined: Option<String>,
}

/// Drive the full handshake over `conn` (synchronous, bounded by
/// [`HANDSHAKE_TIMEOUT`] ACROSS all round trips). Order per spine AD-19:
/// `initialize` (version 1, DEFAULT client capabilities, clientInfo) →
/// validate the agent's counter against the tolerated set → record
/// `agentCapabilities.loadSession` → **the 14-2 resume decision** →
/// `session/new` (cwd = the Agent Home, `mcpServers: []`) unless the session
/// was resumed.
///
/// `resume` (story 14-2, D4) is the session id PERSISTED by a previous Run
/// (`None` on a first start — no resume is attempted, and no note is owed).
/// The decision order:
/// 1. persisted id + `loadSession` advertised → `session/load`; on success the
///    session IS the persisted id (`resumed: true`);
/// 2. persisted id, not advertised → `session/new` +
///    `resume_declined = "the agent does not support resuming"`;
/// 3. persisted id, load fails (error response, write failure, or no answer
///    in time) → `session/new` + a `resume_declined` naming the failure — a
///    load failure is NEVER fatal to the start (the fresh session still
///    supervises, spine AD-19).
pub fn handshake(
    conn: &AcpConnection,
    cwd: &Path,
    resume: Option<&str>,
) -> Result<Handshake, HandshakeError> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let remaining = || deadline.saturating_duration_since(Instant::now());

    // (1) initialize. The waiter is registered BEFORE the write so a fast
    // agent's reply cannot outrun it.
    let init_id = conn.next_request_id();
    let init_rx = conn.register_waiter(init_id);
    let line = codec::encode_request(init_id, "initialize", codec::initialize_params());
    conn.write_line(&line)
        .map_err(|err| HandshakeError::WriteFailed {
            method: "initialize",
            detail: err.to_string(),
        })?;
    let result = conn
        .await_response(init_rx, remaining())
        .ok_or(HandshakeError::Timeout {
            method: "initialize",
        })?
        .map_err(|message| HandshakeError::MalformedResponse {
            method: "initialize",
            detail: format!("the agent did not answer usably: {message}"),
        })?;

    // (2) Validate the version counter against the tolerated set {1}.
    let countered = result.get("protocolVersion").and_then(Value::as_i64);
    let Some(countered) = countered else {
        return Err(HandshakeError::MissingProtocolVersion);
    };
    if !codec::protocol_version_is_tolerated(countered) {
        return Err(HandshakeError::VersionMismatch {
            countered,
            tolerated: codec::TOLERATED_PROTOCOL_VERSIONS
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        });
    }

    // (3) Record agentCapabilities.loadSession (tolerant: absent → false).
    let load_session = result
        .get("agentCapabilities")
        .and_then(|caps| caps.get("loadSession"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    conn.set_load_session(load_session);

    // (3b) The 14-2 RESUME DECISION (D4) — the order documented on this
    // function. The load round trip shares the handshake's single deadline
    // (the total stays bounded), and ANY load failure only sets
    // `resume_declined` — the fallback below still opens a fresh session, so
    // the start proceeds (surfaced-not-silent, never fatal).
    let mut resumed = false;
    let mut resume_declined: Option<String> = None;
    if let Some(session_id) = resume {
        if load_session {
            let load_id = conn.next_request_id();
            let load_rx = conn.register_waiter(load_id);
            let line = codec::encode_request(
                load_id,
                "session/load",
                codec::session_load_params(session_id, cwd),
            );
            let outcome = conn
                .write_line(&line)
                .map_err(|err| format!("could not send it: {err}"))
                .and_then(|()| {
                    conn.await_response(load_rx, remaining())
                        .ok_or_else(|| "the agent did not answer it in time".to_string())?
                });
            match outcome {
                Ok(_) => {
                    // The agent accepted the persisted id: the session IS
                    // that id (an ACP v1 load result carries no replacement
                    // id; the agent replays history as session/update
                    // notifications, which land in the ordinary record).
                    conn.set_session_id(session_id.to_string());
                    resumed = true;
                }
                Err(detail) => {
                    // Traffic-free detail (the response router maps error
                    // responses to "error response (code N)"; payloads never
                    // ride a diagnostic).
                    resume_declined = Some(format!(
                        "session/load failed ({detail}); a new session opens"
                    ));
                }
            }
        } else {
            resume_declined =
                Some("the agent does not support resuming (no loadSession capability)".to_string());
        }
    }

    // (4) session/new (cwd = the Agent Home, no MCP servers) — same
    // register-before-write ordering; SKIPPED when the session was resumed.
    let session_id = if resumed {
        // The accepted id was already recorded on the connection.
        resume
            .expect("resumed implies a persisted id was offered")
            .to_string()
    } else {
        let new_id = conn.next_request_id();
        let new_rx = conn.register_waiter(new_id);
        let line = codec::encode_request(new_id, "session/new", codec::session_new_params(cwd));
        conn.write_line(&line)
            .map_err(|err| HandshakeError::WriteFailed {
                method: "session/new",
                detail: err.to_string(),
            })?;
        let result = conn
            .await_response(new_rx, remaining())
            .ok_or(HandshakeError::Timeout {
                method: "session/new",
            })?
            .map_err(|message| HandshakeError::MalformedResponse {
                method: "session/new",
                detail: format!("the agent did not answer usably: {message}"),
            })?;
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(session_id) = session_id else {
            return Err(HandshakeError::MissingSessionId);
        };
        conn.set_session_id(session_id.clone());
        session_id
    };

    Ok(Handshake {
        session_id,
        load_session,
        resumed,
        resume_declined,
    })
}

/// The `session/request_permission` DENIAL policy (spine AD-19 / D3): the
/// engine advertises no fs/terminal/elicitation, so an incoming permission
/// request is answered DENIED — the denial option the agent offered if one
/// fits, else `cancelled`. Returns the response `result` payload to encode.
///
/// Pure + tolerant (unit-tested): options may be absent/malformed (→
/// cancelled); the denial pick is the FIRST option whose `kind` starts with
/// `reject`; the reply echoes the option's `id` when present, else its
/// `name` (the two identity fields ACP v1 agents use).
pub fn permission_denial_result(params: &Value) -> Value {
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for option in &options {
        let kind = option.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind.starts_with("reject") {
            let identity = option
                .get("id")
                .or_else(|| option.get("name"))
                .cloned()
                .unwrap_or(Value::Null);
            return serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": identity },
            });
        }
    }
    // No denial option fits: cancel the request (the agent aborts the turn,
    // which surfaces through the ordinary stopReason path).
    serde_json::json!({ "outcome": { "outcome": "cancelled" } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn denial_picks_the_first_reject_option_and_echoes_its_identity() {
        // A deny option with an id: selected with THAT id.
        let params = json!({
            "sessionId": "s-1",
            "options": [
                { "kind": "allow_once", "name": "Allow", "id": "opt-allow" },
                { "kind": "reject_once", "name": "Deny", "id": "opt-deny" },
            ],
        });
        let result = permission_denial_result(&params);
        assert_eq!(result["outcome"]["outcome"], "selected");
        assert_eq!(result["outcome"]["optionId"], "opt-deny");
        // An id-less deny option falls back to its name (tolerant identity).
        let idless = json!({
            "options": [ { "kind": "reject_always", "name": "Never" } ],
        });
        let result = permission_denial_result(&idless);
        assert_eq!(result["outcome"]["optionId"], "Never");
    }

    #[test]
    fn denial_without_a_reject_option_cancels() {
        // Only allow options: cancelled (D3 — we never allow).
        let allow_only = json!({
            "options": [ { "kind": "allow_once", "name": "Allow", "id": "a" } ],
        });
        assert_eq!(
            permission_denial_result(&allow_only),
            json!({ "outcome": { "outcome": "cancelled" } })
        );
        // Absent / malformed options likewise cancel — never a panic, never
        // an allow.
        assert_eq!(
            permission_denial_result(&json!({})),
            json!({ "outcome": { "outcome": "cancelled" } })
        );
        assert_eq!(
            permission_denial_result(&json!({ "options": "garbage" })),
            json!({ "outcome": { "outcome": "cancelled" } })
        );
        // An option with no kind never matches the denial pick.
        let kindless = json!({ "options": [ { "name": "mystery", "id": "m" } ] });
        assert_eq!(
            permission_denial_result(&kindless),
            json!({ "outcome": { "outcome": "cancelled" } })
        );
    }

    #[test]
    fn handshake_errors_render_traffic_free_naming_methods_and_integers_only() {
        // The diagnostics name methods + version integers; the render path is
        // what the start refusal surfaces, so pin its shape here.
        let timeout = HandshakeError::Timeout {
            method: "initialize",
        };
        assert!(timeout.to_string().contains("initialize"));
        let mismatch = HandshakeError::VersionMismatch {
            countered: 2,
            tolerated: "1".to_string(),
        };
        let text = mismatch.to_string();
        assert!(text.contains('2') && text.contains("1"), "{text}");
        assert!(text.contains("tolerated"), "{text}");
        let write = HandshakeError::WriteFailed {
            method: "session/new",
            detail: "bounded write timed out".to_string(),
        };
        assert!(write.to_string().contains("session/new"), "{write}");
    }
}
