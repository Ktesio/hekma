//! `hekma policy` — Policies (story 21-3, PRD FR-60).
//!
//! A Policy is a set of limits — Token Budgets, Cost Caps, a Breach Action —
//! the Operator sets for every Run its Selector matches by bound label.
//! Limits set explicitly on an agent or a Run outrank every Policy; the
//! stored `default` Policy fills whatever nothing else sets (its Breach
//! Action is `warn`), and it carries the model-keyed Rate table.
//!
//! Results go to stdout (human lines or the `--json` document), diagnostics
//! and refusals to stderr (AD-12). Reads and `--dry-run` open `state.db`
//! read-only and start no Service Host; each write is one `/v0/policies`
//! call routed to the state root's local Service Host (AD-20).

use std::collections::BTreeMap;

use hekma_engine::{
    parse_dollars_to_micros, BreachAction, PolicyChange, PolicyLimits, PolicyOutcome, PolicyRecord,
    Rate, RunError, Selector,
};
use serde_json::{json, Value};

use super::agent::{open_read_only_engine, route, RoutedError};
use super::host_client::{segment, CallError};
use crate::cli::run::{map_run_code, run_error_class};
use crate::error::RunInvalid;

/// The `--json` document's schema version (the policy family).
const POLICY_DOCUMENT_SCHEMA_VERSION: u32 = 1;

/// The limit flags shared by `create` and `change`.
#[derive(Clone, Debug, Default)]
pub struct LimitFlags {
    pub tokens_per_run: Option<u64>,
    pub tokens_cumulative: Option<u64>,
    /// Dollars, as typed (`3.00`, `$3`).
    pub dollars_per_run: Option<String>,
    pub dollars_cumulative: Option<String>,
    pub breach_action: Option<String>,
    /// Dollars per 1M tokens, as typed.
    pub rate_input: Option<String>,
    pub rate_output: Option<String>,
    pub rate_cached: Option<String>,
}

/// `hekma policy create|change` arguments.
pub struct WriteArgs {
    pub name: String,
    pub selector: Option<String>,
    pub limits: LimitFlags,
    /// `change` only: limits to clear (`tokens-per-run`, …, `rate`,
    /// `rate:<model>`).
    pub clear: Vec<String>,
    /// `change default` only: the model the `--rate-*` flags price.
    pub rate_for: Option<String>,
    pub dry_run: bool,
    pub json: bool,
}

fn invalid(message: impl Into<String>) -> Box<dyn std::error::Error> {
    RunInvalid {
        message: message.into(),
    }
    .into()
}

fn dollars(flag: &str, value: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let micros =
        parse_dollars_to_micros(value).map_err(|err| invalid(format!("--{flag}: {err}")))?;
    u64::try_from(micros.0).map_err(|_| invalid(format!("--{flag}: must not be negative")))
}

fn parse_action(value: &str) -> Result<BreachAction, Box<dyn std::error::Error>> {
    serde_json::from_value(Value::String(value.to_string())).map_err(|_| {
        invalid(format!(
            "--breach-action: `{value}` is not a Breach Action (warn, pause, stop or block)"
        ))
    })
}

/// The Rate the `--rate-*` flags describe, if any were given. Input and
/// output are both required: a half-set Rate would price nothing.
fn rate_from(flags: &LimitFlags) -> Result<Option<Rate>, Box<dyn std::error::Error>> {
    match (&flags.rate_input, &flags.rate_output) {
        (None, None) if flags.rate_cached.is_none() => Ok(None),
        (Some(input), Some(output)) => Ok(Some(Rate {
            input_micros_per_1m: dollars("rate-input", input)?,
            output_micros_per_1m: dollars("rate-output", output)?,
            cached_micros_per_1m: flags
                .rate_cached
                .as_deref()
                .map(|cached| dollars("rate-cached", cached))
                .transpose()?,
        })),
        _ => Err(invalid(
            "a Rate needs both --rate-input and --rate-output (dollars per 1M tokens)",
        )),
    }
}

/// Build the engine request from the flags, before anything is opened.
fn change_from(args: &WriteArgs, create: bool) -> Result<PolicyChange, Box<dyn std::error::Error>> {
    let selector = args
        .selector
        .as_deref()
        .map(|raw| Selector::parse(raw).map_err(|err| invalid(format!("--selector: {err}"))))
        .transpose()?;
    if create && selector.is_none() {
        return Err(invalid(
            "a Policy needs a --selector naming the bound labels of the Runs it governs \
             (key=value[,key=value...]); to set limits for every Run, change the default \
             Policy: hekma policy change default ...",
        ));
    }
    let flags = &args.limits;
    let rate = rate_from(flags)?;
    let mut rates = BTreeMap::new();
    let mut flat_rate = rate;
    if let Some(model) = &args.rate_for {
        let Some(rate) = rate else {
            return Err(invalid(
                "--rate-for needs --rate-input and --rate-output (dollars per 1M tokens)",
            ));
        };
        rates.insert(model.clone(), Some(rate));
        flat_rate = None;
    }
    let mut clear = Vec::new();
    for entry in args.clear.iter().flat_map(|raw| raw.split(',')) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match entry.strip_prefix("rate:") {
            Some(model) if !model.is_empty() => {
                rates.insert(model.to_string(), None);
            }
            _ => clear.push(entry.to_string()),
        }
    }
    Ok(PolicyChange {
        name: args.name.clone(),
        selector,
        set: PolicyLimits {
            tokens_per_run: flags.tokens_per_run,
            tokens_cumulative: flags.tokens_cumulative,
            dollars_per_run_micros: flags
                .dollars_per_run
                .as_deref()
                .map(|value| dollars("dollars-per-run", value))
                .transpose()?,
            dollars_cumulative_micros: flags
                .dollars_cumulative
                .as_deref()
                .map(|value| dollars("dollars-cumulative", value))
                .transpose()?,
            breach_action: flags
                .breach_action
                .as_deref()
                .map(parse_action)
                .transpose()?,
            scent_check: None,
            rate: flat_rate,
        },
        clear,
        rates,
    })
}

/// A routed refusal as the CLI's exit class.
fn routed_error(err: RoutedError) -> Box<dyn std::error::Error> {
    match err {
        RoutedError::Call(CallError::Coded { code, message }) => map_run_code(&code, message),
        other => other.into_diagnostic(),
    }
}

fn engine_error(err: RunError) -> Box<dyn std::error::Error> {
    let message = err.to_string();
    run_error_class(&err, message)
}

fn limits_summary(limits: &PolicyLimits) -> String {
    let mut parts = Vec::new();
    if let Some(tokens) = limits.tokens_per_run {
        parts.push(format!("tokens/run {tokens}"));
    }
    if let Some(tokens) = limits.tokens_cumulative {
        parts.push(format!("tokens cumulative {tokens}"));
    }
    if let Some(micros) = limits.dollars_per_run_micros {
        parts.push(format!("est. ${:.2}/run", micros as f64 / 1_000_000.0));
    }
    if let Some(micros) = limits.dollars_cumulative_micros {
        parts.push(format!(
            "est. ${:.2} cumulative",
            micros as f64 / 1_000_000.0
        ));
    }
    if let Some(action) = limits.breach_action {
        parts.push(format!("on breach {}", action_str(action)));
    }
    if let Some(rate) = limits.rate {
        parts.push(format!(
            "rate ${:.2}/${:.2} per 1M in/out",
            rate.input_micros_per_1m as f64 / 1_000_000.0,
            rate.output_micros_per_1m as f64 / 1_000_000.0
        ));
    }
    if parts.is_empty() {
        "no limits".to_string()
    } else {
        parts.join(", ")
    }
}

fn action_str(action: BreachAction) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn policy_line(policy: &PolicyRecord) -> String {
    let selector = policy
        .selector
        .as_ref()
        .map(|selector| selector.to_string())
        .unwrap_or_else(|| "(every Run)".to_string());
    format!(
        "{}  {}  {}",
        policy.name,
        selector,
        limits_summary(&policy.limits)
    )
}

/// `hekma policy list`.
pub fn list(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_read_only_engine()?;
    let policies = engine.blocking().policies().map_err(engine_error)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": POLICY_DOCUMENT_SCHEMA_VERSION,
                "policies": policies,
            }))?
        );
        return Ok(());
    }
    for policy in &policies {
        println!("{}", policy_line(policy));
    }
    Ok(())
}

/// `hekma policy show <name>`.
pub fn show(name: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_read_only_engine()?;
    let policy = engine.blocking().policy(name).map_err(engine_error)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": POLICY_DOCUMENT_SCHEMA_VERSION,
                "policy": policy,
            }))?
        );
        return Ok(());
    }
    println!("{}", policy_line(&policy));
    for (model, rate) in &policy.rates {
        println!(
            "  rate for {model}: ${:.2} in / ${:.2} out per 1M tokens (estimated)",
            rate.input_micros_per_1m as f64 / 1_000_000.0,
            rate.output_micros_per_1m as f64 / 1_000_000.0
        );
    }
    Ok(())
}

fn print_outcome(
    verb: &str,
    name: &str,
    outcome: &PolicyOutcome,
    dry_run: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": POLICY_DOCUMENT_SCHEMA_VERSION,
                "policy": outcome.record,
                "matched": outcome.matched,
                "fallbacks": outcome.fallbacks,
                "notes": outcome.notes,
                "dry_run": dry_run,
            }))?
        );
        return Ok(());
    }
    let lead = if dry_run {
        format!("would {verb} Policy `{name}`")
    } else {
        format!("{verb}d Policy `{name}`")
    };
    println!(
        "{lead}: {} open Run(s) matched, {} fall back to `warn` at their Control Depth",
        outcome.matched, outcome.fallbacks
    );
    for note in &outcome.notes {
        eprintln!("note: {note}");
    }
    Ok(())
}

fn outcome_from(document: Option<Value>) -> PolicyOutcome {
    let document = document.unwrap_or(Value::Null);
    PolicyOutcome {
        record: document
            .get("policy")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok()),
        matched: document
            .get("matched")
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize,
        fallbacks: document
            .get("fallbacks")
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize,
        notes: document
            .get("notes")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
    }
}

/// `hekma policy create <name> --selector ...`.
pub fn create(args: WriteArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.rate_for.is_some() {
        return Err(invalid(
            "--rate-for edits the default Policy's Rate table: hekma policy change default \
             --rate-for <model> ...",
        ));
    }
    let change = change_from(&args, true)?;
    let outcome = if args.dry_run {
        let engine = open_read_only_engine()?;
        engine
            .blocking()
            .create_policy(change, true)
            .map_err(engine_error)?
    } else {
        let limits = serde_json::to_value(&change.set)?;
        let mut body = json!({
            "name": change.name,
            "selector": change.selector.as_ref().map(|selector| selector.to_string()),
        });
        if let (Value::Object(body), Value::Object(limits)) = (&mut body, limits) {
            body.extend(limits);
        }
        outcome_from(
            route("POST", "/v0/policies", Some(body))
                .map_err(routed_error)?
                .value,
        )
    };
    print_outcome("create", &args.name, &outcome, args.dry_run, args.json)
}

/// `hekma policy change <name> ...`.
pub fn change(args: WriteArgs) -> Result<(), Box<dyn std::error::Error>> {
    let change = change_from(&args, false)?;
    let outcome = if args.dry_run {
        let engine = open_read_only_engine()?;
        engine
            .blocking()
            .change_policy(change, true)
            .map_err(engine_error)?
    } else {
        let mut set_rates = serde_json::Map::new();
        let mut remove_rates = Vec::new();
        for (model, rate) in &change.rates {
            match rate {
                Some(rate) => {
                    set_rates.insert(model.clone(), serde_json::to_value(rate)?);
                }
                None => remove_rates.push(model.clone()),
            }
        }
        let body = json!({
            "selector": change.selector.as_ref().map(|selector| selector.to_string()),
            "set": change.set,
            "clear": change.clear,
            "rates": { "set": set_rates, "remove": remove_rates },
        });
        let body = match body {
            Value::Object(mut map) => {
                if map.get("selector").is_some_and(Value::is_null) {
                    map.remove("selector");
                }
                Value::Object(map)
            }
            other => other,
        };
        outcome_from(
            route(
                "PATCH",
                &format!("/v0/policies/{}", segment(&args.name)),
                Some(body),
            )
            .map_err(routed_error)?
            .value,
        )
    };
    print_outcome("change", &args.name, &outcome, args.dry_run, args.json)
}

/// `hekma policy delete <name>`.
pub fn delete(name: &str, dry_run: bool, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let outcome = if dry_run {
        let engine = open_read_only_engine()?;
        engine
            .blocking()
            .delete_policy(name, true)
            .map_err(engine_error)?
    } else {
        route("DELETE", &format!("/v0/policies/{}", segment(name)), None).map_err(routed_error)?;
        PolicyOutcome::default()
    };
    if !dry_run && !json {
        println!("deleted Policy `{name}`");
        return Ok(());
    }
    print_outcome("delete", name, &outcome, dry_run, json)
}
