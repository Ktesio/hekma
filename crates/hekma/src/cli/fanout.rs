//! The shared Selector fan-out every set-taking verb rides (story 21-6, PRD
//! FR-50): `stop`, `pause`, `resume`, `drain`, `run close`, `hekma budget set`
//! and `hekma label`.
//!
//! The flow, one rule for every verb:
//!
//! 1. **resolve** — a Selector is read once from `state.db`, read-only (no
//!    host starts): no match exits `0` with a note; more than 1000 matches is
//!    refused (exit `2`), never cut.
//! 2. **dry run** — `--dry-run` plans each Run on the read-only engine and
//!    prints `would-<verb>` or `refused` with the code; nothing is written
//!    and no Service Host is started.
//! 3. **confirm** — only when more than one Run matched, and only on a
//!    terminal stdin: a pipe is never read; without a terminal the request
//!    is refused (exit `2`) naming `--yes` and `--dry-run`, before any host
//!    starts.
//! 4. **select on the host** — `POST /v0/runs/select` mints a set token for
//!    the exact Runs it matched; that set is what is shown, confirmed and
//!    acted on.
//! 5. **act** — ONE set-form call (`{"set_token": …}` in the body) acts on
//!    exactly that set, the wait bounded (10 s plus each Run's own bound,
//!    capped at 10 minutes).
//! 6. **report** — one result per Run (the human line, or the one shared
//!    `--json` document), and the exit code is the first failure's class
//!    with the count in its message. A refused Run never stops the others.

use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use hekma_engine::{Engine, RunError, RunRecord, Selector};
use serde::Serialize;
use serde_json::{json, Map, Value};

use super::agent::{map_engine_error, open_read_only_engine, route, route_bounded, RoutedError};
use super::host_client::CallError;
use crate::cli::run::{looks_like_run_id, map_run_code, run_error_class};
use crate::error::RunInvalid;
use crate::ui;

/// The act-results document's schema version (the family every verb's
/// `--json` shares).
pub(crate) const ACT_DOCUMENT_SCHEMA_VERSION: u32 = 1;

/// The longest a set act waits for its answer.
const MAX_SET_WAIT: Duration = Duration::from_secs(600);

/// What a verb acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// One Run by id.
    Run(String),
    /// An agent's open Run.
    Instance(String),
    /// Every non-ended Run a Selector matches.
    Selector(Selector),
}

impl Target {
    /// Parse `<TARGET>` or `--selector` (exactly one).
    pub(crate) fn parse(
        target: Option<&str>,
        selector: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        match (target, selector) {
            (Some(_), Some(_)) => Err(invalid(
                "name a run id or an agent, or pass --selector — not both".to_string(),
            )),
            (None, None) => Err(invalid(
                "name a run id or an agent, or pass --selector key=value[,key=value...]"
                    .to_string(),
            )),
            (None, Some(raw)) => Selector::parse(raw)
                .map(Target::Selector)
                .map_err(|reason| invalid(format!("--selector: {reason}"))),
            (Some(raw), None) if looks_like_run_id(raw) => Ok(Target::Run(raw.to_string())),
            (Some(raw), None) => Ok(Target::Instance(raw.to_string())),
        }
    }
}

/// One Run's error in a result row.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ResultError {
    pub code: String,
    pub message: String,
}

/// One Run's result row.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ActResult {
    pub run_id: String,
    pub outcome: String,
    /// The verb's own fields (the instance, the labels, the limits …).
    #[serde(flatten)]
    pub fields: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResultError>,
}

impl ActResult {
    /// A success row.
    pub(crate) fn ok(run_id: impl Into<String>, outcome: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            outcome: outcome.into(),
            fields: Map::new(),
            error: None,
        }
    }

    /// The row with one more field.
    pub(crate) fn with(mut self, key: &str, value: Value) -> Self {
        if !value.is_null() {
            self.fields.insert(key.to_string(), value);
        }
        self
    }

    /// A refused row.
    pub(crate) fn refused(run_id: impl Into<String>, refusal: &Refusal) -> Self {
        Self {
            run_id: run_id.into(),
            outcome: "refused".to_string(),
            fields: Map::new(),
            error: Some(ResultError {
                code: refusal.code.clone(),
                message: refusal.message.clone(),
            }),
        }
    }

    /// One of the verb's fields.
    pub(crate) fn field(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }
}

/// The one `--json` document every verb shares.
#[derive(Serialize)]
pub(crate) struct ActDocument<'a> {
    pub schema_version: u32,
    pub verb: &'a str,
    pub dry_run: bool,
    pub matched: usize,
    pub results: &'a [ActResult],
}

/// Why one Run's act failed: the engine's refusal (planned locally, or
/// decoded from the host), or a host-level failure — with its exit class.
pub(crate) struct Refusal {
    pub code: String,
    pub message: String,
    pub diagnostic: Box<dyn std::error::Error>,
}

impl Refusal {
    /// From an in-process [`RunError`].
    pub(crate) fn from_run_error(err: RunError) -> Self {
        let message = err.to_string();
        Self {
            code: err.code().to_string(),
            diagnostic: run_error_class(&err, message.clone()),
            message,
        }
    }

    /// From a routed call's failure.
    pub(crate) fn from_routed(err: RoutedError) -> Self {
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

    /// From a set act's per-Run `error` object (`kind`, `code`, `message`,
    /// and `engine_error` when the instance verb failed): the engine's own
    /// error decoded back when present, so the exit class never drifts.
    pub(crate) fn from_error_object(error: &Value) -> Self {
        let message = error["message"]
            .as_str()
            .unwrap_or("the Run was refused")
            .to_string();
        if let Some(decoded) = error.get("engine_error").and_then(hekma_host::wire::decode) {
            let diagnostic = match decoded {
                hekma_host::wire::WireError::Engine(err) => map_engine_error(err),
                other => RoutedError::Call(match other {
                    hekma_host::wire::WireError::Registry(err) => CallError::Registry(err),
                    hekma_host::wire::WireError::Config(err) => CallError::Config(err),
                    hekma_host::wire::WireError::Engine(err) => CallError::Engine(err),
                })
                .into_diagnostic(),
            };
            return Self {
                code: error["code"].as_str().unwrap_or("engine").to_string(),
                message: diagnostic.to_string(),
                diagnostic,
            };
        }
        let code = error["code"].as_str().unwrap_or("refused").to_string();
        Self {
            diagnostic: map_run_code(&code, message.clone()),
            code,
            message,
        }
    }
}

/// How one verb reads in the fan-out's words.
pub(crate) struct VerbWords {
    /// The `--json` document's `verb`.
    pub verb: &'static str,
    /// The past tense in "Nothing was stopped." / "not stopped".
    pub past: &'static str,
    /// The question's verb ("Stop these 3 Runs?").
    pub question: &'static str,
    /// The imperative in "pass --yes to stop them all".
    pub imperative: &'static str,
}

/// The flags every set-taking verb shares.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FanoutFlags {
    pub dry_run: bool,
    pub yes: bool,
    pub json: bool,
}

/// A set act: the route and the body (without the token), the bound each
/// Run may take, and the result row each successful answer becomes.
pub(crate) struct SetAct<'a> {
    /// `POST /v0/runs/stop`, `PATCH /v0/runs/labels`, …
    pub method: &'static str,
    pub path: &'static str,
    /// The body's other fields.
    pub body: Value,
    /// How long one Run's act may take.
    pub per_run: Duration,
    /// A successful answer's row → its human line.
    pub render: &'a dyn Fn(&ActResult) -> Option<String>,
    /// Completes a successful answer's row with the verb's own fields
    /// (the budget verb's Run-scoped note).
    pub decorate: &'a dyn Fn(&mut ActResult),
}

/// The no-op [`SetAct::decorate`].
pub(crate) fn as_answered(_: &mut ActResult) {}

/// The listing shown before acting on a set (stderr).
pub(crate) fn listing(runs: &[RunRecord]) -> String {
    let mut out = format!("The Selector matches {} Runs:\n", runs.len());
    let width = runs
        .iter()
        .map(|run| run.run_id.as_str().len())
        .max()
        .unwrap_or(6)
        .max(6);
    out.push_str(&format!(
        "  {:<width$}  {:<20}  {:<7}  AGENT\n",
        "RUN ID", "CONTROL", "STATUS"
    ));
    for run in runs {
        out.push_str(&format!(
            "  {:<width$}  {:<20}  {:<7}  {}\n",
            run.run_id.as_str(),
            run.control_depth.display_with_wire(run.wire_path),
            run.status.as_str(),
            run.instance.as_deref().unwrap_or("—"),
        ));
    }
    out
}

/// Ask before acting on several Runs. Only a terminal stdin is asked (a
/// pipe is never read, so a script or test cannot hang): otherwise the
/// request is refused, naming `--yes` and `--dry-run`.
pub(crate) fn confirm(
    runs: &[RunRecord],
    words: &VerbWords,
    is_terminal: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<bool, Box<dyn std::error::Error>> {
    write!(out, "{}", listing(runs))?;
    if !is_terminal {
        return Err(non_terminal_refusal(runs.len(), words));
    }
    let count = runs.len();
    write!(out, "{} these {count} Runs? [y/N] ", words.question)?;
    out.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Whether the selected set acts: one Run, or `--yes`, acts without a
/// question (stdin untouched); several Runs ask on a terminal, and only a
/// `y`/`yes` acts.
pub(crate) fn go_ahead(
    runs: &[RunRecord],
    words: &VerbWords,
    yes: bool,
    is_terminal: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<bool, Box<dyn std::error::Error>> {
    if runs.len() <= 1 || yes {
        return Ok(true);
    }
    confirm(runs, words, is_terminal, input, out)
}

fn non_terminal_refusal(count: usize, words: &VerbWords) -> Box<dyn std::error::Error> {
    invalid(format!(
        "the Selector matches {count} Runs and there is no terminal to confirm on — pass --yes \
         to {} them all, or --dry-run to see each Run's outcome first",
        words.imperative
    ))
}

/// The wait for a set act: 10 s plus each Run's own bound, capped.
pub(crate) fn set_wait(matched: usize, per_run: Duration) -> Duration {
    let total = Duration::from_secs(10).saturating_add(per_run.saturating_mul(matched as u32));
    total.min(MAX_SET_WAIT)
}

/// The Selector flow (the module docs' six steps) for one verb. `plan`
/// answers one Run's dry run on the read-only engine.
pub(crate) fn run_selector(
    selector: &Selector,
    words: &VerbWords,
    flags: FanoutFlags,
    plan: &dyn Fn(&Engine, &RunRecord) -> Result<ActResult, Refusal>,
    act: SetAct<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let engine = open_read_only_engine()?;
    let local = engine
        .blocking()
        .select_runs(selector)
        .map_err(|err| run_error_class(&err, err.to_string()))?;
    if local.is_empty() {
        ui::note(format!(
            "No open Run matches `{selector}`; nothing was {}.",
            words.past
        ));
        return finish(words, flags, 0, Vec::new(), None, act.render);
    }
    if flags.dry_run {
        let mut results = Vec::with_capacity(local.len());
        let mut first: Option<Refusal> = None;
        let mut failures = 0usize;
        for run in &local {
            match plan(&engine, run) {
                Ok(result) => results.push(result),
                Err(refusal) => {
                    failures += 1;
                    results.push(ActResult::refused(run.run_id.as_str(), &refusal));
                    first.get_or_insert(refusal);
                }
            }
        }
        let failure = first.map(|refusal| summary(refusal, failures, local.len(), words, true));
        return finish(words, flags, local.len(), results, failure, act.render);
    }
    // An early refusal before any host starts: several Runs, no --yes and
    // no terminal to ask on (stdin is never read).
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    if local.len() > 1 && !flags.yes && !terminal {
        eprint!("{}", listing(&local));
        return Err(non_terminal_refusal(local.len(), words));
    }
    drop(engine);
    let (token, runs) = select_on_host(selector)?;
    if runs.is_empty() {
        ui::note(format!(
            "No open Run matches `{selector}` any more; nothing was {}.",
            words.past
        ));
        return finish(words, flags, 0, Vec::new(), None, act.render);
    }
    if !go_ahead(
        &runs,
        words,
        flags.yes,
        terminal,
        &mut stdin.lock(),
        &mut std::io::stderr(),
    )? {
        ui::note(format!("Nothing was {}.", words.past));
        return finish(words, flags, runs.len(), Vec::new(), None, act.render);
    }
    let mut body = act.body.clone();
    body["set_token"] = json!(token);
    let wait = set_wait(runs.len(), act.per_run);
    let answer = route_bounded(act.method, act.path, Some(body), wait)
        .map_err(RoutedError::into_diagnostic)?;
    let rows = result_rows(answer.value.as_ref(), answer.host_pid)?;
    let mut results = Vec::with_capacity(rows.len());
    let mut first: Option<Refusal> = None;
    let mut failures = 0usize;
    for row in rows {
        let run_id = row["run_id"].as_str().unwrap_or_default().to_string();
        let outcome = row["outcome"].as_str().unwrap_or_default().to_string();
        if outcome == "refused" {
            let refusal = Refusal::from_error_object(&row["error"]);
            failures += 1;
            if !flags.json {
                eprintln!(
                    "{}",
                    ui::error_text(format!(
                        "Run {run_id} not {}: {}",
                        words.past, refusal.message
                    ))
                );
            }
            results.push(ActResult::refused(run_id, &refusal));
            first.get_or_insert(refusal);
            continue;
        }
        let mut result = ActResult::ok(run_id, outcome);
        if let Value::Object(fields) = row {
            for (key, value) in fields {
                if !matches!(key.as_str(), "run_id" | "outcome" | "error") {
                    result.fields.insert(key, value);
                }
            }
        }
        (act.decorate)(&mut result);
        results.push(result);
    }
    let matched = results.len();
    let failure = first.map(|refusal| summary(refusal, failures, matched, words, false));
    finish(words, flags, matched, results, failure, act.render)
}

/// A set act's per-Run rows. A success answer without a `results` array
/// is refused, never read as "nothing happened": the acts may have
/// committed, so the command says so and exits non-zero.
fn result_rows(
    document: Option<&Value>,
    host_pid: u32,
) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    document
        .and_then(|document| document["results"].as_array().cloned())
        .ok_or_else(|| {
            crate::error::AgentIo {
                message: format!(
                    "the local Service Host (pid {host_pid}) answered the set act without its \
                     per-Run results; the acts may still have committed — check the Runs with: \
                     hekma ps --all"
                ),
            }
            .into()
        })
}

/// Mint a set on the local Service Host: the token and the exact Runs.
pub(crate) fn select_on_host(
    selector: &Selector,
) -> Result<(String, Vec<RunRecord>), Box<dyn std::error::Error>> {
    let answer = route(
        "POST",
        "/v0/runs/select",
        Some(json!({ "selector": selector.to_string() })),
    )
    .map_err(|err| Refusal::from_routed(err).diagnostic)?;
    let document = answer.value.unwrap_or(Value::Null);
    let token = document["token"].as_str().map(str::to_string);
    // Each Run is its listing row (`RunSummary`); the record is what the
    // confirmation lists and the act addresses.
    let runs: Option<Vec<RunRecord>> =
        serde_json::from_value::<Vec<hekma_engine::RunSummary>>(document["runs"].clone())
            .ok()
            .map(|runs| runs.into_iter().map(|run| run.record).collect());
    match (token, runs) {
        (Some(token), Some(runs)) => Ok((token, runs)),
        _ => Err(crate::error::AgentIo {
            message: format!(
                "the local Service Host (pid {}) answered the selection without a set token \
                 or its Runs; nothing was changed",
                answer.host_pid
            ),
        }
        .into()),
    }
}

/// A partial failure's summary under its first refusal's class.
pub(crate) fn summary(
    refusal: Refusal,
    failures: usize,
    matched: usize,
    words: &VerbWords,
    planned: bool,
) -> Box<dyn std::error::Error> {
    let what = if planned {
        format!("would not be {}", words.past)
    } else {
        format!("were not {}", words.past)
    };
    reclassed(
        refusal.diagnostic,
        format!(
            "{failures} of {matched} Runs {what}; the first refusal: {}",
            refusal.message
        ),
    )
}

/// `diagnostic`'s exit class carrying `message` instead.
pub(crate) fn reclassed(
    diagnostic: Box<dyn std::error::Error>,
    message: String,
) -> Box<dyn std::error::Error> {
    use crate::error::{
        AgentCapabilityUnsupported, AgentInteractionTimedOut, AgentStore, RunInvalidState,
        RunNotFound,
    };
    use crate::exit_code::ExitCode;
    match crate::exit_code::classify(&*diagnostic) {
        ExitCode::NotFound => RunNotFound { message }.into(),
        ExitCode::Usage => RunInvalid { message }.into(),
        ExitCode::InvalidState => RunInvalidState { message }.into(),
        ExitCode::Unsupported => AgentCapabilityUnsupported { message }.into(),
        ExitCode::TimedOut => AgentInteractionTimedOut { message }.into(),
        _ => AgentStore { message }.into(),
    }
}

/// Print the results (one human line per Run, or the one `--json`
/// document) and return the first failure, if any.
pub(crate) fn finish(
    words: &VerbWords,
    flags: FanoutFlags,
    matched: usize,
    results: Vec<ActResult>,
    failure: Option<Box<dyn std::error::Error>>,
    render: &dyn Fn(&ActResult) -> Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if flags.json {
        let document = ActDocument {
            schema_version: ACT_DOCUMENT_SCHEMA_VERSION,
            verb: words.verb,
            dry_run: flags.dry_run,
            matched,
            results: &results,
        };
        println!("{}", serde_json::to_string_pretty(&document)?);
    } else {
        for result in &results {
            if result.outcome == "refused" {
                if flags.dry_run {
                    if let Some(error) = &result.error {
                        println!(
                            "Run {} would refuse ({}): {}",
                            result.run_id, error.code, error.message
                        );
                    }
                }
                continue;
            }
            if let Some(line) = render(result) {
                println!("{line}");
            }
        }
    }
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub(crate) fn invalid(message: String) -> Box<dyn std::error::Error> {
    RunInvalid { message }.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_act_answer_without_results_is_an_error_not_an_empty_success() {
        let rows = result_rows(Some(&json!({"results": [{"run_id": "run_A"}]})), 7).unwrap();
        assert_eq!(rows.len(), 1);
        for answer in [
            None,
            Some(json!({"schema_version": 1})),
            Some(json!({"results": 3})),
        ] {
            let err = result_rows(answer.as_ref(), 7).unwrap_err();
            assert_ne!(crate::exit_code::classify(&*err).code(), 0, "{err}");
            assert!(
                err.to_string().contains("without its per-Run results"),
                "{err}"
            );
            assert!(err.to_string().contains("pid 7"), "{err}");
        }
    }
    use hekma_engine::{ControlDepth, RunId, RunStatus};

    fn record(id: &str, instance: Option<&str>) -> RunRecord {
        RunRecord {
            run_id: RunId::from_wire(id),
            control_depth: ControlDepth::Supervised,
            wire_path: false,
            status: RunStatus::Active,
            anomalies: Vec::new(),
            parent_run_id: None,
            instance: instance.map(str::to_string),
            process_owner: true,
            end_cause: None,
            created_at: "2026-11-12T00:00:00Z".into(),
            updated_at: "2026-11-12T00:00:00Z".into(),
            ended_at: None,
        }
    }

    const STOP: VerbWords = VerbWords {
        verb: "stop",
        past: "stopped",
        question: "Stop",
        imperative: "stop",
    };

    #[test]
    fn the_target_is_exactly_one_of_a_run_an_agent_or_a_selector() {
        assert_eq!(
            Target::parse(Some("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T"), None).unwrap(),
            Target::Run("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T".into())
        );
        assert_eq!(
            Target::parse(Some("my-agent"), None).unwrap(),
            Target::Instance("my-agent".into())
        );
        assert!(matches!(
            Target::parse(None, Some("team=core")).unwrap(),
            Target::Selector(_)
        ));
        for (target, selector) in [
            (Some("a"), Some("team=core")),
            (None, None),
            (None, Some("team")),
        ] {
            assert!(Target::parse(target, selector)
                .unwrap_err()
                .is::<RunInvalid>());
        }
    }

    #[test]
    fn several_runs_are_confirmed_only_on_a_terminal_and_a_pipe_is_never_read() {
        let runs = vec![record("run_A", Some("a")), record("run_B", None)];
        let mut out = Vec::new();
        let mut never = std::io::Cursor::new(b"y\n".to_vec());
        let refused = confirm(&runs, &STOP, false, &mut never, &mut out).unwrap_err();
        assert!(refused.is::<RunInvalid>());
        let text = refused.to_string();
        assert!(
            text.contains("--yes") && text.contains("--dry-run"),
            "{text}"
        );
        assert_eq!(never.position(), 0, "a non-terminal stdin is never read");
        let listing = String::from_utf8(out).unwrap();
        assert!(
            listing.contains("RUN ID") && listing.contains("CONTROL"),
            "{listing}"
        );
        assert!(listing.contains("supervised (no wire)"), "{listing}");
        assert!(
            listing.contains("run_B") && listing.contains('—'),
            "{listing}"
        );
        for (answer, expected) in [
            ("y\n", true),
            ("YES\n", true),
            ("n\n", false),
            ("\n", false),
        ] {
            let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());
            let mut out = Vec::new();
            assert_eq!(
                confirm(&runs, &STOP, true, &mut input, &mut out).unwrap(),
                expected
            );
            assert!(String::from_utf8(out)
                .unwrap()
                .contains("Stop these 2 Runs? [y/N]"));
        }
    }

    #[test]
    fn the_interactive_gate_acts_on_yes_declines_otherwise_and_skips_the_question() {
        let two = vec![record("run_A", Some("a")), record("run_B", Some("b"))];
        let one = vec![record("run_A", Some("a"))];
        // Several Runs on a terminal: the listing, the question, the answer.
        for (answer, acts) in [("y\n", true), ("no\n", false), ("", false)] {
            let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());
            let mut out = Vec::new();
            assert_eq!(
                go_ahead(&two, &STOP, false, true, &mut input, &mut out).unwrap(),
                acts,
                "{answer:?}"
            );
            let shown = String::from_utf8(out).unwrap();
            assert!(shown.contains("The Selector matches 2 Runs"), "{shown}");
            assert!(shown.contains("Stop these 2 Runs? [y/N]"), "{shown}");
        }
        // --yes, or one Run: no question, stdin never read.
        for (runs, yes) in [(&two, true), (&one, false)] {
            let mut input = std::io::Cursor::new(b"n\n".to_vec());
            let mut out = Vec::new();
            assert!(go_ahead(runs, &STOP, yes, true, &mut input, &mut out).unwrap());
            assert_eq!(input.position(), 0);
            assert!(out.is_empty());
        }
        // Several Runs and no terminal: refused, stdin never read.
        let mut input = std::io::Cursor::new(b"y\n".to_vec());
        let err = go_ahead(&two, &STOP, false, false, &mut input, &mut Vec::new()).unwrap_err();
        assert_eq!(crate::exit_code::classify(&*err).code(), 2);
        assert_eq!(input.position(), 0);
    }

    #[test]
    fn the_set_wait_is_bounded() {
        assert_eq!(
            set_wait(0, Duration::from_secs(30)),
            Duration::from_secs(10)
        );
        assert_eq!(
            set_wait(3, Duration::from_secs(30)),
            Duration::from_secs(100)
        );
        assert_eq!(set_wait(1000, Duration::from_secs(30)), MAX_SET_WAIT);
    }

    #[test]
    fn a_partial_failure_keeps_its_first_refusals_class_and_counts() {
        let refusal = Refusal::from_run_error(RunError::VerbUnavailable {
            run_id: "run_C".into(),
            verb: "stop".into(),
            depth: "announced".into(),
            reason: "on an announced Run …".into(),
        });
        assert_eq!(refusal.code, "verb-unavailable");
        let err = summary(refusal, 1, 3, &STOP, false);
        assert_eq!(crate::exit_code::classify(&*err).code(), 4);
        assert!(
            err.to_string().starts_with("1 of 3 Runs were not stopped"),
            "{err}"
        );
        let planned = summary(
            Refusal::from_run_error(RunError::NotFound { run_id: "r".into() }),
            2,
            2,
            &STOP,
            true,
        );
        assert_eq!(crate::exit_code::classify(&*planned).code(), 3);
        assert!(
            planned.to_string().contains("would not be stopped"),
            "{planned}"
        );
        // Every class survives the reclass.
        for (diagnostic, code) in [
            (
                Box::new(crate::error::AgentCapabilityUnsupported {
                    message: "x".into(),
                }) as Box<dyn std::error::Error>,
                5,
            ),
            (
                Box::new(crate::error::AgentInteractionTimedOut {
                    message: "x".into(),
                }),
                6,
            ),
            (
                Box::new(crate::error::AgentIo {
                    message: "x".into(),
                }),
                1,
            ),
            (
                Box::new(RunInvalid {
                    message: "x".into(),
                }),
                2,
            ),
        ] {
            let again = reclassed(diagnostic, "y".into());
            assert_eq!(crate::exit_code::classify(&*again).code(), code);
        }
    }

    #[test]
    fn a_set_acts_error_object_decodes_back_to_its_class() {
        let coded = Refusal::from_error_object(&json!({
            "kind": "conflict",
            "code": "run-ended",
            "message": "Run `r` has ended, so it cannot change",
        }));
        assert_eq!(coded.code, "run-ended");
        assert_eq!(crate::exit_code::classify(&*coded.diagnostic).code(), 4);
        let engine = hekma_engine::EngineError::NotRunning {
            name: "a".into(),
            state: "stopped".into(),
        };
        let decoded = Refusal::from_error_object(&json!({
            "kind": "conflict",
            "message": engine.to_string(),
            "engine_error": hekma_host::wire::encode_engine(&engine),
        }));
        assert_eq!(crate::exit_code::classify(&*decoded.diagnostic).code(), 4);
        let bare = Refusal::from_error_object(&json!({}));
        assert_eq!(bare.code, "refused");
    }

    #[test]
    fn results_serialize_as_one_shared_document() {
        let rows = vec![
            ActResult::ok("run_A", "stopped").with("instance", json!({"name": "a"})),
            ActResult::refused(
                "run_B",
                &Refusal::from_run_error(RunError::RunEnded {
                    run_id: "run_B".into(),
                }),
            ),
        ];
        let document = ActDocument {
            schema_version: ACT_DOCUMENT_SCHEMA_VERSION,
            verb: "stop",
            dry_run: false,
            matched: 2,
            results: &rows,
        };
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(value["verb"], "stop");
        assert_eq!(value["results"][0]["instance"]["name"], "a");
        assert_eq!(value["results"][1]["error"]["code"], "run-ended");
        assert!(rows[0].field("instance").is_some());
        assert!(ActResult::ok("r", "x")
            .with("gone", Value::Null)
            .fields
            .is_empty());
    }
}
