//! `hekma budget show|set` — a Run's own limits (story 21-6, PRD FR-50 /
//! FR-60).
//!
//! `show` reads a Run's resolved limits (each with its source), its usage,
//! headroom and latches — read-only, no host starts; over a Selector it
//! pages (`--limit`, `--cursor`). `set` writes limits ON THE RUN: a limit
//! set here outranks the agent's configuration and every Policy for that
//! limit, and it BELONGS TO THE RUN — it ends with the Run, and a new start
//! opens a new Run without it. Limits every future Run should carry belong
//! in the agent's `budget.*` configuration or a Policy; every result says
//! so. A raise above the usage re-arms the latch and lifts the matching
//! `block` fence reasons at once; a lowering at or below the usage breaches
//! at once and applies the Breach Action.
//!
//! Writes route to the local Service Host (`PATCH /v0/runs/{id}/limits`,
//! or one `POST /v0/runs/limits` over a selected set); `--dry-run` plans on
//! the read-only engine and starts no host.

use hekma_engine::{
    render_dollars_bare, BreachAction, Engine, LatchKey, LimitActor, LimitSource, Micros,
    PolicyResolution, RunLimitChange, RunLimitField, RunLimits, RunLimitsOutcome, RunLimitsView,
    RunRecord, MAX_SELECTED_RUNS,
};
use serde::Serialize;
use serde_json::{json, Value};

use super::agent::{open_read_only_engine, route};
use super::fanout::{self, invalid, ActResult, FanoutFlags, Refusal, SetAct, Target, VerbWords};
use super::host_client::segment;
use crate::cli::run::run_error_class;
use crate::error::AgentIo;
use crate::ui;

/// The note every `budget set` result carries (human line and `--json`).
pub(crate) const RUN_SCOPED_NOTE: &str = "set on this Run only: it ends with the Run, and a new \
     start opens a new Run without it — for limits every future Run should carry, use the \
     agent's budget.* configuration or a Policy";

/// The `budget show --json` document's schema version.
const SHOW_DOCUMENT_SCHEMA_VERSION: u32 = 1;

/// The default and largest `--limit` of a Selector read.
const DEFAULT_PAGE: usize = 50;

/// `hekma budget set` flags.
pub struct SetArgs {
    pub target: Option<String>,
    pub selector: Option<String>,
    pub tokens_per_run: Option<u64>,
    pub tokens_cumulative: Option<u64>,
    pub dollars_per_run: Option<String>,
    pub dollars_cumulative: Option<String>,
    pub breach_action: Option<String>,
    /// `--clear <limit>[,<limit>]` (repeatable).
    pub clear: Vec<String>,
    pub dry_run: bool,
    pub yes: bool,
    pub json: bool,
}

/// `hekma budget show` flags.
pub struct ShowArgs {
    pub target: Option<String>,
    pub selector: Option<String>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub json: bool,
}

const WORDS: VerbWords = VerbWords {
    verb: "budget",
    past: "changed",
    question: "Change the limits of",
    imperative: "change",
};

fn dollars(flag: &str, raw: &str) -> Result<Micros, Box<dyn std::error::Error>> {
    let trimmed = raw.trim().trim_start_matches('$');
    hekma_engine::parse_dollars_to_micros(trimmed)
        .map_err(|reason| invalid(format!("--{flag}: {reason}")))
}

/// Build the change from the flags (nothing opened yet).
fn change_from(args: &SetArgs) -> Result<RunLimitChange, Box<dyn std::error::Error>> {
    let breach_action = args
        .breach_action
        .as_deref()
        .map(|raw| {
            raw.parse::<BreachAction>().map_err(|_| {
                invalid(format!(
                    "--breach-action: `{raw}` is not a Breach Action (warn, pause, stop or block)"
                ))
            })
        })
        .transpose()?;
    let mut clear = Vec::new();
    for raw in args.clear.iter().flat_map(|value| value.split(',')) {
        let raw = raw.trim();
        let field = RunLimitField::from_wire(raw).ok_or_else(|| {
            invalid(format!(
                "--clear: `{raw}` is not a limit — use tokens-per-run, tokens-cumulative, \
                 dollars-per-run, dollars-cumulative or breach-action"
            ))
        })?;
        if !clear.contains(&field) {
            clear.push(field);
        }
    }
    let change = RunLimitChange {
        set: RunLimits {
            token_per_run: args.tokens_per_run,
            dollar_per_run_micros: args
                .dollars_per_run
                .as_deref()
                .map(|raw| dollars("dollars-per-run", raw))
                .transpose()?,
            token_cumulative: args.tokens_cumulative,
            dollar_cumulative_micros: args
                .dollars_cumulative
                .as_deref()
                .map(|raw| dollars("dollars-cumulative", raw))
                .transpose()?,
            breach_action,
        },
        clear,
    };
    change
        .validate()
        .map_err(|err| run_error_class(&err, err.to_string()))?;
    Ok(change)
}

/// The routed body for `change` (dollars travel as decimal strings).
fn change_body(change: &RunLimitChange) -> Value {
    let mut body = json!({});
    if let Some(tokens) = change.set.token_per_run {
        body["tokens_per_run"] = json!(tokens);
    }
    if let Some(tokens) = change.set.token_cumulative {
        body["tokens_cumulative"] = json!(tokens);
    }
    if let Some(micros) = change.set.dollar_per_run_micros {
        body["dollars_per_run"] = json!(micros_text(micros));
    }
    if let Some(micros) = change.set.dollar_cumulative_micros {
        body["dollars_cumulative"] = json!(micros_text(micros));
    }
    if let Some(action) = change.set.breach_action {
        body["breach_action"] = json!(action.as_str());
    }
    if !change.clear.is_empty() {
        body["clear"] = json!(change
            .clear
            .iter()
            .map(RunLimitField::as_str)
            .collect::<Vec<_>>());
    }
    body
}

/// Exact decimal dollars for the wire (`"3.000001"` — no cent rounding: the
/// host parses back to the same micro-dollars).
fn micros_text(micros: Micros) -> String {
    let raw = micros.get().max(0);
    format!("{}.{:06}", raw / 1_000_000, raw % 1_000_000)
}

/// A budget-job outcome as a result row (the Run-scoped note on every one).
fn outcome_row(outcome: &RunLimitsOutcome) -> ActResult {
    let name = if outcome.dry_run {
        "would-set".to_string()
    } else {
        outcome.direction.as_str().to_string()
    };
    ActResult::ok(outcome.run_id.to_string(), name)
        .with("direction", json!(outcome.direction.as_str()))
        .with("limits", json!(outcome))
        .with("run_scoped", json!(true))
        .with("note", json!(RUN_SCOPED_NOTE))
}

/// `hekma budget set <TARGET|--selector SEL> …`.
pub fn set(args: SetArgs) -> Result<(), Box<dyn std::error::Error>> {
    let change = change_from(&args)?;
    let target = Target::parse(args.target.as_deref(), args.selector.as_deref())?;
    let flags = FanoutFlags {
        dry_run: args.dry_run,
        yes: args.yes,
        json: args.json,
    };
    if let Target::Selector(selector) = &target {
        let planned = change.clone();
        let plan =
            move |engine: &Engine, run: &RunRecord| plan_one(engine, run.run_id.as_str(), &planned);
        return fanout::run_selector(
            selector,
            &WORDS,
            flags,
            &plan,
            SetAct {
                method: "POST",
                path: "/v0/runs/limits",
                body: change_body(&change),
                per_run: std::time::Duration::from_secs(10),
                render: &render_set,
                decorate: &|row: &mut ActResult| {
                    row.fields.insert("run_scoped".to_string(), json!(true));
                    row.fields
                        .insert("note".to_string(), json!(RUN_SCOPED_NOTE));
                },
            },
        );
    }
    let engine = open_read_only_engine()?;
    let run_id = target_run(&engine, &target)?;
    let answered = if flags.dry_run {
        plan_one(&engine, &run_id, &change)
    } else {
        drop(engine);
        apply(&run_id, &change)
    };
    match answered {
        Ok(row) => fanout::finish(&WORDS, flags, 1, vec![row], None, &render_set),
        Err(refusal) => {
            let row = ActResult::refused(run_id, &refusal);
            fanout::finish(
                &WORDS,
                flags,
                1,
                vec![row],
                Some(refusal.diagnostic),
                &render_set,
            )
        }
    }
}

/// The run id a single target names (read first: an unknown id or agent
/// is refused before any host starts).
fn target_run(engine: &Engine, target: &Target) -> Result<String, Box<dyn std::error::Error>> {
    let view = match target {
        Target::Run(id) => engine.blocking().run(id),
        Target::Instance(name) => engine.blocking().instance_run(name),
        Target::Selector(_) => unreachable!("a Selector is not one target"),
    }
    .map_err(|err| run_error_class(&err, err.to_string()))?;
    Ok(view.record.run_id.to_string())
}

/// One Run's dry run on the read-only engine.
fn plan_one(engine: &Engine, run_id: &str, change: &RunLimitChange) -> Result<ActResult, Refusal> {
    let mut answered = engine
        .blocking()
        .set_run_limits(
            vec![run_id.to_string()],
            change.clone(),
            LimitActor::Operator,
            true,
        )
        .map_err(Refusal::from_run_error)?;
    match answered.pop() {
        Some((_, Ok(outcome))) => Ok(outcome_row(&outcome)),
        Some((_, Err(err))) => Err(Refusal::from_run_error(err)),
        None => Err(Refusal::from_run_error(hekma_engine::RunError::NotFound {
            run_id: run_id.to_string(),
        })),
    }
}

/// One Run's write: `PATCH /v0/runs/{id}/limits` through the local host.
fn apply(run_id: &str, change: &RunLimitChange) -> Result<ActResult, Refusal> {
    let routed = route(
        "PATCH",
        &format!("/v0/runs/{}/limits", segment(run_id)),
        Some(change_body(change)),
    )
    .map_err(Refusal::from_routed)?;
    let host_pid = routed.host_pid;
    routed
        .value
        .and_then(|document| serde_json::from_value::<RunLimitsOutcome>(document).ok())
        .map(|outcome| outcome_row(&outcome))
        .ok_or_else(|| {
            let message = format!(
                "the local Service Host (pid {host_pid}) applied the change but sent no outcome \
                 in its answer; check the Run with: hekma budget show {run_id}"
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

fn key_text(key: &LatchKey) -> String {
    format!("{} {}", key.dimension.as_str(), key.scope.as_str())
}

/// The Run's own limits, rendered (`tokens per-run 500000, breach action
/// pause`), or `no Run-level limit`.
fn limits_text(limits: &RunLimits) -> String {
    let mut parts = Vec::new();
    if let Some(tokens) = limits.token_per_run {
        parts.push(format!("tokens per-run {tokens}"));
    }
    if let Some(tokens) = limits.token_cumulative {
        parts.push(format!("tokens cumulative {tokens}"));
    }
    if let Some(micros) = limits.dollar_per_run_micros {
        parts.push(format!("dollars per-run {}", render_dollars_bare(micros)));
    }
    if let Some(micros) = limits.dollar_cumulative_micros {
        parts.push(format!(
            "dollars cumulative {}",
            render_dollars_bare(micros)
        ));
    }
    if let Some(action) = limits.breach_action {
        parts.push(format!("breach action {}", action.as_str()));
    }
    if parts.is_empty() {
        "no Run-level limit".to_string()
    } else {
        parts.join(", ")
    }
}

/// One human line per changed (or planned) Run — the Run-scoped note rides
/// every line.
fn render_set(result: &ActResult) -> Option<String> {
    let outcome: RunLimitsOutcome = serde_json::from_value(result.field("limits")?.clone()).ok()?;
    let verb = if outcome.dry_run {
        format!("would be {}", outcome.direction.as_str())
    } else {
        outcome.direction.as_str().to_string()
    };
    let mut line = format!(
        "Run {} {verb}: {}",
        result.run_id,
        limits_text(&outcome.limits)
    );
    if !outcome.re_armed.is_empty() {
        let keys: Vec<String> = outcome.re_armed.iter().map(key_text).collect();
        line.push_str(&format!("; re-armed {}", keys.join(", ")));
    }
    if !outcome.fence_cleared.is_empty() {
        let keys: Vec<String> = outcome.fence_cleared.iter().map(key_text).collect();
        line.push_str(&format!("; fence lifted for {}", keys.join(", ")));
    }
    for fired in &outcome.fired {
        let (limit, observed) = match fired.dimension {
            hekma_engine::BreachDimension::Tokens => {
                (fired.limit.to_string(), fired.observed.to_string())
            }
            hekma_engine::BreachDimension::Dollars => (
                render_dollars_bare(Micros(fired.limit as i64)),
                render_dollars_bare(Micros(fired.observed as i64)),
            ),
        };
        line.push_str(&format!(
            "; {}breach: {} {} at {observed} (ceiling {limit}, {})",
            if outcome.dry_run { "would " } else { "" },
            fired.dimension.as_str(),
            fired.scope.as_str(),
            fired.action.as_str(),
        ));
    }
    line.push_str(&format!(" — {RUN_SCOPED_NOTE}"));
    Some(if outcome.dry_run {
        line
    } else {
        ui::success_text(line)
    })
}

/// One `budget show` item.
#[derive(Serialize)]
struct ShowDocument<'a> {
    schema_version: u32,
    items: &'a [RunLimitsView],
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// `hekma budget show [<TARGET>|--selector SEL] [--limit] [--cursor] [--json]`.
pub fn show(args: ShowArgs) -> Result<(), Box<dyn std::error::Error>> {
    let target = Target::parse(args.target.as_deref(), args.selector.as_deref())?;
    let limit = args.limit.unwrap_or(DEFAULT_PAGE);
    if limit == 0 || limit > MAX_SELECTED_RUNS {
        return Err(invalid(format!(
            "--limit must be from 1 to {MAX_SELECTED_RUNS}"
        )));
    }
    if args.cursor.is_some() && !matches!(target, Target::Selector(_)) {
        return Err(invalid(
            "--cursor pages a --selector read; a single Run has one page".to_string(),
        ));
    }
    let engine = open_read_only_engine()?;
    let facade = engine.blocking();
    let (ids, next_cursor) = match &target {
        Target::Selector(selector) => {
            let mut ids: Vec<String> = facade
                .select_runs(selector)
                .map_err(|err| run_error_class(&err, err.to_string()))?
                .into_iter()
                .map(|run| run.run_id.to_string())
                .collect();
            ids.sort();
            if let Some(cursor) = &args.cursor {
                ids.retain(|id| id.as_str() > cursor.as_str());
            }
            let more = ids.len() > limit;
            ids.truncate(limit);
            let next = more.then(|| ids.last().cloned()).flatten();
            if ids.is_empty() && args.cursor.is_none() {
                ui::note(format!("No open Run matches `{selector}`."));
            }
            (ids, next)
        }
        single => (vec![target_run(&engine, single)?], None),
    };
    let mut views = Vec::with_capacity(ids.len());
    for id in &ids {
        views.push(
            facade
                .run_limits_view(id)
                .map_err(|err| run_error_class(&err, err.to_string()))?,
        );
    }
    if args.json {
        let document = ShowDocument {
            schema_version: SHOW_DOCUMENT_SCHEMA_VERSION,
            items: &views,
            next_cursor,
        };
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }
    for (index, view) in views.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print!("{}", render_view(view));
    }
    if let Some(cursor) = next_cursor {
        ui::note(format!("more Runs match; the next page: --cursor {cursor}"));
    }
    Ok(())
}

fn source_text(source: &LimitSource) -> String {
    match source {
        LimitSource::Run => "run".to_string(),
        LimitSource::Instance => "instance".to_string(),
        LimitSource::Policy(name) => format!("policy:{name}"),
        LimitSource::Default => "default".to_string(),
        LimitSource::Unset => "unset".to_string(),
    }
}

/// One rendered limit row: its label, ceiling, source, usage and headroom.
type Row<'a> = (
    &'a str,
    Option<String>,
    &'a LimitSource,
    String,
    Option<String>,
);

/// One Run's budget block.
fn render_view(view: &RunLimitsView) -> String {
    let run = &view.run;
    let resolution: &PolicyResolution = &view.resolution;
    let mut out = format!(
        "Run {} (agent {}, {}, {})\n",
        run.run_id,
        run.instance.as_deref().unwrap_or("—"),
        run.control_depth.display_with_wire(run.wire_path),
        run.status.as_str()
    );
    let budget = view.budget.as_ref();
    let tokens_used = view
        .usage
        .input_tokens
        .saturating_add(view.usage.output_tokens);
    let cumulative_used = view
        .cumulative_usage
        .map(|usage| usage.input_tokens.saturating_add(usage.output_tokens));
    let rows: [Row<'_>; 4] = [
        (
            "tokens per-run",
            resolution.tokens.per_run.map(|v| v.to_string()),
            &resolution.sources.tokens_per_run,
            tokens_used.to_string(),
            budget
                .and_then(|b| b.per_run_remaining)
                .map(|v| v.to_string()),
        ),
        (
            "tokens cumulative",
            resolution.tokens.cumulative.map(|v| v.to_string()),
            &resolution.sources.tokens_cumulative,
            cumulative_used
                .map(|v| v.to_string())
                .unwrap_or_else(|| "—".into()),
            budget
                .and_then(|b| b.cumulative_remaining)
                .map(|v| v.to_string()),
        ),
        (
            "dollars per-run",
            resolution.dollars.per_run.map(render_dollars_bare),
            &resolution.sources.dollars_per_run,
            render_dollars_bare(view.usage.cost_micros),
            budget
                .and_then(|b| b.per_run_dollars_remaining)
                .map(render_dollars_bare),
        ),
        (
            "dollars cumulative",
            resolution.dollars.cumulative.map(render_dollars_bare),
            &resolution.sources.dollars_cumulative,
            view.cumulative_usage
                .map(|usage| render_dollars_bare(usage.cost_micros))
                .unwrap_or_else(|| "—".into()),
            budget
                .and_then(|b| b.cumulative_dollars_remaining)
                .map(render_dollars_bare),
        ),
    ];
    for (label, ceiling, source, used, headroom) in rows {
        match ceiling {
            Some(ceiling) => out.push_str(&format!(
                "  {label:<18} {ceiling} ({})  used {used}  headroom {}\n",
                source_text(source),
                headroom.unwrap_or_else(|| "—".into())
            )),
            None => out.push_str(&format!("  {label:<18} —  used {used}\n")),
        }
    }
    out.push_str(&format!(
        "  {:<18} {} ({})\n",
        "breach action",
        resolution.effective_action.as_str(),
        source_text(&resolution.sources.breach_action)
    ));
    if view.latched.is_empty() {
        out.push_str(&format!("  {:<18} none\n", "latched"));
    } else {
        let latched: Vec<String> = view
            .latched
            .iter()
            .map(|latched| {
                format!(
                    "{} ({})",
                    key_text(&latched.reason),
                    latched.action.as_str()
                )
            })
            .collect();
        out.push_str(&format!("  {:<18} {}\n", "latched", latched.join(", ")));
    }
    out.push_str(&format!(
        "  {:<18} {}\n",
        "set on this Run",
        limits_text(&view.limits)
    ));
    for note in &view.notes {
        out.push_str(&format!("  note: {note}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_args() -> SetArgs {
        SetArgs {
            target: Some("my-agent".into()),
            selector: None,
            tokens_per_run: None,
            tokens_cumulative: None,
            dollars_per_run: None,
            dollars_cumulative: None,
            breach_action: None,
            clear: Vec::new(),
            dry_run: false,
            yes: false,
            json: false,
        }
    }

    #[test]
    fn the_flags_build_one_change_and_bad_ones_are_usage_errors() {
        let change = change_from(&SetArgs {
            tokens_per_run: Some(500_000),
            dollars_cumulative: Some("$3.000001".into()),
            breach_action: Some("block".into()),
            clear: vec!["tokens-cumulative,dollars-per-run".into()],
            ..set_args()
        })
        .unwrap();
        assert_eq!(change.set.token_per_run, Some(500_000));
        assert_eq!(change.set.dollar_cumulative_micros, Some(Micros(3_000_001)));
        assert_eq!(change.set.breach_action, Some(BreachAction::Block));
        assert_eq!(
            change.clear,
            vec![
                RunLimitField::TokensCumulative,
                RunLimitField::DollarsPerRun
            ]
        );
        let body = change_body(&change);
        assert_eq!(body["dollars_cumulative"], "3.000001");
        assert_eq!(
            body["clear"],
            json!(["tokens-cumulative", "dollars-per-run"])
        );
        assert_eq!(body["breach_action"], "block");
        for bad in [
            set_args(),
            SetArgs {
                breach_action: Some("explode".into()),
                ..set_args()
            },
            SetArgs {
                clear: vec!["rate".into()],
                ..set_args()
            },
            SetArgs {
                dollars_per_run: Some("-1".into()),
                ..set_args()
            },
            SetArgs {
                tokens_per_run: Some(5),
                clear: vec!["tokens-per-run".into()],
                ..set_args()
            },
        ] {
            let err = change_from(&bad).unwrap_err();
            assert_eq!(crate::exit_code::classify(&*err).code(), 2, "{err}");
        }
        assert_eq!(micros_text(Micros(1_500_000)), "1.500000");
    }

    #[test]
    fn every_set_result_says_the_limit_is_run_scoped() {
        let outcome = RunLimitsOutcome {
            run_id: hekma_engine::RunId::from_wire("run_A"),
            direction: hekma_engine::LimitsDirection::Raised,
            prior_limits: RunLimits::default(),
            limits: RunLimits {
                token_per_run: Some(500),
                dollar_per_run_micros: Some(Micros(2_000_000)),
                token_cumulative: Some(9),
                dollar_cumulative_micros: Some(Micros(1)),
                breach_action: Some(BreachAction::Pause),
            },
            resolution: hekma_engine::PolicyResolution {
                tokens: hekma_engine::TokenBudget::none(),
                dollars: hekma_engine::CostCap::none(),
                action: BreachAction::Warn,
                effective_action: BreachAction::Warn,
                scent_check: hekma_engine::ScentCheckMode::Off,
                rate: None,
                sources: hekma_engine::LimitSources {
                    tokens_per_run: LimitSource::Run,
                    tokens_cumulative: LimitSource::Unset,
                    dollars_per_run: LimitSource::Unset,
                    dollars_cumulative: LimitSource::Unset,
                    breach_action: LimitSource::Default,
                    scent_check: LimitSource::Default,
                    rate: LimitSource::Unset,
                },
                fallback: None,
                unpriced: None,
                fail_closed: None,
            },
            fired: vec![hekma_engine::GovernanceDecision {
                dimension: hekma_engine::BreachDimension::Dollars,
                scope: hekma_engine::BreachScope::PerRun,
                action: BreachAction::Pause,
                limit: 2_000_000,
                observed: 2_500_000,
            }],
            re_armed: vec![hekma_engine::FENCE_REASON_ORDER[0]],
            fence_cleared: vec![hekma_engine::FENCE_REASON_ORDER[0]],
            notes: Vec::new(),
            dry_run: false,
            run_scoped: true,
        };
        let row = outcome_row(&outcome);
        assert_eq!(row.outcome, "raised");
        assert_eq!(row.field("run_scoped"), Some(&json!(true)));
        assert_eq!(row.field("note"), Some(&json!(RUN_SCOPED_NOTE)));
        let line = render_set(&row).unwrap();
        for needle in [
            "Run run_A raised",
            "tokens per-run 500",
            "dollars per-run $2.00",
            "tokens cumulative 9",
            "breach action pause",
            "re-armed tokens per-run",
            "fence lifted for tokens per-run",
            "breach: dollars per-run at $2.50",
            "set on this Run only",
            "budget.* configuration or a Policy",
        ] {
            assert!(line.contains(needle), "{needle}: {line}");
        }
        let planned = outcome_row(&RunLimitsOutcome {
            dry_run: true,
            ..outcome
        });
        assert_eq!(planned.outcome, "would-set");
        assert!(render_set(&planned)
            .unwrap()
            .contains("Run run_A would be raised"));
        assert!(render_set(&planned).unwrap().contains("would breach"));
        assert_eq!(limits_text(&RunLimits::default()), "no Run-level limit");
        assert_eq!(
            source_text(&LimitSource::Policy("team".into())),
            "policy:team"
        );
        for source in [
            LimitSource::Instance,
            LimitSource::Default,
            LimitSource::Unset,
        ] {
            assert!(!source_text(&source).is_empty());
        }
    }
}
