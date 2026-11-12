//! `hekma run` — Runs as first-class entities (story 21-2, PRD FR-43 /
//! FR-49): open a child Run under a supervised Run, close it, and show any
//! Run. A child Run is a Host's unit of work inside an agent's process: its
//! usage rolls up to every ancestor, and every ancestor's ceilings apply.
//!
//! Results go to stdout (a table, or the `--json` document), diagnostics to
//! stderr (AD-12). Listing every Run is `hekma ps`'s job.
//!
//! One data-path owner (AD-20): `open` and `close` are writes, routed to the
//! state root's local Service Host (`POST /v0/runs`,
//! `POST /v0/runs/{id}/close`), found or started as every `hekma agent`
//! write is; `show` reads `state.db` read-only and starts nothing.

use hekma_engine::{BreachAction, ChildRunRequest, RunError, RunLimits, RunParent, RunView};
use serde::Serialize;
use serde_json::{json, Value};

use super::agent::{open_read_only_engine, route, RoutedError};
use super::host_client::{segment, CallError};
use crate::error::{AgentIo, AgentNotFound, AgentStore, RunInvalid, RunInvalidState, RunNotFound};
use crate::ui;

/// The `--json` document for one Run.
#[derive(Serialize)]
struct RunDocument<'a> {
    schema_version: u32,
    run: &'a RunView,
}

/// The `--json` document's schema version (the Run document family).
const RUN_DOCUMENT_SCHEMA_VERSION: u32 = 1;

/// The flags of `hekma run open`.
pub struct OpenArgs {
    pub parent: String,
    pub labels: Vec<String>,
    pub token_budget: Option<u64>,
    pub dollar_cap: Option<String>,
    pub breach_action: Option<String>,
    pub external_id: Option<String>,
    pub json: bool,
}

/// `hekma run open --parent <run-id|instance> …` — open a child Run.
pub fn open(args: OpenArgs) -> Result<(), Box<dyn std::error::Error>> {
    let request = open_request(&args)?;
    let mut body = json!({
        "labels": request
            .labels
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<String, String>>(),
    });
    match &request.parent {
        RunParent::Run(id) => body["parent_run_id"] = json!(id.as_str()),
        RunParent::Instance(name) => body["parent_instance"] = json!(name),
    }
    if let Some(tokens) = args.token_budget {
        body["token_budget"] = json!(tokens);
    }
    if let Some(cap) = &args.dollar_cap {
        body["dollar_cap"] = json!(cap);
    }
    if let Some(action) = request.limits.breach_action {
        body["breach_action"] = json!(action.as_str());
    }
    if let Some(external) = &args.external_id {
        body["external_id"] = json!(external);
    }
    let view = routed_run(route("POST", "/v0/runs", Some(body)))?;
    render(&view, args.json, "opened")
}

/// `hekma run close <run-id>` — close a child Run.
pub fn close(run_id: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let view = routed_run(route(
        "POST",
        &format!("/v0/runs/{}/close", segment(run_id)),
        None,
    ))?;
    render(&view, json, "closed")
}

/// `hekma run show <run-id>` — one Run, read-only.
pub fn show(run_id: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_read_only_engine()?;
    let view = engine.blocking().run(run_id).map_err(map_run_error)?;
    render(&view, json, "")
}

/// The Run in a routed write's answer (`{"run": …}`), or the refusal mapped
/// onto the documented exit classes.
fn routed_run(
    routed: Result<super::agent::Routed, RoutedError>,
) -> Result<RunView, Box<dyn std::error::Error>> {
    let routed = match routed {
        Ok(routed) => routed,
        Err(RoutedError::Call(CallError::Coded { code, message })) => {
            return Err(map_run_code(&code, message))
        }
        Err(err) => return Err(err.into_diagnostic()),
    };
    routed
        .value
        .as_ref()
        .and_then(|document| document.get("run"))
        .cloned()
        .and_then(|run: Value| serde_json::from_value(run).ok())
        .ok_or_else(|| {
            AgentIo {
                message: format!(
                    "The local Service Host (pid {}) applied the command but sent no Run in \
                     its answer; check it with: hekma run show <run-id>",
                    routed.host_pid
                ),
            }
            .into()
        })
}

/// Map a Run route's `error.code` onto the exit classes [`map_run_error`]
/// uses in-process (shared by `hekma run` and `hekma label`).
pub(crate) fn map_run_code(code: &str, message: String) -> Box<dyn std::error::Error> {
    match code {
        "run-not-found" => RunNotFound {
            message: format!("{message}. Check the id with `hekma run show <run-id>`."),
        }
        .into(),
        "instance-not-found" => instance_not_found(message),
        // Story 21-3.
        "policy-not-found" => RunNotFound {
            message: format!("{message}. List them with `hekma policy list`."),
        }
        .into(),
        "policy-exists" => RunInvalid { message }.into(),
        // Story 21-3: the default Policy's reserved shape is an Operator
        // refusal (2); a Scent-Check Mode before Scent-Check ships (2).
        "policy-default" | "scent-unavailable" => RunInvalid { message }.into(),
        "nesting-too-deep" | "action-unavailable" | "invalid-request" | "too-many-runs"
        // Story 21-6.
        | "scope-unavailable" => RunInvalid { message }.into(),
        "no-open-run" | "parent-ended" | "owns-process" | "run-ended" | "label-from-config"
        | "label-not-bound" | "label-bound"
        // Story 21-6.
        | "verb-unavailable" | "raise-forbidden" => RunInvalidState { message }.into(),
        _ => AgentStore { message }.into(),
    }
}

/// No agent has the name (exit `3`, the not-found class).
fn instance_not_found(message: String) -> Box<dyn std::error::Error> {
    AgentNotFound {
        message: format!("{message}. List the agents with: hekma agent list"),
    }
    .into()
}

/// Build the engine request from the flags (no engine needed, so a bad flag
/// is refused before the state directory is opened).
fn open_request(args: &OpenArgs) -> Result<ChildRunRequest, Box<dyn std::error::Error>> {
    // A run id names a Run; anything else names an instance's open Run.
    // Instance names may start with `run-`, so only the two run-id shapes
    // count: `run_` + a ULID, or the legacy `run-<nanos>-<nonce>`.
    let parent = if looks_like_run_id(&args.parent) {
        RunParent::Run(hekma_engine::RunId::from_wire(args.parent.clone()))
    } else {
        RunParent::Instance(args.parent.clone())
    };
    let mut labels = Vec::with_capacity(args.labels.len());
    for raw in &args.labels {
        let Some((key, value)) = raw.split_once('=') else {
            return Err(invalid(format!(
                "--label `{raw}` must be key=value (for example --label team=payments)"
            )));
        };
        labels.push((key.to_string(), value.to_string()));
    }
    let breach_action = args
        .breach_action
        .as_deref()
        .map(str::parse::<BreachAction>)
        .transpose()
        .map_err(|err| invalid(format!("--breach-action: {err}")))?;
    let dollar_per_run_micros = args
        .dollar_cap
        .as_deref()
        .map(hekma_engine::parse_dollars_to_micros)
        .transpose()
        .map_err(|reason| invalid(format!("--dollar-cap: {reason}")))?;
    Ok(ChildRunRequest {
        parent,
        labels,
        limits: RunLimits {
            token_per_run: args.token_budget,
            dollar_per_run_micros,
            breach_action,
            ..RunLimits::default()
        },
        external_id: args.external_id.clone(),
    })
}

/// Whether `raw` has a run id's shape (a ULID id, or a legacy id).
pub(crate) fn looks_like_run_id(raw: &str) -> bool {
    if hekma_engine::RunId::from_wire(raw).is_ulid() {
        return true;
    }
    raw.strip_prefix("run-").is_some_and(|rest| {
        let mut parts = rest.split('-');
        let digits = |part: Option<&str>| {
            part.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        };
        digits(parts.next()) && digits(parts.next()) && parts.next().is_none()
    })
}

fn invalid(message: String) -> Box<dyn std::error::Error> {
    RunInvalid { message }.into()
}

/// Map a [`RunError`] to the documented exit classes: not found `3`, a
/// request that cannot be honored `2`, a state that forbids it `4`, a store
/// failure `1`.
fn map_run_error(err: RunError) -> Box<dyn std::error::Error> {
    let message = err.to_string();
    run_error_class(&err, message)
}

/// The diagnostic carrying `err`'s exit class with `message`: not found `3`,
/// a request that cannot be honored `2`, a state that forbids it `4`, a
/// store failure `1`. Shared by `hekma run` and `hekma label` (which reports
/// a partial failure under its first failure's class).
pub(crate) fn run_error_class(err: &RunError, message: String) -> Box<dyn std::error::Error> {
    match err {
        RunError::NotFound { .. } => RunNotFound {
            message: format!("{message}. Check the id with `hekma run show <run-id>`."),
        }
        .into(),
        RunError::InstanceNotFound { .. } => instance_not_found(message),
        // Story 21-3: an unknown Policy is not found (3); a taken name is a
        // request that cannot be honored (2).
        RunError::PolicyNotFound { .. } => RunNotFound {
            message: format!("{message}. List them with `hekma policy list`."),
        }
        .into(),
        RunError::PolicyExists { .. } => RunInvalid { message }.into(),
        // Story 21-3: the default Policy's reserved shape is a refusal the
        // Operator fixes on the command line (2), and a Scent-Check Mode
        // before Scent-Check ships likewise (2).
        RunError::PolicyDefault { .. } | RunError::ScentUnavailable { .. } => {
            RunInvalid { message }.into()
        }
        RunError::NestingTooDeep { .. }
        | RunError::ActionUnavailable { .. }
        | RunError::TooManyRuns { .. }
        // Story 21-5: a listing cursor that is not the codec's form.
        | RunError::InvalidCursor { .. }
        | RunError::Invalid { .. } => RunInvalid { message }.into(),
        RunError::NoOpenRun { .. }
        | RunError::ParentEnded { .. }
        | RunError::OwnsProcess { .. }
        | RunError::RunEnded { .. }
        | RunError::LabelFromConfig { .. }
        | RunError::LabelNotBound { .. }
        | RunError::LabelBound { .. }
        // Story 21-6: the Run's depth does not serve the verb yet, and a
        // Caller may only tighten (the Run's state forbids the change).
        | RunError::VerbUnavailable { .. }
        | RunError::RaiseForbidden { .. } => RunInvalidState { message }.into(),
        // Story 21-6: a limit scope the Run cannot carry (the request).
        RunError::ScopeUnavailable { .. } => RunInvalid { message }.into(),
        RunError::Store(_) => AgentStore { message }.into(),
    }
}

fn render(view: &RunView, json: bool, verb: &str) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        let document = RunDocument {
            schema_version: RUN_DOCUMENT_SCHEMA_VERSION,
            run: view,
        };
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }
    print!("{}", render_text(view, verb));
    Ok(())
}

/// The human rendering: a headline, then one field per line.
fn render_text(view: &RunView, verb: &str) -> String {
    let record = &view.record;
    let mut out = String::new();
    let headline = if verb.is_empty() {
        format!("Run {}", record.run_id)
    } else {
        format!("Run {} {verb}", record.run_id)
    };
    out.push_str(&ui::success_text(headline));
    out.push('\n');
    let dash = "—".to_string();
    let mut row = |label: &str, value: String| {
        out.push_str(&format!("  {label:<12} {value}\n"));
    };
    row(
        "CONTROL",
        record.control_depth.display_with_wire(record.wire_path),
    );
    row("STATUS", record.status.as_str().to_string());
    row(
        "ANOMALIES",
        if record.anomalies.is_empty() {
            dash.clone()
        } else {
            record
                .anomalies
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    row(
        "PARENT",
        record
            .parent_run_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| dash.clone()),
    );
    row(
        "INSTANCE",
        record.instance.clone().unwrap_or_else(|| dash.clone()),
    );
    row("LABELS", crate::cli::label::render_labels(&view.labels));
    row("LIMITS", render_limits(&view.limits));
    row(
        "USAGE",
        format!(
            "{} tokens ({} in / {} out), {}",
            view.usage
                .input_tokens
                .saturating_add(view.usage.output_tokens),
            view.usage.input_tokens,
            view.usage.output_tokens,
            hekma_engine::render_dollars(view.usage.cost_micros, view.usage.estimate),
        ),
    );
    row("CHILDREN", format!("{} open", view.open_children));
    if let Some(cause) = &record.end_cause {
        row("ENDED", cause.clone());
    }
    out
}

fn render_limits(limits: &RunLimits) -> String {
    let mut parts = Vec::new();
    if let Some(tokens) = limits.token_per_run {
        parts.push(format!("{tokens} tokens per run"));
    }
    if let Some(cap) = limits.dollar_per_run_micros {
        parts.push(format!(
            "{} per run",
            hekma_engine::render_dollars_bare(cap)
        ));
    }
    if parts.is_empty() {
        return "—".to_string();
    }
    format!("{} ({})", parts.join(", "), limits.action().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parent: &str) -> OpenArgs {
        OpenArgs {
            parent: parent.to_string(),
            labels: vec!["task=review".to_string()],
            token_budget: Some(500),
            dollar_cap: Some("1.50".to_string()),
            breach_action: Some("warn".to_string()),
            external_id: Some("job-1".to_string()),
            json: false,
        }
    }

    #[test]
    fn the_parent_flag_names_a_run_or_an_instance() {
        let request = open_request(&args("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T")).unwrap();
        assert!(matches!(request.parent, RunParent::Run(_)));
        let request = open_request(&args("run-1730000000000000000-7")).unwrap();
        assert!(matches!(request.parent, RunParent::Run(_)));
        // An instance may be named like a run id prefix.
        for name in ["run-bot", "run_short", "run-12-ab"] {
            let request = open_request(&args(name)).unwrap();
            assert_eq!(request.parent, RunParent::Instance(name.to_string()));
        }
        let request = open_request(&args("my-agent")).unwrap();
        assert_eq!(request.parent, RunParent::Instance("my-agent".to_string()));
        assert_eq!(request.labels, vec![("task".into(), "review".into())]);
        assert_eq!(request.limits.token_per_run, Some(500));
        assert_eq!(
            request.limits.dollar_per_run_micros,
            Some(hekma_engine::Micros(1_500_000))
        );
        assert_eq!(request.limits.breach_action, Some(BreachAction::Warn));
        assert_eq!(request.external_id.as_deref(), Some("job-1"));
    }

    #[test]
    fn malformed_flags_are_usage_errors() {
        let mut bad = args("a");
        bad.labels = vec!["no-equals".to_string()];
        assert!(open_request(&bad).unwrap_err().is::<RunInvalid>());
        let mut bad = args("a");
        bad.breach_action = Some("explode".to_string());
        assert!(open_request(&bad).unwrap_err().is::<RunInvalid>());
        let mut bad = args("a");
        bad.dollar_cap = Some("-1".to_string());
        assert!(open_request(&bad).unwrap_err().is::<RunInvalid>());
    }

    #[test]
    fn host_codes_map_like_the_engine_errors() {
        assert!(map_run_code("run-not-found", "x".into()).is::<RunNotFound>());
        for code in ["nesting-too-deep", "action-unavailable", "invalid-request"] {
            assert!(map_run_code(code, "x".into()).is::<RunInvalid>(), "{code}");
        }
        for code in ["no-open-run", "parent-ended", "owns-process"] {
            assert!(
                map_run_code(code, "x".into()).is::<RunInvalidState>(),
                "{code}"
            );
        }
        assert!(map_run_code("store", "x".into()).is::<AgentStore>());
        let missing = routed_run(Ok(super::super::agent::Routed {
            value: Some(json!({"schema_version": 1})),
            host_pid: 7,
        }));
        assert!(missing.unwrap_err().is::<AgentIo>());
        let coded = routed_run(Err(RoutedError::Call(CallError::Coded {
            code: "parent-ended".into(),
            message: "ended".into(),
        })));
        assert!(coded.unwrap_err().is::<RunInvalidState>());
    }

    #[test]
    fn run_errors_map_to_the_documented_exit_classes() {
        let not_found = map_run_error(RunError::NotFound {
            run_id: "run_X".into(),
        });
        assert!(not_found.is::<RunNotFound>());
        let invalid = map_run_error(RunError::NestingTooDeep {
            parent: "p".into(),
            depth: 9,
            max: 8,
        });
        assert!(invalid.is::<RunInvalid>());
        let state = map_run_error(RunError::ParentEnded { run_id: "p".into() });
        assert!(state.is::<RunInvalidState>());
        let store = map_run_error(RunError::Store(hekma_engine::ports::StoreError::Backend(
            "x".into(),
        )));
        assert!(store.is::<AgentStore>());
    }
}
