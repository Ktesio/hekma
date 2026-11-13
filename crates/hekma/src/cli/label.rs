//! `hekma label` — bound labels on Runs (story 21-9, PRD FR-49 / FR-50).
//!
//! `set` binds labels on one Run, an agent's open Run, or every open Run a
//! Selector matches; `remove` unbinds labels the verb set. A label set here
//! is bound with origin `label-verb`: it outranks the agent's `labels`
//! configuration and any label the Run set about itself, and only bound
//! labels select Policies.
//!
//! Results go to stdout (one line per Run, or the `--json` document),
//! diagnostics and refusals to stderr (AD-12). A Selector rides the shared
//! fan-out ([`super::fanout`]): resolved once, confirmed, then the verb acts
//! on exactly the listed Runs (FR-50) with ONE set-form call.
//!
//! One data-path owner (AD-20): the target and the Selector are read from
//! `state.db` read-only, and so is a `--dry-run` (it writes nothing, so it
//! starts no Service Host). A single target's write is one
//! `PATCH /v0/runs/{id}/labels`; a Selector's is one `PATCH /v0/runs/labels`
//! carrying the set token `POST /v0/runs/select` minted — both routed to the
//! state root's local Service Host, found or started as every `hekma agent`
//! write is.

use hekma_engine::{Engine, Label, LabelChange, LabelOrigin, LabelOutcome, LabelSource, Selector};
use serde_json::{json, Value};

use super::agent::{open_read_only_engine, route};
use super::fanout::{self, ActResult, FanoutFlags, Refusal, SetAct, Target, VerbWords};
use super::host_client::segment;
use crate::cli::run::{looks_like_run_id, run_error_class};
use crate::error::{AgentIo, RunInvalid};
use crate::ui;

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

/// The fan-out's words for each label verb.
fn words(verb: LabelVerb) -> VerbWords {
    match verb {
        LabelVerb::Set => VerbWords {
            verb: "label",
            past: "labeled",
            question: "Label",
            imperative: "label",
        },
        LabelVerb::Remove => VerbWords {
            verb: "label",
            past: "labeled",
            question: "Unlabel",
            imperative: "unlabel",
        },
    }
}

/// `hekma label set|remove …`.
pub fn run(args: LabelArgs) -> Result<(), Box<dyn std::error::Error>> {
    let (target, change) = parse(&args)?;
    let words = words(args.verb);
    let flags = FanoutFlags {
        dry_run: args.dry_run,
        yes: args.yes,
        json: args.json,
    };
    if let Target::Selector(selector) = &target {
        let plan_change = change.clone();
        let plan = move |engine: &Engine, run: &hekma_engine::RunRecord| {
            engine
                .blocking()
                .label_run(run.run_id.as_str(), plan_change.clone(), true)
                .map(|outcome| success(&outcome))
                .map_err(Refusal::from_run_error)
        };
        return fanout::run_selector(
            selector,
            &words,
            flags,
            &plan,
            SetAct {
                method: "PATCH",
                path: "/v0/runs/labels",
                body: json!({ "set": change.set, "remove": change.remove }),
                per_run: std::time::Duration::from_secs(5),
                render: &render_result,
                decorate: &fanout::as_answered,
            },
        );
    }
    let engine = open_read_only_engine()?;
    let facade = engine.blocking();
    // Read first, so an unknown id is refused without starting a host.
    let run_id = match &target {
        Target::Run(id) => facade.run(id),
        Target::Instance(name) => facade.instance_run(name),
        Target::Selector(_) => unreachable!("handled above"),
    }
    .map_err(|err| run_error_class(&err, err.to_string()))?
    .record
    .run_id
    .to_string();
    match apply(&engine, &run_id, &change, args.dry_run) {
        Ok(outcome) => fanout::finish(
            &words,
            flags,
            1,
            vec![success(&outcome)],
            None,
            &render_result,
        ),
        Err(refusal) => {
            let row = ActResult::refused(run_id, &refusal);
            fanout::finish(
                &words,
                flags,
                1,
                vec![row],
                Some(refusal.diagnostic),
                &render_result,
            )
        }
    }
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

/// A successful outcome as a result row.
fn success(outcome: &LabelOutcome) -> ActResult {
    let verb = match (outcome.changed, outcome.dry_run) {
        (false, _) => "unchanged",
        (true, false) => "labeled",
        (true, true) => "would-label",
    };
    ActResult::ok(outcome.run_id.to_string(), verb)
        .with("labels", json!(outcome.labels))
        .with("prior_labels", json!(outcome.prior_labels))
}

/// One human line per Run; a refusal renders nothing here (it went to
/// stderr, or is the returned error).
fn render_result(result: &ActResult) -> Option<String> {
    let labels: Vec<Label> = serde_json::from_value(result.field("labels")?.clone()).ok()?;
    let prior: Vec<Label> = result
        .field("prior_labels")
        .cloned()
        .and_then(|value: Value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let rendered = render_labels(&labels);
    Some(match result.outcome.as_str() {
        "labeled" => ui::success_text(format!("Run {} labeled: {rendered}", result.run_id)),
        "would-label" => format!(
            "Run {} would label: {rendered} (now: {})",
            result.run_id,
            render_labels(&prior)
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
                (LabelSource::Bound, Some(LabelOrigin::Binding)) => "bound, binding".to_string(),
                (LabelSource::Bound, _) => "bound, config".to_string(),
            };
            format!("{}={} ({source})", label.key, label.value)
        })
        .collect::<Vec<_>>()
        .join(", ")
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
                Target::Run(first.clone()).prefer_agent()
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
    use hekma_engine::RunError;

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
        let refused = ActResult::refused(
            "run_B",
            &Refusal::from_run_error(RunError::RunEnded {
                run_id: "run_B".into(),
            }),
        );
        assert!(render_result(&refused).is_none());
        assert_eq!(words(LabelVerb::Remove).question, "Unlabel");
        assert_eq!(words(LabelVerb::Set).verb, "label");
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
