//! `hekma label` — bound labels on Runs (story 21-9, PRD FR-49 / FR-50).
//!
//! `set` binds labels on one Run, an agent's open Run, or every open Run a
//! Selector matches; `remove` unbinds labels the verb set. A label set here
//! is bound with origin `label-verb`: it outranks the agent's `labels`
//! configuration and any label the Run set about itself, and only bound
//! labels select Policies.
//!
//! Results go to stdout (one line per Run, or the `--json` document),
//! diagnostics and refusals to stderr (AD-12). A Selector is resolved here,
//! then the verb acts on exactly the listed Runs (FR-50).
//!
//! One data-path owner (AD-20): the target and the Selector are read from
//! `state.db` read-only, and so is a `--dry-run` (it writes nothing, so it
//! starts no Service Host); each write is one `PATCH /v0/runs/{id}/labels`
//! routed to the state root's local Service Host, found or started as every
//! `hekma agent` write is.

use std::io::{BufRead, IsTerminal, Write};

use hekma_engine::{
    Engine, Label, LabelChange, LabelOrigin, LabelOutcome, LabelSource, RunError, Selector,
};
use serde::Serialize;
use serde_json::json;

use super::agent::{open_read_only_engine, route, RoutedError};
use super::host_client::{segment, CallError};
use crate::cli::run::{looks_like_run_id, map_run_code, run_error_class};
use crate::error::{AgentIo, RunInvalid};
use crate::ui;

/// The `--json` document's schema version (the label-results family).
const LABEL_DOCUMENT_SCHEMA_VERSION: u32 = 1;

/// Which label verb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelVerb {
    /// `hekma label set`.
    Set,
    /// `hekma label remove`.
    Remove,
}

/// The arguments of `hekma label set|remove`.
pub struct LabelArgs {
    pub verb: LabelVerb,
    /// `--selector key=value[,key=value…]`.
    pub selector: Option<String>,
    /// Positionals: the target (without `--selector`), then the labels
    /// (`key=value` for set, `key` for remove).
    pub args: Vec<String>,
    pub dry_run: bool,
    pub yes: bool,
    pub json: bool,
}

/// What the verb acts on.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// One Run by id.
    Run(String),
    /// An agent's open Run.
    Instance(String),
    /// Every open Run a Selector matches.
    Selector(Selector),
}

/// One Run's result.
#[derive(Serialize)]
struct RunResult {
    run_id: String,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<Vec<Label>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prior_labels: Option<Vec<Label>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ResultError>,
}

#[derive(Serialize)]
struct ResultError {
    code: String,
    message: String,
}

/// Why one Run's write failed: the engine's refusal (planned locally, or
/// decoded from the host's `error.code`), or a host-level failure.
struct Refusal {
    code: String,
    message: String,
    diagnostic: Box<dyn std::error::Error>,
}

impl Refusal {
    fn from_run_error(err: RunError) -> Self {
        let message = err.to_string();
        Self {
            code: err.code().to_string(),
            diagnostic: run_error_class(&err, message.clone()),
            message,
        }
    }

    fn from_routed(err: RoutedError) -> Self {
        match err {
            RoutedError::Call(CallError::Coded { code, message }) => Self {
                diagnostic: map_run_code(&code, message.clone()),
                code,
                message,
            },
            other => {
                let diagnostic = other.into_diagnostic();
                Self {
                    code: "service-host".to_string(),
                    message: diagnostic.to_string(),
                    diagnostic,
                }
            }
        }
    }
}

/// The `--json` document.
#[derive(Serialize)]
struct LabelDocument {
    schema_version: u32,
    dry_run: bool,
    matched: usize,
    results: Vec<RunResult>,
}

/// `hekma label set|remove …`.
pub fn run(args: LabelArgs) -> Result<(), Box<dyn std::error::Error>> {
    let (target, change) = parse(&args)?;
    let engine = open_read_only_engine()?;
    let facade = engine.blocking();
    let run_ids = match &target {
        // Read first, so an unknown id is refused without starting a host.
        Target::Run(id) => vec![facade
            .run(id)
            .map_err(|err| run_error_class(&err, err.to_string()))?
            .record
            .run_id
            .to_string()],
        Target::Instance(name) => vec![facade
            .instance_run(name)
            .map_err(|err| run_error_class(&err, err.to_string()))?
            .record
            .run_id
            .to_string()],
        Target::Selector(selector) => facade
            .select_runs(selector)
            .map_err(|err| run_error_class(&err, err.to_string()))?
            .into_iter()
            .map(|record| record.run_id.to_string())
            .collect(),
    };
    if run_ids.is_empty() {
        if let Target::Selector(selector) = &target {
            ui::note(format!(
                "No open Run matches `{selector}`; nothing was labeled."
            ));
        }
        return finish(&args, 0, Vec::new(), None);
    }
    let selecting = matches!(target, Target::Selector(_));
    if selecting && run_ids.len() > 1 && !args.dry_run && !args.yes {
        let stdin = std::io::stdin();
        let confirmed = confirm(
            &run_ids,
            args.verb,
            stdin.is_terminal(),
            &mut stdin.lock(),
            &mut std::io::stderr(),
        )?;
        if !confirmed {
            ui::note("Nothing was labeled.");
            return finish(&args, run_ids.len(), Vec::new(), None);
        }
    }
    let mut results = Vec::with_capacity(run_ids.len());
    let mut first_failure: Option<Refusal> = None;
    let mut failures = 0usize;
    for run_id in &run_ids {
        match apply(&engine, run_id, &change, args.dry_run) {
            Ok(outcome) => results.push(success(&outcome)),
            Err(refusal) => {
                failures += 1;
                if selecting && !args.json {
                    eprintln!(
                        "{}",
                        ui::error_text(format!("Run {run_id} not labeled: {}", refusal.message))
                    );
                }
                results.push(RunResult {
                    run_id: run_id.clone(),
                    outcome: "refused",
                    labels: None,
                    prior_labels: None,
                    error: Some(ResultError {
                        code: refusal.code.clone(),
                        message: refusal.message.clone(),
                    }),
                });
                first_failure.get_or_insert(refusal);
            }
        }
    }
    let matched = run_ids.len();
    let failure = first_failure.map(|refusal| {
        if selecting {
            // Keep the first refusal's exit class, with the count.
            reclassed(
                refusal.diagnostic,
                format!(
                    "{failures} of {matched} Runs were not labeled; the first refusal: {}",
                    refusal.message
                ),
            )
        } else {
            refusal.diagnostic
        }
    });
    finish(&args, matched, results, failure)
}

/// Label one Run: a dry run plans on the read-only engine (nothing is
/// written, so no Service Host is started); a write is one
/// `PATCH /v0/runs/{id}/labels` routed to the local Service Host.
fn apply(
    engine: &Engine,
    run_id: &str,
    change: &LabelChange,
    dry_run: bool,
) -> Result<LabelOutcome, Refusal> {
    if dry_run {
        return engine
            .blocking()
            .label_run(run_id, change.clone(), true)
            .map_err(Refusal::from_run_error);
    }
    let body = json!({ "set": change.set, "remove": change.remove });
    let routed = route(
        "PATCH",
        &format!("/v0/runs/{}/labels", segment(run_id)),
        Some(body),
    )
    .map_err(Refusal::from_routed)?;
    let host_pid = routed.host_pid;
    routed
        .value
        .and_then(|document| serde_json::from_value::<LabelOutcome>(document).ok())
        .ok_or_else(|| {
            let message = format!(
                "the local Service Host (pid {host_pid}) applied the change but sent no labels \
                 in its answer; check them with: hekma run show {run_id}"
            );
            Refusal {
                code: "service-host".to_string(),
                diagnostic: AgentIo {
                    message: message.clone(),
                }
                .into(),
                message,
            }
        })
}

/// `diagnostic`'s exit class carrying `message` instead (a partial
/// failure's summary under its first refusal's class).
fn reclassed(
    diagnostic: Box<dyn std::error::Error>,
    message: String,
) -> Box<dyn std::error::Error> {
    use crate::error::{
        AgentNotFound, AgentStore, RunInvalidState, RunNotFound, ServiceHostRefused,
        ServiceHostUnavailable,
    };
    if diagnostic.is::<RunNotFound>() {
        RunNotFound { message }.into()
    } else if diagnostic.is::<RunInvalid>() {
        RunInvalid { message }.into()
    } else if diagnostic.is::<AgentNotFound>() {
        AgentNotFound { message }.into()
    } else if diagnostic.is::<RunInvalidState>() {
        RunInvalidState { message }.into()
    } else if diagnostic.is::<ServiceHostRefused>() {
        ServiceHostRefused { message }.into()
    } else if diagnostic.is::<ServiceHostUnavailable>() {
        ServiceHostUnavailable { message }.into()
    } else {
        AgentStore { message }.into()
    }
}

/// Print the results (human lines or the `--json` document) and return the
/// first failure's class, if any.
fn finish(
    args: &LabelArgs,
    matched: usize,
    results: Vec<RunResult>,
    failure: Option<Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if args.json {
        let document = LabelDocument {
            schema_version: LABEL_DOCUMENT_SCHEMA_VERSION,
            dry_run: args.dry_run,
            matched,
            results,
        };
        println!("{}", serde_json::to_string_pretty(&document)?);
    } else {
        for result in &results {
            if let Some(line) = render_result(result) {
                println!("{line}");
            }
        }
    }
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// A successful outcome as a result row.
fn success(outcome: &LabelOutcome) -> RunResult {
    let verb = match (outcome.changed, outcome.dry_run) {
        (false, _) => "unchanged",
        (true, false) => "labeled",
        (true, true) => "would-label",
    };
    RunResult {
        run_id: outcome.run_id.to_string(),
        outcome: verb,
        labels: Some(outcome.labels.clone()),
        prior_labels: Some(outcome.prior_labels.clone()),
        error: None,
    }
}

/// One human line per Run; a refusal renders nothing here (it went to
/// stderr, or is the returned error).
fn render_result(result: &RunResult) -> Option<String> {
    let labels = result.labels.as_deref()?;
    let rendered = render_labels(labels);
    Some(match result.outcome {
        "labeled" => ui::success_text(format!("Run {} labeled: {rendered}", result.run_id)),
        "would-label" => format!(
            "Run {} would label: {rendered} (now: {})",
            result.run_id,
            render_labels(result.prior_labels.as_deref().unwrap_or_default())
        ),
        _ => format!("Run {} unchanged: {rendered}", result.run_id),
    })
}

/// `key=value (bound, origin)` / `key=value (caller)`, or `—`.
pub(crate) fn render_labels(labels: &[Label]) -> String {
    if labels.is_empty() {
        return "—".to_string();
    }
    labels
        .iter()
        .map(|label| {
            let source = match (label.source, label.bound_origin()) {
                (LabelSource::Caller, _) => "caller".to_string(),
                (LabelSource::Bound, Some(LabelOrigin::LabelVerb)) => {
                    "bound, label-verb".to_string()
                }
                (LabelSource::Bound, _) => "bound, config".to_string(),
            };
            format!("{}={} ({source})", label.key, label.value)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Ask before acting on several Runs. Only a terminal stdin is asked (a
/// pipe is never read, so a script or test cannot hang): otherwise the
/// request is refused, naming `--yes` and `--dry-run`.
fn confirm(
    run_ids: &[String],
    verb: LabelVerb,
    is_terminal: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<bool, Box<dyn std::error::Error>> {
    let (action, question) = match verb {
        LabelVerb::Set => ("label", "Label"),
        LabelVerb::Remove => ("unlabel", "Unlabel"),
    };
    let mut listing = format!("The Selector matches {} Runs:\n", run_ids.len());
    for id in run_ids {
        listing.push_str(&format!("  {id}\n"));
    }
    write!(out, "{listing}")?;
    if !is_terminal {
        return Err(invalid(format!(
            "the Selector matches {} Runs and there is no terminal to confirm on — pass --yes \
             to {action} them all, or --dry-run to see each Run's change first",
            run_ids.len()
        )));
    }
    let count = run_ids.len();
    write!(out, "{question} these {count} Runs? [y/N] ")?;
    out.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Parse the target and the change (no engine needed, so a malformed
/// request is refused before the state directory is opened).
fn parse(args: &LabelArgs) -> Result<(Target, LabelChange), Box<dyn std::error::Error>> {
    let (target, labels) = match &args.selector {
        Some(raw) => {
            let selector =
                Selector::parse(raw).map_err(|reason| invalid(format!("--selector: {reason}")))?;
            (Target::Selector(selector), args.args.as_slice())
        }
        None => {
            let Some((first, rest)) = args.args.split_first() else {
                return Err(invalid(
                    "name a run id or an agent (or pass --selector), then the labels".to_string(),
                ));
            };
            let target = if looks_like_run_id(first) {
                Target::Run(first.clone())
            } else {
                Target::Instance(first.clone())
            };
            (target, rest)
        }
    };
    if labels.is_empty() {
        return Err(invalid(match args.verb {
            LabelVerb::Set => "name at least one label to set, as key=value".to_string(),
            LabelVerb::Remove => "name at least one label key to remove".to_string(),
        }));
    }
    let mut change = LabelChange::default();
    for raw in labels {
        match args.verb {
            LabelVerb::Set => {
                let Some((key, value)) = raw.split_once('=') else {
                    return Err(invalid(format!(
                        "`{raw}` must be key=value (for example team=payments)"
                    )));
                };
                if change
                    .set
                    .insert(key.to_string(), value.to_string())
                    .is_some()
                {
                    return Err(invalid(format!("label key `{key}` is given twice")));
                }
            }
            LabelVerb::Remove => {
                if raw.contains('=') {
                    return Err(invalid(format!(
                        "`{raw}`: remove takes label keys, not key=value"
                    )));
                }
                change.remove.push(raw.clone());
            }
        }
    }
    change
        .validate()
        .map_err(|err| run_error_class(&err, err.to_string()))?;
    Ok((target, change))
}

fn invalid(message: String) -> Box<dyn std::error::Error> {
    RunInvalid { message }.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{RunInvalidState, RunNotFound};

    fn args(verb: LabelVerb, selector: Option<&str>, positionals: &[&str]) -> LabelArgs {
        LabelArgs {
            verb,
            selector: selector.map(str::to_string),
            args: positionals.iter().map(|s| s.to_string()).collect(),
            dry_run: false,
            yes: false,
            json: false,
        }
    }

    #[test]
    fn the_target_is_a_run_id_an_agent_or_a_selector() {
        let (target, change) = parse(&args(
            LabelVerb::Set,
            None,
            &["run_01JZ6Q9G3S8V2X7K4M1N5P0R2T", "team=core", "env=prod"],
        ))
        .unwrap();
        assert!(matches!(target, Target::Run(_)));
        assert_eq!(change.set.len(), 2);
        let (target, _) = parse(&args(LabelVerb::Set, None, &["my-agent", "team=core"])).unwrap();
        assert_eq!(target, Target::Instance("my-agent".to_string()));
        let (target, change) = parse(&args(
            LabelVerb::Remove,
            Some("team=core,env=prod"),
            &["team"],
        ))
        .unwrap();
        assert!(matches!(target, Target::Selector(_)));
        assert_eq!(change.remove, vec!["team".to_string()]);
    }

    #[test]
    fn malformed_requests_are_usage_errors() {
        for bad in [
            args(LabelVerb::Set, None, &[]),
            args(LabelVerb::Set, None, &["my-agent"]),
            args(LabelVerb::Set, Some("team=core"), &[]),
            args(LabelVerb::Set, None, &["my-agent", "no-equals"]),
            args(LabelVerb::Set, None, &["my-agent", "team=a", "team=b"]),
            args(LabelVerb::Set, None, &["my-agent", "Team=a"]),
            args(LabelVerb::Set, None, &["my-agent", "team=has space"]),
            args(LabelVerb::Remove, None, &["my-agent", "team=core"]),
            args(LabelVerb::Remove, None, &["my-agent", "team", "team"]),
            args(LabelVerb::Set, Some("team"), &["a=b"]),
        ] {
            let err = parse(&bad).unwrap_err();
            assert!(err.is::<RunInvalid>(), "{:?}: {err}", bad.args);
        }
    }

    #[test]
    fn several_runs_are_confirmed_only_on_a_terminal() {
        let ids = vec!["run_A".to_string(), "run_B".to_string()];
        let mut out = Vec::new();
        // Not a terminal: refused, and stdin is never read.
        let mut never = std::io::Cursor::new(b"y\n".to_vec());
        let refused = confirm(&ids, LabelVerb::Set, false, &mut never, &mut out).unwrap_err();
        assert!(refused.is::<RunInvalid>());
        assert!(refused.to_string().contains("--yes"), "{refused}");
        assert_eq!(never.position(), 0, "a non-terminal stdin is never read");
        let listing = String::from_utf8(out).unwrap();
        assert!(listing.contains("run_A") && listing.contains("run_B"));
        for (answer, expected) in [
            ("y\n", true),
            ("YES\n", true),
            ("n\n", false),
            ("\n", false),
        ] {
            let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());
            let mut out = Vec::new();
            assert_eq!(
                confirm(&ids, LabelVerb::Remove, true, &mut input, &mut out).unwrap(),
                expected,
                "{answer:?}"
            );
            assert!(String::from_utf8(out)
                .unwrap()
                .contains("Unlabel these 2 Runs?"));
        }
    }

    #[test]
    fn results_render_one_line_per_run() {
        let outcome = LabelOutcome {
            run_id: hekma_engine::RunId::from_wire("run_A"),
            labels: vec![Label::verb("team", "core"), Label::caller("task", "x")],
            prior_labels: vec![Label::bound("team", "payments")],
            changed: true,
            dry_run: false,
        };
        let line = render_result(&success(&outcome)).unwrap();
        assert!(line.contains("Run run_A labeled"), "{line}");
        assert!(line.contains("team=core (bound, label-verb)"), "{line}");
        assert!(line.contains("task=x (caller)"), "{line}");
        let planned = LabelOutcome {
            dry_run: true,
            ..outcome.clone()
        };
        let line = render_result(&success(&planned)).unwrap();
        assert!(line.contains("would label"), "{line}");
        assert!(
            line.contains("now: team=payments (bound, config)"),
            "{line}"
        );
        let same = LabelOutcome {
            changed: false,
            ..outcome
        };
        assert!(render_result(&success(&same))
            .unwrap()
            .contains("unchanged"));
        assert_eq!(render_labels(&[]), "—");
        let refused = RunResult {
            run_id: "run_B".to_string(),
            outcome: "refused",
            labels: None,
            prior_labels: None,
            error: None,
        };
        assert!(render_result(&refused).is_none());
    }

    #[test]
    fn a_partial_failure_keeps_the_first_failures_class() {
        let state = run_error_class(
            &RunError::LabelFromConfig {
                run_id: "r".into(),
                key: "k".into(),
                instance: "a".into(),
            },
            "x".to_string(),
        );
        assert!(state.is::<RunInvalidState>());
        let missing = run_error_class(&RunError::NotFound { run_id: "r".into() }, "x".into());
        assert!(missing.is::<RunNotFound>());
        let cap = run_error_class(
            &RunError::TooManyRuns {
                selector: "a=b".into(),
                max: 1000,
            },
            "x".into(),
        );
        assert!(cap.is::<RunInvalid>());
    }
}
