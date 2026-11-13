//! `hekma calls` — the metering proxy's per-call records: `list` pages
//! them newest first with filters, `show` prints one by its id.
//!
//! A read-only open, like `hekma ps`: no host is started, nothing is
//! created, nothing is written. Results go to stdout (a table, or the
//! `--json` document, byte-shape-equal to `GET /v0/calls`), the next-page
//! command and the scan-limited note to stderr.
//!
//! Exit classes: `2` an invalid flag, cursor, call id or time; `1` a store
//! failure or an unknown call id; `0` otherwise, an empty result included
//! (a note, not an error).

use hekma_engine::{
    CallError, CallListing, CallRecord, CallView, CallsQuery, CALLS_DEFAULT_LIMIT, CALLS_MAX_LIMIT,
};

use super::agent::open_read_only_engine;
use crate::error::RunInvalid;
use crate::ui;

/// The flags of `hekma calls list`.
#[derive(Clone, Debug, Default)]
pub struct ListArgs {
    pub run: Option<String>,
    pub token_id: Option<String>,
    pub outcome: Option<String>,
    pub disposition: Option<String>,
    pub status_class: Option<String>,
    pub model: Option<String>,
    pub depth: Option<String>,
    pub binding: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Only the calls that did not complete cleanly.
    pub problems: bool,
    /// The page size (`0` = the maximum; above it clamps, with a note).
    pub limit: Option<u64>,
    pub cursor: Option<String>,
    pub json: bool,
}

/// `hekma calls list [filters] [--problems] [--limit N] [--cursor C] [--json]`.
pub fn list(args: ListArgs) -> Result<(), Box<dyn std::error::Error>> {
    let limit = args.limit.unwrap_or(CALLS_DEFAULT_LIMIT as u64);
    if limit > CALLS_MAX_LIMIT as u64 {
        ui::note(format!(
            "--limit {limit} is above the page cap of {CALLS_MAX_LIMIT} — serving \
             {CALLS_MAX_LIMIT}"
        ));
    }
    let query = CallsQuery {
        run: args.run.clone(),
        token_id: args.token_id.clone(),
        outcome: args.outcome.clone(),
        disposition: args.disposition.clone(),
        status_class: args.status_class.clone(),
        model: args.model.clone(),
        depth: args.depth.clone(),
        binding: args.binding.clone(),
        from: args.from.clone(),
        to: args.to.clone(),
        view: if args.problems {
            CallView::Problems
        } else {
            CallView::All
        },
        after: args.cursor.clone(),
        limit: limit.min(CALLS_MAX_LIMIT as u64) as usize,
    };
    let filters = query.echoed_filters();
    let engine = open_read_only_engine()?;
    let listing = engine
        .blocking()
        .list_calls(query)
        .map_err(|err| call_error_class(&err))?;
    if args.json {
        let document = listing.clone().into_document(filters);
        println!("{}", serde_json::to_string_pretty(&document)?);
    } else if listing.items.is_empty() {
        ui::note(if args.cursor.is_some() {
            "no more calls after this cursor"
        } else {
            "no calls match"
        });
    } else {
        print_table(&listing);
    }
    footer(&listing, &args);
    Ok(())
}

/// `hekma calls show <CALL_ID> [--json]`.
pub fn show(call_id: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !hekma_engine::is_call_id(call_id) {
        return Err(RunInvalid {
            message: format!(
                "{call_id} is not a call id this engine records (call_ plus 26 characters)"
            ),
        }
        .into());
    }
    let engine = open_read_only_engine()?;
    let record = engine
        .blocking()
        .call(call_id)
        .map_err(|err| call_error_class(&err))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&record)?);
        return Ok(());
    }
    for (field, value) in show_lines(&record) {
        println!("{field:<14} {value}");
    }
    Ok(())
}

/// A calls refusal's exit class: a request the Operator fixes on the
/// command line is `2`; an unknown id or an unreadable store is `1`.
fn call_error_class(err: &CallError) -> Box<dyn std::error::Error> {
    match err {
        CallError::InvalidCursor { .. }
        | CallError::InvalidCallId { .. }
        | CallError::InvalidRun { .. }
        | CallError::InvalidTime { .. }
        | CallError::InvalidFilter { .. } => RunInvalid {
            message: err.to_string(),
        }
        .into(),
        CallError::NotFound { .. } => {
            format!("{err}. List the recorded calls with `hekma calls list`.").into()
        }
        CallError::Unavailable { .. } => err.to_string().into(),
    }
}

fn print_table(listing: &CallListing) {
    let columns = [
        ui::TableColumn::new("CALL", 10, 31),
        ui::TableColumn::new("RUN", 10, 30),
        // 15: `refused (block)` renders whole.
        ui::TableColumn::new("OUTCOME", 15, 17),
        ui::TableColumn::new("STATUS", 6, 6),
        ui::TableColumn::new("MODEL", 5, 24),
        ui::TableColumn::new("TOKENS (in/out)", 6, 20),
        ui::TableColumn::flexible("STARTED", 20),
    ];
    let rows = listing
        .items
        .iter()
        .map(|call| {
            vec![
                ui::TableCell::plain(call.call_id.clone()),
                ui::TableCell::plain(call.run_id.as_str().to_string()),
                ui::TableCell::plain(outcome_cell(call)),
                ui::TableCell::plain(
                    call.status
                        .map_or_else(|| "-".to_string(), |status| status.to_string()),
                ),
                ui::TableCell::plain(call.model.clone().unwrap_or_else(|| "-".to_string())),
                ui::TableCell::plain(tokens_cell(call)),
                ui::TableCell::plain(call.started_at.clone()),
            ]
        })
        .collect::<Vec<_>>();
    ui::print_table("Calls", &columns, &rows);
}

/// The outcome, with the disposition when the proxy did not pass the call.
fn outcome_cell(call: &CallRecord) -> String {
    let disposition = call.disposition.as_str();
    if disposition == "pass" {
        call.outcome.as_str().to_string()
    } else {
        format!("{} ({disposition})", call.outcome.as_str())
    }
}

/// `in/out`, with `?` for an unknown count (never a made-up zero).
fn tokens_cell(call: &CallRecord) -> String {
    let count = |n: Option<u64>| n.map_or_else(|| "?".to_string(), |n| n.to_string());
    format!("{}/{}", count(call.input_tokens), count(call.output_tokens))
}

/// The footer on stderr: the scan-limited note, the skipped segments, and
/// the exact command for the next page.
fn footer(listing: &CallListing, args: &ListArgs) {
    if listing.skipped_segments > 0 {
        ui::note(format!(
            "{} calls segment{} could not be read and {} skipped: {}",
            listing.skipped_segments,
            if listing.skipped_segments == 1 {
                ""
            } else {
                "s"
            },
            if listing.skipped_segments == 1 {
                "was"
            } else {
                "were"
            },
            listing.skipped_files.join(", ")
        ));
    }
    if listing.scan_limited {
        ui::note(format!(
            "this page stopped at the scan budget before it filled{}; the next page \
             continues the search",
            listing
                .searched_back_to
                .as_deref()
                .map(|at| format!(" (searched back to {at})"))
                .unwrap_or_default()
        ));
    }
    if let Some(cursor) = &listing.next_cursor {
        ui::note(format!("next page: {}", next_command(args, cursor)));
    }
}

/// The exact command that continues after `cursor`: every filter and the
/// page size repeat, so the next page is the SAME listing.
fn next_command(args: &ListArgs, cursor: &str) -> String {
    let mut command = "hekma calls list".to_string();
    let flags = [
        ("--run", &args.run),
        ("--token-id", &args.token_id),
        ("--outcome", &args.outcome),
        ("--disposition", &args.disposition),
        ("--status-class", &args.status_class),
        ("--model", &args.model),
        ("--depth", &args.depth),
        ("--binding", &args.binding),
        ("--from", &args.from),
        ("--to", &args.to),
    ];
    for (flag, value) in flags {
        if let Some(value) = value {
            command.push_str(&format!(" {flag} {}", shell_word(value)));
        }
    }
    if args.problems {
        command.push_str(" --problems");
    }
    if let Some(limit) = args.limit {
        command.push_str(&format!(" --limit {}", limit.min(CALLS_MAX_LIMIT as u64)));
    }
    command.push_str(&format!(" --cursor {cursor}"));
    command
}

/// A flag value quoted when the shell would split or expand it.
fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:+/=@".contains(&b))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// One record as `field value` lines (`hekma calls show`).
fn show_lines(call: &CallRecord) -> Vec<(&'static str, String)> {
    let opt = |value: &Option<String>| value.clone().unwrap_or_else(|| "-".to_string());
    let count = |n: Option<u64>| n.map_or_else(|| "unknown".to_string(), |n| n.to_string());
    vec![
        ("call", call.call_id.clone()),
        ("run", call.run_id.as_str().to_string()),
        ("token id", opt(&call.token_id)),
        ("control", call.control_depth.clone()),
        ("binding", opt(&call.binding)),
        ("disposition", call.disposition.as_str().to_string()),
        ("outcome", call.outcome.as_str().to_string()),
        (
            "status",
            call.status
                .map_or_else(|| "-".to_string(), |status| status.to_string()),
        ),
        ("status class", call.status_class.as_str().to_string()),
        ("model", opt(&call.model)),
        ("input tokens", count(call.input_tokens)),
        ("output tokens", count(call.output_tokens)),
        ("cached tokens", count(call.cached_tokens)),
        ("started", call.started_at.clone()),
        ("ended", call.ended_at.clone()),
        ("latency", format!("{} µs", call.latency_us)),
        (
            "upstream",
            call.upstream_us
                .map_or_else(|| "-".to_string(), |us| format!("{us} µs")),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_page_command_repeats_every_filter_and_quotes_what_the_shell_would_split() {
        let args = ListArgs {
            run: Some("run_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()),
            model: Some("gpt 4o's".to_string()),
            from: Some("2026-11-13T00:00:00Z".to_string()),
            problems: true,
            limit: Some(5_000),
            ..ListArgs::default()
        };
        assert_eq!(
            next_command(&args, "v1:abc"),
            "hekma calls list --run run_01ARZ3NDEKTSV4RRFFQ69G5FAV --model 'gpt 4o'\\''s' \
             --from 2026-11-13T00:00:00Z --problems --limit 1000 --cursor v1:abc"
        );
    }

    #[test]
    fn refusals_map_to_their_exit_classes() {
        let invalid = call_error_class(&CallError::InvalidTime {
            detail: "bad".to_string(),
        });
        assert!(invalid.is::<RunInvalid>());
        let missing = call_error_class(&CallError::NotFound {
            call_id: "call_x".to_string(),
        });
        assert!(!missing.is::<RunInvalid>());
        assert!(missing.to_string().contains("hekma calls list"));
        let down = call_error_class(&CallError::Unavailable {
            detail: "disk".to_string(),
        });
        assert!(!down.is::<RunInvalid>());
    }
}
