//! `hekma ps` — the exception-first Run listing: the Runs that need you,
//! newest first.
//!
//! The default view lists a Run iff it carries an Anomaly (an ended Run
//! only inside the 7-day ended-Run window) or a `held`/`silent`/`stale`
//! status; healthy open Runs are counted, not listed. `--all` lists every
//! Run inside the window. `--selector` filters by label (bound or caller);
//! `--limit` with the printed `--cursor` value pages. Results go to stdout
//! (a table, or the `--json` document), the counts footer and the
//! next-page hint to stderr. A read-only open: no host is started,
//! nothing is created, nothing is written.
//!
//! Exit classes: `2` an invalid Selector/cursor/limit, `1` a store
//! failure, `0` otherwise — an empty result included (a note, not an
//! error).

use hekma_engine::{
    render_dollars_bare, ListRunsQuery, ListingView, Micros, RunListing, LISTING_DEFAULT_LIMIT,
    LISTING_MAX_LIMIT,
};

use super::agent::open_read_only_engine;
use super::run::run_error_class;
use crate::error::RunInvalid;
use crate::ui;

/// The flags of `hekma ps`.
pub struct PsArgs {
    /// Only Runs carrying every `key=value` term (bound or caller).
    pub selector: Option<String>,
    /// List every Run inside the ended-Run window, not just the exceptions.
    pub all: bool,
    /// The page size (`0` = the maximum of 1000; above it clamps, with a
    /// note).
    pub limit: Option<u64>,
    /// Continue after a previous page's cursor.
    pub cursor: Option<String>,
    /// Print the listing as one JSON document.
    pub json: bool,
}

/// `hekma ps [--selector SEL] [--all] [--limit N] [--cursor C] [--json]`.
pub fn run(args: PsArgs) -> Result<(), Box<dyn std::error::Error>> {
    // The Selector parses BEFORE the state directory is opened (a bad
    // flag is refused first — the read discipline).
    let selector = args
        .selector
        .as_deref()
        .map(|raw| {
            raw.parse::<hekma_engine::Selector>()
                .map_err(|err| invalid(format!("--selector: {err}")))
        })
        .transpose()?;
    let limit = args.limit.unwrap_or(LISTING_DEFAULT_LIMIT as u64);
    if limit > LISTING_MAX_LIMIT as u64 {
        ui::note(format!(
            "--limit {limit} is above the page cap of {LISTING_MAX_LIMIT} — serving \
             {LISTING_MAX_LIMIT}"
        ));
    }
    let engine = open_read_only_engine()?;
    let view = if args.all {
        ListingView::All
    } else {
        ListingView::Exceptions
    };
    let listing = engine
        .blocking()
        .list_runs(ListRunsQuery {
            view,
            selector: selector.clone(),
            after: args.cursor.clone(),
            limit: limit.min(LISTING_MAX_LIMIT as u64) as usize,
        })
        .map_err(|err| run_error_class(&err, err.to_string()))?;
    let next_page = NextPage {
        all: args.all,
        selector: selector.as_ref().map(ToString::to_string),
        limit: args.limit.map(|limit| limit.min(LISTING_MAX_LIMIT as u64)),
        cursor_given: args.cursor.is_some(),
    };
    render(&listing, args.json, view, selector, &next_page)
}

/// What the footer's next-page hint must repeat so the next page is the
/// SAME listing: the view, the Selector and the page size (the cursor
/// encodes only the last run id). `cursor_given` marks a later page (the
/// empty note's wording).
#[derive(Clone)]
struct NextPage {
    all: bool,
    selector: Option<String>,
    limit: Option<u64>,
    cursor_given: bool,
}

impl NextPage {
    /// The exact command that continues after `cursor`.
    fn command(&self, cursor: &str) -> String {
        let mut command = "hekma ps".to_string();
        if self.all {
            command.push_str(" --all");
        }
        if let Some(selector) = &self.selector {
            command.push_str(&format!(" --selector {selector}"));
        }
        if let Some(limit) = self.limit {
            command.push_str(&format!(" --limit {limit}"));
        }
        command.push_str(&format!(" --cursor {cursor}"));
        command
    }
}

/// Render one page: the `--json` document on stdout, or the human table on
/// stdout with the counts footer and next-page hint on stderr.
fn render(
    listing: &RunListing,
    json: bool,
    view: ListingView,
    selector: Option<hekma_engine::Selector>,
    next_page: &NextPage,
) -> Result<(), Box<dyn std::error::Error>> {
    let all = view == ListingView::All;
    if json {
        let document = listing.clone().into_document(view, selector);
        println!("{}", serde_json::to_string_pretty(&document)?);
        footer(listing, all, next_page);
        return Ok(());
    }
    if listing.items.is_empty() {
        // The note, then the footer: the counts (the healthy open Runs
        // above all) still show on an empty page.
        ui::note(empty_note(all, next_page));
        footer(listing, all, next_page);
        return Ok(());
    }
    let columns = [
        ui::TableColumn::new("RUN", 10, 32),
        // 38: `supervised · custody off (no-held-key)` renders whole.
        ui::TableColumn::new("CONTROL", 8, 38),
        ui::TableColumn::new("STATUS", 6, 6),
        ui::TableColumn::new("ANOMALIES", 9, 24),
        ui::TableColumn::new("LABELS", 6, 24),
        // The estimate qualifier lives in the header (AI-45's narrow-column
        // rule): a truncating cell can never strip it off a dollar figure.
        ui::TableColumn::new("Usage (tok, est. $)", 8, 26),
        ui::TableColumn::flexible("Headroom (tok, est. $)", 12),
    ];
    let rows = listing
        .items
        .iter()
        .map(|item| {
            vec![
                ui::TableCell::plain(run_cell(item)),
                ui::TableCell::plain(control_cell(
                    &item
                        .record
                        .control_depth
                        .display_with_wire(item.record.wire_path),
                    item.custody.as_ref(),
                )),
                ui::TableCell::status(status_cell(item)),
                ui::TableCell::plain(anomalies_cell(item)),
                ui::TableCell::plain(labels_cell(item)),
                ui::TableCell::plain(usage_cell(item)),
                ui::TableCell::plain(headroom_cell(item)),
            ]
        })
        .collect::<Vec<_>>();
    ui::print_table("Runs", &columns, &rows);
    footer(listing, all, next_page);
    Ok(())
}

/// The CONTROL cell (story 22-2): the depth, then the custody of a Run that
/// owns a process (`supervised · custody on`, `supervised · custody off
/// (opted-out)`).
fn control_cell(depth: &str, custody: Option<&hekma_engine::CustodyView>) -> String {
    match custody {
        Some(view) => format!("{depth} · custody {}", custody_cell(Some(view))),
        None => depth.to_string(),
    }
}

/// A Run's custody (story 22-2): `on`, `off (<reason>)`, or `-` for a Run
/// that owns no process.
pub(crate) fn custody_cell(custody: Option<&hekma_engine::CustodyView>) -> String {
    match custody {
        Some(view) if view.state == "on" => "on".to_string(),
        Some(view) => format!("{} ({})", view.state, view.reason),
        None => "-".to_string(),
    }
}

/// The empty page's note, worded for what was asked: a later page, a
/// Selector, the `--all` view or the exceptions view.
fn empty_note(all: bool, next_page: &NextPage) -> String {
    if next_page.cursor_given {
        return "no more Runs after this cursor".to_string();
    }
    if let Some(selector) = &next_page.selector {
        return if all {
            format!(
                "no Runs carry {selector} — none open and none ended inside the 7-day \
                 ended-Run window"
            )
        } else {
            format!("no Runs carrying {selector} need attention")
        };
    }
    if all {
        "no Runs to list — none open and none ended inside the 7-day ended-Run window".to_string()
    } else {
        "no Runs need attention — no Anomalies and no held, silent or stale statuses".to_string()
    }
}

/// The counts footer and the exact next-page hint (stderr; stdout stays
/// parseable).
fn footer(listing: &RunListing, all: bool, next_page: &NextPage) {
    ui::note(footer_text(listing, all, next_page));
}

/// The footer's text (one construction site, unit-testable).
fn footer_text(listing: &RunListing, all: bool, next_page: &NextPage) -> String {
    let RunListing {
        counts,
        next_cursor,
        ..
    } = listing;
    let mut note = format!(
        "{} listed · {} healthy open Run{}",
        counts.listed,
        counts.healthy_open,
        if counts.healthy_open == 1 { "" } else { "s" },
    );
    if !all {
        note.push_str(" not shown (--all lists them)");
    }
    if let Some(cursor) = next_cursor {
        note.push_str(&format!(" · next page: {}", next_page.command(cursor)));
    }
    note
}

/// The RUN cell: the instance name on process-owning rows (the grain an
/// operator acts on), a wire Run's identity (`unattributed`,
/// `binding <name>`, the external id — story 23-1), else the run id (a
/// child Run inside a process).
fn run_cell(item: &hekma_engine::RunSummary) -> String {
    let cell = identity_cell(item).unwrap_or_else(|| {
        match (&item.record.instance, item.record.process_owner) {
            (Some(instance), true) => instance.clone(),
            _ => item.record.run_id.as_str().to_string(),
        }
    });
    // The legacy root route's Run is an adopted agent's (it has an
    // instance), so the note rides whatever the cell names.
    if item.before_shared_proxy {
        format!("{cell} (started before the shared proxy)")
    } else {
        cell
    }
}

/// A wire Run's rendered identity, when it has one: the unattributed Run,
/// a binding's Run, a split Run or a hint child (story 23-1). Instanceless
/// Runs without an identity fall back to their id.
fn identity_cell(item: &hekma_engine::RunSummary) -> Option<String> {
    if item.record.instance.is_some() {
        return None;
    }
    let (namespace, external) = item.identity.as_ref()?;
    let rendered = match namespace.as_str() {
        // The unattributed Run (proxy/unattributed).
        "proxy" => "unattributed".to_string(),
        // A binding's own Run (binding/<name>).
        "binding" => format!("binding {external}"),
        // A split Run (binding:<name>/<external>) or a hint child
        // (run:<parent>/<external>): the external id names the grain.
        _ => external.clone(),
    };
    Some(rendered)
}

/// The ANOMALIES cell: `breach[block]` — the breach Anomaly with each
/// latched action — beside every other Anomaly, or the honest `—`.
fn anomalies_cell(item: &hekma_engine::RunSummary) -> String {
    if item.record.anomalies.is_empty() {
        return "—".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for anomaly in &item.record.anomalies {
        if *anomaly == hekma_engine::Anomaly::Breach {
            let actions: Vec<&str> = item
                .latched
                .iter()
                .map(|latched| latched.action.as_str())
                .collect();
            if actions.is_empty() {
                parts.push("breach".to_string());
            } else {
                let mut unique = actions;
                unique.sort_unstable();
                unique.dedup();
                parts.push(format!("breach[{}]", unique.join(",")));
            }
        } else {
            parts.push(anomaly.as_str().to_string());
        }
    }
    parts.join(",")
}

/// The LABELS cell: `k=v`, a Caller label suffixed `(caller)`.
fn labels_cell(item: &hekma_engine::RunSummary) -> String {
    if item.labels.is_empty() {
        return "—".to_string();
    }
    item.labels
        .iter()
        .map(|label| {
            if label.source == hekma_engine::LabelSource::Caller {
                format!("{}={} (caller)", label.key, label.value)
            } else {
                format!("{}={}", label.key, label.value)
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The USAGE cell: the subtree tokens plus the derived dollars (the
/// estimate qualifier lives in the column header). A Run with no usage
/// shows an honest zero token count, never a fabricated `$0.00` — the
/// dollar figure rides only when the derivation priced one.
fn usage_cell(item: &hekma_engine::RunSummary) -> String {
    let tokens = item
        .usage
        .input_tokens
        .saturating_add(item.usage.output_tokens);
    let dollars = if item.usage.cost_micros.0 > 0 {
        format!(" · {}", render_dollars_bare(item.usage.cost_micros))
    } else {
        String::new()
    };
    format!("{tokens} tok{dollars}")
}

/// The HEADROOM cell: the per-run tokens and dollars remaining against the
/// RESOLVED ceilings, `policy:<name>` suffixed when a Policy sourced them,
/// and — on process-owning rows with a cumulative ceiling — the cumulative
/// token headroom appended (`· cum N tok`). The honest `—` when nothing is
/// resolved.
fn headroom_cell(item: &hekma_engine::RunSummary) -> String {
    let Some(budget) = &item.budget else {
        return "—".to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    if let (Some(_limit), Some(remaining)) =
        (budget.view.per_run_limit, budget.view.per_run_remaining)
    {
        parts.push(format!("{remaining} tok"));
    }
    if let Some(remaining) = budget.view.per_run_dollars_remaining {
        parts.push(render_dollars_bare(Micros(remaining.0.max(0))).to_string());
    }
    if parts.is_empty() {
        // A resolved action with no ceiling: show the action honestly
        // rather than an empty cell.
        return format!("({})", budget.view.breach_action.as_str());
    }
    let mut cell = parts.join(" · ");
    match &budget.sources.tokens_per_run {
        hekma_engine::LimitSource::Policy(name) => {
            cell.push_str(&format!(" policy:{name}"));
        }
        hekma_engine::LimitSource::Default => cell.push_str(" policy:default"),
        _ => {}
    }
    if let (Some(_limit), Some(remaining)) = (
        budget.view.cumulative_limit,
        budget.view.cumulative_remaining,
    ) {
        cell.push_str(&format!(" · cum {remaining} tok"));
    }
    cell
}

/// A request that cannot be honored as given (exit `2`).
fn invalid(message: String) -> Box<dyn std::error::Error> {
    RunInvalid { message }.into()
}

/// The STATUS cell (story 23-4): a held Run with parked calls shows how
/// many and how long the oldest has waited.
pub(crate) fn status_cell(item: &hekma_engine::RunSummary) -> String {
    let status = item.record.status.as_str();
    if item.held_calls == 0 {
        return status.to_string();
    }
    match item.oldest_held_age_secs {
        Some(age) => format!("{status} ({} calls, oldest {age}s)", item.held_calls),
        None => format!("{status} ({} calls)", item.held_calls),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hekma_engine::{
        Anomaly, BreachAction, BreachDimension, BreachScope, BudgetView, Label, LabelSource,
        LatchedBreach, ListingCounts, RunRecord, RunStatus, RunUsage,
    };

    fn summary(run_id: &str, owner: bool) -> hekma_engine::RunSummary {
        hekma_engine::RunSummary {
            record: RunRecord {
                run_id: hekma_engine::RunId::from_wire(run_id),
                control_depth: hekma_engine::ControlDepth::Supervised,
                wire_path: false,
                status: RunStatus::Active,
                anomalies: Vec::new(),
                parent_run_id: None,
                instance: Some("demo".to_string()),
                process_owner: owner,
                end_cause: None,
                created_at: "t".into(),
                updated_at: "t".into(),
                ended_at: None,
            },
            labels: Vec::new(),
            usage: RunUsage::default(),
            cumulative_usage: None,
            latched: Vec::new(),
            budget: None,
            kept_on_pause: false,
            identity: None,
            before_shared_proxy: false,
            custody: None,
            dispositions: Vec::new(),
            held_calls: 0,
            oldest_held_age_secs: None,
            last_hold_end: None,
        }
    }

    fn budgetted(budget: hekma_engine::RunBudgetView) -> hekma_engine::RunSummary {
        let mut item = summary("run_X", true);
        item.budget = Some(budget);
        item
    }

    #[test]
    fn the_run_cell_names_the_instance_on_owner_rows_and_the_id_on_children() {
        let owner = summary("run_OWNER", true);
        assert_eq!(run_cell(&owner), "demo");
        let child = summary("run_CHILD", false);
        assert_eq!(run_cell(&child), "run_CHILD");
    }

    #[test]
    fn the_run_cell_renders_wire_identities_and_the_legacy_route_note() {
        let wire = |namespace: &str, external: &str| {
            let mut item = summary("run_W", false);
            item.record.instance = None;
            item.identity = Some((namespace.to_string(), external.to_string()));
            item
        };
        assert_eq!(run_cell(&wire("proxy", "unattributed")), "unattributed");
        assert_eq!(run_cell(&wire("binding", "openai")), "binding openai");
        assert_eq!(run_cell(&wire("binding:openai", "team-a")), "team-a");
        // An instance's own Run keeps its name even when it has an identity.
        let mut owned = wire("binding", "openai");
        owned.record.instance = Some("demo".to_string());
        owned.record.process_owner = true;
        assert_eq!(run_cell(&owned), "demo");
        // The legacy root route's Run is an adopted agent's (it has an
        // instance), and the note still shows.
        let mut legacy = summary("run_L", true);
        legacy.before_shared_proxy = true;
        assert_eq!(run_cell(&legacy), "demo (started before the shared proxy)");
    }

    #[test]
    fn the_anomalies_cell_renders_each_latched_action_on_the_breach() {
        let mut fenced = summary("run_F", true);
        fenced.record.anomalies = vec![Anomaly::Breach, Anomaly::Failed];
        fenced.latched = vec![LatchedBreach {
            dimension: BreachDimension::Tokens,
            scope: BreachScope::PerRun,
            action: BreachAction::Block,
        }];
        assert_eq!(anomalies_cell(&fenced), "breach[block],failed");
        // A breach with no latch renders bare (an unpriced cap keeps the
        // Anomaly and never invents an action).
        let mut bare = summary("run_B", true);
        bare.record.anomalies = vec![Anomaly::Breach];
        assert_eq!(anomalies_cell(&bare), "breach");
        let clean = summary("run_C", true);
        assert_eq!(anomalies_cell(&clean), "—");
    }

    #[test]
    fn the_labels_cell_marks_caller_labels() {
        let mut labeled = summary("run_L", false);
        labeled.labels = vec![
            Label::bound("team", "core"),
            Label::caller("flow", "triage"),
        ];
        assert_eq!(labels_cell(&labeled), "team=core,flow=triage (caller)");
        assert_eq!(labels_cell(&summary("run_N", false)), "—");
    }

    #[test]
    fn the_usage_cell_renders_subtree_tokens_and_only_priced_dollars() {
        let mut used = summary("run_U", true);
        used.usage = RunUsage {
            input_tokens: 100,
            output_tokens: 50,
            cached_tokens: None,
            cost_micros: Micros(50_000),
            estimate: hekma_engine::EstimateLabel::Estimated,
        };
        assert_eq!(usage_cell(&used), "150 tok · $0.05");
        // Unpriced: tokens only, never a fabricated $0.00.
        let mut unpriced = used.clone();
        unpriced.usage.cost_micros = Micros(0);
        assert_eq!(usage_cell(&unpriced), "150 tok");
    }

    #[test]
    fn the_headroom_cell_names_the_policy_and_the_cumulative_scope() {
        use hekma_engine::{BudgetView, LimitSource, LimitSources};
        let sources = |tokens_per_run: LimitSource| LimitSources {
            tokens_per_run,
            tokens_cumulative: LimitSource::Unset,
            dollars_per_run: LimitSource::Unset,
            dollars_cumulative: LimitSource::Unset,
            breach_action: LimitSource::Default,
            scent_check: LimitSource::Default,
            rate: LimitSource::Unset,
        };
        let budget = hekma_engine::RunBudgetView {
            view: BudgetView {
                per_run_limit: Some(1_000),
                per_run_remaining: Some(500),
                cumulative_limit: Some(5_000),
                cumulative_remaining: Some(1_200),
                per_run_dollars_remaining: Some(Micros(400_000)),
                breach_action: BreachAction::Warn,
                ..empty_view()
            },
            sources: sources(LimitSource::Policy("tight".to_string())),
            effective_action: BreachAction::Warn,
        };
        let item = budgetted(budget.clone());
        assert_eq!(
            headroom_cell(&item),
            "500 tok · $0.40 policy:tight · cum 1200 tok"
        );
        // The default Policy names itself too; a child row carries no
        // cumulative block at all.
        let budget = hekma_engine::RunBudgetView {
            view: BudgetView {
                per_run_limit: Some(1_000),
                per_run_remaining: Some(500),
                per_run_dollars_remaining: Some(Micros(400_000)),
                breach_action: BreachAction::Warn,
                ..empty_view()
            },
            sources: sources(LimitSource::Default),
            effective_action: BreachAction::Warn,
        };
        let item = budgetted(budget);
        assert_eq!(headroom_cell(&item), "500 tok · $0.40 policy:default");
        // An instance-configured ceiling carries no Policy suffix.
        let budget = hekma_engine::RunBudgetView {
            view: BudgetView {
                per_run_limit: Some(1_000),
                per_run_remaining: Some(0),
                breach_action: BreachAction::Pause,
                ..empty_view()
            },
            sources: sources(LimitSource::Instance),
            effective_action: BreachAction::Pause,
        };
        let item = budgetted(budget);
        assert_eq!(headroom_cell(&item), "0 tok");
        // Nothing resolved: the honest absence.
        assert_eq!(headroom_cell(&summary("run_Z", true)), "—");
        let _ = LabelSource::Bound;
    }

    /// An all-absent [`BudgetView`] base for the cell tests.
    fn empty_view() -> BudgetView {
        BudgetView {
            per_run_limit: None,
            per_run_remaining: None,
            cumulative_limit: None,
            cumulative_remaining: None,
            per_run_cost_cap: None,
            per_run_dollars_remaining: None,
            cumulative_cost_cap: None,
            cumulative_dollars_remaining: None,
            estimate_label: None,
            breach_action: BreachAction::Warn,
            policies: Vec::new(),
        }
    }

    #[test]
    fn the_footer_names_the_counts_and_the_exact_next_page() {
        let mut listing = RunListing {
            items: Vec::new(),
            next_cursor: None,
            counts: ListingCounts {
                listed: 3,
                healthy_open: 212,
            },
        };
        let plain = NextPage {
            all: false,
            selector: None,
            limit: None,
            cursor_given: false,
        };
        assert_eq!(
            footer_text(&listing, false, &plain),
            "3 listed · 212 healthy open Runs not shown (--all lists them)"
        );
        listing.next_cursor = Some("dnVuXzE".to_string());
        assert_eq!(
            footer_text(&listing, false, &plain),
            "3 listed · 212 healthy open Runs not shown (--all lists them) · next page: \
             hekma ps --cursor dnVuXzE"
        );
        listing.counts.healthy_open = 1;
        let all = NextPage { all: true, ..plain };
        assert_eq!(
            footer_text(&listing, true, &all),
            "3 listed · 1 healthy open Run · next page: hekma ps --all --cursor dnVuXzE"
        );
    }

    #[test]
    fn the_next_page_hint_repeats_the_view_the_selector_and_the_limit() {
        // The cursor encodes only the last run id: the hint must carry the
        // rest, or the copied command pages a DIFFERENT listing.
        let next = NextPage {
            all: true,
            selector: Some("team=core,env=prod".to_string()),
            limit: Some(100),
            cursor_given: true,
        };
        assert_eq!(
            next.command("dnVuXzE"),
            "hekma ps --all --selector team=core,env=prod --limit 100 --cursor dnVuXzE"
        );
        let next = NextPage {
            all: false,
            selector: None,
            limit: Some(0),
            cursor_given: false,
        };
        assert_eq!(next.command("c"), "hekma ps --limit 0 --cursor c");
    }

    #[test]
    fn the_empty_note_names_what_was_asked() {
        let base = NextPage {
            all: false,
            selector: None,
            limit: None,
            cursor_given: false,
        };
        assert_eq!(
            empty_note(false, &base),
            "no Runs need attention — no Anomalies and no held, silent or stale statuses"
        );
        assert!(empty_note(
            true,
            &NextPage {
                all: true,
                ..base.clone()
            }
        )
        .contains("none open and none ended inside the 7-day ended-Run window"));
        let selected = NextPage {
            selector: Some("team=core".to_string()),
            ..base.clone()
        };
        assert_eq!(
            empty_note(false, &selected),
            "no Runs carrying team=core need attention"
        );
        assert!(empty_note(true, &selected).starts_with("no Runs carry team=core"));
        let later = NextPage {
            cursor_given: true,
            ..base
        };
        assert_eq!(empty_note(false, &later), "no more Runs after this cursor");
    }

    #[test]
    fn a_malformed_selector_is_refused_before_the_state_directory_opens() {
        let err = run(PsArgs {
            selector: Some("no-equals".to_string()),
            all: false,
            limit: None,
            cursor: None,
            json: false,
        })
        .unwrap_err();
        assert!(err.is::<RunInvalid>(), "{err}");
        assert!(err.to_string().contains("no-equals"), "{err}");
    }

    #[test]
    fn a_held_run_shows_its_parked_calls_and_the_oldest_age() {
        let mut item = summary("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T", false);
        item.record.status = hekma_engine::RunStatus::Held;
        assert_eq!(status_cell(&item), "held");
        item.held_calls = 3;
        assert_eq!(status_cell(&item), "held (3 calls)");
        item.oldest_held_age_secs = Some(12);
        assert_eq!(status_cell(&item), "held (3 calls, oldest 12s)");
    }
}
