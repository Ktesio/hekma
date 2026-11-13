//! The Run-grain lifecycle verbs (story 21-6, PRD FR-50): `hekma agent
//! stop|pause|resume|drain <TARGET|--selector SEL>` and `hekma run close
//! <RUN_ID|--selector SEL>`.
//!
//! A TARGET is a run id or an agent name. An agent name with no other flag
//! keeps today's instance route and output exactly. A run id acts through
//! `POST /v0/runs/{id}/<verb>`: on the Run that owns its agent's supervised
//! process the verb is the agent's own (same ladder, same bound, same
//! honesty notes); any other Run is refused per Run, naming its depth. A
//! Selector rides the shared fan-out ([`super::fanout`]).
//!
//! `start` and `send` take no Selector: a Selector resolves non-ended Runs
//! and there is nothing to start, and `send` carries content to one agent's
//! input channel.

use std::time::Duration;

use hekma_engine::{Engine, RunLifecycleVerb, RunRecord};
use serde_json::{json, Value};

use super::agent::{
    map_engine_error, note_if_best_effort, open_read_only_engine, route, RoutedError,
};
use super::fanout::{self, ActResult, FanoutFlags, Refusal, SetAct, Target, VerbWords};
use super::host_client::{segment, CallError};
use crate::cli::run::{map_run_code, run_error_class};
use crate::error::AgentIo;
use crate::ui;

/// The flags every lifecycle verb shares, plus its own bound.
pub struct LifecycleArgs {
    /// A run id or an agent name (without `--selector`).
    pub target: Option<String>,
    /// `--selector key=value[,key=value…]`.
    pub selector: Option<String>,
    pub dry_run: bool,
    pub yes: bool,
    pub json: bool,
    /// `stop --timeout <secs>`.
    pub timeout_secs: Option<u64>,
    /// `drain --timeout-ms <ms>`.
    pub timeout_ms: Option<u64>,
}

fn words(verb: RunLifecycleVerb) -> VerbWords {
    match verb {
        RunLifecycleVerb::Stop => VerbWords {
            verb: "stop",
            past: "stopped",
            question: "Stop",
            imperative: "stop",
        },
        RunLifecycleVerb::Pause => VerbWords {
            verb: "pause",
            past: "paused",
            question: "Pause",
            imperative: "pause",
        },
        RunLifecycleVerb::Resume => VerbWords {
            verb: "resume",
            past: "resumed",
            question: "Resume",
            imperative: "resume",
        },
        RunLifecycleVerb::Drain => VerbWords {
            verb: "drain",
            past: "drained",
            question: "Drain",
            imperative: "drain",
        },
    }
}

/// The verb's body fields (the bound) and each Run's own bound for the
/// set act's wait.
fn body_and_bound(verb: RunLifecycleVerb, args: &LifecycleArgs) -> (Value, Duration) {
    match verb {
        RunLifecycleVerb::Stop => {
            let window = args.timeout_secs.map(super::agent::served_stop_window);
            let body = window.map_or_else(|| json!({}), |secs| json!({ "window_secs": secs }));
            // The graceful window, then the forced kill.
            (body, Duration::from_secs(window.unwrap_or(30) + 5))
        }
        RunLifecycleVerb::Drain => {
            let body = args
                .timeout_ms
                .map_or_else(|| json!({}), |ms| json!({ "timeout_ms": ms }));
            // The bounded wait, then the stop ladder.
            let wait = Duration::from_millis(args.timeout_ms.unwrap_or(30_000));
            (body, wait + Duration::from_secs(35))
        }
        RunLifecycleVerb::Pause | RunLifecycleVerb::Resume => (json!({}), Duration::from_secs(10)),
    }
}

/// `hekma agent stop|pause|resume|drain <TARGET|--selector SEL>`.
pub fn run(verb: RunLifecycleVerb, args: LifecycleArgs) -> Result<(), Box<dyn std::error::Error>> {
    let target = Target::parse(args.target.as_deref(), args.selector.as_deref())?.prefer_agent();
    let flags = FanoutFlags {
        dry_run: args.dry_run,
        yes: args.yes,
        json: args.json,
    };
    let words = words(verb);
    // Today's instance route and output, byte for byte.
    if let Target::Instance(name) = &target {
        if !flags.dry_run && !flags.json {
            return match verb {
                RunLifecycleVerb::Stop => super::agent::stop(name, args.timeout_secs),
                RunLifecycleVerb::Pause => super::agent::pause(name),
                RunLifecycleVerb::Resume => super::agent::resume(name),
                RunLifecycleVerb::Drain => super::agent::drain(name, args.timeout_ms),
            };
        }
    }
    let (body, per_run) = body_and_bound(verb, &args);
    match target {
        Target::Instance(name) => {
            let engine = open_read_only_engine()?;
            let run_id = engine
                .blocking()
                .instance_run(&name)
                .map_err(|err| run_error_class(&err, err.to_string()))?
                .record
                .run_id
                .to_string();
            single(verb, &engine, &run_id, &words, flags, body)
        }
        Target::Run(run_id) => {
            let engine = open_read_only_engine()?;
            single(verb, &engine, &run_id, &words, flags, body)
        }
        Target::Selector(selector) => {
            let plan = move |engine: &Engine, run: &RunRecord| plan_one(engine, verb, run);
            let path = match verb {
                RunLifecycleVerb::Stop => "/v0/runs/stop",
                RunLifecycleVerb::Pause => "/v0/runs/pause",
                RunLifecycleVerb::Resume => "/v0/runs/resume",
                RunLifecycleVerb::Drain => "/v0/runs/drain",
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
                    per_run,
                    render: &render,
                    decorate: &fanout::as_answered,
                },
            )
        }
    }
}

/// One Run's dry run: `would-<verb>` (naming the agent) or the refusal the
/// act would answer. Planned, subject to the act: the instance verb's own
/// refusals (capability, a race with another command) surface only there.
fn plan_one(
    engine: &Engine,
    verb: RunLifecycleVerb,
    run: &RunRecord,
) -> Result<ActResult, Refusal> {
    let record = engine
        .blocking()
        .plan_run_verb(run.run_id.as_str(), verb)
        .map_err(Refusal::from_run_error)?;
    Ok(
        ActResult::ok(record.run_id.as_str(), format!("would-{}", verb.as_str()))
            .with("agent", json!(record.instance)),
    )
}

/// One target (a run id, or an agent's open Run with `--json`/`--dry-run`).
fn single(
    verb: RunLifecycleVerb,
    engine: &Engine,
    run_id: &str,
    words: &VerbWords,
    flags: FanoutFlags,
    body: Value,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read first: an unknown id is refused before any host starts.
    let record = engine
        .blocking()
        .run(run_id)
        .map_err(|err| run_error_class(&err, err.to_string()))?
        .record;
    if flags.dry_run {
        return match plan_one(engine, verb, &record) {
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
    let routed = route(
        "POST",
        &format!("/v0/runs/{}/{}", segment(run_id), verb.as_str()),
        Some(body),
    );
    let routed = match routed {
        Ok(routed) => routed,
        Err(RoutedError::Call(CallError::Coded { code, message })) => {
            let diagnostic = map_run_code(&code, message.clone());
            if flags.json {
                let refusal = Refusal {
                    code,
                    message,
                    diagnostic,
                };
                let row = ActResult::refused(run_id, &refusal);
                return fanout::finish(
                    words,
                    flags,
                    1,
                    vec![row],
                    Some(refusal.diagnostic),
                    &render,
                );
            }
            return Err(diagnostic);
        }
        Err(RoutedError::Call(CallError::Engine(err))) if flags.json => {
            let diagnostic = map_engine_error(err);
            let refusal = Refusal {
                code: "engine".to_string(),
                message: diagnostic.to_string(),
                diagnostic,
            };
            let row = ActResult::refused(run_id, &refusal);
            return fanout::finish(
                words,
                flags,
                1,
                vec![row],
                Some(refusal.diagnostic),
                &render,
            );
        }
        Err(err) => return Err(err.into_diagnostic()),
    };
    let document = routed.value.unwrap_or(Value::Null);
    // Story 23-4: on a wire Run the verb acted on its model calls at the
    // metering proxy — qualified, never a process verb.
    if let Some(disposition) = hekma_engine::disposition_for(verb, &record) {
        let row = ActResult::ok(run_id, words.past).with("acts_on", json!("calls"));
        if flags.json {
            return fanout::finish(words, flags, 1, vec![row], None, &render);
        }
        ui::success(format!("{} Run {run_id}", capitalized(words.past)));
        ui::note(format!(
            "on this Run, {} {} its model calls at the metering proxy: the process keeps \
             running, and calls already sent complete and are billed",
            verb.as_str(),
            match disposition {
                hekma_engine::DispositionVerb::Hold => "holds",
                hekma_engine::DispositionVerb::Release => "releases",
                _ => "rejects",
            }
        ));
        return Ok(());
    }
    let instance = document.get("instance").cloned().ok_or_else(|| {
        Box::new(AgentIo {
            message: format!(
                "the local Service Host (pid {}) applied the command to Run {run_id} but sent no \
                 read-back of its agent; check it with: hekma run show {run_id}",
                routed.host_pid
            ),
        }) as Box<dyn std::error::Error>
    })?;
    let row = ActResult::ok(run_id, words.past)
        .with("instance", instance.clone())
        .with(
            "completed",
            document.get("completed").cloned().unwrap_or(Value::Null),
        )
        .with(
            "waited_ms",
            document.get("waited_ms").cloned().unwrap_or(Value::Null),
        );
    if flags.json {
        return fanout::finish(words, flags, 1, vec![row], None, &render);
    }
    let name = instance["name"].as_str().unwrap_or_default().to_string();
    let state = instance["state"].as_str().unwrap_or_default().to_string();
    ui::success(format!(
        "{} Run {run_id} (Agent Instance {})",
        capitalized(words.past),
        ui::skill_name(&name)
    ));
    println!("{state}");
    match verb {
        RunLifecycleVerb::Pause | RunLifecycleVerb::Resume => {
            note_if_best_effort(&name, verb.as_str())
        }
        RunLifecycleVerb::Drain if document["completed"].as_bool() == Some(false) => {
            ui::note(format!(
                "the drain's bounded wait did NOT complete within {}ms (timed out); the \
                 instance was stopped anyway and the outcome is recorded on its final transition",
                document["waited_ms"].as_u64().unwrap_or(0)
            ));
        }
        _ => {}
    }
    Ok(())
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// One human line per acted (or planned) Run.
fn render(result: &ActResult) -> Option<String> {
    if let Some(verb) = result.outcome.strip_prefix("would-") {
        let agent = result
            .field("agent")
            .and_then(Value::as_str)
            .map(|agent| format!(" (agent {agent})"))
            .unwrap_or_default();
        return Some(format!("Run {} would {verb}{agent}", result.run_id));
    }
    let instance = result.field("instance");
    let name = instance
        .and_then(|instance| instance["name"].as_str())
        .unwrap_or("?");
    let state = instance
        .and_then(|instance| instance["state"].as_str())
        .unwrap_or("?");
    let timed_out = if result.field("completed").and_then(Value::as_bool) == Some(false) {
        " — the bounded wait timed out; stopped anyway"
    } else {
        ""
    };
    Some(ui::success_text(format!(
        "Run {} {} (agent {name}: {state}){timed_out}",
        result.run_id, result.outcome
    )))
}

/// `hekma run close <RUN_ID|--selector SEL> [--dry-run] [--yes] [--json]`.
pub fn close(args: LifecycleArgs) -> Result<(), Box<dyn std::error::Error>> {
    let target = Target::parse(args.target.as_deref(), args.selector.as_deref())?;
    let flags = FanoutFlags {
        dry_run: args.dry_run,
        yes: args.yes,
        json: args.json,
    };
    let words = VerbWords {
        verb: "close",
        past: "closed",
        question: "Close",
        imperative: "close",
    };
    let plan = |engine: &Engine, run: &RunRecord| plan_close(engine, run);
    match target {
        Target::Selector(selector) => fanout::run_selector(
            &selector,
            &words,
            flags,
            &plan,
            SetAct {
                method: "POST",
                path: "/v0/runs/close",
                body: json!({}),
                per_run: Duration::from_secs(5),
                render: &render_close,
                decorate: &fanout::as_answered,
            },
        ),
        Target::Run(run_id) | Target::Instance(run_id) if !flags.dry_run => {
            super::run::close(&run_id, flags.json)
        }
        Target::Run(run_id) | Target::Instance(run_id) => {
            let engine = open_read_only_engine()?;
            let record = engine
                .blocking()
                .run(&run_id)
                .map_err(|err| run_error_class(&err, err.to_string()))?
                .record;
            match plan_close(&engine, &record) {
                Ok(row) => fanout::finish(&words, flags, 1, vec![row], None, &render_close),
                Err(refusal) => {
                    let row = ActResult::refused(run_id, &refusal);
                    fanout::finish(
                        &words,
                        flags,
                        1,
                        vec![row],
                        Some(refusal.diagnostic),
                        &render_close,
                    )
                }
            }
        }
    }
}

/// A close's dry run: a Run that owns its agent's process is stopped,
/// never closed.
fn plan_close(_engine: &Engine, run: &RunRecord) -> Result<ActResult, Refusal> {
    if run.process_owner && !run.is_ended() {
        return Err(Refusal::from_run_error(
            hekma_engine::RunError::OwnsProcess {
                run_id: run.run_id.to_string(),
                instance: run.instance.clone().unwrap_or_default(),
            },
        ));
    }
    Ok(ActResult::ok(run.run_id.as_str(), "would-close"))
}

fn render_close(result: &ActResult) -> Option<String> {
    Some(match result.outcome.as_str() {
        "would-close" => format!("Run {} would close", result.run_id),
        _ => ui::success_text(format!("Run {} closed", result.run_id)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> LifecycleArgs {
        LifecycleArgs {
            target: None,
            selector: None,
            dry_run: false,
            yes: false,
            json: false,
            timeout_secs: None,
            timeout_ms: None,
        }
    }

    #[test]
    fn each_verb_sends_its_own_bound_and_waits_for_it() {
        let (body, bound) = body_and_bound(RunLifecycleVerb::Stop, &args());
        assert_eq!(body, json!({}));
        assert_eq!(bound, Duration::from_secs(35));
        let stop = LifecycleArgs {
            timeout_secs: Some(0),
            ..args()
        };
        let (body, bound) = body_and_bound(RunLifecycleVerb::Stop, &stop);
        assert_eq!(
            body,
            json!({ "window_secs": 1 }),
            "served within the host's bounds"
        );
        assert_eq!(bound, Duration::from_secs(6));
        let drain = LifecycleArgs {
            timeout_ms: Some(1_500),
            ..args()
        };
        let (body, bound) = body_and_bound(RunLifecycleVerb::Drain, &drain);
        assert_eq!(body, json!({ "timeout_ms": 1500 }));
        assert_eq!(bound, Duration::from_millis(36_500));
        let (body, bound) = body_and_bound(RunLifecycleVerb::Pause, &args());
        assert_eq!((body, bound), (json!({}), Duration::from_secs(10)));
        for verb in RunLifecycleVerb::ALL {
            assert_eq!(words(verb).verb, verb.as_str());
        }
    }

    #[test]
    fn results_render_one_line_per_run() {
        let planned = ActResult::ok("run_A", "would-stop").with("agent", json!("a"));
        assert_eq!(render(&planned).unwrap(), "Run run_A would stop (agent a)");
        let acted = ActResult::ok("run_A", "drained")
            .with("instance", json!({"name": "a", "state": "stopped"}))
            .with("completed", json!(false));
        let line = render(&acted).unwrap();
        assert!(
            line.contains("Run run_A drained (agent a: stopped)"),
            "{line}"
        );
        assert!(line.contains("timed out"), "{line}");
        assert_eq!(capitalized("stopped"), "Stopped");
        assert_eq!(capitalized(""), "");
        assert!(render_close(&ActResult::ok("r", "would-close"))
            .unwrap()
            .contains("would close"));
        assert!(render_close(&ActResult::ok("r", "closed"))
            .unwrap()
            .contains("Run r closed"));
    }
}
