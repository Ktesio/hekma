//! `hekma hold`, `hekma release`, `hekma reroute` and `hekma reject` (story
//! 23-4, PRD FR-56): act on a Run's model calls at the shared metering
//! proxy — park them, let them through, send them to another route, or
//! refuse them. One Run by id, or every open Run a Selector matches through
//! the shared fan-out (`--dry-run`, `--yes`, `--json`).

use std::time::Duration;

use hekma_engine::{ControlDepth, Engine, RunRecord};
use serde_json::{json, Value};

use super::agent::{open_read_only_engine, route, RoutedError};
use super::fanout::{self, ActResult, FanoutFlags, Refusal, SetAct, Target, VerbWords};
use super::host_client::{segment, CallError};
use crate::cli::run::{map_run_code, run_error_class};
use crate::ui;

/// What `hekma hold|release|reroute|reject` acts with.
#[derive(Debug, Default)]
pub struct HoldArgs {
    /// A run id (without `--selector`).
    pub target: Option<String>,
    /// `--selector key=value[,key=value…]`.
    pub selector: Option<String>,
    /// `reroute --via <binding>/<route>`.
    pub via: Option<String>,
    /// `reroute --off`.
    pub off: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub json: bool,
}

/// The four verbs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldVerb {
    Hold,
    Release,
    Reroute,
    Reject,
}

impl HoldVerb {
    fn name(self) -> &'static str {
        match self {
            HoldVerb::Hold => "hold",
            HoldVerb::Release => "release",
            HoldVerb::Reroute => "reroute",
            HoldVerb::Reject => "reject",
        }
    }

    fn words(self) -> VerbWords {
        match self {
            HoldVerb::Hold => VerbWords {
                verb: "hold",
                past: "held",
                question: "Hold",
                imperative: "hold",
            },
            HoldVerb::Release => VerbWords {
                verb: "release",
                past: "released",
                question: "Release",
                imperative: "release",
            },
            HoldVerb::Reroute => VerbWords {
                verb: "reroute",
                past: "rerouted",
                question: "Reroute",
                imperative: "reroute",
            },
            HoldVerb::Reject => VerbWords {
                verb: "reject",
                past: "rejected",
                question: "Reject",
                imperative: "reject",
            },
        }
    }
}

/// The note every answer carries.
const NOTE: &str = "this acts on the Run's model calls at the metering proxy: the process keeps \
                    running, and calls already sent complete and are billed";

/// One Run's dry run: what the verb would do, or the refusal the act would
/// answer (the engine's own gate, read-only).
fn plan_one(verb: HoldVerb, run: &RunRecord) -> Result<ActResult, Refusal> {
    let run_id = run.run_id.as_str();
    let reason = if run.is_ended() {
        Some(("run-ended", format!("Run {run_id} has ended")))
    } else if run.control_depth == ControlDepth::Observed {
        Some((
            "verb-unavailable",
            format!("Run {run_id} is observed: its calls do not pass through the metering proxy"),
        ))
    } else if !run.wire_path {
        Some((
            "verb-unavailable",
            format!("Run {run_id}'s model calls skip the metering proxy"),
        ))
    } else {
        None
    };
    match reason {
        Some((code, message)) => Err(Refusal {
            code: code.to_string(),
            diagnostic: map_run_code(code, message.clone()),
            message,
        }),
        None => Ok(ActResult::ok(run_id, format!("would-{}", verb.name()))),
    }
}

fn render(result: &ActResult) -> Option<String> {
    if let Some(verb) = result.outcome.strip_prefix("would-") {
        return Some(format!("Run {} would {verb}", result.run_id));
    }
    Some(format!("Run {} {}", result.run_id, result.outcome))
}

fn body(verb: HoldVerb, args: &HoldArgs) -> Result<Value, Box<dyn std::error::Error>> {
    match verb {
        HoldVerb::Reroute => match (&args.via, args.off) {
            (Some(via), false) => Ok(json!({ "route": via })),
            (None, true) => Ok(json!({ "route": Value::Null })),
            _ => Err(fanout::invalid(
                "reroute takes exactly one of --via <binding>/<route> or --off".to_string(),
            )),
        },
        _ if args.via.is_some() || args.off => Err(fanout::invalid(format!(
            "--via and --off apply to reroute only, not {}",
            verb.name()
        ))),
        _ => Ok(json!({})),
    }
}

/// `hekma hold|release|reroute|reject <RUN_ID|--selector SEL>`.
pub fn run(verb: HoldVerb, args: HoldArgs) -> Result<(), Box<dyn std::error::Error>> {
    let target = Target::parse(args.target.as_deref(), args.selector.as_deref())?;
    let flags = FanoutFlags {
        dry_run: args.dry_run,
        yes: args.yes,
        json: args.json,
    };
    let words = verb.words();
    let body = body(verb, &args)?;
    match target {
        Target::Selector(selector) => {
            let plan = move |_: &Engine, run: &RunRecord| plan_one(verb, run);
            let path = match verb {
                HoldVerb::Hold => "/v0/runs/hold",
                HoldVerb::Release => "/v0/runs/release",
                HoldVerb::Reroute => "/v0/runs/reroute",
                HoldVerb::Reject => "/v0/runs/reject",
            };
            fanout::run_selector(
                &selector,
                &words,
                flags,
                &plan,
                SetAct {
                    method: "POST",
                    path,
                    body,
                    per_run: Duration::from_secs(5),
                    render: &render,
                    decorate: &fanout::as_answered,
                },
            )
        }
        Target::Run(run_id) | Target::Instance(run_id) => {
            single(verb, &run_id, &words, flags, body)
        }
    }
}

fn single(
    verb: HoldVerb,
    run_id: &str,
    words: &VerbWords,
    flags: FanoutFlags,
    body: Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_read_only_engine()?;
    let record = engine
        .blocking()
        .run(run_id)
        .map_err(|err| run_error_class(&err, err.to_string()))?
        .record;
    drop(engine);
    if flags.dry_run {
        return match plan_one(verb, &record) {
            Ok(row) => fanout::finish(words, flags, 1, vec![row], None, &render),
            Err(refusal) => {
                let row = ActResult::refused(run_id, &refusal);
                fanout::finish(
                    words,
                    flags,
                    1,
                    vec![row],
                    Some(refusal.diagnostic),
                    &render,
                )
            }
        };
    }
    let routed = match route(
        "POST",
        &format!("/v0/runs/{}/{}", segment(run_id), verb.name()),
        Some(body),
    ) {
        Ok(routed) => routed,
        Err(RoutedError::Call(CallError::Coded { code, message })) => {
            return Err(map_run_code(&code, message))
        }
        Err(err) => return Err(err.into_diagnostic()),
    };
    let document = routed.value.unwrap_or(Value::Null);
    let changed = document["changed"].as_bool().unwrap_or(false);
    let reasons: Vec<String> = document["reasons"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let outcome = if changed { words.past } else { "unchanged" };
    let row = ActResult::ok(run_id, outcome).with("reasons", json!(reasons));
    if flags.json {
        return fanout::finish(words, flags, 1, vec![row], None, &render);
    }
    if changed {
        ui::success(format!("Run {run_id} {}", words.past));
    } else {
        ui::note(format!(
            "Run {run_id} was already {}; nothing changed",
            words.past
        ));
    }
    println!(
        "dispositions: {}",
        if reasons.is_empty() {
            "none".to_string()
        } else {
            reasons.join(", ")
        }
    );
    ui::note(NOTE);
    Ok(())
}
